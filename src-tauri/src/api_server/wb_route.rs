//! WorkBuddy 上游请求路径（T2.1/T2.4/T2.7 集成点）
//!
//! 请求流程：
//! 1. 模型级冷却检查（F-34：优先级高于 Key 级，命中直接快速失败）；
//! 2. 会话粘性双模式（T2.4/F-31）：显式 conversationId / 前 3 消息指纹 60s 窗，
//!    命中绑定则锁定账号与上游会话（双段分配）；
//! 3. 上游请求（headers 三铁律 + 强制 stream + effort 降级 + 审核模板黑名单
//!    最小改写）；
//! 4. 分级重试（T2.2/F-33）：RetrySame 同号重试 / SwitchKey 换号（401 先刷新
//!    一次凭证，T2.6）/ Fatal 透传终止；
//! 5. SSE keep-alive 15s（T2.7/F-34）+ 首字超时 10s 故障转移；
//! 6. 用量记账 + 请求级日志（含 TTFB，F-32）。
//!
//! 客户端断连（F-34 §5.5 #8，issue #41 系列 0028c93/73f3480/5abb891 修订）：
//! 转发链路三层检测——轮换/重试入口 tx.is_closed() 快速终止、停滞期
//! next_event_polling 轮询（LINE_POLL）、活跃流逐事件顶部检测；断连即终止
//! 转发并释放账号并发槽（usage 记账取断连前已收到的上游 usage 事件，
//! 不再为残缺流量保持消费上游到 EOF）。

use std::collections::HashSet;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use super::pool::PickedAccount;
use super::retry::{retry_plan, RetryAction};
use super::wb_payload;
use super::wb_sse;
use super::wb_sse::{cache_write_tokens_of, cached_tokens_of, u64_pair};
use super::wb_sticky::SessionKey;
use super::wb_upstream::{self, WbCreds};
use super::{classify_error, ApiSharedState, ErrKind, InflightGuard};
use crate::api_server::routes::{anthropic_error, openai_error, Protocol, stream_msg_or_fallback};

/// 安全获取 Mutex 锁：若锁被毒化（panic 导致），仍恢复内部数据继续运行
fn safe_lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 记录「免费模型额度耗尽」到账号池（免费额度可见性，Buddy 域，对齐 Qoder 侧
/// `note_free_quota_exhausted`）。
///
/// 触发：某账号在某模型上命中额度类错误（HardCredit）且该模型属免费档
/// （目录 rate=0，实测 hy3 / step-5-preview）。写入 wb_accounts 行的
/// `free_models[].exhausted_at`（耗尽时刻）与 `reset_at`（下次恢复时刻，
/// 从上游限额文案解析——WB 6004 类文案含「将在 YYYY-MM-DD HH:MM:SS UTC+8 重置」），
/// 供账号页显示「已用完（时刻，X 后恢复）」——回答「多个号池里哪个号在这个
/// 免费模型上还有额度」。
///
/// 只在免费档模型上记录：付费模型额度不足属常规冷却（HardCredit 已有恢复机制），
/// 标成「免费模型额度用完」会误导。目录加载失败由 free_models 内部回退内置表兜底。
fn note_free_model_exhausted(data_dir: &std::path::Path, uid: &str, model: &str, msg: Option<&str>) {
    // 命中判定：账号既有快照为主（该账号已采集过的免费模型清单），目录现查为辅
    //（快照尚未生成的首轮场景）——两侧任一命中即认定为免费模型
    let store = crate::store::db(data_dir);
    let in_snapshot = crate::store::docs::wb_pool_load(&store)
        .get("accounts")
        .and_then(Value::as_array)
        .and_then(|accs| {
            accs.iter()
                .find(|a| a.get("id").and_then(Value::as_str) == Some(uid))
        })
        .and_then(|a| a.get("free_models"))
        .and_then(Value::as_array)
        .map(|models| {
            models.iter().any(|m| {
                m.get("id")
                    .and_then(Value::as_str)
                    .map(|id| id.eq_ignore_ascii_case(model))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    let in_catalog = super::wb_catalog::free_models(data_dir).iter().any(|m| {
        m.get("id")
            .and_then(Value::as_str)
            .map(|id| id.eq_ignore_ascii_case(model))
            .unwrap_or(false)
    });
    if !in_snapshot && !in_catalog {
        return;
    }
    // 持池锁读-改-写（与 credits 回写同锁域，防整池覆盖丢写）
    let _g = crate::store::docs::wb_pool_rmw_lock();
    let mut pool = crate::store::docs::wb_pool_load(&store);
    let Some(accounts) = pool.get_mut("accounts").and_then(Value::as_array_mut) else {
        return;
    };
    let Some(a) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(uid))
    else {
        return;
    };
    let now = crate::fs_utils::now_iso()[..19].replace('T', " ");
    // 快照不存在时按目录现查先建清单（首轮场景：请求先于采集到达）
    if a.get("free_models").and_then(Value::as_array).is_none() {
        let fresh = super::wb_catalog::free_models(data_dir);
        if !fresh.is_empty() {
            a["free_models"] = json!(fresh);
            a["free_models_fetched_at"] = json!(crate::fs_utils::now_iso());
        }
    }
    let mut updated = false;
    // 恢复时刻（可选）：上游限额文案携带精确重置时间（parse_quota_reset_at）
    let reset_at = msg.and_then(parse_quota_reset_at);
    if let Some(models) = a.get_mut("free_models").and_then(Value::as_array_mut) {
        for m in models.iter_mut() {
            let hit = m
                .get("id")
                .and_then(Value::as_str)
                .map(|id| id.eq_ignore_ascii_case(model))
                .unwrap_or(false);
            if hit {
                m["exhausted_at"] = json!(now);
                if let Some(ts) = reset_at {
                    // Unix 秒 → "YYYY-MM-DD HH:MM:SS"（UTC+8，与 exhausted_at 同形态）
                    use chrono::TimeZone;
                    let txt = chrono::FixedOffset::east_opt(8 * 3600)
                        .and_then(|cn| cn.timestamp_opt(ts, 0).single())
                        .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string());
                    if let Some(txt) = txt {
                        m["reset_at"] = json!(txt);
                    }
                }
                updated = true;
                break;
            }
        }
    }
    if updated {
        let _ = crate::store::docs::wb_pool_save(&store, &pool);
        crate::fs_utils::app_log(
            data_dir,
            &format!("buddy: 免费模型额度耗尽已记录 uid={uid} model={model} at={now}"),
        );
        // 免费用量计数联动（额度中心）：计数归零 + 恢复时间
        let reset_txt = reset_at.and_then(|ts| {
            use chrono::TimeZone;
            chrono::FixedOffset::east_opt(8 * 3600)
                .and_then(|cn| cn.timestamp_opt(ts, 0).single())
                .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
        });
        super::free_usage::mark_exhausted(data_dir, "buddy", uid, model, reset_txt.as_deref());
    }
}

/// 生成上游 conversation_id（粘性绑定的第二段：客户端会话 → 上游会话）
pub(crate) fn gen_conv_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seed = (nanos as u64).wrapping_mul(0x517cc1b727220a95);
    let mut buf = [0u8; 16];
    buf[0..8].copy_from_slice(&seed.to_le_bytes());
    buf[8..16].copy_from_slice(&(seed.wrapping_add(0x9e3779b97f4a7c15)).to_le_bytes());
    let hex: String = buf.iter().map(|b| format!("{:02x}", b)).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]
    )
}

// ==================== 模型级冷却（T2.7/F-34） ====================

/// 匿名路径模型级冷却上限（修复3）：uid 未归因时即便上游文案带「次日重置」
/// 精确时刻，全局模型锁也封顶 10 分钟——不可归因的日额度语义本属账号级冻结
///（uid 已知路径走 pool.cooldown_until 到精确时刻），模型级长锁只会整池停摆
///（实测旧版锁 19 小时，期间全部请求调度层 1ms 秒拒 429）。
const MODEL_COOLDOWN_MAX_SECS: i64 = 600;

/// 模型是否处于冷却中；返回剩余秒数
pub fn model_cooling_remaining(state: &ApiSharedState, model: &str) -> Option<i64> {
    let map = safe_lock(&state.model_cooldowns);
    let (until, _) = map.get(model)?;
    let now = now_ts();
    if *until > now {
        Some(*until - now)
    } else {
        None
    }
}

/// 429 文案是否明示「仅该模型受限，可切换其他模型继续使用」——上游语义为
/// **模型级**限频，账号本身健康。SoftRate 遇到此类文案不得整号冷却：
/// 单账号区域池（海外 Buddy 仅 1 号）会被一个模型的限流毒死成整池 503，
/// 且 step-5-preview/hy3 等无关模型一并冻结（2026-10-19 实测 deepseek-v4.1-flash
/// 429 → 全海外池不可用）。模型级冷却交 note_account_model_failure 按
/// (uid, model) 记录；账号级日额度（不带「可切换」文案）仍走整号冷却。
/// 与 note_account_model_failure 的 per_model 判定同源（同一对文案短语）。
pub(crate) fn soft_rate_model_only(msg: &str) -> bool {
    msg.contains("切换其他模型") || msg.contains("switch to the other model")
}

/// 记录一次模型级失败：渐进退避 10→20→40s（封顶 40s，成功请求后清除）。
/// 带错误消息时先尝试解析精确恢复时刻（note_account_model_failure）。
pub fn note_model_failure(state: &ApiSharedState, model: &str) {
    note_account_model_failure(state, model, None, None);
}

/// 记录一次失败（带可选上游错误消息与涉事账号 uid）：
/// ① 消息含「将在 YYYY-MM-DD HH:MM:SS UTC+8 重置」文案（WB 6004 限额，agent2api
///    errors.rs 情报互证 2026-10-03）→ 这是**账号级日额度**而非全池故障：只把该账号
///    冻结到恢复时刻（`cooldown_until`，取号 `healthy` 自然排除），**不设模型级冷却**
///    ——下一个请求立刻由池内其他账号顶上。修复旧版「一个账号爆量 → 整个模型全局
///    冷却 19 小时、之后所有账号的请求都在调度层被快速 429」的停摆问题；
/// ② 解析失败/无消息 → 既有模型级渐进退避（取「渐进退避 vs 既有冷却截止」较大者）。
/// `uid` 为 None 时（匿名/未取号路径）退回模型级旧语义（冷却到精确恢复时刻）。
pub fn note_account_model_failure(
    state: &ApiSharedState,
    model: &str,
    msg: Option<&str>,
    uid: Option<&str>,
) {
    if let Some(ts) = msg.and_then(parse_quota_reset_at) {
        if let Some(uid) = uid {
            // 区分两种限额语义（用户实测 6004 诉求）：
            // ① 文案含「切换其他模型继续使用」(中英两形) = 上游明示**仅该模型**
            //    频率受限，其他模型（含积分模型）照常可用 → 按 (uid, model) 冻结；
            // ② 否则为账号级日额度（不带可切换语义）→ 整号冻结到恢复时刻。
            // 一刀切按模型会让真·日额度账号的其他模型继续被选中、反复 429。
            let per_model = msg.map(soft_rate_model_only).unwrap_or(false);
            if per_model {
                state.wb_pool.per_model_cooldown_until(uid, model, ts);
            } else {
                state.wb_pool.cooldown_until(uid, ts);
            }
            return;
        }
        // 匿名/未取号路径：只能按模型降级冷却（不能绑定具体 uid）。
        // 封顶到 [MODEL_COOLDOWN_MAX_SECS] 防止停摆，同时允许后续调用走
        // note_account_model_failure_by_model(uid) 做更细粒度的逐账号解冻。
        let mut map = safe_lock(&state.model_cooldowns);
        let (prev_until, fails) = map.get(model).copied().unwrap_or((0, 0));
        let until = ts.min(now_ts() + MODEL_COOLDOWN_MAX_SECS).max(prev_until);
        map.insert(model.to_string(), (until, fails + 1));
        return;
    }
    let mut map = safe_lock(&state.model_cooldowns);
    let (prev_until, fails) = map.get(model).copied().unwrap_or((0, 0));
    let fails = fails + 1;
    let now = now_ts();
    let delay = 10i64 << (fails - 1).min(2); // 10/20/40
    map.insert(model.to_string(), ((now + delay).max(prev_until), fails));
}

/// 从限额文案解析精确恢复时刻（agent2api parse_quota_reset_at 同源语义）：
/// 命中 `UTC+8` 字面量后向前扫描 `YYYY-MM-DD HH:MM:SS`（按 UTC+8 解释为 Unix 秒）。
/// 零正则依赖（项目约定）；有效性钳制：必须在未来且 ≤7 天——上游改措辞/时区
/// 即解析失败，调用方落回渐进退避（本函数只作快路径，不承担唯一职责）。
/// 复审修复：遍历**所有**锚点——文案可能含两个时刻（「您于 X UTC+8 触发限额，
/// 将在 Y UTC+8 重置」），首个锚点前是过去时刻（触发时间），跳过它继续找下一个，
/// 直到解析出可信的未来恢复时刻。
pub(crate) fn parse_quota_reset_at(msg: &str) -> Option<i64> {
    const DT_LEN: usize = 19; // yyyy-mm-dd hh:mm:ss
    let bytes = msg.as_bytes();
    let mut from = 0usize;
    loop {
        let anchor = msg[from..].find("UTC+8")? + from;
        from = anchor + "UTC+8".len();
        if anchor >= DT_LEN {
            // 在锚点紧前方扫描日期时间（最多 48 字节前缀）。**双向扫描**：锚点与时刻
            // 之间的间距上游不固定——中文「将在 X UTC+8」紧邻，英文「... reset at
            // X UTC+8」中间隔一个空格。只往回扫会漏掉 `anchor-DT_LEN` 右侧的合法
            // 起点（2026-10-18 实测：WB 6004 英文频率限额文案漏解析 → 账号只吃到
            // 60s 短冷却而非官方 reset 时刻，冷却结束后继续打上游连续 429）。
            let start_limit = anchor.saturating_sub(48);
            let ideal = anchor.saturating_sub(DT_LEN);
            // ① 向左回扫（间距 < DT_LEN 的文案形态）
            let mut i = ideal;
            loop {
                if let Some(ts) = try_parse_dt(bytes, i) {
                    if future_within_week(ts) {
                        return Some(ts);
                    }
                }
                if i == start_limit {
                    break;
                }
                i -= 1;
            }
            // ② 向右扫（间距 ≥ DT_LEN，即锚点前不止一个分隔符）
            let mut j = ideal + 1;
            while j + DT_LEN <= anchor {
                if let Some(ts) = try_parse_dt(bytes, j) {
                    if future_within_week(ts) {
                        return Some(ts);
                    }
                }
                j += 1;
            }
        }
    }
}

