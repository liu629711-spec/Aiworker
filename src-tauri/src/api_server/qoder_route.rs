//! Qoder 上游路由执行层（p3-3-wire）。
//!
//! 请求流程（wb_route 的 Qoder 侧镜像，v1 精简）：
//! 1. 目录条目解析（`qoder_upstream::resolve` → upstreamKey/config，执行时再查
//!    ——调度与执行之间目录可能刷新，条目为准）；
//! 2. 请求体构造（`prepare_qoder_body`：OpenAI body → agent 固定信封，
//!    session 由账号 uid + 下游种子派生）；
//! 3. 取号（qoder_pool 调度，Key 白名单/专一约束同构适用）→ identity 回调解析
//!    凭证（PAT 换 24h 作业令牌 / 刷新链路在回调闭包内走全防护）→
//!    `make_qoder_request`（COSY 19 头签名）；
//! 4. 分级重试：HTTP 层走 `retry_plan`（429/5xx/502 同号退避、401/403 换号）；
//!    **排队（业务码 10605）同号退避优先**——首个进入排队的请求按上游建议
//!    时长同号退避重试（国内 ≤3 次 / 海外 ≤8 次，超限放回调度轮换，防止占死
//!    并发槽）。排队按模型不按账号，**换号必然从头重排**，故海外放宽预算而非
//!    换号；并发场景下若该模型已记录冷却且本区域无其他可选账号则快速失败；
//!    其余流内错误按 ErrMeta 分类映射 ErrKind 后换号；
//! 5. 流式：首字窗口按区域（国内 10s / **海外 60s**，`first_byte_timeout_for`）→ QoderTranslate 翻译为标准
//!    OpenAI chunk → `wb_sse::stream_forward_ex`（keep-alive 15s + 断连三层
//!    检测同构）；非流式：`aggregate_qoder` → 协议投影复用 wb_sse 转换器；
//! 6. 用量记账（`record_usage_qoder` 独立 qoder 桶）+ 请求级日志。
//!
//! 与 wb_route 的差异（v2）：**会话粘性**（F-80-余 v2，开关默认关）与**竞速对冲**
//! （F-80-余 v2，阈值热参数默认 8s）已按 WB 同构补齐——粘性命中锁定账号（busy
//! 且有空闲候选时让位，F-77④），同账号 + 同种子派生同一上游 session_id 保住
//! 会话侧复用；对冲在原始行源层竞速，胜者行源统一经 QoderTranslate 翻译。仍无
//! 模板清洗、无工具代执行、无 401 刷新重试（凭证新鲜度由 identity 回调按次解析
//! 兜住）。模型级冷却已补齐（审查修复）：排队/超限类错误记录 model → 冷却截止，
//! dispatch 预检快速回退、执行路径跳过同号退避（见下方「模型级冷却」节）。
//! 对冲接管后排队同号退避不适用（重试凭证/请求体属主账号，与生效账号不一致）。
//!
//! **首字超时不再按故障计错（2026-10-19）**：「未产出首字节」与「请求失败」是
//! 两回事。海外上游普遍静默排队（不发 10605 排队票，只握连接不吐字节，实测首包
//! p50≈6.5s / p90≈14s、当日 20% 请求 >10s），此前 10s 判死 → note_error(Server)
//! 冷却 + 换号重排，逐步把海外池抽干（20:41 连报 `no healthy account available
//! (healthy=2)`）。现首字超时仅放弃连接换号，不动账号健康度、不进冷却。
//!
//! 客户端断连：与 wb_route 同款三层检测（轮换/重试入口 tx.is_closed、停滞期
//! next_event_polling 轮询、活跃流逐事件顶部检测）。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use tokio_stream::wrappers::ReceiverStream;

use super::retry::{retry_plan, RetryAction};
use super::wb_sse;
use super::wb_sticky::SessionKey;
use super::wb_upstream::{
    first_byte_timeout_for, lines_with_first_byte_hedged_timeout,
    lines_with_first_byte_timeout_for, InterruptibleLines,
};
use super::{ApiSharedState, ErrKind, InflightGuard};
use crate::api_server::routes::{anthropic_error, openai_error, Protocol, stream_msg_or_fallback};
use crate::tasks::qoder_common::QoderCreds;
use crate::tasks::qoder_upstream::{self, ErrMeta, UpstreamKind};

/// 排队同号重试上限（10605 按上游建议退避；超限放回调度轮换防占死并发槽）
const QUEUE_RETRY_LIMIT: u32 = 3;
/// 海外区排队同号重试上限（2026-10-19）：海外免费档排队是常态，3 次（≈92s）
/// 打满就返回 429 对用户比一直等更糟——实测两次都是等了 92–97s 拿到报错。
/// 海外放宽到 8 次（≈180s 预算），期间不换号（排队按模型不按账号，换号必然
/// 从头重排）。国内沿用 3 次短预算。
const QUEUE_RETRY_LIMIT_GLOBAL: u32 = 8;
/// 排队退避缺省秒数（上游 retryAfterSeconds/waitTime 缺失时）
const QUEUE_DEFAULT_BACKOFF_SECS: u64 = 5;
/// 退避上限（上游建议值异常大时钳制）
const QUEUE_BACKOFF_MAX_SECS: u64 = 30;

/// 按区域取排队同号重试上限
fn queue_retry_limit_for(want_global: bool) -> u32 {
    if want_global {
        QUEUE_RETRY_LIMIT_GLOBAL
    } else {
        QUEUE_RETRY_LIMIT
    }
}

/// 安全获取 Mutex 锁：若锁被毒化（panic 导致），仍恢复内部数据继续运行
fn safe_lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

// ==================== 模型级冷却（审查修复，镜像 wb_route::model_cooldowns） ====================

/// Qoder 模型级冷却表：model → (冷却截止时刻, 连续失败次数)。排队/超限类错误
/// （ErrMeta 分类 Queued）是模型级现象（换账号同样排队），与账号无关，故进程级
/// 静态表语义等价；挂 ApiSharedState 需改 server.rs（他人负责），静态 OnceLock
/// 最小改动。失败次数仅由 Server 渐进冷却维护（issue #90 修复①），成功即随
/// 条目清除。
/// 备忘（审查 G6，2026-10-19 已落地）：键经 [cqk] 带区域前缀——海外 `g:{model}`、
/// 国内裸名。表不做主动回收，残留条目随冷却窗口自然过期，重启即清零。
static QODER_MODEL_COOLDOWNS: OnceLock<Mutex<HashMap<String, (Instant, u32)>>> = OnceLock::new();

fn cooldown_map() -> &'static Mutex<HashMap<String, (Instant, u32)>> {
    QODER_MODEL_COOLDOWNS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 冷却表键（2026-10-19 落地 G6 备忘）：海外区用 `g:{model}` 前缀、国内保持
/// 裸名（历史在途条目与既有测试兼容）。同名模型两区各自冷却——海外号排队
/// （上游 Global 区容量）不再把国内同名请求一起判死，反之亦然。
fn cqk(model: &str, global: bool) -> String {
    if global {
        format!("g:{model}")
    } else {
        model.to_string()
    }
}

/// Server 类（5xx/业务 504「上游服务异常」）渐进冷却参数（issue #90 修复①）：
/// 30s 起、连续失败翻倍（30/60/120/240/480），上限 8 分钟（base 30s 左移 4 位
/// 即 480s，MAX 取同值保证封顶可达；原 600s 恒不可达与注释矛盾——审查中等项）。
/// 模型级冷却窗口内 dispatch 健康预检（dispatch.rs pool_health）快速回退多源
/// 模型到 Trae/Buddy 池，不再每请求硬打已知 504 的 qoder 上游；成功即清
///（clear_model_cooldown）。
const SERVER_COOLDOWN_BASE_SECS: u64 = 30;
const SERVER_COOLDOWN_MAX_SECS: u64 = 480;

/// 记录模型级冷却：按本次排队退避时长（已钳制）设定截止时刻，保留既有连续
/// 失败计数（排队退避与 Server 渐进冷却共用一张表）。冷却窗口内后续请求由
/// dispatch 健康预检快速回退（多源）/显式 429（单源），执行路径跳过同号退避，
/// 不再每请求空转 5s×3~30s×3。`global` = 本次请求生效区域（键见 [cqk]）。
fn note_model_cooldown(model: &str, secs: u64, global: bool) {
    cooldown_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(cqk(model, global))
        // 取较大者：既有 Server 渐进冷却窗口（可能远长于排队建议值）不得被
        // 本次建议值缩短（issue #90 修复①配套语义，queued_cooldown 测试固化）
        .and_modify(|(until, _)| {
            let candidate = Instant::now() + Duration::from_secs(secs);
            if candidate > *until {
                *until = candidate;
            }
        })
        .or_insert_with(|| (Instant::now() + Duration::from_secs(secs), 0));
}

/// Server 类错误（上游 5xx / 业务 504「上游服务异常」）模型级渐进冷却
/// （issue #90 修复①）：冷却窗口 = min(30s × 2^n, 480s)，n = 失败片段数。
/// **递增条件是既有窗口已过期**而非调用次数——流内/HTTP 两路径在 !sent_any
/// 时都会 `break` 逐账号换号，一次请求内连续换号会对同一模型记账多次；若按
/// 调用次数递增，5 账号一次请求即可把窗口推到 480s，把「跨片段渐进」放大成
/// 「单请求轮换放大」，恢复探测被拉长到 8 分钟。按片段计数后：同请求内换号
/// （窗口未过期）不递增，探测节奏 = 冷却窗口（30s→60s→…→8min），成功即清。
fn note_model_server_cooldown(model: &str, global: bool) {
    cooldown_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(cqk(model, global))
        .and_modify(|(until, fails)| {
            // 既有窗口已过期 = 新一次失败片段 → 递增并重设窗口；未过期 =
            // 同片段内的换号/并发失败，窗口维持原截止时刻不后延（审查中等项：
            // 原实现每次失败都把 until 推到 now+delay，持续失败可把窗口无限
            // 滑动后延，恢复探测被不断推迟——半开探测语义由「窗口到期后下一次
            // 请求自然放行」保证，无需额外探测机制）
            if Instant::now() >= *until {
                *fails = fails.saturating_add(1);
                // saturating_sub：fails 可能为 0（note_model_cooldown 排队退避路径
                // or_insert 的 (until, 0) 条目）——裸 `*fails - 1` 会下溢（debug panic /
                // release 回绕致 min(4) 恒取 4、冷却被错误推到 480s）
                let delay = (SERVER_COOLDOWN_BASE_SECS
                    << (*fails).saturating_sub(1).min(4))
                .min(SERVER_COOLDOWN_MAX_SECS);
                *until = Instant::now() + Duration::from_secs(delay);
            } else if *fails == 0 {
                // 排队退避落下的短窗（fails=0）：Server 失败应至少抬到基准窗
                // （5s 排队窗不足以覆盖 5xx），但不递增计数（同片段语义；
                // fails>=1 的 Server 冷却窗维持「未过期不后延」防滑动）
                let cand = Instant::now() + Duration::from_secs(SERVER_COOLDOWN_BASE_SECS);
                if cand > *until {
                    *until = cand;
                }
            }
        })
        .or_insert_with(|| {
            (
                Instant::now() + Duration::from_secs(SERVER_COOLDOWN_BASE_SECS),
                1,
            )
        });
}

/// 模型冷却剩余秒数；None = 无冷却（过期残留条目同 wb 版语义按无冷却处理）。
/// `global`：Some(true) 查海外键；None/Some(false) 按国内裸名键（区域未知按 CN
/// ——与 dispatch 历史语义一致）。
pub fn model_cooling_remaining_secs(model: &str, global: Option<bool>) -> Option<u64> {
    let map = cooldown_map().lock().unwrap_or_else(|e| e.into_inner());
    let until = map.get(&cqk(model, global.unwrap_or(false)))?;
    let left = until.0.saturating_duration_since(Instant::now());
    if left.is_zero() {
        None
    } else {
        Some(left.as_secs())
    }
}

/// 请求成功后清除该模型冷却（同 wb_route::clear_model_failure 语义）；连续
/// 失败计数随条目一并清除（成功 = 该模型恢复服务，渐进冷却重新从基准值起步）
fn clear_model_cooldown(model: &str, global: bool) {
    cooldown_map()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&cqk(model, global));
}