/// 未来且 ≤7 天才可信（解析到过去时刻/异常远期 = 该时刻不是恢复时刻）
fn future_within_week(ts: i64) -> bool {
    let now = now_ts();
    ts > now && ts - now <= 7 * 24 * 3600
}

/// 在 bytes[i..] 处匹配 `dddd-dd-dd dd:dd:dd` 并按 UTC+8 解析为 Unix 秒
fn try_parse_dt(b: &[u8], i: usize) -> Option<i64> {
    if i + 19 > b.len() {
        return None;
    }
    let p = &b[i..i + 19];
    let digit = |x: u8| x.is_ascii_digit();
    let sep = |x: u8, c: u8| x == c;
    let num = |s: &[u8]| -> Option<i64> {
        s.iter().try_fold(0i64, |acc, c| {
            if digit(*c) { Some(acc * 10 + (*c - b'0') as i64) } else { None }
        })
    };
    let shape = sep(p[4], b'-')
        && sep(p[7], b'-')
        && sep(p[10], b' ')
        && sep(p[13], b':')
        && sep(p[16], b':')
        && p[0..4].iter().all(|c| digit(*c))
        && digit(p[5])
        && digit(p[6])
        && digit(p[8])
        && digit(p[9])
        && digit(p[11])
        && digit(p[12])
        && digit(p[14])
        && digit(p[15])
        && digit(p[17])
        && digit(p[18]);
    if !shape {
        return None;
    }
    let (y, mo, d) = (num(&p[0..4])?, num(&p[5..7])?, num(&p[8..10])?);
    let (h, mi, s) = (num(&p[11..13])?, num(&p[14..16])?, num(&p[17..19])?);
    // 按 UTC+8 解释：Unix 时刻 = naive(UTC 解释) - 8h
    chrono::NaiveDate::from_ymd_opt(y as i32, mo as u32, d as u32)
        .and_then(|date| date.and_hms_opt(h as u32, mi as u32, s as u32))
        .map(|dt| dt.and_utc().timestamp() - 8 * 3600)
}

/// 从上游 HTTP 错误响应体提取 message（6004 限额文案随 JSON body 下发）；
/// 兼容 message / error.message / msg（腾讯系接口两种字段名都有）；
/// 解析失败返回 None → 调用方落回渐进退避
fn upstream_msg(resp_body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(resp_body).ok()?;
    v.get("message")
        .and_then(|m| m.as_str())
        .or_else(|| v.pointer("/error/message").and_then(|m| m.as_str()))
        .or_else(|| v.get("msg").and_then(|m| m.as_str()))
        .map(str::to_string)
}

/// 请求成功后清除该模型的冷却与失败计数
pub fn clear_model_failure(state: &ApiSharedState, model: &str) {
    safe_lock(&state.model_cooldowns).remove(model);
}

// ==================== 审核模板映射热更新（T2.1） ====================

/// 每请求读取外置映射表（fs_utils::read_json_cached：mtime+size 线程安全缓存，
/// 免自建缓存结构与 Mutex 内磁盘 IO）；data/ 新路径缺失回退旧根路径（存量数据
/// 兼容）；KV 缺失/损坏/空表 = 纯默认。issue #99 ⑥ 合并语义：内置默认表全量
/// 生效，自定义表按 from 逐字覆盖（merged_template_rules）——默认表新增布防
/// 规则自动对存量自定义生效，不再被旧「整体替代」语义吞掉
pub fn load_templates(state: &ApiSharedState) -> Vec<(String, String)> {
    // SQLite 化（P2）：data/wb_template_map.json → kv `wb_template_map`（热路径单行读取）
    let custom = crate::store::db(&state.data_dir)
        .kv_get::<wb_payload::TemplateMapFile>("wb_template_map")
        .into_rules();
    wb_payload::merged_template_rules(custom)
}

// ==================== 错误分类（WB 上游） ====================

/// WB 流内错误 → ErrKind：WB 无 SOLO 业务码体系，按类 HTTP 状态码 +
/// message 关键词判定（积分耗尽标记词表集中维护，F-33）
pub fn classify_wb_error(code: i64, msg: &str) -> ErrKind {
    let lower = msg.to_lowercase();
    // 11133/400001「request parameters were rejected by the model provider」
    // （input_invalid）：上游对**请求内容**的拒绝，与账号健康无关。此前落
    // Client（10 分钟冷却 + 计 err_count），buddy 池仅一个健康号时会连续
    // 熔断整池——错误分类应是 None（不冷却、不计数），由调用方原样透传客户端。
    if code == 11133 || lower.contains("request parameters were rejected") {
        return ErrKind::None;
    }
    if code == 401 || lower.contains("unauthorized") || lower.contains("token expired") {
        return ErrKind::SessionDead;
    }
    if code == 403 || lower.contains("forbidden") || lower.contains("banned") {
        return ErrKind::Forbidden;
    }
    if lower.contains("insufficient credit")
        || lower.contains("积分不足")
        || lower.contains("额度不足")
    {
        return ErrKind::HardCredit;
    }
    if code == 429
        // 限流词收窄（2026-10-20）：裸 "rate" 子串会命中 prepare/separate/
        // generated/moderation 等普通英文单词，把无关错误判成限频并给账号加
        // rate_trips（退避翻倍到 30 分钟）。限频只认明确的 rate limit 语汇。
        || lower.contains("rate limit")
        || lower.contains("rate-limit")
        || lower.contains("ratelimit")
        || lower.contains("too many")
        || lower.contains("限频")
        || lower.contains("频率")
        // P0-2 口径统一：英文 "quota exceeded" 与 HTTP 路径（mod.rs
        // is_credit_exhausted_error 有意排除该词）、Trae 4008 一致归频率限流。
        // 6004「使用量已超出频率限制…可切换其他模型」正是此类——整号 HardCredit
        // 冻结到次日 04:00 会误杀同账号的积分模型，精确冻结由并行的
        // note_account_model_failure 按重置时刻/按模型语义负责。
        || lower.contains("quota exceeded")
        || lower.contains("usage limit")
    {
        return ErrKind::SoftRate;
    }
    if code >= 500 {
        return ErrKind::Server;
    }
    // 未识别的 4xx：客户端侧错误（换号重试无意义），Client 短冷却即可；
    // 归 Server 会误触 30m 账号熔断。code<400 的未知业务码维持 Server 保守判定。
    if code >= 400 {
        return ErrKind::Client;
    }
    ErrKind::Server
}

// ==================== F-76③ 慢请求竞速对冲（取号侧编排） ====================

/// 对冲账号在途计数租约（F-76③/F-77，P0 泄漏修复）：
/// 构造即 +1（竞速窗口占用），Drop 即 -1——建连失败（闭包内 `?` 提前返回）、
/// 双败（`lines_with_first_byte_hedged` Err 路径内 Drop）、竞速胜出
/// （随 `RaceOutcome.hedge` 移交调用方，`settle_hedge` 落定后 Drop）三类
/// 退出路径均恰好释放一次，杜绝计数泄漏导致的账号永久 busy。
struct HedgeLease {
    /// 对冲账号 uid（日志 / guard 重绑定位）
    uid: String,
    /// 该账号在途计数句柄（与 `wb_pool.inflight_handle(uid)` 同一 Arc）
    counter: Arc<std::sync::atomic::AtomicU32>,
}

impl HedgeLease {
    fn acquire(state: &ApiSharedState, uid: &str) -> Self {
        let counter = state.wb_pool.inflight_handle(uid);
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self { uid: uid.to_string(), counter }
    }
}

impl Drop for HedgeLease {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 首字竞速胜者信息：生效账号（主/对冲接管者）+ 对冲计数租约
struct RaceWin {
    lines: Box<dyn Iterator<Item = String> + Send>,
    /// 生效账号 uid（对冲接管时为对冲账号）
    uid: String,
    /// 对冲侧计数租约（None = 未触发对冲；Some = 竞速已定，待 settle 释放）
    hedge: Option<HedgeLease>,
    /// 对冲接管（对冲请求先出首字）
    takeover: bool,
}

/// 首字竞速（F-76③）：对冲关闭（阈值 0）时与纯首字超时语义完全一致；开启时
/// 主请求首字超阈值 → 从池内取第二账号（走同一 busy 过滤/负载因子——主账号已
/// inflight 天然让位，受 F-77 并发上限约束）发对冲请求，先出首字者胜。
/// 对冲账号取号时在途计数 +1 由 `HedgeLease` RAII 管理全生命周期；
/// 接管时 guard 重绑到对冲账号（流计数随流存续）。
#[allow(clippy::too_many_arguments)]
fn race_first_byte(
    state: &Arc<ApiSharedState>,
    primary_uid: &str,
    converted: &[u8],
    reader: Box<dyn Read + Send>,
    tried: &HashSet<String>,
    allowed: Option<&HashSet<String>>,
    dedicated: Option<&str>,
    want_global: Option<bool>,
) -> Result<RaceWin, ()> {
    let hedge_ms = state.wb_hedge_threshold_ms.load(std::sync::atomic::Ordering::Relaxed);
    if hedge_ms == 0 {
        return wb_upstream::lines_with_first_byte_timeout(reader).map(|lines| RaceWin {
            lines,
            uid: primary_uid.to_string(),
            hedge: None,
            takeover: false,
        });
    }
    let state2 = state.clone();
    let tried2 = tried.clone();
    let allowed2 = allowed.cloned();
    let dedicated2 = dedicated.map(str::to_string);
    let want_global2 = want_global;
    let body = converted.to_vec();
    let spawn_backup = move || -> Option<(Box<dyn Read + Send>, HedgeLease)> {
        let (picked2, ev) = state2
            .wb_pool
            .pick_excluding_constrained_ev_region(&tried2, allowed2.as_ref(), dedicated2.as_deref(), want_global2)?;
        // F-77⑤ 可观测：对冲取号同样记录 busy 让位/降级事件
        if let Some(ev) = ev {
            state2.logger.log_sched_event(&ev);
        }
        // 租约先于建连获取：建连失败（下行 `?`）时随闭包局部变量 Drop 自动 -1
        let lease = HedgeLease::acquire(&state2, &picked2.uid);
        let creds2 = WbCreds {
            id: picked2.uid.clone(),
            uid: picked2.uid.clone(),
            name: String::new(),
            token: picked2.jwt.clone(),
            domain: picked2.domain.clone(),
            enterprise_id: picked2.enterprise_id.clone(),
            global_region: picked2.global_region,
        };
        let reader2 = wb_upstream::make_wb_request(&creds2, &body).ok()?;
        Some((reader2, lease))
    };
    match wb_upstream::lines_with_first_byte_hedged(reader, hedge_ms, spawn_backup) {
        Ok(out) => {
            let uid = if out.takeover {
                out.hedge
                    .as_ref()
                    .map(|l| l.uid.clone())
                    .unwrap_or_else(|| primary_uid.to_string())
            } else {
                primary_uid.to_string()
            };
            Ok(RaceWin { uid, lines: out.lines, hedge: out.hedge, takeover: out.takeover })
        }
        Err(()) => Err(()),
    }
}

/// 竞速结束后处理对冲计数与日志（F-76③/F-77）：
/// - 释放对冲账号竞速窗口的在途占用（租约 Drop，-1 恰好一次）；
/// - 接管时 guard 重绑到对冲账号（原账号解绑 -1、对冲账号 +1，流计数随流存续）；
/// - [SCHED] 日志记录 hedge_takeover / hedge_lost。
fn settle_hedge(
    state: &ApiSharedState,
    win: &mut RaceWin,
    mut guard: InflightGuard,
    primary_uid: &str,
) -> InflightGuard {
    // take 移交所有权：本函数返回前 Drop（竞速窗口计数 -1 恰好一次）
    let Some(lease) = win.hedge.take() else {
        return guard;
    };
    let hedge_uid = lease.uid.as_str();
    if win.takeover {
        state.logger.log_sched_event(&format!(
            "hedge_takeover primary={} hedge={}",
            primary_uid, hedge_uid
        ));
        guard = guard.bind_account(lease.counter.clone());
    } else {
        state.logger.log_sched_event(&format!(
            "hedge_lost primary={} hedge={}",
            primary_uid, hedge_uid
        ));
    }
    drop(lease); // 释放竞速窗口占用（接管路径先重绑流计数再释放，语义与原实现一致）
    guard
}

/// 长上下文提示（F-76④）：输入粗估超阈值时返回 (token 估算, 降档开关状态)
fn longctx_estimate(state: &ApiSharedState, peek: &Value) -> Option<(u64, bool)> {
    let t = super::wb_model_route::estimate_input_tokens(peek);
    if t < super::wb_model_route::LONGCTX_TOKEN_THRESHOLD {
        return None;
    }
    Some((
        t,
        state
            .wb_longctx_downgrade
            .load(std::sync::atomic::Ordering::Relaxed),
    ))
}

/// 上下文过大提示日志（F-76④：仅标注不做真裁剪）
fn log_longctx_hint(state: &ApiSharedState, peek: &Value, model: &str) {
    if let Some((tokens, downgrade)) = longctx_estimate(state, peek) {
        state.logger.log_sched_event(&format!(
            "longctx_hint model={} tokens≈{}k downgrade={}",
            model,
            tokens / 1000,
            downgrade
        ));
    }
}

// ==================== 流式入口 ====================

pub fn wb_stream_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
    encoded: std::collections::HashSet<String>,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    // 批次 D-1 线程隔离：流任务迁入专用阻塞池（见 mod.rs stream_runtime 注释）
    super::stream_runtime().spawn_blocking(move || {
        // inflight guard 随后台任务存续至流结束（§4.5，客户端断连由 Drop 兜底）；
        // 取号后经 bind_account 维护账号级在途计数（F-77），流结束 Drop 配对释放
        let chat_id = match proto {
            Protocol::OpenAi => format!("chatcmpl-{}", now_ts()),
            Protocol::OpenAiText => format!("cmpl-{}", now_ts()),
            Protocol::Anthropic => format!("msg_{}", now_ts()),
            Protocol::Responses => format!("resp_{}", now_ts()),
        };

        // SSE keep-alive 15s（T2.7/F-34 §5.5 #7：防中间层回收长流）。
        // P1 修复：与 routes.rs 同款 watch + DoneSignal 方案，替换旧 AtomicBool
        // 15s 轮询（主任务发完后 ticker 仍可空转至多一个 15s 周期，流终结被拖延）；
        // 主任务结束（DoneSignal Drop，含 panic 展开）置 done=true，ticker select!
        // 收到退出信号即退出 → sender 全部关闭 → 流正常终结
        let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);
        let _done = super::routes::DoneSignal(done_tx);
        {
            let tx2 = tx.clone();
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
                tick.tick().await; // 首个 tick 立即返回，跳过
                loop {
                    tokio::select! {
                        _ = tick.tick() => {
                            if tx2
                                .send(Ok(bytes::Bytes::from(": keep-alive\n\n")))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        // 主任务已结束：ticker 退出，放行流终结
                        _ = done_rx.changed() => break,
                    }
                }
            });
        }

        run_wb_stream(&state, &body_vec, &model, proto, &key_id, &chat_id, &tx, start_ts, guard, &encoded);
    });

    let stream = ReceiverStream::new(rx);
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| {
            Response::builder()
                .status(StatusCode::INTERNAL_SERVER_ERROR)
                .body(Body::from("internal server error"))
                .unwrap()
        })
}

#[allow(clippy::too_many_arguments)]
fn run_wb_stream(
    state: &Arc<ApiSharedState>,
    body_vec: &[u8],
    model: &str,
    proto: Protocol,
    key_id: &str,
    chat_id: &str,
    tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    start_ts: Instant,
    mut guard: InflightGuard,
    encoded: &std::collections::HashSet<String>,
) {
    // 请求日志附带的 API Key 展示名（匿名/未知 → 空串，日志显示 "-"）
    let key_name = super::api_keys::key_name_for(&state.data_dir, key_id);
    let peek: Value = serde_json::from_slice(body_vec).unwrap_or(json!({}));
    let sticky_key = SessionKey::from_body(&peek);
    // F-76④ 上下文过大提示（仅日志标注，不做真裁剪）
    log_longctx_hint(state, &peek, model);
    let templates = load_templates(state);
    let mut sanitize = state.wb_sanitize.load(std::sync::atomic::Ordering::Relaxed);

    // F-35 子 Key 约束：限定上游 + 专一/临期优先（匿名/无约束 Key 全空 → 走默认调度）。
    // issue #25 资源池绑定：仅当 Key 约束作用域含 buddy 池时应用（绑定 trae 的
    // Key 白名单指 Trae 账号，不得误过滤 WB 池）。
    // issue #30 混合白名单：按前缀作用域提取 Buddy 条目（空集 = 排除，不得过滤）
    let (allowed_set, dedicated, key_want) = super::api_keys::constraints_for(&state.data_dir, key_id)
        .and_then(|k| k.pool_constraints("buddy"))
        .map_or((None, None, None), |c| (c.allowed, c.dedicated, c.want_global));

    // 区域判定（国内/海外号池隔离）：按模型所属区取本区账号——国内模型不出
    // 海外号、海外模型不出国内号（上游按账号区校验模型权限，跨区必失败）。
    // 未知模型（不在目录）不过滤，保持旧透传语义。
    // Key 区域约束（2026-10-17 海外接入）显式覆盖模型推导：国内号池用国内 Key、
    // 海外号池用海外 Key 的硬隔离手段
    let want_global = key_want.or_else(|| super::wb_catalog::region_of(&state.data_dir, model));
    // 粘性首轮：命中绑定且账号 healthy → 锁定账号与上游会话（双段分配）；
    // 子 Key 限定上游不含粘性账号时忽略粘性
    let sticky0: Option<(String, String)> = state
        .wb_sticky
        .resolve(&sticky_key, now_ts())
        .and_then(|b| {
            if allowed_set.as_ref().map_or(false, |a| !a.contains(&b.uid)) {
                return None;
            }
            state
                .wb_pool
                .pick_by_uid_region(&b.uid, want_global)
                .map(|_| (b.uid, b.conv_id))
        });
    let sticky_uid: Option<String> = sticky0.as_ref().map(|(u, _)| u.clone());
    let sticky_conv: String = sticky0
        .as_ref()
        .map(|(_, c)| c.clone())
        .unwrap_or_default();
    // T5.3 按模型冷却（上游 6004「可切换其他模型继续使用」）：被限的只是
    // 该账号在这个模型上的额度，其他模型（含积分）照常可用。
    // 实现要点（2026-10-19 修正）：冷却是**状态**，tried 是「本轮已试过且失败」
    // 的**事件**集合，两者不能混。旧版把 model_cooled 预注入 tried，池内仅剩
    // 一个账号时会把唯一账号也排除 → 假 503 no_healthy_account（账号明明是
    // 健康的，只是这一个模型在冷却）。现在改为取到号之后再判断：
    // 该模型冷却中 + 池内确有其他备选 → 记入 tried 换号；
    // 该模型冷却中 + 无其他备选 → **放行**，让上游自己决定是否拒绝。
    // 冷却名单（可观测/诊断）仍保留，但不参与取号过滤。
    let model_cooled: HashSet<String> =
        state.wb_pool.model_cooled_uids(&model).into_iter().collect();

    // 首选：粘性 > 专一绑定 > 调度策略（F-77④：粘性账号 busy 且有空闲候选时让位）
    let mut first_pick: Option<PickedAccount> = sticky0
        .as_ref()
        .and_then(|(u, _)| {
            if model_cooled.contains(u.as_str()) {
                return None;
            }
            state
                .wb_pool
                .pick_sticky_yield_region(u, allowed_set.as_ref(), want_global)
                .map(|(p, ev)| {
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                })
        })
        .or_else(|| {
            dedicated
                .as_deref()
                .and_then(|uid| state.wb_pool.pick_by_uid_region(uid, want_global))
        });

    // tried 只装「本轮已试过且失败」的账号（原语义）；按模型冷却在取到号后判断
    let mut tried: HashSet<String> = HashSet::new();
    let mut refreshed: HashSet<String> = HashSet::new(); // 401 刷新每账号一次
    let mut empty_hits: usize = 0; // 空完成计数：轮换耗尽收尾区分指纹拦截与账号耗尽
    let mut last_11128_sample: Option<String> = None; // 11128 诊断样本按请求去重

    loop {
        // 客户端断连检测：通道关闭即终止轮换/重试，不再占用账号并发槽（对齐 routes.rs stream_chat）。
        // 审查 M5：断连也是请求终局，补记一行失败用量（未取号 → "none"，与下方
        // 轮换耗尽收尾同口径），防用量页 requests 缺口
        if tx.is_closed() {
            let duration_ms = start_ts.elapsed().as_millis() as u64;
            state.record_usage(true, model, "none", key_id, false, true, duration_ms, 0, 0);
            return;
        }
        // ── 取号：粘性/专一命中优先，否则按 Key 约束 + 调度策略；换号后仅走策略 ──
        let picked = match first_pick.take() {
            Some(p) => p,
            None => match state
                .wb_pool
                .pick_excluding_constrained_ev_region(&tried, allowed_set.as_ref(), dedicated.as_deref(), want_global)
            {
                Some((p, ev)) => {
                    // F-77⑤ 可观测：busy_yield / busy_fallback 调度事件
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                }
                None => {
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    state.record_usage(true, model, "none", key_id, false, true, duration_ms, 0, 0);
                    // 审查 P2：轮换耗尽收尾对齐 routes.rs（issue #57 可观测）——
                    // 空完成主导 → 明示渠道指纹拦截语义（账号/模型无关，提示更新
                    // wb_template_map.json），否则维持原文案；code 保持 no_healthy_account
                    let (healthy_total, _) = state
                        .wb_pool
                        .selectable_stats_in_region(None, want_global);
                    let (exhaust_msg, exhaust_tag) =
                        super::routes::exhaust_message(empty_hits, tried.len(), healthy_total);
                    state.logger.log_request(
                        "buddy", "POST", "/v2/chat/completions", model, true, 503, "none",
                        duration_ms, &key_name, "",
                        Some(&exhaust_msg),
                    );
                    let diag = state.wb_pool.diagnose();
                    let diag_summary: Vec<String> = diag
                        .iter()
                        .map(|d| format!("{}({})", d.name, d.reason))
                        .collect();
                    // issue #99 ①：影子风控（空完成主导）收尾落原始指纹样本，
                    // 供反查漏网触发句（与 11128 诊断同口径，样本 = 清洗前原始请求体）
                    let sample_note = if exhaust_tag == "CHANNEL_FINGERPRINT_BLOCK" {
                        format!(" 指纹样本: {}", wb_payload::fingerprint_sample(body_vec))
                    } else {
                        String::new()
                    };
                    state.logger.log_debug_line(format!(
                        "[DEBUG] {exhaust_tag} tried={} empty_hits={} pool=buddy reasons=[{}]{}",
                        tried.len(),
                        empty_hits,
                        diag_summary.join(", "),
                        sample_note,
                    ));
                    let err_body = json!({
                        "error": {"message": exhaust_msg, "type": "api_error", "code": "no_healthy_account"}
                    });
                    let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                        "data: {err_body}\n\n"
                    ))));
                    let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
                    return;
                }
            },
        };
        // T5.3 按模型冷却判定（取到号之后）：该账号只是**这个模型**被上游限流，
        // 账号本身健康、其他模型照常可用。因此仅当池内确有其他账号能顶上时才
        // 换号；否则放行——否则等于把账号停了，表现为假 503。
        if state.wb_pool.is_model_cooling(&picked.uid, model)
            && state.wb_pool.has_other_selectable_for_model(
                &picked.uid,
                allowed_set.as_ref(),
                want_global,
            )
        {
            state.logger.log_sched_event(&format!(
                "model_cooldown_skip uid={} model={} (池内有其他账号顶上)",
                picked.uid, model
            ));
            tried.insert(picked.uid.clone());
            continue;
        }
        tried.insert(picked.uid.clone());
        // F-77 账号级在途计数：取号即绑定（重试换号时 bind_account 自动解绑旧账号）。
        // 审查 P1-4 TOCTOU 收紧：普通候选 CAS 复核「在途 < 并发上限」，已满则
        // 不发起请求换下一候选（不计错误不冷却）；over_limit 候选（专一/粘性
        // 锁定、busy_fallback 降级）保留原无条件绑定语义。
        // active_uid 仅在绑定成功后更新（CAS 拒绝的候选不得污染「当前账号」展示）
        let bound = if picked.over_limit {
            guard = guard.bind_account(state.wb_pool.inflight_handle(&picked.uid));
            true
        } else {
            guard.bind_account_cas(
                state.wb_pool.inflight_handle(&picked.uid),
                state.wb_pool.concurrency_limit(),
            )
        };
        if !bound {
            continue;
        }
        *safe_lock(&state.active_uid) = Some(picked.uid.clone());

        // 上游会话 id：粘性命中复用，否则新生成（成功后绑定）
        let is_sticky_hit = sticky_uid.as_deref() == Some(picked.uid.as_str()) && !sticky_conv.is_empty();
        let conv_id = if is_sticky_hit {
            sticky_conv.clone()
        } else {
            gen_conv_id()
        };

        let catalog = super::wb_catalog::load(&state.data_dir);
        let effort = super::wb_catalog::find(&catalog, model)
            .and_then(|m| m.resolve_effort(peek.get("reasoning_effort").and_then(|v| v.as_str())));
        let mut converted = wb_payload::prepare_wb_chat_body(
            body_vec, model, &picked.uid, &conv_id, effort.as_deref(), sanitize, &templates,
        );
        let mut creds = WbCreds {
            id: picked.uid.clone(),
            uid: picked.uid.clone(),
            name: String::new(),
            token: picked.jwt.clone(),
            domain: picked.domain.clone(),
            enterprise_id: picked.enterprise_id.clone(),
            global_region: picked.global_region,
        };

        let mut same_attempt: u32 = 0;
        loop {
            // 客户端断连检测：重试等待/长路径期间离开则终止（同账号 guard 随 Drop 释放）。
            // 审查 M5：断连为请求终局，补记一行失败用量（已绑定 picked 账号）
            if tx.is_closed() {
                let duration_ms = start_ts.elapsed().as_millis() as u64;
                state.record_usage(true, model, &picked.uid, key_id, false, true, duration_ms, 0, 0);
                return;
            }
            let ttfb_start = Instant::now();
            match wb_upstream::make_wb_request(&creds, &converted) {
                Ok(reader) => {
                    // 首字超时 10s（T2.7/F-34）：超时视为上游故障 → 换号；
                    // F-76③ 慢请求竞速对冲：首字超阈值时向第二账号发对冲请求，
                    // 先出首字者胜（对冲关闭时与纯首字超时语义一致）
                    let mut win = match race_first_byte(
                        state,
                        &picked.uid,
                        &converted,
                        reader,
                        &tried,
                        allowed_set.as_ref(),
                        dedicated.as_deref(),
                        want_global,
                    ) {
                        Ok(w) => w,
                        Err(()) => {
                            state.wb_pool.note_error(&picked.uid, ErrKind::Server);
                            note_model_failure(state, model);
                            break;
                        }
                    };
                    let ttfb_ms = ttfb_start.elapsed().as_millis() as u64;
                    // 对冲计数落定 + guard 重绑（接管时生效账号 = 对冲账号）
                    guard = settle_hedge(state, &mut win, guard, &picked.uid);
                    let win_uid = win.uid.as_str();
                    // 竞速胜者行源（Box 迭代器）桥接为可中断行源：stream_forward_ex
                    // 停滞期间周期性检查断连（转发线程复用；语义与既有 ttfb 包装一致）
                    let win_lines = super::wb_upstream::InterruptibleLines::from_iterator(
                        Box::new(win.lines) as Box<dyn Iterator<Item = String> + Send>,
                    );
                    let (error_info, sent_any, failed_inline, usage) =
                        wb_sse::stream_forward_ex(win_lines, tx, proto, chat_id, model, encoded);
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    {
                        // 宽口径提取（别名 + 字符串数字形态，审查 P2）
                        let (pt, ct) = usage.as_ref().map(u64_pair).unwrap_or((0, 0));
                        // F-76① TTFT 入账：用量页 P50/P95/TTFT 分位统计。
                        // 每请求终态单行（审查问题6）：换号重试（空完成未流出 /
                        // 错误未流出，对应下方两处 break）为中间态不落账，避免
                        // requests 虚高；终态（成功 / 就地收尾 / 流内失败透传）恰落一行
                        if !(error_info.is_some() && !sent_any) {
                            // M4 审查修复：缓存读 tokens 旁路入账（宽口径别名提取；
                            // 上游未下发得 0 不入账，与 credits「不得伪造」口径一致）
                            state.record_usage_cached(
                                super::usage::UsageBucket::Wb,
                                usage.as_ref().map(cached_tokens_of).unwrap_or(0),
                            );
                            state.record_usage_ttfb(
                                true, model, win_uid, key_id, error_info.is_none() && !failed_inline,
                                true, duration_ms, pt, ct, Some(ttfb_ms),
                            );
                        }
                    }
                    match error_info {
                        Some((code, msg)) => {
                            if super::is_empty_completion(code, &msg) {
                                // sent_any 护栏（对齐 qoder_route）：内容已流出时换号重发
                                // 会向同一 SSE 流拼接第二份完整响应 → 就地收尾不再重试
                                if sent_any {
                                    state.logger.log_request_ttfb(
                                        "buddy", "POST", "/v2/chat/completions", model, true, 200, win_uid,
                                        duration_ms, Some(ttfb_ms), &key_name,
                                        &state.wb_pool.name_of(win_uid), Some("空完成（内容已流出）→ 就地收尾"),
                                    );
                                    return;
                                }
                                // 空完成（影子风控/上游异常，issue #57）：不冷却、不透传、
                                // 不绑定粘性，换号重试（收尾帧未发，重试流可续传）
                                empty_hits += 1;
                                state.logger.log_request_ttfb(
                                    "buddy", "POST", "/v2/chat/completions", model, true, 200, win_uid,
                                    duration_ms, Some(ttfb_ms), &key_name,
                                    &state.wb_pool.name_of(win_uid), Some(&format!("空完成 → 换号重试{}", wb_payload::template_hit_note())),
                                );
                                break;
                            }
                            let kind = classify_wb_error(code, &msg);
                            if kind != ErrKind::None {
                                // 模型级 429（「可切换其他模型」文案）不整号冷却：
                                // 单账号区域池会被一个模型的限流毒死成整池 503
                                if !(kind == ErrKind::SoftRate && soft_rate_model_only(&msg)) {
                                    state.wb_pool.note_error(win_uid, kind);
                                }
                                note_account_model_failure(state, model, Some(&msg), Some(win_uid));
                                // 免费模型额度耗尽记录（免费额度可见性）：额度类错误
                                // 是「账号×模型」维度的事实，池冷却只按账号记，
                                // 「哪个号在这个免费模型上还有额度」此前无观测
                                if kind == ErrKind::HardCredit {
                                    note_free_model_exhausted(
                                        &state.data_dir, win_uid, model, Some(&msg),
                                    );
                                }
                                *safe_lock(&state.last_error) =
                                    Some(format!("wb uid={} code={} msg={}", win_uid, code, msg));
                                // issue #67：硬冷却/禁用类错误解绑该账号粘性
                                if kind.unbinds_sticky() {
                                    state.wb_sticky.unbind_uid(&win_uid);
                                }
                            }
                            if !sent_any {
                                // 流未开始：错误不下发，允许换号重试
                                break;
                            }
                            state.logger.log_request_ttfb(
                                "buddy", "POST", "/v2/chat/completions", model, true, 200, win_uid,
                                duration_ms, Some(ttfb_ms), &key_name,
                                &state.wb_pool.name_of(win_uid), Some(&msg),
                            );
                            return; // 已有数据流出：就地收尾
                        }
                        None => {
                            if failed_inline {
                                // 流内失败已就地透传客户端（response.failed / error 事件
                                // 已下发）：不重试、不 note_success、不清模型冷却、不绑定
                                // 粘性会话；亦不 note_error——错误已原样给到客户端，内容类
                                // 失败计入冷却会造成账号过度冷却
                                state.logger.log_request_ttfb(
                                    "buddy", "POST", "/v2/chat/completions", model, true, 200, win_uid,
                                    duration_ms, Some(ttfb_ms), &key_name,
                                    &state.wb_pool.name_of(win_uid), Some("流内失败已透传客户端"),
                                );
                                return;
                            }
                            state.wb_pool.note_success(win_uid);
                            clear_model_failure(state, model);
                            // 绑定粘性会话（Mutex 内 re-check 防 TOCTOU）
                            state.wb_sticky.bind(&sticky_key, win_uid, &conv_id, now_ts());
                            state.wb_sticky.save(&state.data_dir);
                            state.logger.log_request_ttfb(
                                "buddy", "POST", "/v2/chat/completions", model, true, 200, win_uid,
                                duration_ms, Some(ttfb_ms), &key_name,
                                &state.wb_pool.name_of(win_uid), None,
                            );
                            return;
                        }
                    }
                }
                Err((status, resp_body, retry_after)) => {
                    // 11128 渠道风控（issue #57）：按请求指纹拦截、与账号/模型无关，
                    // 换号无意义。清洗未启用时强制清洗后同号重试一次；已清洗仍命中
                    // → 走下方 retry_plan 原样 Fatal（需更新 wb_template_map.json）
                    if status == 400 && super::retry::is_illegal_channel_error(&resp_body) {
                        if !sanitize {
                            sanitize = true;
                            state.logger.log_sched_event(&format!(
                                "11128 强制清洗重试{}", wb_payload::template_hit_note()
                            ));
                            converted = wb_payload::prepare_wb_chat_body(
                                body_vec, model, &picked.uid, &conv_id, effort.as_deref(), true, &templates,
                            );
                            std::thread::sleep(std::time::Duration::from_millis(200));
                            // 断连检测：重试等待期间客户端离开则终止（审查 M5：补记失败终态用量）
                            if tx.is_closed() {
                                let duration_ms = start_ts.elapsed().as_millis() as u64;
                                state.record_usage(true, model, &picked.uid, key_id, false, true, duration_ms, 0, 0);
                                return;
                            }
                            continue;
                        }
                        // 清洗已启用仍命中（issue #99 诊断闭环）：落原始指纹样本 +
                        // 模板命中注记，差集即漏网触发句 → UI「指纹清洗」tab 热更新；
                        // 按请求去重（换号轮换重复命中样本恒定，不重复打样）
                        let sample = wb_payload::fingerprint_sample(body_vec);
                        if last_11128_sample.as_deref() != Some(sample.as_str()) {
                            last_11128_sample = Some(sample.clone());
                            state.logger.log_sched_event(&format!(
                                "11128 指纹样本: {}{}",
                                sample,
                                wb_payload::template_hit_note()
                            ));
                            // Fatal 透传前的排查指引（仅日志，客户端响应体不变）
                            state.logger.log_sched_event(
                                "11128 清洗已启用仍被拦截：模板需更新（UI「指纹清洗」tab 热更新 wb_template_map.json），指纹样本见上方日志",
                            );
                        }
                    }
                    // 分级重试策略表（T2.2/F-33 v1.2）。
                    // 429 且文案带精确重置时刻 = 日额度/频率已耗尽，重试无意义
                    // （同非流式路径注释）：直接换号，由后续 note_account_model_failure
                    // 把该账号冻结到上游给出的时刻。
                    let retry_action = if status == 429
                        && parse_quota_reset_at(&resp_body).is_some()
                    {
                        RetryAction::SwitchKey
                    } else {
                        retry_plan(status, &resp_body, same_attempt, retry_after)
                    };
                    match retry_action {
                        RetryAction::RetrySame { delay_ms } => {
                            same_attempt += 1;
                            std::thread::sleep(std::time::Duration::from_millis(delay_ms.min(60_000)));
                            // 断连检测：重试等待期间客户端离开则终止，不再占用该账号并发槽
                            // （审查 M5：补记失败终态用量）
                            if tx.is_closed() {
                                let duration_ms = start_ts.elapsed().as_millis() as u64;
                                state.record_usage(true, model, &picked.uid, key_id, false, true, duration_ms, 0, 0);
                                return;
                            }
                            continue;
                        }
                        RetryAction::SwitchKey => {
                            // T2.6：401 → 刷新一次凭证后同号重试（每账号每请求一次）
                            if status == 401 && !refreshed.contains(&picked.uid) {
                                refreshed.insert(picked.uid.clone());
                                match wb_upstream::refresh_access_token(&state.data_dir, &picked.uid, Some(&creds.token)) {
                                    Ok(new_token) => {
                                        state.wb_pool.update_jwt(&picked.uid, &new_token);
                                        creds.token = new_token;
                                        continue;
                                    }
                                    Err(e) => {
                                        *safe_lock(&state.last_error) =
                                            Some(format!("wb refresh uid={} err={}", picked.uid, e));
                                        // 他方进程刷新中（≠真实刷新失败）：本轮直接换号，不走
                                        // classify_error→note_error，防瞬时忙态误置 disabled；
                                        // 对方刷新结果由下轮请求持锁双检/stale 对比复用
                                        if e == wb_upstream::ERR_PEER_REFRESHING {
                                            break;
                                        }
                                    }
                                }
                            }
                            let kind = classify_error(status, &resp_body);
                            // ErrKind::None（如 4001 模型配置缺失）属请求/模型级问题，
                            // 与账号健康无关——不计入账号失败，防全池被同号模型错误连坐熔断
                            // 模型级 429（「可切换其他模型」文案）不整号冷却（同流内路径）
                            if kind != ErrKind::None
                                && !(kind == ErrKind::SoftRate
                                    && upstream_msg(&resp_body)
                                        .as_deref()
                                        .is_some_and(|m| soft_rate_model_only(m)))
                            {
                                state.wb_pool.note_error(&picked.uid, kind);
                            }
                            // issue #67：硬冷却/禁用类错误解绑该账号粘性
                            if kind.unbinds_sticky() {
                                state.wb_sticky.unbind_uid(&picked.uid);
                            }
                            note_account_model_failure(state, model, upstream_msg(&resp_body).as_deref(), Some(&picked.uid));
                            // 免费模型额度耗尽记录（免费额度可见性）：非流式路径
                            if kind == ErrKind::HardCredit {
                                note_free_model_exhausted(
                                    &state.data_dir, &picked.uid, model,
                                    upstream_msg(&resp_body).as_deref(),
                                );
                            }
                            *safe_lock(&state.last_error) = Some(format!(
                                "wb uid={} status={} body={}",
                                picked.uid,
                                status,
                                safe_slice(&resp_body, 200)
                            ));
                            state.logger.log_request(
                                "buddy", "POST", "/v2/chat/completions", model, true, status, &picked.uid,
                                start_ts.elapsed().as_millis() as u64,
                                &key_name, &state.wb_pool.name_of(&picked.uid),
                                Some(&format!("upstream status={}", status)),
                            );
                            break; // 换号
                        }
                        RetryAction::Fatal => {
                            let msg = format!("upstream {} error: {}", status, safe_slice(&resp_body, 300));
                            // Fatal 只代表「本次请求换号无意义」，不等于账号没问题：
                            // 若报文里带着精确重置时刻（额度类 4xx 也常这么写），仍要把
                            // 该账号冻结到上游给的时刻，否则下一次请求会重新选中它、再
                            // Fatal 一次（审计发现此前 Fatal 分支完全不记录账号状态）。
                            note_account_model_failure(
                                state, model,
                                upstream_msg(&resp_body).as_deref(),
                                Some(&picked.uid),
                            );
                            // 终态记账对齐 SwitchKey/耗尽收尾（审查 F3）：Fatal 是请求
                            // 终局，此前仅 log_request 导致用量统计页缺口
                            state.record_usage(true, model, &picked.uid, key_id, false, true,
                                start_ts.elapsed().as_millis() as u64, 0, 0);
                            state.logger.log_request(
                                "buddy", "POST", "/v2/chat/completions", model, true, status, &picked.uid,
                                start_ts.elapsed().as_millis() as u64,
                                &key_name, &state.wb_pool.name_of(&picked.uid),
                                Some(&msg),
                            );
                            send_stream_error_wb(tx, proto, status as i64, &msg);
                            return;
                        }
                    }
                }
            }
        }
    }
}