/// last_error 时间戳（UTC+8，与 app 日志时间线同区，issue #90 修复②）：
/// 504 类上游异常是否持续、何时开始，用户此前只能靠猜（正是「切换模型导致」
/// 误解的成因），带时刻后可与日志逐行对照
fn now_cn_timestamp() -> String {
    use chrono::FixedOffset;
    let cn = FixedOffset::east_opt(8 * 3600).expect("UTC+8 offset is valid");
    chrono::Utc::now()
        .with_timezone(&cn)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// 池内除 `uid` 之外是否还有**本请求作用域**（区域 ∩ Key 白名单）的可选账号。
/// 模型级冷却按区域键记账、不区分账号，一个号撞限频会把该区整个模型判死；
/// 调用方据此避免「有备选却直接 429 终止轮换」。区域过滤是修复 189s 假死的
/// 关键（2026-10-19）：此前不传 want_global，国内健康号让海外请求误判「有
/// 备选」→ 快退 429 永不触发，排队逐号退避到轮换耗尽（30s×3×号数）。
fn pool_has_other_selectable(
    state: &Arc<ApiSharedState>,
    uid: &str,
    allowed: Option<&std::collections::HashSet<String>>,
    want_global: Option<bool>,
) -> bool {
    state
        .qoder_pool
        .selectable_stats_in_region_excluding(allowed, want_global, Some(uid))
        .1
        > 0
}

/// 记录「免费档模型额度耗尽」到账号池（免费额度可见性，Qoder 域）。
///
/// 触发：上游返回额度类业务码（QUOTA_CODES：110 每日用量上限 / 114 试用额度用完 /
/// 115 免费用户配额用完 / 119 所选模型的免费额度用完 等）且账号该模型属免费档
/// （目录 rate=0）。写入 qoder_pool 账号行的 `free_models[].exhausted_at`，
/// 供账号页显示「已用完（时刻）」；后续刷新（目录/积分链路）按
/// `merge_free_models` 重建时保留该历史。
///
/// 只在免费档模型上记录：付费模型额度不足属常规冷却（HardCredit 已有 04:00
/// 恢复机制），标成「免费额度用完」会误导。目录查询失败（无免费档信息）时
/// 静默跳过——不写比写错好。
fn note_free_quota_exhausted(data_dir: &std::path::Path, uid: &str, model: &str) {
    // 匹配口径：账号现有 free_models 快照为主（该账号实际区域已由回写固化），
    // 目录现查为辅（快照尚未生成的首轮场景）。两侧任一命中即认定为免费档——
    // 倍率由上游按区下发，按账号自己的快照判定比按全局清单更贴合
    // 「这个账号的免费额度是否耗尽」这一语义。
    let in_snapshot = {
        let store = crate::store::db(data_dir);
        let pool = crate::store::docs::qoder_pool_load(&store);
        pool.get("accounts")
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
            .unwrap_or(false)
    };
    let in_catalog = qoder_upstream::free_tier_models().iter().any(|m| {
        m.get("id")
            .and_then(Value::as_str)
            .map(|id| id.eq_ignore_ascii_case(model))
            .unwrap_or(false)
    });
    if !in_snapshot && !in_catalog {
        return;
    }
    // 持池锁读-改-写（与 credits 回写同锁域，防整池覆盖丢写）
    let store = crate::store::db(data_dir);
    let _g = crate::store::docs::qoder_pool_rmw_lock();
    let mut pool = crate::store::docs::qoder_pool_load(&store);
    let Some(accounts) = pool.get_mut("accounts").and_then(Value::as_array_mut) else {
        return;
    };
    let Some(a) = accounts
        .iter_mut()
        .find(|a| a.get("id").and_then(Value::as_str) == Some(uid))
    else {
        return;
    };
    let now = now_cn_timestamp();
    let mut updated = false;
    if let Some(models) = a.get_mut("free_models").and_then(Value::as_array_mut) {
        for m in models.iter_mut() {
            let hit = m
                .get("id")
                .and_then(Value::as_str)
                .map(|id| id.eq_ignore_ascii_case(model))
                .unwrap_or(false);
            if hit {
                m["exhausted_at"] = json!(now);
                updated = true;
                break;
            }
        }
    }
    if updated {
        // 重置提示（119 文案语义「明天 00:00 再重试当前免费模型」）：把恢复时刻
        // 写进快照供前端 FreeModelsCell 展示「MM-DD HH:MM 恢复」，额度中心同口径
        let reset_str = super::pool::fmt_local_ts(super::pool::next_0000(now_ts()));
        if let Some(models) = a.get_mut("free_models").and_then(Value::as_array_mut) {
            for m in models.iter_mut() {
                let hit = m
                    .get("id")
                    .and_then(Value::as_str)
                    .map(|id| id.eq_ignore_ascii_case(model))
                    .unwrap_or(false);
                if hit {
                    m["reset_at"] = json!(reset_str);
                }
            }
        }
        let _ = crate::store::docs::qoder_pool_save(&store, &pool);
        crate::fs_utils::app_log(
            data_dir,
            &format!("qoder: 免费额度耗尽已记录 uid={uid} model={model} at={now} reset={reset_str}"),
        );
        // 免费用量计数联动（额度中心）：计数归零 + 重置时刻
        super::free_usage::mark_exhausted(data_dir, "qoder", uid, model, Some(&reset_str));
    }
}

/// 排队/退避分段等待（审查修复）：长 sleep 切成 ≤500ms 片段逐段睡，每段后
/// 检查流通道是否关闭——客户端已断连即提前退出，不再空等数十秒排队/退避
/// 时长；返回 false = 已断连。非流式路径无下行通道可查（handler 返回
/// Response 前无断连通知机制），保持整段 sleep
fn sleep_interruptible(
    total: Duration,
    tx: Option<&tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>>,
) -> bool {
    let mut left = total;
    while !left.is_zero() {
        let step = left.min(Duration::from_millis(500));
        std::thread::sleep(step);
        left = left.saturating_sub(step);
        if let Some(t) = tx {
            if t.is_closed() {
                return false;
            }
        }
    }
    true
}

/// 从上游 usage JSON 提取 Qoder 积分测量（issue #82 T2）：
/// 返回 (credits, original_credits)。credits 取复数字段——单数 `credit` 是旧版
/// 字段，读它恒为 0（qoder2api-hub issue #22 命门），故仅在复数缺失时参与回退。
/// **等价性前提**：与 Python `or` 链「首值非零即用」等价，仅在上游保证单数
/// credit 恒为 0/缺失时成立；两值同现时取 max（按更高积分计）是有意为之的
/// 保守口径，不追求与 or 链逐值等价。usage 缺失或计费字段都取不到 → None
/// （调用方传 None 不入账，与「上游未下发 usage 不得伪造」口径一致）
fn extract_billing(usage: Option<&Value>) -> (Option<f64>, Option<f64>) {
    let Some(u) = usage else {
        return (None, None);
    };
    let num = |key: &str| -> Option<f64> {
        u.get(key)
            .and_then(|v| v.as_f64())
            .filter(|n| n.is_finite() && *n >= 0.0)
    };
    let pick = |a: Option<f64>, b: Option<f64>| match (a, b) {
        (Some(x), Some(y)) => Some(x.max(y)),
        (Some(x), None) => Some(x),
        (None, y) => y,
    };
    let credits = pick(num("credits"), num("credit"));
    let original = pick(num("original_credits"), num("original_credit"));
    (credits, original)
}

/// system-reminder 剥离正则（惰性编译一次）：非贪婪跨行匹配整块注入。
fn reminder_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?s)\s*<system-reminder>.*?</system-reminder>\s*")
            .expect("system-reminder regex")
    })
}

/// user 输入包裹标签正则（惰性编译一次）：<user_input> / </user_input>（容忍属性）。
fn user_input_tag_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"</?user_input[^>]*>").expect("user_input tag regex")
    })
}

/// 剥掉客户端注入的 `<system-reminder>` 块与 `<user_input>` 包裹标签（agent
/// 客户端把它们拼进 user 消息——reminder 前置劫持正文、输入被标签包裹），
/// 剩余即用户原话。剥离后仅剩空白时返回空串（调用方按无 preview 处理）。
fn strip_reminders(text: &str) -> String {
    let no_reminders = reminder_re().replace_all(text, " ");
    let stripped = user_input_tag_re().replace_all(&no_reminders, " ");
    // 连续空白压成单个空格后 trim（多块剥离后常残留空行/空格）
    let mut out = String::with_capacity(stripped.len());
    let mut in_ws = false;
    for ch in stripped.chars() {
        if ch.is_whitespace() {
            in_ws = true;
        } else {
            if in_ws && !out.is_empty() {
                out.push(' ');
            }
            in_ws = false;
            out.push(ch);
        }
    }
    out
}

/// issue #82 问题5：会话 preview——末条 user 消息文本（与 build_upstream_body
/// last_user_text 同款逆序口径），剥离注入块/包裹标签后截 200 字符。
fn session_preview(peek: &Value) -> String {
    let mut text = String::new();
    if let Some(messages) = peek.get("messages").and_then(Value::as_array) {
        for message in messages.iter().rev() {
            if message.get("role").and_then(Value::as_str) == Some("user") {
                text = qoder_upstream::content_to_text(
                    message.get("content").unwrap_or(&Value::Null),
                );
                break;
            }
        }
    }
    strip_reminders(&text).chars().take(200).collect()
}

/// issue #82 问题5：失败路径记账会话上下文。失败时本无上游信封可解析
/// （两处调用恒为占位），恒退化为 uuid 占位（request_id 仅幂等用途，占位不
/// 影响正确性）。成功路径由预解析 id 走 session_ctx_from_ids，不经本函数。
fn session_ctx(peek: &Value) -> super::usage::QoderSessionCtx {
    super::usage::QoderSessionCtx::placeholder(session_preview(peek))
}

/// issue #82 审查修复 K：信封顶层 request_id/session_id 提取。信封含完整对话历史
/// （大 body），内层重试循环若每次记账都全量反序列化属热路径冗余；
/// id 随信封恒定，应在信封生成后解析一次、循环内复用（本函数即一次解析的产物源）。
/// 会话聚合扩展：三元组带 chat_record_id（轮键，「展开会话看各轮明细」分组用）。
fn extract_envelope_ids(body: &[u8]) -> Option<(String, String, String)> {
    let v = serde_json::from_slice::<Value>(body).ok()?;
    let get = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string()
    };
    let (rid, sid, rec) = (get("request_id"), get("session_id"), get("chat_record_id"));
    (!rid.is_empty()).then_some((rid, sid, rec))
}

/// 由预解析的信封 id 构造记账上下文（id 为 None = 无信封/解析失败 → uuid 占位）
fn session_ctx_from_ids(
    peek: &Value,
    ids: Option<&(String, String, String)>,
) -> super::usage::QoderSessionCtx {
    match ids {
        Some((rid, sid, rec)) => super::usage::QoderSessionCtx {
            request_id: rid.clone(),
            session_id: sid.clone(),
            record_id: rec.clone(),
            ts: chrono::Local::now().timestamp(),
            preview: session_preview(peek),
        },
        None => super::usage::QoderSessionCtx::placeholder(session_preview(peek)),
    }
}

/// resolve 命中条目是否 Global 专属（审查修复：v1 恒 CN 网关执行，Global
/// 专属条目调度 resolve 双区兜底判活、执行必死，需在执行入口明确拒绝）。
///
/// qoder_upstream 未暴露分区分目录/兜底表的查询接口（他人文件不可改），判定
/// 基于两路**确定性证据**，不依赖旧实现「resolve(Global) 结果与命中条目 Value
/// 全等 ⟹ 两区同名条目声明必有差异」的脆弱值假设（同名同值场景会误判）：
/// ① list() 并集按 id 去重、Global 侧先注册：标注 "cn" ⟺ Global 侧（远程目录
///    + 静态兜底表）完全无此 id → CN resolve 的命中必来自 CN 侧 → 必非专属；
/// ② 标注 "global" 时借 resolve 双区优先序差异取第二证据：resolve(Global) 序
///    = [Global 目录, CN 目录, CN 兜底, Global 兜底]，resolve(Cn) 序 = [CN 目录,
///    Global 目录, Global 兜底, CN 兜底]。两路命中**不同**条目时可逐源证明
///    CN 侧必有此 id（CN 远程目录或 CN 兜底表至少其一声明）→ 必非专属；
/// ③ 两路命中全等：既有 pub 接口无法区分「仅 Global 独持」与「两区同名同值」
///    （后者仅可能来自远程目录动态刷新；静态兜底表两区同名条目倍率/档位全
///    不同）。按 v1 恒 CN 执行的 fail-closed 策略判 Global 专属：Global 侧
///    声明了此 id 而 CN 侧无任何确定性可得证据，放行只会把请求送进 CN 网关
///    收上游诡异报错，拒绝则走明确的 404 提示通道。
fn entry_is_global_only(model: &str, entry: &Value) -> bool {
    let id = entry.get("id").and_then(Value::as_str).unwrap_or("");
    if id.is_empty() {
        return false; // name/upstreamKey 命中且无 id 的边缘：保守放行
    }
    // 唯一直接证据：CN 侧**自有来源**（远程 CN 清单或 CN 兜底表）能否列出此 id。
    // resolve(Cn) 会跨区回落（CN 缺失时返回 Global 条目），不能用它的命中来证明
    // CN 可得；必须直查 CN 源清单。
    // 事故复盘（2026-10-19）：旧 fail-closed 分支「resolve(Global) 与入参条目全等
    // ⟹ 判 Global 专属」把两区都上架的 qwen3.8-flash 误判成 Global 独有——国内
    // Key 全量硬 404（Qoder 目录同步被关闭、双区均回落兜底/远程条目一致时触发）。
    qoder_upstream::catalog_for_region(qoder_upstream::QoderRegion::Cn)
        .into_iter()
        .any(|m| m.get("id").and_then(Value::as_str) == Some(id))
        .then_some(false) // CN 源有此 id：两区都有，放行 CN
        .unwrap_or_else(|| {
            // CN 源无此 id：看 list() 合并视图的归属标注（Global 优先去重注入），
            // 标注 "global" 即 Global 独有；无标注（name/upstreamKey 命中）保守放行
            qoder_upstream::list()
                .into_iter()
                .find(|m| m.get("id").and_then(Value::as_str) == Some(id))
                .and_then(|m| m.get("region").and_then(Value::as_str).map(String::from))
                .map(|r| r == "global")
                .unwrap_or(false)
        })
}