// ==================== 非流式（上游只回 SSE → 本地聚合） ====================

pub async fn wb_aggregate_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    stream: bool,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
    encoded: std::collections::HashSet<String>,
) -> Response {
    let model_out = model.clone();
    // P2 修复：聚合含分级重试（RetrySame 退避 std::thread::sleep 最长 60s×N），
    // 长阻塞占主池会饿死鉴权等短任务，迁入 stream_runtime 专用阻塞池
    let result = super::stream_runtime().spawn_blocking(move || {
        // inflight guard 随后台任务存续至聚合完成（§4.5）；F-77 取号后绑定账号级计数
        let mut guard = guard;
        // 请求日志附带的 API Key 展示名（匿名/未知 → 空串，日志显示 "-"）
        let key_name = super::api_keys::key_name_for(&state.data_dir, &key_id);
        let peek: Value = serde_json::from_slice(&body_vec).unwrap_or(json!({}));
        let sticky_key = SessionKey::from_body(&peek);
        // F-76④ 上下文过大提示（仅日志标注，不做真裁剪）
        log_longctx_hint(&state, &peek, &model);
        let templates = load_templates(&state);
        let mut sanitize = state.wb_sanitize.load(std::sync::atomic::Ordering::Relaxed);

        // F-35 子 Key 约束（与非流式同款；issue #30 混合白名单按前缀作用域分池）
        let (allowed_set, dedicated, key_want) = super::api_keys::constraints_for(&state.data_dir, &key_id)
            .and_then(|k| k.pool_constraints("buddy"))
            .map_or((None, None, None), |c| (c.allowed, c.dedicated, c.want_global));

        // 区域判定（国内/海外号池隔离，同流式路径）；Key 区域约束显式覆盖
        let want_global = key_want.or_else(|| super::wb_catalog::region_of(&state.data_dir, &model));
        let sticky0: Option<(String, String)> = state
            .wb_sticky
            .resolve(&sticky_key, now_ts())
            .and_then(|b| {
                if allowed_set.as_ref().map_or(false, |a| !a.contains(&b.uid)) {
                    return None;
                }
                state
                    .wb_pool
                    .pick_by_uid_region(&b.uid, want_global)
                    .map(|_| (b.uid, b.conv_id))
            });
        let sticky_uid: Option<String> = sticky0.as_ref().map(|(u, _)| u.clone());
        // T5.3：按模型冷却预注入 tried
        let model_cooled: HashSet<String> =
            state.wb_pool.model_cooled_uids(&model).into_iter().collect();
        let sticky_conv: String = sticky0
            .as_ref()
            .map(|(_, c)| c.clone())
            .unwrap_or_default();
        let mut first_pick: Option<PickedAccount> = sticky0
            .as_ref()
            .and_then(|(u, _)| {
                state
                    .wb_pool
                    .pick_sticky_yield_region(u, allowed_set.as_ref(), want_global)
                    .map(|(p, ev)| {
                        if let Some(ev) = ev {
                            state.logger.log_sched_event(&ev);
                        }
                        p
                    })
            })
            .or_else(|| {
                dedicated
                    .as_deref()
                    .and_then(|uid| state.wb_pool.pick_by_uid_region(uid, want_global))
            });

        let mut tried: HashSet<String> = HashSet::new();
        let mut refreshed: HashSet<String> = HashSet::new();
        let mut empty_hits: usize = 0; // 空完成计数：轮换耗尽收尾区分指纹拦截与账号耗尽
        let mut last_11128_sample: Option<String> = None; // 11128 诊断样本按请求去重

        loop {
            let picked = match first_pick.take() {
                Some(p) => p,
                None => match state
                    .wb_pool
                    .pick_excluding_constrained_ev_region(&tried, allowed_set.as_ref(), dedicated.as_deref(), want_global)
                {
                    Some((p, ev)) => {
                        if let Some(ev) = ev {
                            state.logger.log_sched_event(&ev);
                        }
                        p
                    }
                    None => {
                        state.record_usage(true, &model, "none", &key_id, false, stream,
                            start_ts.elapsed().as_millis() as u64, 0, 0);
                        // 审查 P2：轮换耗尽收尾对齐 routes.rs——空完成主导 → 指纹拦截语义
                        let (healthy_total, _) = state
                        .wb_pool
                        .selectable_stats_in_region(None, want_global);
                        let (exhaust_msg, exhaust_tag) =
                            super::routes::exhaust_message(empty_hits, tried.len(), healthy_total);
                        // issue #99 ①：影子风控收尾落原始指纹样本（非流式路径无
                        // 收尾 [DEBUG] 行，就地补齐；样本 = 清洗前原始请求体）
                        if exhaust_tag == "CHANNEL_FINGERPRINT_BLOCK" {
                            state.logger.log_debug_line(format!(
                                "[DEBUG] {exhaust_tag} tried={} empty_hits={} model_cooled={} pool=buddy 指纹样本: {}",
                                tried.len(),
                                empty_hits,
                                model_cooled.len(),
                                wb_payload::fingerprint_sample(&body_vec),
                            ));
                        }
                        return Err(exhaust_msg);
                    }
                },
            };
            // T5.3 按模型冷却判定（同流式）：仅池内有其他备选时才换号，否则放行
            if state.wb_pool.is_model_cooling(&picked.uid, &model)
                && state.wb_pool.has_other_selectable_for_model(
                    &picked.uid,
                    allowed_set.as_ref(),
                    want_global,
                )
            {
                state.logger.log_sched_event(&format!(
                    "model_cooldown_skip uid={} model={} (池内有其他账号顶上)",
                    picked.uid, model
                ));
                tried.insert(picked.uid.clone());
                continue;
            }
            tried.insert(picked.uid.clone());
            // F-77 账号级在途计数：取号即绑定。
            // 审查 P1-4 TOCTOU 收紧：普通候选 CAS 复核「在途 < 并发上限」，已满则
            // 不发起请求换下一候选（不计错误不冷却）；over_limit 候选（专一/粘性
            // 锁定、busy_fallback 降级）保留原无条件绑定语义。
            // active_uid 仅在绑定成功后更新（CAS 拒绝的候选不得污染「当前账号」展示）
            let bound = if picked.over_limit {
                guard = guard.bind_account(state.wb_pool.inflight_handle(&picked.uid));
                true
            } else {
                guard.bind_account_cas(
                    state.wb_pool.inflight_handle(&picked.uid),
                    state.wb_pool.concurrency_limit(),
                )
            };
            if !bound {
                continue;
            }
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());

            let is_sticky_hit =
                sticky_uid.as_deref() == Some(picked.uid.as_str()) && !sticky_conv.is_empty();
            let conv_id = if is_sticky_hit {
                sticky_conv.clone()
            } else {
                gen_conv_id()
            };

            let catalog = super::wb_catalog::load(&state.data_dir);
            let effort = super::wb_catalog::find(&catalog, &model)
                .and_then(|m| m.resolve_effort(peek.get("reasoning_effort").and_then(|v| v.as_str())));
            let mut converted = wb_payload::prepare_wb_chat_body(
                &body_vec, &model, &picked.uid, &conv_id, effort.as_deref(), sanitize, &templates,
            );
            let mut creds = WbCreds {
                id: picked.uid.clone(),
                uid: picked.uid.clone(),
                name: String::new(),
                token: picked.jwt.clone(),
                domain: picked.domain.clone(),
                enterprise_id: picked.enterprise_id.clone(),
                global_region: picked.global_region,
            };

            let mut same_attempt: u32 = 0;
            loop {
                let ttfb_start = Instant::now();
                match wb_upstream::make_wb_request(&creds, &converted) {
                    Ok(reader) => {
                        // 首字超时：非流式聚合同样适用（上游只回 SSE）；
                        // F-76③ 慢请求竞速对冲（对冲关闭时与纯首字超时语义一致）
                        let mut win = match race_first_byte(
                            &state,
                            &picked.uid,
                            &converted,
                            reader,
                            &tried,
                            allowed_set.as_ref(),
                            dedicated.as_deref(),
                            want_global,
                        ) {
                            Ok(w) => w,
                            Err(()) => {
                                state.wb_pool.note_error(&picked.uid, ErrKind::Server);
                                note_model_failure(&state, &model);
                                break;
                            }
                        };
                        let ttfb_ms = ttfb_start.elapsed().as_millis() as u64;
                        guard = settle_hedge(&state, &mut win, guard, &picked.uid);
                        let win_uid = win.uid.as_str();
                        let (resp, error_info) =
                            wb_sse::aggregate(win.lines, &format!("chatcmpl-{}", now_ts()));
                        let duration_ms = start_ts.elapsed().as_millis() as u64;
                        match (resp, error_info) {
                            (Some(mut r), None) => {
                                if super::aggregated_response_is_empty(&r) {
                                    // 空完成（影子风控/上游异常，issue #57）：换号重试，
                                    // 口径同下方「empty response」（Server 级短冷却）
                                    empty_hits += 1;
                                    state.wb_pool.note_error(win_uid, ErrKind::Server);
                                    note_model_failure(&state, &model);
                                    // 每请求终态单行（审查问题6）：换号是中间态不落账
                                    // （终局由后续成功行/耗尽 none 行统一落）
                                    state.logger.log_request_ttfb(
                                        "buddy", "POST", "/v2/chat/completions", &model, stream, 502, win_uid,
                                        duration_ms, Some(ttfb_ms), &key_name,
                                        &state.wb_pool.name_of(win_uid), Some(&format!("空完成 → 换号重试{}", wb_payload::template_hit_note())),
                                    );
                                    break;
                                }
                                r["model"] = json!(model);
                                // 宽口径提取（别名 + 字符串数字形态，审查 P2）
                                let (pt, ct) = r.get("usage").map(u64_pair).unwrap_or((0, 0));
                                // M4 审查修复：缓存读 tokens 旁路入账（未下发得 0 不入账）
                                state.record_usage_cached(
                                    super::usage::UsageBucket::Wb,
                                    r.get("usage").map(cached_tokens_of).unwrap_or(0),
                                );
                                // F-76① TTFT 入账
                                state.record_usage_ttfb(true, &model, win_uid, &key_id, true, stream, duration_ms, pt, ct, Some(ttfb_ms));
                                state.wb_pool.note_success(win_uid);
                                clear_model_failure(&state, &model);
                                state.wb_sticky.bind(&sticky_key, win_uid, &conv_id, now_ts());
                                state.wb_sticky.save(&state.data_dir);
                                state.logger.log_request_ttfb(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, 200, win_uid,
                                    duration_ms, Some(ttfb_ms), &key_name,
                                    &state.wb_pool.name_of(win_uid), None,
                                );
                                return Ok(r);
                            }
                            (None, Some((code, msg))) => {
                                let kind = classify_wb_error(code, &msg);
                                if kind != ErrKind::None {
                                    // 模型级 429 不整号冷却（同流式主路径）
                                    if !(kind == ErrKind::SoftRate && soft_rate_model_only(&msg)) {
                                        state.wb_pool.note_error(win_uid, kind);
                                    }
                                    note_account_model_failure(&state, &model, Some(&msg), Some(win_uid));
                                    // 免费模型额度耗尽记录（免费额度可见性）：流式路径
                                    if kind == ErrKind::HardCredit {
                                        note_free_model_exhausted(
                                            &state.data_dir, win_uid, &model, Some(&msg),
                                        );
                                    }
                                    // issue #67：硬冷却/禁用类错误解绑该账号粘性
                                    if kind.unbinds_sticky() {
                                        state.wb_sticky.unbind_uid(&win_uid);
                                    }
                                }
                                *safe_lock(&state.last_error) =
                                    Some(format!("wb uid={} code={} msg={}", win_uid, code, msg));
                                // 每请求终态单行（审查问题6）：换号是中间态不落账
                                // （终局由后续成功行/耗尽 none 行统一落）
                                state.logger.log_request_ttfb(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, 200, win_uid,
                                    duration_ms, Some(ttfb_ms), &key_name,
                                    &state.wb_pool.name_of(win_uid), Some(&msg),
                                );
                                // 流内错误且未产出内容 → 换号重试
                                break;
                            }
                            _ => {
                                state.wb_pool.note_error(win_uid, ErrKind::Server);
                                note_model_failure(&state, &model);
                                // 每请求终态单行（审查问题6）：换号是中间态不落账
                                state.logger.log_request_ttfb(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, 502, win_uid,
                                    duration_ms, Some(ttfb_ms), &key_name,
                                    &state.wb_pool.name_of(win_uid), Some("empty response"),
                                );
                                break;
                            }
                        }
                    }
                    Err((status, resp_body, retry_after)) => {
                        // 11128 渠道风控（issue #57）：清洗未启用时强制清洗后同号重试一次
                        if status == 400 && super::retry::is_illegal_channel_error(&resp_body) {
                            if !sanitize {
                                sanitize = true;
                                state.logger.log_sched_event(&format!(
                                    "11128 强制清洗重试{}", wb_payload::template_hit_note()
                                ));
                                converted = wb_payload::prepare_wb_chat_body(
                                    &body_vec, &model, &picked.uid, &conv_id, effort.as_deref(), true, &templates,
                                );
                                std::thread::sleep(std::time::Duration::from_millis(200));
                                continue;
                            }
                            // 清洗已启用仍命中（issue #99 诊断闭环）：落原始指纹样本
                            // + 按请求去重（换号轮换重复命中样本恒定）；Fatal 透传前
                            // 附排查指引（仅日志，客户端响应体不变）
                            let sample = wb_payload::fingerprint_sample(&body_vec);
                            if last_11128_sample.as_deref() != Some(sample.as_str()) {
                                last_11128_sample = Some(sample.clone());
                                state.logger.log_sched_event(&format!(
                                    "11128 指纹样本: {}{}",
                                    sample,
                                    wb_payload::template_hit_note()
                                ));
                                state.logger.log_sched_event(
                                    "11128 清洗已启用仍被拦截：模板需更新（UI「指纹清洗」tab 热更新 wb_template_map.json），指纹样本见上方日志",
                                );
                            }
                        }
                        // 上游已明确给出「日额度/频率耗尽 + 精确重置时刻」（WB 6004 一族）：
                        // 这不是瞬时抖动，重试必然继续 429。此前 retry_plan 对 429 一律
                        // 先同号重试 3 次（共 ~6s），把已超限的账号又硬打三遍，而池内
                        // 其他健康账号一次都没被选中——用户感知为「明明还有余额的号却
                        // 一直打在爆量的号上」。判据用「能解析出未来重置时刻」而非状态码，
                        // 因为 429 既有瞬时限频也有日额度耗尽，前者仍应同号短退避。
                        let quota_exhausted_429 =
                            status == 429 && parse_quota_reset_at(&resp_body).is_some();
                        let action = if quota_exhausted_429 {
                            RetryAction::SwitchKey
                        } else {
                            retry_plan(status, &resp_body, same_attempt, retry_after)
                        };
                        match action {
                            RetryAction::RetrySame { delay_ms } => {
                                same_attempt += 1;
                                std::thread::sleep(std::time::Duration::from_millis(delay_ms.min(60_000)));
                                continue;
                            }
                            RetryAction::SwitchKey => {
                                if status == 401 && !refreshed.contains(&picked.uid) {
                                    refreshed.insert(picked.uid.clone());
                                    match wb_upstream::refresh_access_token(&state.data_dir, &picked.uid, Some(&creds.token)) {
                                        Ok(new_token) => {
                                            state.wb_pool.update_jwt(&picked.uid, &new_token);
                                            creds.token = new_token;
                                            continue;
                                        }
                                        Err(e) => {
                                            *safe_lock(&state.last_error) =
                                                Some(format!("wb refresh uid={} err={}", picked.uid, e));
                                            // 他方进程刷新中：本轮换号且不走 note_error（防瞬时
                                            // 忙态误置 disabled），对方刷新结果由下轮请求复用
                                            if e == wb_upstream::ERR_PEER_REFRESHING {
                                                break;
                                            }
                                        }
                                    }
                                }
                            let kind = classify_error(status, &resp_body);
                            // ErrKind::None（如 4001 模型配置缺失）属请求/模型级问题，
                            // 与账号健康无关——不计入账号失败，防全池被同号模型错误连坐熔断
                            // 模型级 429（「可切换其他模型」文案）不整号冷却（同流内路径）
                            if kind != ErrKind::None
                                && !(kind == ErrKind::SoftRate
                                    && upstream_msg(&resp_body)
                                        .as_deref()
                                        .is_some_and(|m| soft_rate_model_only(m)))
                            {
                                state.wb_pool.note_error(&picked.uid, kind);
                            }
                            // issue #67：硬冷却/禁用类错误解绑该账号粘性
                            if kind.unbinds_sticky() {
                                state.wb_sticky.unbind_uid(&picked.uid);
                            }
                            note_account_model_failure(&state, &model, upstream_msg(&resp_body).as_deref(), Some(&picked.uid));
                            // 免费模型额度耗尽记录（免费额度可见性）：非流式 HTTP 分支
                                if kind == ErrKind::HardCredit {
                                    note_free_model_exhausted(
                                        &state.data_dir, &picked.uid, &model,
                                        upstream_msg(&resp_body).as_deref(),
                                    );
                                }
                                *safe_lock(&state.last_error) =
                                    Some(format!("wb uid={} status={}", picked.uid, status));
                                // 每请求终态单行（审查问题6）：SwitchKey → 换号是中间态，
                                // 不落账（对齐流式路径），由终局分支统一落行
                                state.logger.log_request(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, status, &picked.uid,
                                    start_ts.elapsed().as_millis() as u64,
                                    &key_name, &state.wb_pool.name_of(&picked.uid),
                                    Some(&format!("upstream status={}", status)),
                                );
                                break;
                            }
                            RetryAction::Fatal => {
                                // 同流式 Fatal：报文带精确重置时刻时仍冻结该账号
                                note_account_model_failure(
                                    &state, &model,
                                    upstream_msg(&resp_body).as_deref(),
                                    Some(&picked.uid),
                                );
                                // 终态记账对齐 SwitchKey 分支（审查 F3）：Fatal 是请求
                                // 终局，此前仅 log_request 导致用量统计页缺口
                                state.record_usage(true, &model, &picked.uid, &key_id, false, stream,
                                    start_ts.elapsed().as_millis() as u64, 0, 0);
                                state.logger.log_request(
                                    "buddy", "POST", "/v2/chat/completions", &model, stream, status, &picked.uid,
                                    start_ts.elapsed().as_millis() as u64,
                                    &key_name, &state.wb_pool.name_of(&picked.uid),
                                    Some(&safe_slice(&resp_body, 300)),
                                );
                                return Err(format!(
                                    "upstream {} error: {}",
                                    status,
                                    safe_slice(&resp_body, 300)
                                ));
                            }
                        }
                    }
                }
            }
        }
    })
    .await;

    match result {
        Ok(Ok(resp)) => {
            let body = match proto {
                Protocol::Anthropic => wb_sse::completion_to_anthropic(
                    &resp,
                    &format!("msg_{}", now_ts()),
                    &model_out,
                ),
                Protocol::OpenAiText => wb_sse::completion_to_text(&resp, &model_out),
                Protocol::Responses => super::wb_responses::completion_to_responses(
                    &resp,
                    &format!("resp_{}", now_ts()),
                    &model_out,
                    &encoded,
                ),
                Protocol::OpenAi => resp,
            };
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap_or_else(|_| internal_error_response())
        }
        Ok(Err(msg)) => {
            // 审查修复：Fatal 错误透传上游状态码（对齐 SOLO 聚合路径 AggregateFail::Upstream）。
            // 上游 400/404/413 等请求级错误此前被统一降级 503 no_healthy_account，
            // 严格客户端（Codex/Claude Code）会按「可重试临时故障」无意义重试；
            // 401/403/429 属账号/池问题，维持 503 语义（换号重试仍由池层决策）。
            let upstream_status = msg
                .strip_prefix("upstream ")
                .and_then(|rest| rest.split(' ').next())
                .and_then(|s| s.parse::<u16>().ok());
            let status = match upstream_status {
                Some(s) if (400..500).contains(&s) && !matches!(s, 401 | 403 | 429) => {
                    StatusCode::from_u16(s).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
                }
                _ => StatusCode::SERVICE_UNAVAILABLE,
            };
            match proto {
                Protocol::Anthropic => anthropic_error(status, "api_error", &msg),
                _ => openai_error(status, "upstream_error", &msg),
            }
        }
        Err(e) => openai_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            &format!("task join error: {}", e),
        ),
    }
}

// ==================== T5.5/F-64 网关工具代执行编排 ====================

/// Responses 工具代执行主流程（仅 /v1/responses 且声明 web_search 类工具时进入）：
/// 上游请求内部固定非流式（stream:false）→ 聚合 → 提取代执行工具调用 → 本地执行
/// → 结果回喂 → 循环直至最终回复（上限 MAX_ROUNDS 防积分失控）。
///
/// 输出投影：
/// - 历史代执行搜索轮 → 原生 `web_search_call` 输出项（置于 message 之前）；
/// - 最终回复 → 既有 completion_to_responses 投影；
/// - 客户端 stream=true 时按 Responses SSE 事件序列（created → items → completed）
///   由聚合结果合成下发；stream=false 直接返回 JSON。
///
/// 简化说明：本路径不复用粘性绑定（多轮工具回喂非单轮会话语义），账号选取与
/// Key 约束/分级重试与非流式管线同一套规则。
pub async fn wb_tool_exec_chat(
    state: Arc<ApiSharedState>,
    mut chat_body: Value,
    model: String,
    stream: bool,
    start_ts: Instant,
    key_id: String,
    guard: InflightGuard,
    encoded: std::collections::HashSet<String>,
) -> Response {
    let model_inner = model.clone();
    // P2 修复：工具代执行多轮上游请求 + 重试退避（最长 60s×N），长阻塞占主池
    // 会饿死鉴权等短任务，与非流式聚合一并迁入 stream_runtime 专用阻塞池
    let result = super::stream_runtime().spawn_blocking(move || {
        // inflight guard 随后台任务存续至编排完成（§4.5）；F-77 取号后绑定账号级计数
        let mut guard = guard;
        let model = model_inner;
        // 请求日志附带的 API Key 展示名（匿名/未知 → 空串，日志显示 "-"）
        let key_name = super::api_keys::key_name_for(&state.data_dir, &key_id);
        let templates = load_templates(&state);
        let mut sanitize = state.wb_sanitize.load(std::sync::atomic::Ordering::Relaxed);
        // P1-6 劫持防护：false = 客户端已声明同名 function 工具，未注入代理工具，
        // 全程跳过代执行（客户端工具调用照常透传，由客户端自行执行）
        let proxy_exec = super::wb_toolexec::inject_proxy_tools(&mut chat_body);
        // F-76④ 上下文过大提示（仅日志标注，不做真裁剪）
        log_longctx_hint(&state, &chat_body, &model);

        // F-35 子 Key 约束（与非流式同款；issue #30 混合白名单按前缀作用域分池）
        let (allowed_set, dedicated, key_want) = super::api_keys::constraints_for(&state.data_dir, &key_id)
            .and_then(|k| k.pool_constraints("buddy"))
            .map_or((None, None, None), |c| (c.allowed, c.dedicated, c.want_global));

        let catalog = super::wb_catalog::load(&state.data_dir);
        let effort = super::wb_catalog::find(&catalog, &model)
            .and_then(|m| m.resolve_effort(chat_body.get("reasoning_effort").and_then(|v| v.as_str())));

        // T5.3：按模型冷却预注入 tried（toolexec 路径无粘性首选）
        let mut tried: HashSet<String> =
            state.wb_pool.model_cooled_uids(&model).into_iter().collect();
        let mut refreshed: HashSet<String> = HashSet::new();
        let mut empty_hits: usize = 0; // 空完成计数：轮换耗尽收尾区分指纹拦截与账号耗尽
        let mut last_11128_sample: Option<String> = None; // 11128 诊断样本按请求去重
        let mut records: Vec<super::wb_toolexec::SearchRecord> = Vec::new();
        let mut final_completion: Option<Value> = None;
        let mut success_uid: Option<String> = None; // 审查修复：保留真实账号归因
        let mut last_err: Option<String> = None;
        // F-76① TTFT：各轮首字耗时（最终轮即最终回复的首字延迟）
        let mut last_ttfb_ms: Option<u64> = None;
        // 请求级 usage 累加（跨账号跨轮次，含失败账号与中间轮的真实消耗）——
        // 失败终局记账带上（审查 P1：原硬编码 0,0 把已消耗的 tokens 全部丢弃）；
        // 成功终局仍以 completion 写回值为准（只含最终成功账号，维持不变）
        let mut req_acc_usage = (0u64, 0u64);
        let resp_id = format!("resp_{}", now_ts());
        // 区域判定（国内/海外号池隔离，同流式/聚合路径）；Key 区域约束显式覆盖
        let want_global = key_want.or_else(|| super::wb_catalog::region_of(&state.data_dir, &model));

        'accounts: loop {
            let picked = match state
                .wb_pool
                .pick_excluding_constrained_ev_region(&tried, allowed_set.as_ref(), dedicated.as_deref(), want_global)
            {
                Some((p, ev)) => {
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                }
                None => break,
            };
            // T5.3 按模型冷却判定（同流式）：仅池内有其他备选时才换号，否则放行
            if state.wb_pool.is_model_cooling(&picked.uid, &model)
                && state.wb_pool.has_other_selectable_for_model(
                    &picked.uid,
                    allowed_set.as_ref(),
                    want_global,
                )
            {
                state.logger.log_sched_event(&format!(
                    "model_cooldown_skip uid={} model={} (池内有其他账号顶上)",
                    picked.uid, model
                ));
                tried.insert(picked.uid.clone());
                continue;
            }
            tried.insert(picked.uid.clone());
            let conv_id = gen_conv_id();
            // F-77 账号级在途计数：取号即绑定。
            // 审查 P1-4 TOCTOU 收紧：普通候选 CAS 复核「在途 < 并发上限」，已满则
            // 不发起请求换下一候选（不计错误不冷却）；over_limit 候选（专一锁定）
            // 保留原无条件绑定语义。
            // active_uid 仅在绑定成功后更新（CAS 拒绝的候选不得污染「当前账号」展示）
            let bound = if picked.over_limit {
                guard = guard.bind_account(state.wb_pool.inflight_handle(&picked.uid));
                true
            } else {
                guard.bind_account_cas(
                    state.wb_pool.inflight_handle(&picked.uid),
                    state.wb_pool.concurrency_limit(),
                )
            };
            if !bound {
                continue;
            }
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());
            let mut creds = WbCreds {
                id: picked.uid.clone(),
                uid: picked.uid.clone(),
                name: String::new(),
                token: picked.jwt.clone(),
                domain: picked.domain.clone(),
                enterprise_id: picked.enterprise_id.clone(),
                global_region: picked.global_region,
            };
            let mut same_attempt: u32 = 0;

            // 工具代执行轮次循环（每轮 = 一次上游请求）
            let mut round: usize = 0;
            // 各轮上游 usage 累加（含最终轮），成功时写回最终 completion——
            // 多轮回喂的真实消耗 = 各轮之和，只记最终轮会少计中间轮
            let mut acc_usage = (0u64, 0u64);
            let mut acc_cached = 0u64;
            let mut acc_cache_write = 0u64;
            let outcome = loop {
                if round >= super::wb_toolexec::MAX_ROUNDS {
                    break Err(format!(
                        "工具代执行轮数已达上限（{} 轮），上游仍未产出最终回复；已执行 {} 次搜索/读取",
                        super::wb_toolexec::MAX_ROUNDS,
                        records.len()
                    ));
                }
                round += 1;

                let body_bytes = serde_json::to_vec(&chat_body).unwrap_or_default();
                let converted = wb_payload::prepare_wb_chat_body(
                    &body_bytes, &model, &picked.uid, &conv_id, effort.as_deref(), sanitize, &templates,
                );

                // F-76① 各轮记首字耗时（最终轮即最终回复的 TTFT）
                let ttfb_start = Instant::now();
                match wb_upstream::make_wb_request(&creds, &converted) {
                    Ok(reader) => {
                        let lines = match wb_upstream::lines_with_first_byte_timeout(reader) {
                            Ok(l) => l,
                            Err(()) => {
                                state.wb_pool.note_error(&picked.uid, ErrKind::Server);
                                note_model_failure(&state, &model);
                                break Err("上游首字超时".to_string());
                            }
                        };
                        last_ttfb_ms = Some(ttfb_start.elapsed().as_millis() as u64);
                        let (completion, error_info) =
                            wb_sse::aggregate(lines, &format!("chatcmpl-{}", now_ts()));
                        if let Some((code, msg)) = error_info {
                            let kind = classify_wb_error(code, &msg);
                            if kind != ErrKind::None {
                                // 模型级 429 不整号冷却（同流式主路径）
                                if !(kind == ErrKind::SoftRate && soft_rate_model_only(&msg)) {
                                    state.wb_pool.note_error(&picked.uid, kind);
                                }
                                note_account_model_failure(&state, &model, Some(&msg), Some(&picked.uid));
                                // 免费模型额度耗尽记录（免费额度可见性）：非流式流内错误
                                if kind == ErrKind::HardCredit {
                                    note_free_model_exhausted(
                                        &state.data_dir, &picked.uid, &model, Some(&msg),
                                    );
                                }
                                // issue #67：硬冷却/禁用类错误解绑该账号粘性
                                if kind.unbinds_sticky() {
                                    state.wb_sticky.unbind_uid(&picked.uid);
                                }
                            }
                            *safe_lock(&state.last_error) =
                                Some(format!("wb-toolexec uid={} code={} msg={}", picked.uid, code, msg));
                            break Err(msg);
                        }
                        let Some(completion) = completion else {
                            break Err("上游返回空响应".to_string());
                        };
                        if super::aggregated_response_is_empty(&completion) {
                            // 空完成（影子风控/上游异常，issue #57）：换号重试。
                            // 口径：同流式路径不冷却（哨兵注释见 mod.rs）；此处补即时
                            // 观测日志，接通「空完成 → 模板命中」反查通道（与其余路径一致）
                            empty_hits += 1;
                            let duration_ms = start_ts.elapsed().as_millis() as u64;
                            state.logger.log_request_ttfb(
                                "buddy", "POST", "/v1/responses", &model, stream, 502, &picked.uid,
                                duration_ms, last_ttfb_ms, &key_name,
                                &state.wb_pool.name_of(&picked.uid),
                                Some(&format!("空完成 → 换号重试{}", wb_payload::template_hit_note())),
                            );
                            break Err("空完成（影子风控/上游异常）".to_string());
                        }
                        if let Some(u) = completion.get("usage") {
                            // 宽口径提取（别名 + 字符串数字形态，审查 P2）；
                            // 同步累加请求级口径（审查 P1：失败终局带上真实消耗）
                            let (pt, ct) = u64_pair(u);
                            acc_usage = (acc_usage.0 + pt, acc_usage.1 + ct);
                            req_acc_usage = (req_acc_usage.0 + pt, req_acc_usage.1 + ct);
                            acc_cached += cached_tokens_of(u);
                            acc_cache_write += cache_write_tokens_of(u);
                        }
                        // P1-6：仅代理成功注入工具时才按名提取代执行调用；
                        // 客户端同名工具的调用一律视为普通透传（不提取不执行）
                        let calls = if proxy_exec {
                            super::wb_toolexec::extract_proxy_calls(&completion)
                        } else {
                            Vec::new()
                        };
                        if calls.is_empty() {
                            // 最终回复：usage 写回全部轮次累加值（中间轮消耗计入总账）
                            let mut completion = completion;
                            match completion.get_mut("usage") {
                                Some(u) => {
                                    write_back_acc_usage(u, acc_usage, acc_cached, acc_cache_write)
                                }
                                None => {
                                    completion["usage"] = json!({
                                        "prompt_tokens": acc_usage.0,
                                        "completion_tokens": acc_usage.1,
                                        "cache_read_token": acc_cached,
                                        "cache_write_token": acc_cache_write,
                                    });
                                }
                            }
                            break Ok(completion); // 最终回复
                        }
                        // 本地代执行 + 回喂
                        let mut tool_msgs: Vec<Value> = Vec::new();
                        let assistant_msg = completion
                            .pointer("/choices/0/message")
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        for (call_id, name, args) in &calls {
                            let (output, ok) = super::wb_toolexec::execute(name, args);
                            let query = serde_json::from_str::<Value>(args)
                                .ok()
                                .and_then(|a| {
                                    a.get("query")
                                        .or_else(|| a.get("url"))
                                        .and_then(|v| v.as_str())
                                        .map(str::to_string)
                                })
                                .unwrap_or_default();
                            records.push(super::wb_toolexec::SearchRecord {
                                tool: name.clone(),
                                query,
                                ok,
                            });
                            tool_msgs.push(json!({
                                "role": "tool",
                                "tool_call_id": call_id,
                                "content": output,
                            }));
                        }
                        if let Some(msgs) = chat_body
                            .get_mut("messages")
                            .and_then(|m| m.as_array_mut())
                        {
                            msgs.push(assistant_msg);
                            msgs.extend(tool_msgs);
                        }
                        continue;
                    }
                    Err((status, resp_body, retry_after)) => {
                        // 11128 渠道风控（issue #57）：清洗未启用时强制清洗后重试
                        // （round 回退，不消耗轮次；下一轮循环顶部按新 sanitize 重建请求体）
                        if status == 400 && super::retry::is_illegal_channel_error(&resp_body) {
                            if !sanitize {
                                sanitize = true;
                                state.logger.log_sched_event(&format!(
                                    "11128 强制清洗重试{}", wb_payload::template_hit_note()
                                ));
                                round = round.saturating_sub(1);
                                std::thread::sleep(std::time::Duration::from_millis(200));
                                continue;
                            }
                            // 清洗已启用仍命中（issue #99 诊断闭环）：落原始指纹样本
                            // + 按请求去重；Fatal 透传前附排查指引（仅日志）
                            let sample = wb_payload::fingerprint_sample(
                                &serde_json::to_vec(&chat_body).unwrap_or_default(),
                            );
                            if last_11128_sample.as_deref() != Some(sample.as_str()) {
                                last_11128_sample = Some(sample.clone());
                                state.logger.log_sched_event(&format!(
                                    "11128 指纹样本: {}{}",
                                    sample,
                                    wb_payload::template_hit_note()
                                ));
                                state.logger.log_sched_event(
                                    "11128 清洗已启用仍被拦截：模板需更新（UI「指纹清洗」tab 热更新 wb_template_map.json），指纹样本见上方日志",
                                );
                            }
                        }
                        // 同前两处：带精确重置时刻的 429 直接换号，不再同号空转
                        let retry_action = if status == 429
                            && parse_quota_reset_at(&resp_body).is_some()
                        {
                            RetryAction::SwitchKey
                        } else {
                            retry_plan(status, &resp_body, same_attempt, retry_after)
                        };
                        match retry_action {
                            RetryAction::RetrySame { delay_ms } => {
                                same_attempt += 1;
                                std::thread::sleep(std::time::Duration::from_millis(
                                    delay_ms.min(60_000),
                                ));
                                continue;
                            }
                            RetryAction::SwitchKey => {
                                if status == 401 && !refreshed.contains(&picked.uid) {
                                    refreshed.insert(picked.uid.clone());
                                    match wb_upstream::refresh_access_token(&state.data_dir, &picked.uid, Some(&creds.token)) {
                                        Ok(new_token) => {
                                            state.wb_pool.update_jwt(&picked.uid, &new_token);
                                            creds.token = new_token;
                                            continue;
                                        }
                                        Err(e) => {
                                            *safe_lock(&state.last_error) = Some(format!(
                                                "wb-toolexec refresh uid={} err={}",
                                                picked.uid, e
                                            ));
                                            // 他方进程刷新中：以 Err 收束本轮 outcome（跳过
                                            // classify_error→note_error 防误置 disabled），外层
                                            // 换号重试；对方刷新结果由下轮请求持锁双检复用
                                            if e == wb_upstream::ERR_PEER_REFRESHING {
                                                break Err(format!(
                                                    "wb-toolexec refresh uid={} 他方进程正在刷新，本轮换号",
                                                    picked.uid
                                                ));
                                            }
                                        }
                                    }
                                }
                                let kind = classify_error(status, &resp_body);
                                // ErrKind::None（如 4001 模型配置缺失）属请求/模型级问题，
                                // 与账号健康无关——不计入账号失败，防全池被同号模型错误连坐熔断
                                // 模型级 429（「可切换其他模型」文案）不整号冷却（同流式主路径）
                                if kind != ErrKind::None
                                    && !(kind == ErrKind::SoftRate
                                        && upstream_msg(&resp_body)
                                            .as_deref()
                                            .is_some_and(|m| soft_rate_model_only(m)))
                                {
                                    state.wb_pool.note_error(&picked.uid, kind);
                                }
                                // issue #67：硬冷却/禁用类错误解绑该账号粘性
                                if kind.unbinds_sticky() {
                                    state.wb_sticky.unbind_uid(&picked.uid);
                                }
                                note_account_model_failure(&state, &model, upstream_msg(&resp_body).as_deref(), Some(&picked.uid));
                                break Err(format!("upstream {} error: {}", status, safe_slice(&resp_body, 200)));
                            }
                            RetryAction::Fatal => {
                                // 同前两处：带精确重置时刻时冻结账号，避免下个请求再撞
                                note_account_model_failure(
                                    &state, &model,
                                    upstream_msg(&resp_body).as_deref(),
                                    Some(&picked.uid),
                                );
                                break Err(format!("upstream {} error: {}", status, safe_slice(&resp_body, 300)));
                            }
                        }
                    }
                }
            };

            match outcome {
                Ok(c) => {
                    state.wb_pool.note_success(&picked.uid);
                    clear_model_failure(&state, &model);
                    success_uid = Some(picked.uid.clone());
                    final_completion = Some(c);
                    break 'accounts;
                }
                Err(e) => {
                    last_err = Some(e);
                    continue; // 换号
                }
            }
        }

        let duration_ms = start_ts.elapsed().as_millis() as u64;
        match final_completion {
            Some(mut completion) => {
                // 宽口径提取（别名 + 字符串数字形态，审查 P2）；
                // completion 写回值为最终账号跨轮累加（acc_usage），维持不变
                let (pt, ct) = completion.get("usage").map(u64_pair).unwrap_or((0, 0));
                let usage_uid = success_uid.as_deref().unwrap_or("wb-toolexec");
                // M4 审查修复：缓存读 tokens 旁路入账——聚合终态 usage 经
                // write_back_acc_usage 写回 cache_read_token，cached_tokens_of
                // 键集覆盖该键可直接提取；未下发得 0 不入账
                state.record_usage_cached(
                    super::usage::UsageBucket::Wb,
                    completion.get("usage").map(cached_tokens_of).unwrap_or(0),
                );
                // F-76① TTFT 入账：最终轮首字耗时
                state.record_usage_ttfb(true, &model, usage_uid, &key_id, true, stream, duration_ms, pt, ct, last_ttfb_ms);
                state.logger.log_request_ttfb(
                    "buddy", "POST", "/v1/responses", &model, stream, 200, usage_uid,
                    duration_ms, last_ttfb_ms, &key_name, &state.wb_pool.name_of(usage_uid),
                    Some(&format!("rounds={} searches={}", records.len(), records.iter().filter(|r| r.tool == super::wb_toolexec::TOOL_SEARCH).count())),
                );
                // Responses 投影：web_search_call 历史项前置
                completion["model"] = json!(model.clone());
                let ws_items = super::wb_toolexec::search_call_items(&records, &resp_id);
                Ok((completion, ws_items, resp_id))
            }
            None => {
                // 失败终局带上请求级真实消耗（审查 P1：跨账号跨轮次已消耗的
                // tokens 原硬编码 0,0 全部丢弃，含失败账号与中间轮回喂）
                state.record_usage(true, &model, "wb-toolexec", &key_id, false, stream, duration_ms, req_acc_usage.0, req_acc_usage.1);
                state.logger.log_request(
                    "buddy", "POST", "/v1/responses", &model, stream, 502, "wb-toolexec",
                    duration_ms, &key_name, "", last_err.as_deref(),
                );
                // 审查 P2：轮换耗尽收尾对齐 routes.rs——空完成主导 → 指纹拦截语义
                Err(last_err.unwrap_or_else(|| {
                    let (healthy_total, _) = state
                        .wb_pool
                        .selectable_stats_in_region(None, want_global);
                    let (exhaust_msg, exhaust_tag) =
                        super::routes::exhaust_message(empty_hits, tried.len(), healthy_total);
                    // issue #99 ①：影子风控收尾落原始指纹样本（chat_body 轮末已追加
                    // tool 消息，样本取首条 system + 工具清单，语义不受影响）
                    if exhaust_tag == "CHANNEL_FINGERPRINT_BLOCK" {
                        state.logger.log_debug_line(format!(
                            "[DEBUG] {exhaust_tag} tried={} empty_hits={} pool=buddy 指纹样本: {}",
                            tried.len(),
                            empty_hits,
                            wb_payload::fingerprint_sample(
                                &serde_json::to_vec(&chat_body).unwrap_or_default()
                            ),
                        ));
                    }
                    exhaust_msg
                }))
            }
        }
    })
    .await;

    let (final_completion, ws_items, resp_id) = match result {
        Ok(Ok(t)) => t,
        Ok(Err(msg)) => {
            return openai_error(StatusCode::BAD_GATEWAY, "tool_exec_error", &msg);
        }
        Err(e) => {
            return openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                &format!("task join error: {}", e),
            );
        }
    };

    let resp_obj = {
        let mut r = super::wb_responses::completion_to_responses(&final_completion, &resp_id, &model, &encoded);
        if !ws_items.is_empty() {
            let mut output = ws_items;
            if let Some(old) = r.get("output").and_then(|o| o.as_array()).cloned() {
                output.extend(old);
            }
            r["output"] = json!(output);
        }
        r
    };

    if stream {
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(64);
        // 由聚合结果合成 Responses SSE 事件序列（created → items → completed）
        tokio::task::spawn_blocking(move || {
            let send = |event: &str, data: &Value| {
                let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                    "event: {}\ndata: {}\n\n",
                    event, data
                ))));
            };
            let mut created = resp_obj.clone();
            created["status"] = json!("in_progress");
            created["output"] = json!([]);
            send("response.created", &json!({"type": "response.created", "response": created}));
            let output_items = resp_obj.get("output").and_then(|o| o.as_array()).cloned().unwrap_or_default();
            for (i, item) in output_items.iter().enumerate() {
                send("response.output_item.added", &json!({
                    "type": "response.output_item.added",
                    "output_index": i,
                    "item": item,
                }));
                // message 文本增量（整段一次下发，聚合投影无逐 token 流）
                if item.get("type").and_then(|v| v.as_str()) == Some("message") {
                    if let Some(text) = item
                        .pointer("/content/0/text")
                        .and_then(|t| t.as_str())
                        .filter(|s| !s.is_empty())
                    {
                        send("response.output_text.delta", &json!({
                            "type": "response.output_text.delta",
                            "item_id": item.get("id").cloned().unwrap_or(json!("")),
                            "output_index": i,
                            "content_index": 0,
                            "delta": text,
                        }));
                    }
                }
                // function_call 参数增量
                if item.get("type").and_then(|v| v.as_str()) == Some("function_call") {
                    if let Some(args) = item.get("arguments").and_then(|a| a.as_str()).filter(|s| !s.is_empty()) {
                        send("response.function_call_arguments.delta", &json!({
                            "type": "response.function_call_arguments.delta",
                            "item_id": item.get("id").cloned().unwrap_or(json!("")),
                            "output_index": i,
                            "delta": args,
                        }));
                    }
                }
                send("response.output_item.done", &json!({
                    "type": "response.output_item.done",
                    "output_index": i,
                    "item": item,
                }));
            }
            send("response.completed", &json!({"type": "response.completed", "response": resp_obj}));
        });
        let stream = ReceiverStream::new(rx);
        Response::builder()
            .header("content-type", "text/event-stream")
            .header("cache-control", "no-cache")
            .header("connection", "keep-alive")
            .body(Body::from_stream(stream))
            .unwrap_or_else(|_| internal_error_response())
    } else {
        Response::builder()
            .header("content-type", "application/json")
            .body(Body::from(resp_obj.to_string()))
            .unwrap_or_else(|_| internal_error_response())
    }
}