/// 字符边界安全截断（自抄 wb_route::safe_slice，私有不可复用）：
/// 字节落点在多字节字符内时回退到前一个边界，避免超长上游响应体
/// 整段进入错误消息/日志
fn safe_slice(s: &str, n: usize) -> &str {
    if let Some(t) = s.get(..n) {
        return t;
    }
    let mut end = n.min(s.len());
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ==================== F-80-余 v2 慢请求竞速对冲（取号侧编排，wb_route 同构） ====================

/// 对冲账号在途计数租约（镜像 wb_route::HedgeLease）：构造即 +1（竞速窗口
/// 占用），Drop 即 -1——建连/凭证解析失败（闭包内提前返回）、双败、竞速胜出
/// 三类退出路径均恰好释放一次，杜绝计数泄漏导致的账号永久 busy
struct QoderHedgeLease {
    uid: String,
    counter: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// 对冲账号信封的 (request_id, session_id, record_id)（G4）：对冲请求按对冲账号 uid
    /// 重建信封，两账号信封 id 不同——接管记账必须落对冲账号信封的 id，
    /// 否则行 uid=对冲账号而会话键属主账号信封，归属错位
    session_ids: Option<(String, String, String)>,
}

impl QoderHedgeLease {
    fn acquire(state: &ApiSharedState, uid: &str, session_ids: Option<(String, String, String)>) -> Self {
        let counter = state.qoder_pool.inflight_handle(uid);
        counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Self { uid: uid.to_string(), counter, session_ids }
    }
}

impl Drop for QoderHedgeLease {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 首字竞速胜者信息：**原始未翻译**上游行源（Qoder 双层信封翻译在竞速落定后
/// 统一进行，胜者行源经 QoderTranslate 转换）+ 生效账号 + 对冲计数租约
struct QoderRaceWin {
    lines: Box<dyn std::iter::Iterator<Item = String> + Send>,
    uid: String,
    hedge: Option<QoderHedgeLease>,
    takeover: bool,
    /// 胜者信封的 (request_id, session_id, record_id)（G4）：主账号胜 = 主信封 id，
    /// 对冲接管 = 对冲信封 id；记账按胜者落定，与会话行 uid 同源
    session_ids: Option<(String, String, String)>,
}

/// 首字竞速（F-80-余 v2，镜像 wb_route::race_first_byte）：对冲关闭（阈值 0）
/// 时与纯首字超时语义完全一致；开启时主请求首字节超阈值 → 从池内取第二账号
/// （走同一 busy 过滤——主账号已 inflight 天然让位，受 F-77 并发上限约束）
/// 发对冲请求，先出首字者胜。对冲请求独立解析凭证（identity 回调）并按对冲
/// 账号 uid 重建 agent 信封（session 派生依赖 uid）。
#[allow(clippy::too_many_arguments)]
fn race_qoder_first_byte(
    state: &Arc<ApiSharedState>,
    primary_uid: &str,
    primary_reader: Box<dyn std::io::Read + Send>,
    peek: &Value,
    entry: &Value,
    model_key: &str,
    model_source: &str,
    tried: &HashSet<String>,
    allowed: Option<&HashSet<String>>,
    dedicated: Option<&str>,
    primary_session_ids: &Option<(String, String, String)>,
    want_global: bool,
) -> Result<QoderRaceWin, ()> {
    let hedge_ms = state
        .qoder_hedge_threshold_ms
        .load(std::sync::atomic::Ordering::Relaxed);
    if hedge_ms == 0 {
        // 首字窗口按区域：海外静默排队普遍 6–14s，10s 判死会把排队误判故障
        let lines = lines_with_first_byte_timeout_for(primary_reader, want_global)?;
        return Ok(QoderRaceWin {
            lines,
            uid: primary_uid.to_string(),
            hedge: None,
            takeover: false,
            session_ids: primary_session_ids.clone(),
        });
    }
    let state2 = state.clone();
    let tried2 = tried.clone();
    let allowed2 = allowed.cloned();
    let dedicated2 = dedicated.map(str::to_string);
    let peek2 = peek.clone();
    let entry2 = entry.clone();
    let mk2 = model_key.to_string();
    let ms2 = model_source.to_string();
    let spawn_backup = move || -> Option<(Box<dyn std::io::Read + Send>, QoderHedgeLease)> {
        let (picked2, ev) = state2
            .qoder_pool
            .pick_excluding_constrained_ev_region(&tried2, allowed2.as_ref(), dedicated2.as_deref(), Some(want_global))?;
        // F-77⑤ 可观测：对冲取号同样记录 busy 让位/降级事件
        if let Some(ev) = ev {
            state2.logger.log_sched_event(&ev);
        }
        // 租约先于凭证解析获取：失败路径随闭包局部变量 Drop 自动 -1
        let creds2 = (state2.qoder_identity.as_ref()? )(&picked2.uid).ok()?;
        let converted2 = qoder_upstream::prepare_qoder_body(&peek2, &entry2, &creds2.uid).ok()?;
        // G4：对冲信封 id 随租约带出，接管记账按对冲账号信封落定
        let session_ids = extract_envelope_ids(&converted2);
        let lease = QoderHedgeLease::acquire(&state2, &picked2.uid, session_ids);
        let reader2 = qoder_upstream::make_qoder_request(&creds2, &converted2, &mk2, &ms2).ok()?;
        Some((reader2, lease))
    };
    match lines_with_first_byte_hedged_timeout(
        primary_reader,
        hedge_ms,
        spawn_backup,
        first_byte_timeout_for(want_global),
    ) {
        Ok(out) => {
            let uid = if out.takeover {
                out.hedge
                    .as_ref()
                    .map(|l| l.uid.clone())
                    .unwrap_or_else(|| primary_uid.to_string())
            } else {
                primary_uid.to_string()
            };
            // G4：会话 id 按胜者信封落定——接管取对冲信封 id，主胜取主信封 id
            let session_ids = if out.takeover {
                out.hedge.as_ref().and_then(|l| l.session_ids.clone())
            } else {
                primary_session_ids.clone()
            };
            Ok(QoderRaceWin {
                lines: out.lines,
                uid,
                hedge: out.hedge,
                takeover: out.takeover,
                session_ids,
            })
        }
        Err(()) => Err(()),
    }
}

/// 竞速结束后处理对冲计数与日志（镜像 wb_route::settle_hedge）：
/// 释放对冲账号竞速窗口占用；接管时 guard 重绑到对冲账号；[SCHED] 日志记录
/// hedge_takeover / hedge_lost
fn settle_qoder_hedge(
    state: &ApiSharedState,
    win: &mut QoderRaceWin,
    mut guard: InflightGuard,
    primary_uid: &str,
) -> InflightGuard {
    let Some(lease) = win.hedge.take() else {
        return guard;
    };
    let hedge_uid = lease.uid.as_str();
    if win.takeover {
        state
            .logger
            .log_sched_event(&format!("hedge_takeover primary={} hedge={}", primary_uid, hedge_uid));
        guard = guard.bind_account(lease.counter.clone());
    } else {
        state
            .logger
            .log_sched_event(&format!("hedge_lost primary={} hedge={}", primary_uid, hedge_uid));
    }
    drop(lease);
    guard
}

/// 竞速胜者原始行源 → 翻译为 OpenAI chunk 行 → 可中断行源（对冲开启路径）。
/// 对冲关闭时 run_* 直接走 open_qoder_stream（等价实现，少一层迭代器包装）
fn translate_race_lines(
    win: QoderRaceWin,
    err_slot: Arc<Mutex<Option<ErrMeta>>>,
    chat_id: &str,
    model: &str,
) -> InterruptibleLines {
    let translated = qoder_upstream::QoderTranslate::new(win.lines, err_slot, chat_id, model);
    InterruptibleLines::from_iterator(Box::new(translated))
}

// ==================== 流式入口 ====================

/// Qoder 流式对话（routes.rs 各协议端点 TargetPool::Qoder 分支调用）
pub fn qoder_stream_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
) -> Response {
    let (tx, rx) = tokio::sync::mpsc::channel(64);

    // 批次 D-1 线程隔离：流任务迁入专用阻塞池（同 wb_stream_chat）
    super::stream_runtime().spawn_blocking(move || {
        let chat_id = match proto {
            Protocol::OpenAi => format!("chatcmpl-{}", now_ts()),
            Protocol::OpenAiText => format!("cmpl-{}", now_ts()),
            Protocol::Anthropic => format!("msg_{}", now_ts()),
            Protocol::Responses => format!("resp_{}", now_ts()),
        };

        // SSE keep-alive 15s：watch + DoneSignal 方案（主任务结束含 panic 展开
        // 置 done=true，ticker 退出放行流终结）——与 wb_stream_chat 同构
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
                        _ = done_rx.changed() => break,
                    }
                }
            });
        }

        run_qoder_stream(&state, &body_vec, &model, proto, &key_id, &chat_id, &tx, start_ts, guard);
    });

    let stream = ReceiverStream::new(rx);
    Response::builder()
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap_or_else(|_| internal_error_response())
}