// ==================== 小工具 ====================

fn send_stream_error_wb(
    tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    proto: Protocol,
    code: i64,
    msg: &str,
) {
    // 空 message 兜底（issue #71）：与 routes/qoder 的 send_stream_error 同防线
    let msg = stream_msg_or_fallback(msg, code);
    match proto {
        Protocol::Anthropic => {
            let err = json!({"type":"error","error":{"type":"api_error","message":msg}});
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                "event: error\ndata: {}\n\n",
                err
            ))));
        }
        Protocol::Responses => {
            // 取号失败/无健康账号等入口错误 → response.failed（Responses 无 [DONE] 帧）
            let resp = json!({
                "id": format!("resp_{}", now_ts()),
                "object": "response",
                "status": "failed",
                "output": [],
                "error": {"code": code.to_string(), "message": msg},
            });
            let body = json!({"type": "response.failed", "response": resp});
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!(
                "event: response.failed\ndata: {}\n\n",
                body
            ))));
        }
        _ => {
            let body = json!({"error": { "message": msg, "type": "api_error", "code": code }});
            let _ = tx.blocking_send(Ok(bytes::Bytes::from(format!("data: {}\n\n", body))));
            let _ = tx.blocking_send(Ok(bytes::Bytes::from("data: [DONE]\n\n")));
        }
    }
}