#[allow(clippy::too_many_arguments)]
fn run_qoder_stream(
    state: &Arc<ApiSharedState>,
    body_vec: &[u8],
    model: &str,
    proto: Protocol,
    key_id: &str,
    chat_id: &str,
    tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    start_ts: Instant,
    mut guard: InflightGuard,
) {
    let key_name = super::api_keys::key_name_for(&state.data_dir, key_id);
    let peek: Value = serde_json::from_slice(body_vec).unwrap_or(json!({}));

    // 目录条目（upstreamKey/config）：调度命中后目录刷新导致条目消失时按 404 透传。
    // 区域分流（2026-10-17 海外接入）：CN 优先（历史语义）；CN 缺失或命中
    // Global 专属条目时回落 Global 目录 → want_global=true，选号只出海外账号、
    // 对话走 api3.qoder.sh（make_qoder_request 按 creds 区域选网关）
    let (entry, want_global) = match qoder_upstream::resolve(model, qoder_upstream::QoderRegion::Cn)
    {
        Some(e) if !entry_is_global_only(model, &e) => (e, false),
        _ => match qoder_upstream::resolve(model, qoder_upstream::QoderRegion::Global) {
            Some(g) => (g, true),
            None => {
                let msg = format!("model {model} not in Qoder catalog (cn + global)");
                state.logger.log_request(
                    "qoder", "POST", "/v1/chat/completions", model, true, 404, "-",
                    start_ts.elapsed().as_millis() as u64, &key_name, "", Some(&msg),
                );
                send_stream_error(tx, proto, 404, &msg);
                return;
            }
        },
    };
    let model_key = entry
        .get("upstreamKey")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let model_source = entry
        .pointer("/config/source")
        .and_then(Value::as_str)
        .unwrap_or("system")
        .to_string();

    // F-35 子 Key 约束（qoder 池作用域；匿名/无约束 Key 全空 → 走默认调度）
    let (allowed_set, dedicated, key_want_global) =
        super::api_keys::constraints_for(&state.data_dir, key_id)
            .and_then(|k| k.pool_constraints("qoder"))
            .map_or((None, None, None), |c| (c.allowed, c.dedicated, c.want_global));
    // Key 区域约束覆盖模型推导（2026-10-18 修复海外池空池：「海外号池」Key 的
    // region=global 此前在流式主路径被丢弃——只取了 allowed/dedicated，导致
    // want_global 恒由模型目录推导；qwen3.8-flash 在 CN 目录也存在 → false →
    // 选号只在国内号里转，海外号永不入选，国内号全冻结后整池 no_healthy）。
    // 语义：显式 Some 优先；模型仅海外有（want_global=true）而 Key 限国内 →
    // 硬冲突快速失败（不在错误区域浪费一次上游请求）。
    let want_global = match key_want_global {
        Some(false) if want_global => {
            let msg = format!(
                "upstream 404 error: model {model} only available in Qoder Global region, but current key is restricted to the CN pool"
            );
            state.logger.log_request(
                "qoder", "POST", "/v1/chat/completions", model, true, 404, "-",
                start_ts.elapsed().as_millis() as u64, &key_name, "", Some(&msg),
            );
            send_stream_error(tx, proto, 404, &msg);
            return;
        }
        Some(true) if !want_global => {
            // Key 限定海外但模型只在 CN 有 → 硬冲突；两区都有则按 Key 走海外
            if qoder_upstream::resolve(model, qoder_upstream::QoderRegion::Global).is_none() {
                let msg = format!(
                    "upstream 404 error: model {model} only available in Qoder CN region, but current key is restricted to the Global pool"
                );
                state.logger.log_request(
                    "qoder", "POST", "/v1/chat/completions", model, true, 404, "-",
                    start_ts.elapsed().as_millis() as u64, &key_name, "", Some(&msg),
                );
                send_stream_error(tx, proto, 404, &msg);
                return;
            }
            true
        }
        Some(k) => k,
        None => want_global,
    };

    // F-80-余 v2 会话粘性（开关关闭时恒 None，v1 轮换行为零变化）：粘性命中且
    // 账号在池 → 首选粘住账号（busy 且有空闲候选时让位，F-77④ 同构）；子 Key
    // 限定上游不含粘性账号时忽略粘性。同账号 + 同种子派生同一上游 session_id
    //（prepare_qoder_body session_id_for），粘住账号即保住上游会话侧复用
    let sticky_key = SessionKey::from_body(&peek);
    let sticky0: Option<String> = if state
        .qoder_sticky_enabled
        .load(std::sync::atomic::Ordering::Relaxed)
    {
                state
                    .qoder_sticky
                    .resolve(&sticky_key, now_ts())
                    .and_then(|b| {
                        if allowed_set.as_ref().is_some_and(|a| !a.contains(&b.uid)) {
                            return None;
                        }
                        state.qoder_pool.pick_by_uid_region(&b.uid, Some(want_global)).map(|_| b.uid)
                    })
            } else {
                None
            };
            // 绑定回写用会话种子（仅落库留档；session 由 body 构造时按 uid+seed 派生）
            let sticky_seed: String = peek
        .get("session_id")
        .and_then(Value::as_str)
        .or_else(|| peek.get("user").and_then(Value::as_str))
        .unwrap_or("-")
        // 审查 P3：seed 进 sticky 存储做会话匹配，超长值放大留档体积——
        // 限长 128 字符（与上游请求体 session_seed 同款确定性截断）
        .chars()
        .take(128)
        .collect();

    // 首选：粘性 > 调度策略（F-77④：粘性账号 busy 且有空闲候选时让位）
    let mut first_pick: Option<super::pool::PickedAccount> = sticky0.as_ref().and_then(|u| {
        state
            .qoder_pool
            .pick_sticky_yield_region(u, allowed_set.as_ref(), Some(want_global))
            .map(|(p, ev)| {
                if let Some(ev) = ev {
                    state.logger.log_sched_event(&ev);
                }
                p
            })
    });

    let mut tried: HashSet<String> = HashSet::new();
    let mut empty_hits: usize = 0; // 空完成计数：轮换耗尽收尾区分指纹拦截与账号耗尽

    loop {
        // 客户端断连检测：通道关闭即终止轮换/重试（对齐 run_wb_stream）。
        // 审查 M5：断连也是请求终局，补记一行失败用量（未取号 → "none"，
        // 与下方轮换耗尽收尾同口径），防用量页 requests 缺口
        if tx.is_closed() {
            state.record_usage_qoder(
                model, "none", key_id, false, true,
                start_ts.elapsed().as_millis() as u64, 0, 0, None, None, None,
                &session_ctx(&peek),
            );
            return;
        }
        // ── 取号：粘性命中优先，否则按 Key 约束 + 调度策略；换号后仅走策略 ──
        let picked = match first_pick.take() {
            Some(p) => p,
            None => match state
                .qoder_pool
                .pick_excluding_constrained_ev_region(&tried, allowed_set.as_ref(), dedicated.as_deref(), Some(want_global))
            {
                Some((p, ev)) => {
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                }
                None => {
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    state.record_usage_qoder(model, "none", key_id, false, true, duration_ms, 0, 0, None, None, None, &session_ctx(&peek));
                    // 审查 P2：轮换耗尽收尾对齐 routes.rs（issue #57 可观测）——
                    // 空完成主导 → 明示渠道指纹拦截语义（账号/模型无关，提示更新
                    // wb_template_map.json），否则维持无健康账号文案；
                    // code 保持 no_healthy_account（客户端兼容）
                    let (healthy_total, _) = state.qoder_pool.selectable_stats_in_region(None, Some(want_global));
                    let (exhaust_msg, exhaust_tag) =
                        super::routes::exhaust_message(empty_hits, tried.len(), healthy_total);
                    state.logger.log_request(
                        "qoder", "POST", "/v1/chat/completions", model, true, 503, "none",
                        duration_ms, &key_name, "", Some(&exhaust_msg),
                    );
                    state.logger.log_debug_line(format!(
                        "[DEBUG] {exhaust_tag} tried={} empty_hits={} pool=qoder reasons=[{}]{}",
                        tried.len(),
                        empty_hits,
                        // 可观测补齐（对齐 buddy 池）：此前 qoder 无健康账号时只报
                        // tried 计数，无法区分「全在冷却 / 目标区域无可用号 / 被
                        // Key 约束排除」——排查 503 时要靠猜
                        state
                            .qoder_pool
                            .status_list()
                            .iter()
                            .map(|s| {
                                let st = if s.disabled {
                                    format!("disabled({})", s.cooldown_reason.as_deref().unwrap_or("-"))
                                } else if s.hard_credit {
                                    "hard_credit".to_string()
                                } else if s.cooling {
                                    format!("cooling({})", s.cooldown_reason.as_deref().unwrap_or("-"))
                                } else {
                                    s.state.clone()
                                };
                                format!("{}({})", s.name, st)
                            })
                            .collect::<Vec<_>>()
                            .join(", "),
                        // issue #99 ①：影子风控（空完成主导）收尾落原始指纹样本（三池同口径）
                        if exhaust_tag == "CHANNEL_FINGERPRINT_BLOCK" {
                            format!(" 指纹样本: {}", super::wb_payload::fingerprint_sample(body_vec))
                        } else {
                            String::new()
                        }
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
        // 免费额度 (uid,model) 冷却判定（同 wb_route T5.3 语义，2026-10-19）：
        // 该号仅在这个免费模型上被上游限到明天 00:00，账号本身健康。池内有
        // 其他同区备选才换号；无备选（单号池）放行——让上游自己决定是否拒绝，
        // 不伪造 503。
        if state.qoder_pool.is_model_cooling(&picked.uid, model)
            && state.qoder_pool.has_other_selectable_for_model(
                &picked.uid,
                allowed_set.as_ref(),
                Some(want_global),
            )
        {
            state.logger.log_sched_event(&format!(
                "free_quota_skip uid={} model={} (该免费模型冷却至次日00:00)",
                picked.uid, model
            ));
            tried.insert(picked.uid.clone());
            continue;
        }
        tried.insert(picked.uid.clone());
        // 当前账号排队重试计数（账号局部：换号自然重置）
        let mut queue_same: u32 = 0;
        // F-77 账号级在途计数：取号即绑定（换号时 bind_account 自动解绑旧账号）。
        // 审查 P1-4 TOCTOU 收紧：普通候选 CAS 复核「在途 < 并发上限」，已满则
        // 不发起请求换下一候选（不计错误不冷却）；over_limit 候选（粘性锁定、
        // busy_fallback 降级）保留原无条件绑定语义。
        // active_uid 仅在绑定成功后更新（CAS 拒绝的候选不得污染「当前账号」展示）
        let bound = if picked.over_limit {
            guard = guard.bind_account(state.qoder_pool.inflight_handle(&picked.uid));
            true
        } else {
            guard.bind_account_cas(
                state.qoder_pool.inflight_handle(&picked.uid),
                state.qoder_pool.concurrency_limit(),
            )
        };
        if !bound {
            continue;
        }
        *safe_lock(&state.active_uid) = Some(picked.uid.clone());

        // 凭证解析：回调缺失（未注入/单测）→ 换号不冷却（配置问题非账号问题）；
        // 回调报错（PAT 换令牌失败/登录态失效）→ SessionDead 禁用后换号
        let creds: QoderCreds = match state.qoder_identity.as_ref() {
            None => {
                state.logger.log_request(
                    "qoder", "POST", "/v1/chat/completions", model, true, 503, &picked.uid,
                    start_ts.elapsed().as_millis() as u64, &key_name,
                    &state.qoder_pool.name_of(&picked.uid), Some("qoder identity 回调未注入"),
                );
                continue;
            }
            Some(resolve) => match resolve(&picked.uid) {
                Ok(c) => c,
                Err(e) => {
                    // 审查修复：瞬时失败（网络/5xx，带 TRANSIENT_ERR_TAG）记 Server 熔断
                    //（可自愈）；仅永久失效 SessionDead 禁用
                    let kind = if crate::tasks::qoder_common::is_transient_identity_err(&e) {
                        ErrKind::Server
                    } else {
                        ErrKind::SessionDead
                    };
                    state.qoder_pool.note_error(&picked.uid, kind);
                    // issue #67：硬冷却/禁用类错误解绑该账号粘性
                    if kind.unbinds_sticky() {
                        state.qoder_sticky.unbind_uid(&picked.uid);
                    }
                    *safe_lock(&state.last_error) = Some(format!(
                        // 审查修复：err 原文（上游响应/URL 片段可能含令牌）截断脱敏，
                        // 同 HTTP 分支 safe_slice 口径
                        "qoder identity uid={} err={}",
                        picked.uid,
                        safe_slice(&e, 200)
                    ));
                    state.logger.log_request(
                        "qoder", "POST", "/v1/chat/completions", model, true, 503, &picked.uid,
                        start_ts.elapsed().as_millis() as u64, &key_name,
                        &state.qoder_pool.name_of(&picked.uid), Some("凭证解析失败"),
                    );
                    continue;
                }
            },
        };

        // 请求体（agent 信封；依赖账号 uid 派生 session）
        let converted = match qoder_upstream::prepare_qoder_body(&peek, &entry, &creds.uid) {
            Ok(b) => b,
            Err(e) => {
                state.logger.log_request(
                    "qoder", "POST", "/v1/chat/completions", model, true, 500, &picked.uid,
                    start_ts.elapsed().as_millis() as u64, &key_name,
                    &state.qoder_pool.name_of(&picked.uid), Some(&e),
                );
                send_stream_error(tx, proto, 500, &e);
                return;
            }
        };

        // 审查修复 K：信封含完整对话历史（大 body），request_id/session_id 随信封恒定，
        // 在外层换号循环内解析一次供内层各次尝试记账复用，避免热路径重复全量反序列化
        let envelope_ids = extract_envelope_ids(&converted);

        let mut same_attempt: u32 = 0;
        // 对冲接管标记（本轮尝试内有效）：接管后排队同号退避不再适用——
        // 退避重试走主账号 creds/converted，与生效（对冲）账号不一致。
        // 初值在 Ok 分支必然先赋值后读取，lint 对初值误报，显式允许
        #[allow(unused_assignments)]
        let mut hedge_taken = false;
        loop {
            // 断连检测：重试等待/长路径期间离开则终止（guard 随 Drop 释放）。
            // 审查 M5：断连为请求终局，补记一行失败用量（已绑定 picked 账号）
            if tx.is_closed() {
                state.record_usage_qoder(
                    model, &picked.uid, key_id, false, true,
                    start_ts.elapsed().as_millis() as u64, 0, 0, None, None, None,
                    &session_ctx(&peek),
                );
                return;
            }
            // F-76① TTFT 采样（同 wb_route：各次尝试独立计时，竞速胜者首字即 TTFB）
            let ttfb_start = Instant::now();
            match qoder_upstream::make_qoder_request(
                &creds,
                &converted,
                &model_key,
                &model_source,
            ) {
                Ok(reader) => {
                    // 首字窗口按区域（国内 10s / 海外 60s）：超时**不再按故障计错**——
                    // 「未产出首字节」与「请求失败」是两回事：海外上游普遍静默排队
                    //（无 10605 排队票，只握连接不吐字节），10s 判死曾把排队误判为
                    // 账号故障 → note_error(Server) 冷却 + 换号重排，逐步抽干海外池
                    //（2026-10-19 20:41 连报 no healthy account available healthy=2）。
                    // 现仅做连接层面放弃 + 换号，不动账号健康度、不进冷却。
                    let err_slot: Arc<Mutex<Option<ErrMeta>>> = Arc::new(Mutex::new(None));
                    let mut race = match race_qoder_first_byte(
                        state,
                        &picked.uid,
                        reader,
                        &peek,
                        &entry,
                        &model_key,
                        &model_source,
                        &tried,
                        allowed_set.as_ref(),
                        dedicated.as_deref(),
                        &envelope_ids,
                        want_global,
                    ) {
                        Ok(w) => w,
                        Err(()) => {
                            state.logger.log_sched_event(&format!(
                                "first_byte_timeout uid={} model={} region={} ttfb_window={}ms（排队/无首字节，按上游慢处理：不计错不冷却）",
                                picked.uid,
                                model,
                                if want_global { "global" } else { "cn" },
                                ttfb_start.elapsed().as_millis(),
                            ));
                            break;
                        }
                    };
                    let ttfb_ms = ttfb_start.elapsed().as_millis() as u64;
                    // 对冲计数落定 + guard 重绑（接管时生效账号 = 对冲账号）
                    guard = settle_qoder_hedge(state, &mut race, guard, &picked.uid);
                    hedge_taken = race.takeover;
                    let win_uid = race.uid.clone();
                    // G4：记账会话 id 按胜者信封落定（race 在 translate 处被消费，先取快照）
                    let win_session_ids = race.session_ids.clone().or_else(|| envelope_ids.clone());
                    // 竞速胜者行源（原始上游行）统一翻译后喂 stream_forward_ex
                    let ilines = translate_race_lines(race, err_slot.clone(), chat_id, model);
                    let (error_info, sent_any, failed_inline, usage) =
                        // Qoder 无 namespace 展平编码（Responses 分支为防御性兜底，
                        // 实际仅 OpenAi/Anthropic 可达）→ 空集合禁用编码名拆分（审查 F4）
                        wb_sse::stream_forward_ex(ilines, tx, proto, chat_id, model, &std::collections::HashSet::new());
                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                    let meta = err_slot
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .clone();
                    // M1 修复（审查）：as_u64 窄口径对字符串/浮点数字段形态静默
                    // 得 0——改用 wb_sse::u64_pair 宽口径（与 wb/Trae 记账同源）
                    let (pt, ct) = usage.as_ref().map(wb_sse::u64_pair).unwrap_or((0, 0));
                    // M4：缓存读 tokens 同源宽口径提取，终态旁路记账（0 值不入账）
                    let cached = usage.as_ref().map(wb_sse::cached_tokens_of).unwrap_or(0);
                    let (credits, original_credits) = extract_billing(usage.as_ref());
                    // 审查修复（中等项）：流式记账对齐非流式「每请求一行」口径——
                    // 不再逐尝试记账（原每次尝试记一条，换号重试致 requests/errors
                    // 虚高）；usage 仅在请求终局（成功/就地收尾/冷却终止/Fatal/取号
                    // 耗尽）处记录，换号中间态（break 继续轮换）不记
                    match error_info {
                        Some((code, msg)) => {
                            // 流内错误分类以 ErrMeta 为准（translate 写入，含排队/额度信号）
                            match meta.as_ref().map(|m| m.kind) {
                                Some(UpstreamKind::Queued) => {
                                    // 排队：同号退避重试为主（对冲接管后不适用——重试
                                    // 凭证/请求体属主账号，与生效账号不一致，直接换号）；
                                    // 若该模型已被并发请求记入冷却，则直接 break 换号
                                    //（快速失败）
                                    if !sent_any && !hedge_taken {
                                        let secs = meta
                                            .as_ref()
                                            .and_then(|m| m.queue.as_ref())
                                            .and_then(|q| q.retry_after_secs)
                                            .unwrap_or(QUEUE_DEFAULT_BACKOFF_SECS)
                                            .clamp(1, QUEUE_BACKOFF_MAX_SECS);
                                        // 审查修复：模型级冷却——排队是模型级现象，
                                        // 记录窗口供 dispatch 预检快速回退；已在冷却
                                        // 中（并发请求刚记录过）则直接终止轮换返回
                                        // 429，不再对池内每个账号硬打已知故障上游
                                        //（原 break 换号会轮换到耗尽，逐账号真实请求）。
                                        // 快退判定按本请求区域作用域（2026-10-19）：
                                        // 此前不传 want_global，国内健康号让海外请求
                                        // 误判「有备选」，快退永不触发。
                                        if model_cooling_remaining_secs(model, Some(want_global)).is_some()
                                            && !pool_has_other_selectable(
                                                &state,
                                                &win_uid,
                                                allowed_set.as_ref(),
                                                Some(want_global),
                                            )
                                        {
                                            let duration_ms =
                                                start_ts.elapsed().as_millis() as u64;
                                            state.record_usage_qoder(
                                                model, &win_uid, key_id, false, true,
                                                duration_ms, 0, 0, None, None, None,
                                                &session_ctx(&peek),
                                            );
                                            state.logger.log_request(
                                                "qoder", "POST", "/v1/chat/completions",
                                                model, true, 429, &win_uid, duration_ms,
                                                &key_name,
                                                &state.qoder_pool.name_of(&win_uid),
                                                Some("模型冷却中，终止轮换"),
                                            );
                                            send_stream_error(
                                                tx, proto, 429,
                                                &format!("model {model} cooling down, retry later"),
                                            );
                                            return;
                                        }
                                        // 单账号退避打满 → 终止返回 429（189s 假死修复，
                                        // 2026-10-19）：换号必然再排（排队按模型不按账号），
                                        // 逐号 30s×3 轮换把海外池放大成 90s×号数后 503
                                        //「no healthy account」（实测 503 189305ms 三连）。
                                        // 明确 429 让客户端按 Retry 语义等待，优于静默空转。
                                        if queue_same >= queue_retry_limit_for(want_global) {
                                            let duration_ms =
                                                start_ts.elapsed().as_millis() as u64;
                                            state.record_usage_qoder(
                                                model, &win_uid, key_id, false, true,
                                                duration_ms, 0, 0, None, None, None,
                                                &session_ctx(&peek),
                                            );
                                            state.logger.log_request(
                                                "qoder", "POST", "/v1/chat/completions",
                                                model, true, 429, &win_uid, duration_ms,
                                                &key_name,
                                                &state.qoder_pool.name_of(&win_uid),
                                                Some("同号退避打满，终止轮换"),
                                            );
                                            send_stream_error(
                                                tx, proto, 429,
                                                &format!("model {model} queued, retry later"),
                                            );
                                            return;
                                        }
                                        note_model_cooldown(model, secs, want_global);
                                        queue_same += 1;
                                        if !sleep_interruptible(Duration::from_secs(secs), Some(tx)) {
                                            // 审查 M5：断连为请求终局，补记失败终态用量
                                            state.record_usage_qoder(
                                                model, &win_uid, key_id, false, true,
                                                start_ts.elapsed().as_millis() as u64, 0, 0,
                                                None, None, None, &session_ctx(&peek),
                                            );
                                            return; // 客户端断连：提前退出
                                        }
                                        continue;
                                    }
                                    // sent_any：不再 break 硬换号——内容已流出时换号重发
                                    // 会向同一 SSE 流拼接第二份完整响应；交下方公共收尾
                                    //（!sent_any→换号，sent_any→就地收尾），与其他错误
                                    // 分支口径一致；hedge_taken 同前直接落公共收尾换号
                                }
                                Some(kind) => {
                                    let ek = kind.to_err_kind();
                                    if ek != ErrKind::None {
                                        state.qoder_pool.note_error(&win_uid, ek);
                                        // issue #67：硬冷却/禁用类错误解绑该账号粘性
                                        if ek.unbinds_sticky() {
                                            state.qoder_sticky.unbind_uid(&win_uid);
                                        }
                                    }
                                    // 免费额度耗尽（114/115/119/「免费额度」文案）：
                                    // 账号健康（积分照常可用），只按 (uid, model)
                                    // 冷却到次日 00:00——整号 HardCredit 冻结把付费
                                    // 路径一起误杀（2026-10-19 事故）。to_err_kind
                                    // 已返回 None 不碰账号，这里补记录与冷却。
                                    if kind == UpstreamKind::FreeQuota {
                                        note_free_quota_exhausted(&state.data_dir, &win_uid, model);
                                        state.qoder_pool.per_model_cooldown_until(
                                            &win_uid,
                                            model,
                                            super::pool::next_0000(now_ts()),
                                        );
                                    }
                                    // issue #90 修复①：上游服务端异常（5xx/业务 504）纳入
                                    // 模型级渐进冷却——仅在内容未流出时记账（sent_any=true
                                    // 说明模型实际在服务，尾部错误帧不代表模型故障）
                                    if kind == UpstreamKind::Server && !sent_any {
                                        note_model_server_cooldown(model, want_global);
                                    }
                                    // issue #90 修复②：last_error 带模型名 + 时刻（此前只有
                                    // uid/code/msg，无法定位是哪个模型、故障从何时开始）
                                    *safe_lock(&state.last_error) = Some(format!(
                                        "qoder uid={} model={} code={} msg={} at {}",
                                        win_uid,
                                        model,
                                        code,
                                        msg,
                                        now_cn_timestamp()
                                    ));
                                }
                                None => {
                                    // translate 之外的错误帧。EOF 零完成哨兵（-9901）不经
                                    // translate 写 ErrMeta，必落本分支（非「理论不可达」）：
                                    // 上游间歇性空回放属暂态，按 wb_route 口径免熔断，
                                    // 仅由下方 !sent_any 分支换号重试
                                    if super::is_empty_completion(code, &msg) {
                                        empty_hits += 1; // 空完成计数：耗尽收尾区分指纹拦截
                                    } else {
                                        state.qoder_pool.note_error(&win_uid, ErrKind::Server);
                                    }
                                }
                            }
                            if !sent_any {
                                // 流未开始：错误不下发，允许换号重试（换号中间态不记 usage）
                                break;
                            }
                            // 就地收尾终局：记 usage（部分成功按实际 usage 落账）
                            state.record_usage_cached(super::usage::UsageBucket::Qoder, cached);
                            state.record_usage_qoder(
                                model,
                                &win_uid,
                                key_id,
                                false,
                                true,
                                duration_ms,
                                pt,
                                ct,
                                Some(ttfb_ms),
                                credits,
                                original_credits,
                                &session_ctx_from_ids(&peek, win_session_ids.as_ref()),
                            );
                            state.logger.log_request_ttfb(
                                "qoder", "POST", "/v1/chat/completions", model, true, 200,
                                &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                &state.qoder_pool.name_of(&win_uid), Some(&msg),
                            );
                            return; // 已有数据流出：就地收尾
                        }
                        None => {
                            if failed_inline {
                                // 流内失败已就地透传客户端：不重试不记账冷却（同 wb_route）
                                state.logger.log_request_ttfb(
                                    "qoder", "POST", "/v1/chat/completions", model, true, 200,
                                    &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                    &state.qoder_pool.name_of(&win_uid),
                                    Some("流内失败已透传客户端"),
                                );
                                return;
                            }
                            clear_model_cooldown(model, want_global); // 审查修复：成功即清模型级冷却
                            state.qoder_pool.note_success(&win_uid);
                            // 成功终局：记 usage（审查修复——原逐尝试记账已移除，
                            // 成功行在此统一落账）；M4：缓存读 tokens 旁路同落
                            state.record_usage_cached(super::usage::UsageBucket::Qoder, cached);
                            state.record_usage_qoder(
                                model,
                                &win_uid,
                                key_id,
                                true,
                                true,
                                duration_ms,
                                pt,
                                ct,
                                Some(ttfb_ms),
                                credits,
                                original_credits,
                                &session_ctx_from_ids(&peek, win_session_ids.as_ref()),
                            );
                            // F-80-余 v2：绑定粘性会话（开关开启时；Mutex 内 re-check
                            // 防 TOCTOU 由 StickyStore::bind 保证）
                            if state
                                .qoder_sticky_enabled
                                .load(std::sync::atomic::Ordering::Relaxed)
                            {
                                state.qoder_sticky.bind(&sticky_key, &win_uid, &sticky_seed, now_ts());
                                state.qoder_sticky.save(&state.data_dir);
                            }
                            state.logger.log_request_ttfb(
                                "qoder", "POST", "/v1/chat/completions", model, true, 200,
                                &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                &state.qoder_pool.name_of(&win_uid), None,
                            );
                            return;
                        }
                    }
                }
                Err((status, resp_body, retry_after)) => {
                    // HTTP 层分级重试（429/5xx/502 同号退避；401/403/4xx 换号/终止）。
                    // 429 且文案带精确重置时刻 = 该账号额度/频率已耗尽，同号重试必然
                    // 继续失败：直接换号并冻结到上游给出的时刻（对齐 WB 同款处置）。
                    let retry_action = if status == 429 {
                        if let Some(ts) = super::wb_route::parse_quota_reset_at(&resp_body) {
                            state.qoder_pool.cooldown_until(&picked.uid, ts);
                            RetryAction::SwitchKey
                        } else {
                            retry_plan(status, &resp_body, same_attempt, retry_after)
                        }
                    } else {
                        retry_plan(status, &resp_body, same_attempt, retry_after)
                    };
                    match retry_action {
                        RetryAction::RetrySame { delay_ms } => {
                            same_attempt += 1;
                            // 审查修复：分段 sleep + 断连感知（原整段 sleep 最长 60s，
                            // 断连后仍空等；原 sleep 后的 is_closed 检查一并合并）
                            if !sleep_interruptible(
                                Duration::from_millis(delay_ms.min(60_000)),
                                Some(tx),
                            ) {
                                // 审查 M5：断连为请求终局，补记失败终态用量
                                state.record_usage_qoder(
                                    model, &picked.uid, key_id, false, true,
                                    start_ts.elapsed().as_millis() as u64, 0, 0,
                                    None, None, None, &session_ctx(&peek),
                                );
                                return;
                            }
                            continue;
                        }
                        RetryAction::SwitchKey => {
                            // Qoder 业务错误常在 403/200 信封里：按 upstream 分类；
                            // 排队（10605）同号退避优先——模型已在冷却中则
                            // break 换号（快速失败，见下方冷却检查）
                            let classified =
                                qoder_upstream::classify_upstream_error(status, &resp_body);
                            if classified.kind == UpstreamKind::Queued {
                                // 同流内 Queued 分支：单号退避打满/冷却中 → 429 终止
                                // 轮换（不再换号空转放大，189s 假死修复）；快退按区域
                                if queue_same >= queue_retry_limit_for(want_global)
                                    || (model_cooling_remaining_secs(model, Some(want_global)).is_some()
                                        && !pool_has_other_selectable(
                                            &state,
                                            &picked.uid,
                                            allowed_set.as_ref(),
                                            Some(want_global),
                                        ))
                                {
                                    let duration_ms = start_ts.elapsed().as_millis() as u64;
                                    let reason = if queue_same >= queue_retry_limit_for(want_global) {
                                        "同号退避打满，终止轮换"
                                    } else {
                                        "模型冷却中，终止轮换"
                                    };
                                    state.record_usage_qoder(
                                        model, &picked.uid, key_id, false, true,
                                        duration_ms, 0, 0, None, None, None,
                                        &session_ctx(&peek),
                                    );
                                    state.logger.log_request(
                                        "qoder", "POST", "/v1/chat/completions", model, true,
                                        429, &picked.uid, duration_ms, &key_name,
                                        &state.qoder_pool.name_of(&picked.uid),
                                        Some(reason),
                                    );
                                    send_stream_error(
                                        tx, proto, 429,
                                        &format!("model {model} queued, retry later"),
                                    );
                                    return;
                                }
                                let secs = classified
                                    .queue
                                    .as_ref()
                                    .and_then(|q| q.retry_after_secs)
                                    .unwrap_or(QUEUE_DEFAULT_BACKOFF_SECS)
                                    .clamp(1, QUEUE_BACKOFF_MAX_SECS);
                                note_model_cooldown(model, secs, want_global);
                                queue_same += 1;
                                if !sleep_interruptible(Duration::from_secs(secs), Some(tx)) {
                                    // 审查 M5：断连为请求终局，补记失败终态用量
                                    state.record_usage_qoder(
                                        model, &picked.uid, key_id, false, true,
                                        start_ts.elapsed().as_millis() as u64, 0, 0,
                                        None, None, None, &session_ctx(&peek),
                                    );
                                    return; // 客户端断连：提前退出
                                }
                                continue;
                            }
                            let kind = classified.kind.to_err_kind();
                            if kind != ErrKind::None {
                                state.qoder_pool.note_error(&picked.uid, kind);
                                // issue #67：硬冷却/禁用类错误解绑该账号粘性
                                if kind.unbinds_sticky() {
                                    state.qoder_sticky.unbind_uid(&picked.uid);
                                }
                            }
                            // 免费档模型额度耗尽记录（免费额度可见性）：额度类是
                            // 账号×模型维度的事实，池冷却只按账号记，「哪个账号的
                            // 免费模型还能用」此前无观测
                            if classified.kind == UpstreamKind::Quota {
                                note_free_quota_exhausted(&state.data_dir, &picked.uid, model);
                            }
                            // 免费额度类（114/115/119）：to_err_kind 已返回 None
                            // 不整号冻结，这里按 (uid,model) 冷却到次日 00:00
                            if classified.kind == UpstreamKind::FreeQuota {
                                note_free_quota_exhausted(&state.data_dir, &picked.uid, model);
                                state.qoder_pool.per_model_cooldown_until(
                                    &picked.uid,
                                    model,
                                    super::pool::next_0000(now_ts()),
                                );
                            }
                            // issue #90 修复①：HTTP 层 5xx（含 504）换号前记模型级
                            // 渐进冷却——否则 504 只逐账号轮换硬打，dispatch 预检
                            // 永不触发跨池回退（issue #90 主诉场景）
                            if classified.kind == UpstreamKind::Server {
                                note_model_server_cooldown(model, want_global);
                            }
                            *safe_lock(&state.last_error) = Some(format!(
                                "qoder uid={} model={} status={} body={} at {}",
                                picked.uid,
                                model,
                                status,
                                safe_slice(&resp_body, 200),
                                now_cn_timestamp()
                            ));
                            state.logger.log_request(
                                "qoder", "POST", "/v1/chat/completions", model, true, status,
                                &picked.uid, start_ts.elapsed().as_millis() as u64, &key_name,
                                &state.qoder_pool.name_of(&picked.uid),
                                Some(&format!("upstream status={status}")),
                            );
                            break; // 换号
                        }
                        RetryAction::Fatal => {
                            let msg =
                                format!("upstream {} error: {}", status, safe_slice(&resp_body, 300));
                            // 终态记账对齐「每请求一行」口径（审查 F3）：Fatal 是请求
                            // 终局（同 no-healthy 由取号分支记录），此前仅 log_request
                            // 导致用量统计页缺口；ttfb/credits 无失败语义传 None
                            state.record_usage_qoder(model, &picked.uid, key_id, false, true,
                                start_ts.elapsed().as_millis() as u64, 0, 0,
                                None, None, None, &session_ctx(&peek));
                            state.logger.log_request(
                                "qoder", "POST", "/v1/chat/completions", model, true, status,
                                &picked.uid, start_ts.elapsed().as_millis() as u64, &key_name,
                                &state.qoder_pool.name_of(&picked.uid), Some(&msg),
                            );
                            send_stream_error(tx, proto, status as i64, &msg);
                            return;
                        }
                    }
                }
            }
        }
    }
}

// ==================== 非流式（上游只回 SSE → 本地聚合） ====================

/// Qoder 非流式对话：聚合 + 协议投影（routes.rs 各协议端点调用）
#[allow(clippy::too_many_arguments)]
pub async fn qoder_aggregate_chat(
    state: Arc<ApiSharedState>,
    body_vec: Vec<u8>,
    model: String,
    stream: bool,
    start_ts: Instant,
    proto: Protocol,
    key_id: String,
    guard: InflightGuard,
) -> Response {
    let model_out = model.clone();
    // 聚合含分级重试（同号退避最长 60s×N），迁入 stream_runtime 专用阻塞池
    //（同 wb_aggregate_chat；长阻塞占主池会饿死鉴权等短任务）
    let result = super::stream_runtime().spawn_blocking(move || {
        let mut guard = guard;
        let key_name = super::api_keys::key_name_for(&state.data_dir, &key_id);
        let peek: Value = serde_json::from_slice(&body_vec).unwrap_or(json!({}));

        // 区域分流（2026-10-17 海外接入）：同 run_qoder_stream——CN 优先，
        // CN 缺失/Global 专属条目回落 Global 目录，选号只出该区账号
        let (entry, want_global) =
            match qoder_upstream::resolve(&model, qoder_upstream::QoderRegion::Cn) {
                Some(e) if !entry_is_global_only(&model, &e) => (e, false),
                _ => match qoder_upstream::resolve(&model, qoder_upstream::QoderRegion::Global) {
                    Some(g) => (g, true),
                    // 审查修复：走 "upstream {status} error:" 可解析通道——非流式错误
                    // 解析按前缀提取状态码，裸消息此前落 503，与流式 404 语义不一致
                    None => {
                        return Err(format!(
                            "upstream 404 error: model {model} not in Qoder catalog (cn + global)"
                        ))
                    }
                },
            };
        let model_key = entry
            .get("upstreamKey")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let model_source = entry
            .pointer("/config/source")
            .and_then(Value::as_str)
            .unwrap_or("system")
            .to_string();

        let (allowed_set, dedicated, key_want) =
            super::api_keys::constraints_for(&state.data_dir, &key_id)
                .and_then(|k| k.pool_constraints("qoder"))
                .map_or((None, None, None), |c| (c.allowed, c.dedicated, c.want_global));
        // Key 区域约束显式覆盖；硬冲突快速失败（同流式路径）
        let want_global = match key_want {
            Some(false) if want_global => {
                return Err(format!(
                    "upstream 404 error: model {model} only available in Qoder Global region, but current key is restricted to the CN pool"
                ));
            }
            Some(true) if !want_global => {
                if qoder_upstream::resolve(&model, qoder_upstream::QoderRegion::Global).is_none() {
                    return Err(format!(
                        "upstream 404 error: model {model} only available in Qoder CN region, but current key is restricted to the Global pool"
                    ));
                }
                true
            }
            Some(k) => k,
            None => want_global,
        };

        // F-80-余 v2 会话粘性（同 run_qoder_stream；开关关闭恒 None 保持 v1 行为）
        let sticky_key = SessionKey::from_body(&peek);
        let sticky0: Option<String> = if state
            .qoder_sticky_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
        {
        state
            .qoder_sticky
            .resolve(&sticky_key, now_ts())
            .and_then(|b| {
                if allowed_set.as_ref().is_some_and(|a| !a.contains(&b.uid)) {
                    return None;
                }
                state.qoder_pool.pick_by_uid_region(&b.uid, Some(want_global)).map(|_| b.uid)
            })
    } else {
        None
    };
        let sticky_seed: String = peek
            .get("session_id")
            .and_then(Value::as_str)
            .or_else(|| peek.get("user").and_then(Value::as_str))
            .unwrap_or("-")
            .to_string();
        // 首选：粘性 > 调度策略（F-77④：粘性账号 busy 且有空闲候选时让位）
        let mut first_pick: Option<super::pool::PickedAccount> = sticky0.as_ref().and_then(|u| {
            state
                .qoder_pool
                .pick_sticky_yield_region(u, allowed_set.as_ref(), Some(want_global))
                .map(|(p, ev)| {
                    if let Some(ev) = ev {
                        state.logger.log_sched_event(&ev);
                    }
                    p
                })
        });

        let mut tried: HashSet<String> = HashSet::new();
        let mut empty_hits: usize = 0; // 空完成计数：轮换耗尽收尾区分指纹拦截与账号耗尽

        loop {
            // ── 取号：粘性命中优先，否则按 Key 约束 + 调度策略；换号后仅走策略 ──
            let picked = match first_pick.take() {
                Some(p) => p,
                None => match state.qoder_pool.pick_excluding_constrained_ev_region(
                    &tried,
                    allowed_set.as_ref(),
                    dedicated.as_deref(),
                    Some(want_global),
                ) {
                    Some((p, ev)) => {
                        if let Some(ev) = ev {
                            state.logger.log_sched_event(&ev);
                        }
                        p
                    }
                    None => {
                        state.record_usage_qoder(
                            &model,
                            "none",
                            &key_id,
                            false,
                            stream,
                            start_ts.elapsed().as_millis() as u64,
                            0,
                            0,
                            None,
                            None,
                            None,
                            &session_ctx(&peek),
                        );
                        // 审查 P2：轮换耗尽收尾对齐 routes.rs——空完成主导 → 指纹拦截语义
                        let (healthy_total, _) = state.qoder_pool.selectable_stats_in_region(None, Some(want_global));
                        let (exhaust_msg, exhaust_tag) =
                            super::routes::exhaust_message(empty_hits, tried.len(), healthy_total);
                        // issue #99 ①：影子风控收尾落原始指纹样本（非流式路径无收尾
                        // [DEBUG] 行，就地补齐；三池同口径）
                        if exhaust_tag == "CHANNEL_FINGERPRINT_BLOCK" {
                            state.logger.log_debug_line(format!(
                                "[DEBUG] {exhaust_tag} tried={} empty_hits={} pool=qoder 指纹样本: {}",
                                tried.len(),
                                empty_hits,
                                super::wb_payload::fingerprint_sample(&body_vec),
                            ));
                        }
                        return Err(exhaust_msg);
                    }
                },
            };
            // 免费额度 (uid,model) 冷却判定（同流式路径，池内无备选时放行）
            if state.qoder_pool.is_model_cooling(&picked.uid, &model)
                && state.qoder_pool.has_other_selectable_for_model(
                    &picked.uid,
                    allowed_set.as_ref(),
                    Some(want_global),
                )
            {
                state.logger.log_sched_event(&format!(
                    "free_quota_skip uid={} model={} (该免费模型冷却至次日00:00)",
                    picked.uid, model
                ));
                tried.insert(picked.uid.clone());
                continue;
            }
            tried.insert(picked.uid.clone());
            // 当前账号排队重试计数（账号局部：换号自然重置）
            let mut queue_same: u32 = 0;
            // 审查 P1-4 TOCTOU 收紧：普通候选 CAS 复核「在途 < 并发上限」，已满则
            // 不发起请求换下一候选（不计错误不冷却）；over_limit 候选（粘性锁定、
            // busy_fallback 降级）保留原无条件绑定语义。
            // active_uid 仅在绑定成功后更新（CAS 拒绝的候选不得污染「当前账号」展示）
            let bound = if picked.over_limit {
                guard = guard.bind_account(state.qoder_pool.inflight_handle(&picked.uid));
                true
            } else {
                guard.bind_account_cas(
                    state.qoder_pool.inflight_handle(&picked.uid),
                    state.qoder_pool.concurrency_limit(),
                )
            };
            if !bound {
                continue;
            }
            *safe_lock(&state.active_uid) = Some(picked.uid.clone());

            let creds: QoderCreds = match state.qoder_identity.as_ref() {
                None => continue, // 回调未注入：换号（tried 增长自然耗尽后 503）
                Some(resolve) => match resolve(&picked.uid) {
                    Ok(c) => c,
                    Err(e) => {
                        // 审查修复：瞬时失败走 Server 熔断可自愈（同流式路径）
                        let kind = if crate::tasks::qoder_common::is_transient_identity_err(&e) {
                            ErrKind::Server
                        } else {
                            ErrKind::SessionDead
                        };
                        state.qoder_pool.note_error(&picked.uid, kind);
                        // issue #67：硬冷却/禁用类错误解绑该账号粘性
                        if kind.unbinds_sticky() {
                            state.qoder_sticky.unbind_uid(&picked.uid);
                        }
                        *safe_lock(&state.last_error) = Some(format!(
                            // 审查修复：同流式分支，err 原文截断脱敏
                            "qoder identity uid={} err={}",
                            picked.uid,
                            safe_slice(&e, 200)
                        ));
                        continue;
                    }
                },
            };

            let converted = match qoder_upstream::prepare_qoder_body(&peek, &entry, &creds.uid) {
                Ok(b) => b,
                Err(e) => return Err(e),
            };
            // 审查修复 K：信封 id 在外层解析一次，内层记账复用（同流式路径）
            let envelope_ids = extract_envelope_ids(&converted);

            let mut same_attempt: u32 = 0;
            // 对冲接管标记：接管后排队同号退避不适用（重试凭证/请求体属主账号）
            #[allow(unused_assignments)]
            let mut hedge_taken = false;
            loop {
                // F-76① TTFT 采样（同 wb_route 聚合：各轮独立计时，最终轮即最终回复 TTFT）
                let ttfb_start = Instant::now();
                match qoder_upstream::make_qoder_request(
                    &creds,
                    &converted,
                    &model_key,
                    &model_source,
                ) {
                    Ok(reader) => {
                        let err_slot: Arc<Mutex<Option<ErrMeta>>> = Arc::new(Mutex::new(None));
                        // 首字窗口按区域（国内 10s / 海外 60s）+ 竞速对冲；
                        // 超时/双败**不再按故障计错**（同流式分支）：未产出首字节 ≠
                        // 请求失败，海外静默排队记账冷却会抽干池子，故仅放弃换号
                        let mut race = match race_qoder_first_byte(
                            &state,
                            &picked.uid,
                            reader,
                            &peek,
                            &entry,
                            &model_key,
                            &model_source,
                            &tried,
                            allowed_set.as_ref(),
                            dedicated.as_deref(),
                            &envelope_ids,
                            want_global,
                        ) {
                            Ok(w) => w,
                            Err(()) => {
                                state.logger.log_sched_event(&format!(
                                    "first_byte_timeout uid={} model={} region={}（排队/无首字节，不计错不冷却）",
                                    picked.uid,
                                    &model,
                                    if want_global { "global" } else { "cn" },
                                ));
                                break;
                            }
                        };
                        let ttfb_ms = ttfb_start.elapsed().as_millis() as u64;
                        // 对冲计数落定 + guard 重绑（接管时生效账号 = 对冲账号）
                        guard = settle_qoder_hedge(&state, &mut race, guard, &picked.uid);
                        hedge_taken = race.takeover;
                        let win_uid = race.uid.clone();
                        // G4：记账会话 id 按胜者信封落定（race 在聚合处被部分消费，先取快照）
                        let win_session_ids = race.session_ids.clone().or_else(|| envelope_ids.clone());
                        // 竞速胜者原始行源 → 翻译 → 聚合（tool_calls 合并/usage 收集）；
                        // 首字超时已在 race 内裁定，聚合阶段无失败通道
                        let agg_chat_id = format!("chatcmpl-{}", now_ts());
                        let translated =
                            qoder_upstream::QoderTranslate::new(race.lines, err_slot.clone(), &agg_chat_id, &model);
                        let (resp, error) = wb_sse::aggregate(translated, &agg_chat_id);
                        let duration_ms = start_ts.elapsed().as_millis() as u64;
                        match (resp, error) {
                            (Some(mut r), None) => {
                                r["model"] = json!(model);
                                // M1 修复（审查）：as_u64 窄口径改 wb_sse::u64_pair
                                // 宽口径（字符串/浮点形态不再静默得 0）
                                let (pt, ct) = r.get("usage").map(wb_sse::u64_pair).unwrap_or((0, 0));
                                let (credits, original_credits) =
                                    extract_billing(r.get("usage"));
                                // M4：缓存读 tokens 旁路记账（成功终局，0 值不入账）
                                state.record_usage_cached(
                                    super::usage::UsageBucket::Qoder,
                                    r.get("usage").map(wb_sse::cached_tokens_of).unwrap_or(0),
                                );
                                state.record_usage_qoder(
                                    &model, &win_uid, &key_id, true, stream, duration_ms, pt, ct,
                                    Some(ttfb_ms),
                                    credits,
                                    original_credits,
                                    &session_ctx_from_ids(&peek, win_session_ids.as_ref()),
                                );
                                clear_model_cooldown(&model, want_global); // 审查修复：成功即清模型级冷却
                                state.qoder_pool.note_success(&win_uid);
                                // F-80-余 v2：绑定粘性会话（开关开启时；同流式路径）
                                if state
                                    .qoder_sticky_enabled
                                    .load(std::sync::atomic::Ordering::Relaxed)
                                {
                                    state.qoder_sticky.bind(&sticky_key, &win_uid, &sticky_seed, now_ts());
                                    state.qoder_sticky.save(&state.data_dir);
                                }
                                state.logger.log_request_ttfb(
                                    "qoder", "POST", "/v1/chat/completions", &model, stream, 200,
                                    &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                    &state.qoder_pool.name_of(&win_uid), None,
                                );
                                return Ok(r);
                            }
                            (None, Some((code, msg))) => {
                                let meta = err_slot
                                    .lock()
                                    .unwrap_or_else(|e| e.into_inner())
                                    .clone();
                                match meta.as_ref().map(|m| m.kind) {
                                    Some(UpstreamKind::Queued) => {
                                        // 排队：同号退避重试为主（对冲接管后不适用，
                                        // 同流式路径——直接换号）。退避打满/模型冷却中
                                        // 不再 break 换号（超限换号是 189s 逐号放大源），
                                        // 直接终止轮换 429（见下方判定）
                                        if !hedge_taken {
                                            let secs = meta
                                                .as_ref()
                                                .and_then(|m| m.queue.as_ref())
                                                .and_then(|q| q.retry_after_secs)
                                                .unwrap_or(QUEUE_DEFAULT_BACKOFF_SECS)
                                                .clamp(1, QUEUE_BACKOFF_MAX_SECS);
                                            // 审查修复：模型级冷却（同流式分支）——
                                            // 冷却中/单号退避打满直接终止轮换返回
                                            // 429（upstream 前缀借助端点状态码透传
                                            // 映射），不再逐账号硬打空转放大（189s
                                            // 假死修复）；快退判定按本请求区域作用域
                                            if queue_same >= queue_retry_limit_for(want_global)
                                                || (model_cooling_remaining_secs(&model, Some(want_global)).is_some()
                                                    && !pool_has_other_selectable(
                                                        &state,
                                                        &win_uid,
                                                        allowed_set.as_ref(),
                                                        Some(want_global),
                                                    ))
                                            {
                                                let reason = if queue_same >= queue_retry_limit_for(want_global) {
                                                    "同号退避打满，终止轮换"
                                                } else {
                                                    "模型冷却中，终止轮换"
                                                };
                                                state.record_usage_qoder(
                                                    &model, &win_uid, &key_id, false, stream,
                                                    duration_ms, 0, 0, None, None, None,
                                                    &session_ctx(&peek),
                                                );
                                                state.logger.log_request_ttfb(
                                                    "qoder", "POST", "/v1/chat/completions",
                                                    &model, stream, 429, &win_uid, duration_ms,
                                                    Some(ttfb_ms), &key_name,
                                                    &state.qoder_pool.name_of(&win_uid),
                                                    Some(reason),
                                                );
                                                return Err(format!(
                                                    "upstream 429 error: model {model} queued, retry later"
                                                ));
                                            }
                                            note_model_cooldown(&model, secs, want_global);
                                            queue_same += 1;
                                            // 统一走 sleep_interruptible(None)：分段等待，
                                            // 与流式实现口径一致（非流式无下行通道传 None，
                                            // 语义等同整段 sleep，仅封装统一）
                                            sleep_interruptible(Duration::from_secs(secs), None);
                                            continue;
                                        }
                                        break; // 超限换号
                                    }
                                    Some(kind) => {
                                        let ek = kind.to_err_kind();
                                        if ek != ErrKind::None {
                                            state.qoder_pool.note_error(&win_uid, ek);
                                            // issue #67 审查修复：账号级长冷却解绑粘性（与流式分支口径对齐）
                                            if ek.unbinds_sticky() {
                                                state.qoder_sticky.unbind_uid(&win_uid);
                                            }
                                        }
                                        // 免费额度耗尽：同流式分支（(uid,model) 冷却，
                                        // 不整号冻结）
                                        if kind == UpstreamKind::FreeQuota {
                                            note_free_quota_exhausted(&state.data_dir, &win_uid, &model);
                                            state.qoder_pool.per_model_cooldown_until(
                                                &win_uid,
                                                &model,
                                                super::pool::next_0000(now_ts()),
                                            );
                                        }
                                        // issue #90 修复①：同流式分支（非流式无部分流出概念）
                                        if kind == UpstreamKind::Server {
                                            note_model_server_cooldown(&model, want_global);
                                        }
                                    }
                                    None => {
                                        state.qoder_pool.note_error(&win_uid, ErrKind::Server);
                                    }
                                }
                                *safe_lock(&state.last_error) = Some(format!(
                                    "qoder uid={} model={} code={} msg={} at {}",
                                    win_uid,
                                    model,
                                    code,
                                    msg,
                                    now_cn_timestamp()
                                ));
                                state.logger.log_request_ttfb(
                                    "qoder", "POST", "/v1/chat/completions", &model, stream, 200,
                                    &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                    &state.qoder_pool.name_of(&win_uid), Some(&msg),
                                );
                                // 流内错误且未产出内容 → 换号重试
                                break;
                            }
                            _ => {
                                // (None, None)：EOF 零完成（影子风控/上游异常）——计数后换号
                                empty_hits += 1;
                                state.qoder_pool.note_error(&win_uid, ErrKind::Server);
                                state.logger.log_request_ttfb(
                                    "qoder", "POST", "/v1/chat/completions", &model, stream, 502,
                                    &win_uid, duration_ms, Some(ttfb_ms), &key_name,
                                    &state.qoder_pool.name_of(&win_uid),
                                    Some("empty response"),
                                );
                                break;
                            }
                        }
                    }
                    Err((status, resp_body, retry_after)) => {
                        // 同流式路径：带重置时刻的 429 直接换号 + 冻结账号
                        let retry_action = if status == 429 {
                            if let Some(ts) = super::wb_route::parse_quota_reset_at(&resp_body) {
                                state.qoder_pool.cooldown_until(&picked.uid, ts);
                                RetryAction::SwitchKey
                            } else {
                                retry_plan(status, &resp_body, same_attempt, retry_after)
                            }
                        } else {
                            retry_plan(status, &resp_body, same_attempt, retry_after)
                        };
                        match retry_action {
                            RetryAction::RetrySame { delay_ms } => {
                                same_attempt += 1;
                                sleep_interruptible(
                                    Duration::from_millis(delay_ms.min(60_000)),
                                    None,
                                );
                                continue;
                            }
                            RetryAction::SwitchKey => {
                                let classified =
                                    qoder_upstream::classify_upstream_error(status, &resp_body);
                                if classified.kind == UpstreamKind::Queued {
                                    // 同流式分支：退避打满/模型冷却中（按区域作用域）
                                    // → 429 终止轮换，不再换号空转放大（189s 假死修复）
                                    if queue_same >= queue_retry_limit_for(want_global)
                                        || (model_cooling_remaining_secs(&model, Some(want_global)).is_some()
                                            && !pool_has_other_selectable(
                                                &state,
                                                &picked.uid,
                                                allowed_set.as_ref(),
                                                Some(want_global),
                                            ))
                                    {
                                        let reason = if queue_same >= queue_retry_limit_for(want_global) {
                                            "同号退避打满，终止轮换"
                                        } else {
                                            "模型冷却中，终止轮换"
                                        };
                                        state.record_usage_qoder(&model, &picked.uid, &key_id,
                                            false, stream,
                                            start_ts.elapsed().as_millis() as u64, 0, 0,
                                            None, None, None, &session_ctx(&peek));
                                        state.logger.log_request(
                                            "qoder", "POST", "/v1/chat/completions", &model,
                                            stream, 429, &picked.uid,
                                            start_ts.elapsed().as_millis() as u64, &key_name,
                                            &state.qoder_pool.name_of(&picked.uid),
                                            Some(reason),
                                        );
                                        return Err(format!(
                                            "upstream 429 error: model {model} queued, retry later"
                                        ));
                                    }
                                    let secs = classified
                                        .queue
                                        .as_ref()
                                        .and_then(|q| q.retry_after_secs)
                                        .unwrap_or(QUEUE_DEFAULT_BACKOFF_SECS)
                                        .clamp(1, QUEUE_BACKOFF_MAX_SECS);
                                    note_model_cooldown(&model, secs, want_global);
                                    queue_same += 1;
                                    // 审查 Low：对齐同函数其余非流式退避点，统一走
                                    // sleep_interruptible(None)（分段等待，非流式无
                                    // 下行通道传 None，语义等同整段 sleep，仅封装统一）
                                    sleep_interruptible(Duration::from_secs(secs), None);
                                    continue;
                                }
                                let kind = classified.kind.to_err_kind();
                                if kind != ErrKind::None {
                                    state.qoder_pool.note_error(&picked.uid, kind);
                                    // issue #67：硬冷却/禁用类错误解绑该账号粘性
                                    if kind.unbinds_sticky() {
                                        state.qoder_sticky.unbind_uid(&picked.uid);
                                    }
                                }
                                // 免费档模型额度耗尽记录（免费额度可见性）：同流式分支
                                if classified.kind == UpstreamKind::Quota {
                                    note_free_quota_exhausted(&state.data_dir, &picked.uid, &model);
                                }
                                // 免费额度类（114/115/119）：(uid,model) 冷却到次日
                                // 00:00，不整号冻结（同流式 HTTP 分支）
                                if classified.kind == UpstreamKind::FreeQuota {
                                    note_free_quota_exhausted(&state.data_dir, &picked.uid, &model);
                                    state.qoder_pool.per_model_cooldown_until(
                                        &picked.uid,
                                        &model,
                                        super::pool::next_0000(now_ts()),
                                    );
                                }
                                // issue #90 修复①：同流式 HTTP 分支（5xx 换号前记模型级冷却）
                                if classified.kind == UpstreamKind::Server {
                                    note_model_server_cooldown(&model, want_global);
                                }
                                *safe_lock(&state.last_error) = Some(format!(
                                    "qoder uid={} model={} status={} body={} at {}",
                                    picked.uid,
                                    model,
                                    status,
                                    safe_slice(&resp_body, 200),
                                    now_cn_timestamp()
                                ));
                                // 记账口径对齐流式路径（每请求一行）：换号中间态不记
                                // usage（原聚合路径每次换号多记一条失败，qoder 桶
                                // requests/errors 虚高）；最终 no-healthy 由取号分支记录
                                state.logger.log_request(
                                    "qoder", "POST", "/v1/chat/completions", &model, stream,
                                    status, &picked.uid,
                                    start_ts.elapsed().as_millis() as u64, &key_name,
                                    &state.qoder_pool.name_of(&picked.uid),
                                    Some(&format!("upstream status={status}")),
                                );
                                break;
                            }
                            RetryAction::Fatal => {
                                // 终态记账对齐「每请求一行」口径（审查 F3）：Fatal 是
                                // 请求终局（同 no-healthy 由取号分支记录），此前仅
                                // log_request 导致用量统计页缺口
                                state.record_usage_qoder(&model, &picked.uid, &key_id, false, stream,
                                    start_ts.elapsed().as_millis() as u64, 0, 0,
                                    None, None, None, &session_ctx(&peek));
                                state.logger.log_request(
                                    "qoder", "POST", "/v1/chat/completions", &model, stream,
                                    status, &picked.uid,
                                    start_ts.elapsed().as_millis() as u64, &key_name,
                                    &state.qoder_pool.name_of(&picked.uid),
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
                    // Qoder 无 namespace 展平编码（Responses 分支为防御性兜底）
                    &std::collections::HashSet::new(),
                ),
                Protocol::OpenAi => resp,
            };
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap_or_else(|_| internal_error_response())
        }
        Ok(Err(msg)) => {
            // 审查修复（对齐 wb_aggregate_chat）：Fatal 错误透传上游状态码——
            // 上游 400/404/413 等请求级错误不降级 503，避免严格客户端无意义重试；
            // 401/403 维持 503 语义；429 为模型冷却主动限流（审查中等项），透传
            // 让客户端按 Retry-After 语义退避而非立即重试
            let upstream_status = msg
                .strip_prefix("upstream ")
                .and_then(|rest| rest.split(' ').next())
                .and_then(|s| s.parse::<u16>().ok());
            let status = match upstream_status {
                Some(s) if (400..500).contains(&s) && !matches!(s, 401 | 403) => {
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

// ==================== 小工具 ====================

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 流内错误帧下发（自抄 send_stream_error_wb：私有不可复用；形态同构）
fn send_stream_error(
    tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    proto: Protocol,
    code: i64,
    msg: &str,
) {
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
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ==================== issue #90 修复①：Server 渐进模型级冷却 ====================

    /// Server 冷却按「失败片段」渐进（窗口过期才递增）且封顶；成功清除后归零。
    /// 冷却表是进程级共享静态，用独占模型名（前缀 + 计数器）避免并行测试互扰。
    #[test]
    fn server_cooldown_progressive_capped_and_cleared() {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let model = format!(
            "t90-prog-{}",
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        assert!(model_cooling_remaining_secs(&model, None).is_none());
        // 片段 1：基准 30s
        note_model_server_cooldown(&model, false);
        let r1 = model_cooling_remaining_secs(&model, None).unwrap();
        assert!(r1 > 28 && r1 <= 30, "首次失败应为 30s 基准，实际 {r1}");
        // 同片段内连续记账（一次请求逐账号换号场景，窗口未过期）：
        // 不递增片段数，窗口仅延长（剩余仍 ≈30s，绝不翻倍）
        for _ in 0..4 {
            note_model_server_cooldown(&model, false);
        }
        let (_, fails) = *cooldown_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&model)
            .unwrap();
        assert_eq!(fails, 1, "窗口未过期时连续记账不得递增片段数");
        // 人为推进到窗口过期（不真等 30s）：片段 2 → 翻倍 60s
        cooldown_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(model.clone(), (Instant::now() - Duration::from_secs(1), 1));
        note_model_server_cooldown(&model, false);
        let r2 = model_cooling_remaining_secs(&model, None).unwrap();
        assert!(r2 > 55 && r2 <= 60, "过期后新片段应翻倍至 60s，实际 {r2}");
        // 连续失败片段至封顶：窗口恒 ≤ 480s
        for _ in 0..10 {
            let cur = cooldown_map()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&model)
                .copied()
                .unwrap();
            cooldown_map()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(model.clone(), (Instant::now() - Duration::from_secs(1), cur.1));
            note_model_server_cooldown(&model, false);
        }
        let capped = model_cooling_remaining_secs(&model, None).unwrap();
        assert!(capped <= SERVER_COOLDOWN_MAX_SECS, "冷却窗口应封顶 480s，实际 {capped}");
        // 成功清除 → 无冷却（片段计数随之归零）
        clear_model_cooldown(&model, false);
        assert!(model_cooling_remaining_secs(&model, None).is_none());
    }

    /// 排队退避（note_model_cooldown）不缩短既有 Server 冷却窗口：排队建议值
    /// （钳制 ≤30s）到达时若并发请求刚记了更长的 Server 窗口，应取较大者。
    #[test]
    fn queued_cooldown_does_not_shorten_server_window() {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let model = format!(
            "t90-queue-{}",
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        // 两次 Server 失败（同一片段内）→ 窗口维持基准 ~30s 不后延
        note_model_server_cooldown(&model, false);
        note_model_server_cooldown(&model, false);
        let before = model_cooling_remaining_secs(&model, None).unwrap();
        // 排队退避建议 5s：不得把 60s 窗口缩短
        note_model_cooldown(&model, 5, false);
        let after = model_cooling_remaining_secs(&model, None).unwrap();
        assert!(
            after >= before.saturating_sub(1),
            "排队建议值不得缩短既有 Server 窗口（{before} → {after}）"
        );
        clear_model_cooldown(&model, false);
    }

    /// 回归：排队退避先插入 fails=0 条目（note_model_cooldown 的 or_insert 路径），
    /// 随后 Server 失败进入 and_modify —— fails=0 不得下溢（曾为 `*fails - 1`，
    /// debug panic / release 冷却被错误推到 480s），应保持基准 30s 且计数不动
    #[test]
    fn server_cooldown_after_queued_only_entry_no_underflow() {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let model = format!(
            "t90-queue0-{}",
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        // 排队退避先落条目：fails = 0
        note_model_cooldown(&model, 5, false);
        // Server 失败：窗口未过期（5s 内）不递增计数，基准 30s 兜底
        note_model_server_cooldown(&model, false);
        let r = model_cooling_remaining_secs(&model, None).unwrap();
        assert!(r > 28 && r <= 30, "fails=0 时应为 30s 基准窗口，实际 {r}");
        let (_, fails) = *cooldown_map()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&model)
            .unwrap();
        assert_eq!(fails, 0, "窗口未过期不得递增失败计数");
        clear_model_cooldown(&model, false);
    }

    /// 区域键隔离（189s 假死修复配套，2026-10-19）：海外区排队冷却记在
    /// `g:{model}`，不得污染国内同名模型（反之亦然）；国内键保持裸名兼容。
    #[test]
    fn region_cooldown_keys_are_isolated() {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let model = format!(
            "t-reg-{}",
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        // 海外区记冷却：Some(true)/Some(false) 各自独立，None 按国内裸名查
        note_model_cooldown(&model, 30, true);
        assert!(
            model_cooling_remaining_secs(&model, Some(true)).is_some(),
            "海外键应命中冷却"
        );
        assert!(
            model_cooling_remaining_secs(&model, Some(false)).is_none(),
            "国内同名不得被海外冷却误伤"
        );
        assert!(
            model_cooling_remaining_secs(&model, None).is_none(),
            "None 视作国内，不得命中海外键"
        );
        // 国内区再记冷却：两区各自独立
        note_model_cooldown(&model, 30, false);
        assert!(model_cooling_remaining_secs(&model, Some(false)).is_some());
        // 清除只清各自区域键
        clear_model_cooldown(&model, false);
        assert!(model_cooling_remaining_secs(&model, Some(false)).is_none());
        assert!(
            model_cooling_remaining_secs(&model, Some(true)).is_some(),
            "清国内不得连带清海外"
        );
        clear_model_cooldown(&model, true);
        assert!(model_cooling_remaining_secs(&model, Some(true)).is_none());
    }

    /// cqk 键形态：国内裸名（历史条目兼容）、海外加 `g:` 前缀。
    #[test]
    fn cqk_key_shape() {
        assert_eq!(cqk("Qwen3.8-Flash", false), "Qwen3.8-Flash");
        assert_eq!(cqk("Qwen3.8-Flash", true), "g:Qwen3.8-Flash");
    }

    /// issue #82 T2.3：credits 复数优先（新版字段），单数 credit 为旧版兜底
    #[test]
    fn extract_billing_prefers_plural_credits() {
        let u = json!({"credits": 12.5, "credit": 0, "original_credits": 25.0});
        let (c, o) = extract_billing(Some(&u));
        assert_eq!(c, Some(12.5));
        assert_eq!(o, Some(25.0));
        // 仅有旧版单数字段时兜底可读（新调用方 None 缺省不再走此路）
        let old = json!({"credit": 7});
        let (c, o) = extract_billing(Some(&old));
        assert_eq!(c, Some(7.0));
        assert_eq!(o, None);
    }

    /// issue #82 T2.3：双字段均缺 → None（调用方不入账，不得伪造 0）
    #[test]
    fn extract_billing_missing_fields_yields_none() {
        assert_eq!(extract_billing(None), (None, None));
        let empty = json!({"prompt_tokens": 10, "completion_tokens": 5});
        assert_eq!(extract_billing(Some(&empty)), (None, None));
        let nulls = json!({"credits": null});
        assert_eq!(extract_billing(Some(&nulls)), (None, None));
    }

    /// issue #82 T2.3：负值/非有限值视为不可信，整体不入账
    #[test]
    fn extract_billing_rejects_negative() {
        let neg = json!({"credits": -3.0});
        assert_eq!(extract_billing(Some(&neg)), (None, None));
        let neg_pair = json!({"credits": 5.0, "original_credits": -1.0});
        let (c, o) = extract_billing(Some(&neg_pair));
        assert_eq!(c, Some(5.0));
        assert_eq!(o, None);
    }

    /// 会话 preview 剥离 system-reminder 注入块与 <user_input> 包裹标签：
    /// agent 客户端把提醒拼在正文前、输入用标签包裹，preview 必须取剥离后的
    /// 用户原话（多块 / 跨行 / 前置残留空白）。
    #[test]
    fn session_preview_strips_system_reminder_blocks() {
        // 典型形态：多块 reminder 前置 + <user_input> 包裹的用户原话
        let peek = json!({"messages": [{"role": "user", "content":
            "<system-reminder>\nTodos have changed...\n# Todos\n- x\n</system-reminder>\n\n<system-reminder>The maximum number of terminals is 5</system-reminder>\n<user_input>\n说一个100字笑话\n</user_input>"}]});
        assert_eq!(session_preview(&peek), "说一个100字笑话");

        // 仅 <user_input> 包裹、无 reminder
        let wrapped = json!({"messages": [{"role": "user", "content":
            "<user_input>写一个100字笑话</user_input>"}]});
        assert_eq!(session_preview(&wrapped), "写一个100字笑话");

        // 剥离后仅剩空白 → 空串（调用方按无 preview 处理，不展示注入内容）
        let only_reminder = json!({"messages": [{"role": "user", "content":
            "<system-reminder>纯注入</system-reminder>"}]});
        assert_eq!(session_preview(&only_reminder), "");

        // 无 reminder 的普通消息不受影响；末条 user 才是取值来源
        let plain = json!({"messages": [
            {"role": "user", "content": "第一条"},
            {"role": "assistant", "content": "回复"},
            {"role": "user", "content": "<system-reminder>x</system-reminder> 写一个100字笑话"}
        ]});
        assert_eq!(session_preview(&plain), "写一个100字笑话");

        // reminder 之后再无正文 → 空；reminder 内含 "</system-reminder>" 字样也按整块匹配（非贪婪）
        let empty_tail = json!({"messages": [{"role": "user", "content":
            "前文<system-reminder>注入</system-reminder>  \n"}]});
        assert_eq!(session_preview(&empty_tail), "前文");
    }

    /// 远程目录测试条目（adopt_remote 接受的最小合法信封载荷）
    fn remote_entry(key: &str, display: &str, factor: f64) -> Value {
        json!({
            "key": key,
            "display_name": display,
            "enable": true,
            "is_vl": false,
            "is_reasoning": true,
            "price_factor": factor,
            "max_input_tokens": 128000,
            "source": "remote",
        })
    }

    /// G4 确定性判定 remote 态：注入独有前缀（TwaG4）的双区远程目录，
    /// 顺序覆盖 entry_is_global_only 的全部分支。
    /// adopt_remote 写进程级共享目录且远程非空时会替换静态兜底——必须
    /// 持 catalog_test_guard 独占并在退出（含 panic）时自动复位回落兜底，
    /// 否则会污染并行运行的其他目录测试（如 qoder_upstream 的 list 去重）。
    #[test]
    fn entry_is_global_only_remote_states() {
        let _catalog = qoder_upstream::catalog_test_guard();
        // ① CN 远程独有：list 标注 "cn"（Global 侧完全无此 id）→ 必非专属
        qoder_upstream::adopt_remote(
            qoder_upstream::QoderRegion::Cn,
            &json!({"statusCodeValue": 200,
                    "chat": [remote_entry("twa4cn_key", "TwaG4CnOnly", 0.5)]}),
        )
        .unwrap();
        let entry = qoder_upstream::resolve("TwaG4CnOnly", qoder_upstream::QoderRegion::Cn)
            .expect("CN 远程目录应命中");
        assert!(!entry_is_global_only("TwaG4CnOnly", &entry));

        // ② Global 远程独有（CN 侧零可得证据）：两路 resolve 同落 Global 条目
        //    → fail-closed 判专属
        qoder_upstream::adopt_remote(
            qoder_upstream::QoderRegion::Global,
            &json!({"statusCodeValue": 200,
                    "chat": [remote_entry("twa4g_key", "TwaG4GlobalOnly", 0.7)]}),
        )
        .unwrap();
        let entry =
            qoder_upstream::resolve("TwaG4GlobalOnly", qoder_upstream::QoderRegion::Cn)
                .expect("双区兜底应命中 Global 独有条目");
        assert!(entry_is_global_only("TwaG4GlobalOnly", &entry));

        // ③ 两区同名但声明不同（Global 倍率 0.7 / CN 倍率 0.3）：list 标注被
        //    Global 吞并为 "global"，但两路 resolve 命中不同条目 → CN 远程
        //    确定可得 → 非专属（证据②，不再依赖值全等假设的反向推断）
        qoder_upstream::adopt_remote(
            qoder_upstream::QoderRegion::Global,
            &json!({"statusCodeValue": 200,
                    "chat": [remote_entry("twa4same_key", "TwaG4Same", 0.7)]}),
        )
        .unwrap();
        qoder_upstream::adopt_remote(
            qoder_upstream::QoderRegion::Cn,
            &json!({"statusCodeValue": 200,
                    "chat": [remote_entry("twa4same_key", "TwaG4Same", 0.3)]}),
        )
        .unwrap();
        let entry = qoder_upstream::resolve("TwaG4Same", qoder_upstream::QoderRegion::Cn)
            .expect("CN 远程目录应命中");
        assert!(!entry_is_global_only("TwaG4Same", &entry));

        // ④ 两区同名且同值（两区远程条目一致）：resolve(Cn) 已命中 CN 条目 =
        //    CN 侧确定可得该模型；Global 侧同样命中只能证明两区都有。
        //    事故复盘（2026-10-19）：旧 fail-closed 分支在此判 Global 专属，把
        //    两区都上架的 qwen3.8-flash 对国内 Key 硬 404（目录同步被关闭期间
        //    全量触发）。现改为放行 CN——判定 Global 专属的唯一证据是证据①
        //    （list 中 Global 侧确定无此 id）。
        qoder_upstream::adopt_remote(
            qoder_upstream::QoderRegion::Global,
            &json!({"statusCodeValue": 200,
                    "chat": [remote_entry("twa4same_key", "TwaG4Same", 0.3)]}),
        )
        .unwrap();
        let entry = qoder_upstream::resolve("TwaG4Same", qoder_upstream::QoderRegion::Cn)
            .expect("CN 远程目录应命中");
        assert!(!entry_is_global_only("TwaG4Same", &entry), "两区同名同值 = 两区都有，CN Key 必须放行");

        // ⑤ 真正的 Global 专属：CN 源（远程 CN + CN 兜底表）均无此 id，
        //    list() 合并视图标注 region=global → 判 Global 独有，国内 Key 硬 404。
        qoder_upstream::catalog_test_reset();
        qoder_upstream::adopt_remote(
            qoder_upstream::QoderRegion::Global,
            &json!({"statusCodeValue": 200,
                    "chat": [remote_entry("twa4g_key", "TwaG4GlobalOnly", 0.7)]}),
        )
        .unwrap();
        let entry =
            qoder_upstream::resolve("TwaG4GlobalOnly", qoder_upstream::QoderRegion::Cn)
                .expect("Global 独有条目经双区回落应命中");
        assert!(
            entry_is_global_only("TwaG4GlobalOnly", &entry),
            "CN 源（远程+兜底）均无此 id 且 list 标注 global → 判 Global 专属"
        );
    }
}