fn internal_error_response() -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from("{\"error\":{\"message\":\"internal error\"}}"))
        .unwrap()
}

/// 字符边界安全截断（审查 P2-2）：字节落点在多字节字符内时回退到前一个边界，
/// 而非返回整个串（避免超长上游响应体整段进入错误消息/日志）
fn safe_slice(s: &str, n: usize) -> &str {
    if let Some(t) = s.get(..n) {
        return t;
    }
    // n 落在字符边界内：向前找最近的合法边界（最多回退 3 字节，UTF-8 最长 4 字节）
    let mut end = n.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 多轮累计 usage 写回：prompt/completion/total 汇总 + 缓存读/写跨轮累计写回。
/// 先清除上游单轮残留的遮蔽键再写累计值——cached_tokens_of /
/// cache_write_tokens_of 宽口径优先读顶层别名与嵌套容器
/// （prompt_tokens_details / inputTokensDetails），任何正值残留都会让下游读到
/// 单轮值而遮蔽跨轮累计值；写侧别名同理——末轮单轮 cache_creation 残留会被
/// 下游误读为全程累计（首轮写入通常最大，少报最明显）。
fn write_back_acc_usage(
    u: &mut Value,
    acc_usage: (u64, u64),
    acc_cached: u64,
    acc_cache_write: u64,
) {
    u["prompt_tokens"] = json!(acc_usage.0);
    u["completion_tokens"] = json!(acc_usage.1);
    if u.get("total_tokens").is_some() {
        u["total_tokens"] = json!(acc_usage.0 + acc_usage.1);
    }
    if let Some(obj) = u.as_object_mut() {
        // 顶层键：宽口径全部别名（含 cache_read_token / cache_write_token
        // 自身，下方重写累计值）
        for k in [
            "cache_read_input_tokens",
            "cache_read_token",
            "cacheReadInputTokens",
            "prompt_cache_hit_tokens",
            "cached_tokens",
            "cache_read_tokens",
            "cache_creation_input_tokens",
            "cache_write_input_tokens",
            "cacheWriteInputTokens",
            "prompt_cache_write_tokens",
        ] {
            obj.remove(k);
        }
        // 嵌套容器整体移除：cached_tokens_of 的兜底来源，残留即遮蔽
        obj.remove("prompt_tokens_details");
        obj.remove("inputTokensDetails");
    }
    u["cache_read_token"] = json!(acc_cached);
    u["cache_write_token"] = json!(acc_cache_write);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_wb_error_ranges() {
        assert!(matches!(classify_wb_error(401, "x"), ErrKind::SessionDead));
        assert!(matches!(classify_wb_error(429, "x"), ErrKind::SoftRate));
        assert!(matches!(classify_wb_error(503, "x"), ErrKind::Server));
        // 未识别 4xx 归 Client（10m 短冷却），不误触 30m 熔断
        assert!(matches!(classify_wb_error(400, "bad request"), ErrKind::Client));
        assert!(matches!(classify_wb_error(404, "nope"), ErrKind::Client));
        // 未知业务码（<400）维持 Server 保守判定
        assert!(matches!(classify_wb_error(0, "weird"), ErrKind::Server));
        // message 关键词优先级不受 code 分段影响
        assert!(matches!(classify_wb_error(200, "积分不足"), ErrKind::HardCredit));
        // P0-2 口径统一：频率限流语义（含英文 quota exceeded / 中文频率限制）
        // 归 SoftRate 而非整号 HardCredit——与 HTTP 路径 is_credit_exhausted_error
        // 有意排除 "quota exceeded" 的注释口径一致
        assert!(matches!(classify_wb_error(6004, "quota exceeded, will reset later"), ErrKind::SoftRate));
        assert!(matches!(
            classify_wb_error(6004, "您的使用量已超出频率限制，您也可以切换其他模型继续使用"),
            ErrKind::SoftRate
        ));
        // 11133/400001 input_invalid：上游拒绝请求参数（模型侧问题），
        // 与账号健康无关 → None，不得计连续错误熔断整池
        assert!(matches!(
            classify_wb_error(11133, "the request parameters were rejected by the model provider"),
            ErrKind::None
        ));
        // 限流词收窄（2026-10-20）：裸 "rate" 子串曾把 prepare/separate/
        // generated/moderation 里的 "rate" 判成限频，白加 rate_trips 把退避
        // 翻倍到 30 分钟；这些普通词必须落回 Client/Server 而非 SoftRate
        assert!(matches!(
            classify_wb_error(400, "failed to prepare the request"),
            ErrKind::Client
        ));
        assert!(matches!(
            classify_wb_error(400, "invalid moderation result"),
            ErrKind::Client
        ));
        assert!(matches!(
            classify_wb_error(400, "rate limit exceeded for this model"),
            ErrKind::SoftRate
        ));
        // HTTP 路径同口径（400/11133 走 classify_error）
        assert!(matches!(
            super::classify_error(
                400,
                r#"{"code":11133,"msg":"the request parameters were rejected by the model provider","extError":{"code":"400001","type":"input_invalid"}}"#
            ),
            ErrKind::None
        ));
    }

    /// 6004 限额文案 → 精确恢复时刻（agent2api errors.rs 同源语义；
    /// UTC+8 字面量必须命中，按 UTC+8 解释为 Unix 秒）
    /// 回归（2026-10-18 用户线上文案）：6004「使用量已超出频率限制，将在 X UTC+8
    /// 重置」必须被识别为「重试无意义」——否则 retry_plan 会先同号重试 3 次，
    /// 把已超限的账号又硬打三遍，池内健康账号一次都没被选中。
    #[test]
    fn exhausted_429_message_is_retry_futile() {
        let reset = (chrono::Utc::now() + chrono::Duration::hours(7))
            .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
        let body = format!(
            r#"{{"code":6004,"msg":"您的使用量已超出频率限制，将在 {} UTC+8 重置，您也可以切换其他模型继续使用。","requestId":"x"}}"#,
            reset.format("%Y-%m-%d %H:%M:%S")
        );
        // 判据：解析出未来重置时刻 → 走 SwitchKey 而非 RetrySame
        assert!(
            parse_quota_reset_at(&body).is_some(),
            "应识别为额度耗尽形态，直接换号"
        );
        // 瞬时限频（无重置时刻）仍应保留同号短退避
        assert!(parse_quota_reset_at(r#"{"code":6004,"msg":"slow down"}"#).is_none());
    }

    /// 模型级 429 判定（单账号区域池整池 503 修复，2026-10-19）：
    /// 上游明示「可切换其他模型」= 仅该模型限频，账号健康，不得整号冷却；
    /// 账号级日额度（不带可切换文案）必须维持整号冷却。三态覆盖：
    /// ① 含中文可切换短语 → true（模型级）；② 英文可切换短语 → true；
    /// ③ 纯账号额度文案 / 无限流文案 → false（不得放行整号冷却门）。
    #[test]
    fn soft_rate_model_only_detects_switchable_phrases() {
        // ① 中文线上形态
        assert!(soft_rate_model_only(
            "您的使用量已超出频率限制，您也可以切换其他模型继续使用。"
        ));
        // ② 英文同义形态（中英双保险）
        assert!(soft_rate_model_only(
            "rate limit reached, you can switch to the other model to continue"
        ));
        // ③ 账号级日额度（无「可切换」语义）：必须整号冷却，否则真·日额度
        //    账号其他模型反复被选中再 429（note_account_model_failure 同口径）
        assert!(!soft_rate_model_only("您的使用量已超出今日限制，请明日再试"));
        assert!(!soft_rate_model_only("quota exceeded, will reset later"));
        // ④ 纯限频无文案线索 → 保守按账号级（不误放整池）
        assert!(!soft_rate_model_only("slow down"));
    }

    /// 回归（2026-10-18 实测）：中文多字节文案 + 精确重置时刻。
    /// 用户线上文案「您的使用量已超出频率限制，将在 ... UTC+8 重置」解析失败会让
    /// 账号只吃到 60s 短冷却，冷却刚过就再次被选中、再次 429。
    #[test]
    fn parse_quota_reset_at_chinese_frequency_limit() {
        let reset = (chrono::Utc::now() + chrono::Duration::hours(7))
            .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
        let msg = format!(
            "您的使用量已超出频率限制，将在 {} UTC+8 重置，您也可以切换其他模型继续使用。",
            reset.format("%Y-%m-%d %H:%M:%S")
        );
        let ts = parse_quota_reset_at(&msg).expect("中文频率限额文案应解析出重置时刻");
        assert_eq!(ts, reset.timestamp());
        // 英文形态（上一轮已修）也必须仍然通过
        let en = format!(
            "usage exceeds frequency limit, your usage will reset at {} UTC+8",
            reset.format("%Y-%m-%d %H:%M:%S")
        );
        assert_eq!(parse_quota_reset_at(&en), Some(reset.timestamp()));
    }

    #[test]
    fn parse_quota_reset_at_from_limit_message() {
        // 未来时刻动态构造（UTC+8 时区格式化），格式与上游文案一致
        let reset = (chrono::Utc::now() + chrono::Duration::hours(3))
            .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
        let msg = format!(
            "当前模型使用额度已达上限，将在 {} UTC+8 重置",
            reset.format("%Y-%m-%d %H:%M:%S")
        );
        let ts = parse_quota_reset_at(&msg).expect("应解析出恢复时刻");
        assert_eq!(ts, reset.timestamp());
        // 缺 UTC+8 字面量 / 缺时刻 → None
        assert_eq!(parse_quota_reset_at("将在 2099-01-01 00:00:00 重置"), None);
        assert_eq!(parse_quota_reset_at("quota exceeded"), None);
        // 过去时刻 → None（落回渐进退避）
        assert_eq!(parse_quota_reset_at("已在 2020-01-01 00:00:00 UTC+8 重置"), None);
        // 超远未来（>7 天钳制）→ None
        let far = (chrono::Utc::now() + chrono::Duration::days(30))
            .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
        let far_msg = format!("将在 {} UTC+8 重置", far.format("%Y-%m-%d %H:%M:%S"));
        assert_eq!(parse_quota_reset_at(&far_msg), None);
        // 复审修复回归：双时刻文案（触发时刻为过去 + 恢复时刻为未来）→
        // 首锚点解析出过去时刻须跳过，继续用第二锚点解析出恢复时刻
        let past = (chrono::Utc::now() - chrono::Duration::hours(2))
            .with_timezone(&chrono::FixedOffset::east_opt(8 * 3600).unwrap());
        let dual = format!(
            "您于 {} UTC+8 触发限额，将在 {} UTC+8 重置",
            past.format("%Y-%m-%d %H:%M:%S"),
            reset.format("%Y-%m-%d %H:%M:%S")
        );
        assert_eq!(parse_quota_reset_at(&dual), Some(reset.timestamp()));
        // 2026-10-18 实测回归：WB 6004 **英文**频率限额文案（时刻与 UTC+8 之间隔
        // 一个空格，且后缀还有逗号续句）。旧实现只从 anchor-DT_LEN 单向回扫，
        // 合法起点在其右侧 → 漏解析 → 账号只吃到 60s 短冷却、冷却后继续连 429
        let en = format!(
            "usage exceeds frequency limit, but don't worry, your usage will reset at {} UTC+8, alternatively, you can switch to the other models to continue using it.",
            reset.format("%Y-%m-%d %H:%M:%S")
        );
        assert_eq!(parse_quota_reset_at(&en), Some(reset.timestamp()));
    }

    /// upstream_msg 三种字段形态（message / error.message / msg）提取
    #[test]
    fn upstream_msg_field_variants() {
        assert_eq!(
            upstream_msg(r#"{"code":6004,"message":"将在 2099-01-01 00:00:00 UTC+8 重置"}"#).as_deref(),
            Some("将在 2099-01-01 00:00:00 UTC+8 重置")
        );
        assert_eq!(
            upstream_msg(r#"{"error":{"code":6004,"message":"限额"}}"#).as_deref(),
            Some("限额")
        );
        assert_eq!(upstream_msg(r#"{"code":6004,"msg":"腾讯系 msg 字段"}"#).as_deref(), Some("腾讯系 msg 字段"));
        assert_eq!(upstream_msg("not json"), None);
        assert_eq!(upstream_msg("{}"), None);
    }

    /// 多轮累计 usage 写回：累计值写入 + 残留遮蔽键清除（读侧与写侧）+ 宽口径回读一致
    #[test]
    fn write_back_acc_usage_clears_shadow_keys_and_writes_acc() {
        let mut u = serde_json::json!({
            "prompt_tokens": 5,
            "completion_tokens": 2,
            "total_tokens": 7,
            "cache_read_input_tokens": 3,
            "cache_read_token": 4,
            "cacheReadInputTokens": 8,
            "prompt_cache_hit_tokens": 9,
            "cached_tokens": 6,
            "cache_read_tokens": 1,
            "prompt_tokens_details": {"cached_tokens": 10},
            "inputTokensDetails": [{"cached_tokens": 11}],
            "cache_creation_input_tokens": 12,
            "cache_write_input_tokens": 13,
            "cacheWriteInputTokens": 14,
            "prompt_cache_write_tokens": 15,
        });
        write_back_acc_usage(&mut u, (15, 7), 42, 17);
        assert_eq!(u["prompt_tokens"], serde_json::json!(15));
        assert_eq!(u["completion_tokens"], serde_json::json!(7));
        assert_eq!(u["total_tokens"], serde_json::json!(22));
        // 宽口径回读命中累计值，而非任何残留单轮值
        assert_eq!(cached_tokens_of(&u), 42);
        assert_eq!(cache_write_tokens_of(&u), 17);
        // 遮蔽键全部清除，仅剩累计值载体 cache_read_token / cache_write_token
        for k in [
            "cache_read_input_tokens",
            "cacheReadInputTokens",
            "prompt_cache_hit_tokens",
            "cached_tokens",
            "cache_read_tokens",
            "prompt_tokens_details",
            "inputTokensDetails",
            "cache_creation_input_tokens",
            "cache_write_input_tokens",
            "cacheWriteInputTokens",
            "prompt_cache_write_tokens",
        ] {
            assert!(u.get(k).is_none(), "残留遮蔽键 {k}");
        }
        assert_eq!(u["cache_read_token"], serde_json::json!(42));
        assert_eq!(u["cache_write_token"], serde_json::json!(17));
    }

    /// usage 原本无 total_tokens 时保持缺省，不强加字段
    #[test]
    fn write_back_acc_usage_keeps_total_absent() {
        let mut u = serde_json::json!({"prompt_tokens": 1, "completion_tokens": 1});
        write_back_acc_usage(&mut u, (3, 4), 0, 0);
        assert_eq!(u["prompt_tokens"], serde_json::json!(3));
        assert_eq!(u["completion_tokens"], serde_json::json!(4));
        assert!(u.get("total_tokens").is_none());
        assert_eq!(u["cache_read_token"], serde_json::json!(0));
        assert_eq!(u["cache_write_token"], serde_json::json!(0));
    }
}
