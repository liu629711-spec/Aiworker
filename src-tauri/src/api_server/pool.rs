use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Sha256, Digest};

use crate::models::{CooldownEntry, DeviceMap, PoolStatus};

use super::ErrKind;

/// 账号池调度策略（T10 + T2.2 扩展）
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub enum PoolStrategy {
    /// 积分先过期优先（默认，保持既有行为）
    #[default]
    ExpireFirst,
    /// 剩余通用积分多优先
    CreditFirst,
    /// 随机取号
    Random,
    /// 三因子加权随机（T2.2/F-29 v1.2）：积分占比×10 + 闲置补偿（每小时+0.5
    /// 封顶 5.0）+ 成功率×3 → Top5 短名单内二次加权随机——防热点 + 防惊群
    Weighted,
    /// P2C（Power-of-Two-Choices，T2.2 v1.2）：随机选二取优——antigravity-tools
    /// 实证延迟优于轮询/加权随机；与三因子加权并存，实测对比后取默认
    P2C,
}

impl PoolStrategy {
    /// 解析配置字符串（空/未知值回退 expire_first）
    pub fn parse(s: &str) -> Self {
        match s {
            "credit_first" => Self::CreditFirst,
            "random" => Self::Random,
            "weighted" => Self::Weighted,
            "p2c" => Self::P2C,
            _ => Self::ExpireFirst,
        }
    }

    /// Buddy 池生效策略：wb_strategy 独立配置优先；空 = 跟随 Trae 池策略。
    /// 启动（api_server_start）与热应用（pool_set）共用此语义，保证两处行为一致。
    pub fn resolve_wb(strategy: &str, wb_strategy: &str) -> Self {
        Self::parse(if wb_strategy.is_empty() {
            strategy
        } else {
            wb_strategy
        })
    }

    /// Qoder 池生效策略：qoder_strategy 独立配置优先；空 = 跟随 Trae 池策略
    ///（与 Buddy 同语义，复用同一跟随规则）
    pub fn resolve_qoder(strategy: &str, qoder_strategy: &str) -> Self {
        Self::resolve_wb(strategy, qoder_strategy)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::ExpireFirst => "expire_first",
            Self::CreditFirst => "credit_first",
            Self::Random => "random",
            Self::Weighted => "weighted",
            Self::P2C => "p2c",
        }
    }
}

/// reserve 过滤新鲜度门槛（issue #91 缓存问题）：积分快照采集时间距今超过
/// 该值视为陈旧，reserve 过滤失效（fail-open）——陈旧「高积分」快照照样会
/// 调度秃号，fail-closed 反而额外误杀；恢复靠启动保鲜（do_start 后台刷新）
/// 与运行期 update_credits 拨新。24h 与 trae-credits-snapshot daily 模式对齐
const RESERVE_STALE_SECS: i64 = 24 * 3600;

/// 池中单个账号的运行时状态
pub struct PoolEntry {
    pub uid: String,
    pub name: String,
    pub jwt: String,
    pub credits: Option<f64>,
    pub credits_expire_at: Option<i64>,
    /// 积分快照采集时间（Unix 秒，issue #91 缓存保鲜）：
    /// None = 无时间信息（升级前旧数据/无快照），reserve 过滤同样不生效
    pub credits_fetched_at: Option<i64>,
    pub disabled: bool,
    pub err_count: i32,
    /// 冷却截止时间（Unix 秒），0 表示无冷却
    pub until: i64,
    pub reason: String,
    pub device_id: String,
    pub machine_id: String,
    // ── T2.2 五态机与调度因子 ──
    /// hard_credit（积分耗尽）冷却截止：次日 04:00 自动恢复探测（F-29 v1.2）
    pub hard_until: i64,
    /// 最近一次被选中时间（Unix 秒；三因子加权闲置补偿因子）
    pub last_used: i64,
    /// 成功/失败计数（三因子加权成功率因子）
    pub successes: u64,
    pub failures: u64,
    /// 连续熔断次数（Server 错误 30m 起指数递增至 6h）
    pub cb_trips: u32,
    /// 连续限额次数（SoftRate 连续命中，成功即清零）：60s 固定冷却会让
    /// 「日额度已耗尽但文案解析不出时刻」的账号每 60 秒被重新选中、反复 429。
    /// 按次数把冷却拉长 60s→2m→4m→8m→…→30m 上限，让调度自然绕开它。
    pub rate_trips: u32,
    // ── WorkBuddy 上游凭证头字段（T2.1；SOLO 账号为空串/false）──
    pub domain: String,
    pub enterprise_id: String,
    pub global_region: bool,
    /// refresh_token 已判定失效（F-78 批次 3）：同步自 RawAccount.refresh_token_invalid，
    /// 与 disabled 联动（重新 OAuth 登录后 sync 自动恢复）
    pub refresh_invalid: bool,
    // ── T5.3 按模型维度冷却（Issue #99：6004 "可切换其他模型继续使用"）──
    /// model → until 映射；upstream 6004 等文案含 "可在其他模型继续" 时用，
    /// 冻结粒度为 (uid, model)，而非整账号。空字典表示无按模型冷却条目。
    pub per_model_cooldowns: std::collections::HashMap<String, i64>,
    // ── ZCode 套餐画像（仅 zcode 池有值；其余池恒 None）──
    /// 套餐门闸快照：None = 从未同步过画像（fail-open，保持历史行为）；
    /// Some 则按画像过滤（模型集合 / 夜间窗口 / 额度耗尽 / 燃烧顺序）
    pub plan: Option<PlanGate>,
}

/// ZCode 套餐门闸（池内快照，由 `zcode::plan_profile::PlanProfile` 归一化而来）。
///
/// 语义分工：
/// - `servable_models` **空 = 上游明确判定该账号没有任何可用权益** → 不可选号
///   （与"画像未知"区分开：未知是 `plan: None`，仍然可选）
/// - 模型/时段/额度过滤在取号时按请求逐次判定（见 `PlanGate::serves`）
/// - 燃烧顺序 `burn_rank` + `expiry_key` 决定同池内的优先次序
#[derive(Debug, Clone, Default)]
pub struct PlanGate {
    /// new_account | task_card | global_build | start_plan | other
    pub kind: String,
    /// 该账号持有的全部形态（逗号分隔，Key 级套餐路由按此匹配）
    pub kinds: String,
    /// 可服务模型（小写，逗号分隔；已按 zcode_catalog 归一化）
    pub models: String,
    /// 聚合时段口径：daily | daily_night | …
    pub period: String,
    /// true = 只有夜间档权益（白天不可选号）
    pub night_only: bool,
    pub grant_units: i64,
    pub remaining_units: i64,
    /// 最早到期时间（unix 秒）
    pub ends_at: Option<i64>,
    /// 画像同步时间（unix 秒）
    pub synced_at: i64,
}

impl PlanGate {
    /// 是否有任何可用权益（空模型集 = 上游明确说没有 → 不可入池候选）
    pub fn has_entitlement(&self) -> bool {
        !self.models.trim().is_empty()
    }

    /// 该账号当前能否服务指定模型（入参模型需已归一化，比较忽略大小写）
    pub fn serves(&self, model_lc: &str) -> bool {
        if model_lc.is_empty() {
            return true;
        }
        self.models
            .split(',')
            .any(|m| m.trim().eq_ignore_ascii_case(model_lc))
    }

    /// 燃烧顺序权重（越小越先烧）：新号赠送 > 任务卡 > Global Build > Start Plan
    pub fn burn_rank(&self) -> i64 {
        match self.kind.as_str() {
            "new_account" => 0,
            "task_card" => 1,
            "global_build" => 2,
            "start_plan" => 3,
            _ => 4,
        }
    }

    /// 同形态内的次级排序键：先过期先烧；无到期时间排最后
    pub fn expiry_key(&self) -> i64 {
        self.ends_at.filter(|v| *v > 0).unwrap_or(i64::MAX)
    }

    /// 是否命中 Key 级套餐路由白名单（空 = 不限）
    pub fn kind_allowed(&self, allow: Option<&HashSet<String>>) -> bool {
        let Some(allow) = allow else { return true };
        if allow.is_empty() {
            return true;
        }
        self.kinds
            .split(',')
            .any(|k| allow.contains(k.trim()))
            || allow.contains(self.kind.as_str())
    }
}


/// 默认构造：全零运行态（新入池账号）。sync_from_* 的增量合并用
/// `..PoolEntry::default()` 补足未显式列出的字段，避免漏字段导致编译错误
/// 或（更糟）漏掉某个运行态字段的初始化。
impl Default for PoolEntry {
    fn default() -> Self {
        Self {
            uid: String::new(),
            name: String::new(),
            jwt: String::new(),
            credits: None,
            credits_expire_at: None,
            credits_fetched_at: None,
            disabled: false,
            err_count: 0,
            until: 0,
            reason: String::new(),
            device_id: String::new(),
            machine_id: String::new(),
            hard_until: 0,
            last_used: 0,
            successes: 0,
            failures: 0,
            cb_trips: 0,
            rate_trips: 0,
            domain: String::new(),
            enterprise_id: String::new(),
            global_region: false,
            refresh_invalid: false,
            per_model_cooldowns: std::collections::HashMap::new(),
            plan: None,
        }
    }
}

impl PoolEntry {
    fn healthy(&self, now_ts: i64) -> bool {
        if self.disabled {
            return false;
        }
        if self.until > 0 && now_ts < self.until {
            return false;
        }
        // hard_credit 冷却：次日 04:00 前不参与（04:00 恢复探测由到期自然放开）
        if self.hard_until > 0 && now_ts < self.hard_until {
            return false;
        }
        true
    }

    /// reserve 过滤是否生效（issue #91 缓存问题）：积分快照存在且新鲜
    /// （≤ RESERVE_STALE_SECS）才生效；缺失（None）/陈旧/未来时间戳均视为
    /// 陈旧 → fail-open。零积分（c<=0）过滤不经过此门槛，保持既有无条件行为
    fn reserve_filter_active(&self, now: i64) -> bool {
        match self.credits_fetched_at {
            Some(t) => t <= now && now - t <= RESERVE_STALE_SECS,
            None => false,
        }
    }

    /// 账号五态机（F-29 v1.2）：Available / QuotaProtection / RateLimited /
    /// Forbidden / ProxyDisabled（供 /status 画像与状态迁移观测）
    pub fn state_str(&self, now_ts: i64) -> &'static str {
        if self.disabled {
            "Forbidden"
        } else if self.hard_until > 0 && now_ts < self.hard_until {
            "QuotaProtection"
        } else if self.until > 0 && now_ts < self.until {
            "RateLimited"
        } else {
            "Available"
        }
    }

    /// 三因子得分（T2.2）：积分占比×10 + 闲置补偿×0.5/h（封顶 5.0）+ 成功率×3
    /// 纯函数；`total_credits` 为候选集积分总和（0 时积分因子取 0）
    fn weighted_score(&self, total_credits: f64, now_ts: i64) -> f64 {
        let credit_factor = if total_credits > 0.0 {
            (self.credits.unwrap_or(0.0).max(0.0) / total_credits) * 10.0
        } else {
            0.0
        };
        let idle_hours = if self.last_used > 0 {
            ((now_ts - self.last_used).max(0) as f64) / 3600.0
        } else {
            // 从未被选用：按满闲置补偿（鼓励冷启动账号）
            10.0
        };
        let idle_factor = (idle_hours * 0.5).min(5.0);
        let total = self.successes + self.failures;
        let success_rate = if total == 0 { 0.5 } else { self.successes as f64 / total as f64 };
        credit_factor + idle_factor + success_rate * 3.0
    }
}

/// 账号池：内存索引 + 冷却/禁用状态机
pub struct ApiPool {
    entries: Mutex<HashMap<String, PoolEntry>>,
    strategy: Mutex<PoolStrategy>,
    /// 防惊群（T2.2）：100ms 内重复选中同一 uid 且存在其他候选时让位
    recent_pick: Mutex<(String, i64)>, // (uid, 毫秒时间戳)
    /// 账号级在途并发计数（F-77）：uid → 计数器共享句柄；
    /// InflightGuard 取号后 bind，Drop（流结束/断连/panic 展开）时 -1。
    /// 计数器经 Arc 与 guard 共享，池重建（sync_*）不中断在途请求的配对释放
    inflight: Mutex<HashMap<String, Arc<std::sync::atomic::AtomicU32>>>,
    /// 账号并发上限（F-77，api_pool.json 热应用）：0 = 不限；默认 1。
    /// inflight ≥ 上限的账号视为 busy 不参与候选；全部 busy 时降级取
    /// inflight 最小者（不过载拒绝，保证请求不失败）
    concurrency_limit: std::sync::atomic::AtomicU32,
    /// 账号保留剩余积分阈值（issue #91，dispatch_policy_set 热应用）：
    /// f64 bits 存储；剩余积分 ≤ 阈值的账号不参与调度（0 = 不过滤）。
    /// 防止账号积分被调度耗尽后连免费模型也不可用
    reserve_credits: std::sync::atomic::AtomicU64,
    /// 池标签（trae/buddy/qoder）：P1 冷却持久化时作 pool_cooldowns 表 pk 前缀。
    /// do_start 装配三池时随 set_log_ctx 一并注入；空 = 未装配（测试不落库）
    pool_label: Mutex<String>,
    /// 运行态冷却自上次落库后是否变化（P1）：note_error/cooldown_until/
    /// clear_cooldowns 等置脏，flusher 仅在脏时整池快照落库（2s 周期，
    /// 请求热路径零 DB 写入）
    cooldown_dirty: std::sync::atomic::AtomicBool,
    /// 冷却/禁用标记的 app.log 观测上下文：(data_dir, 池标签)。
    /// do_start 装配三池（trae/buddy/qoder）时注入；None（测试/未启动）不落日志
    log_ctx: Mutex<Option<(std::path::PathBuf, String)>>,
}

/// 安全获取 Mutex 锁：若锁被毒化（panic 导致），仍恢复内部数据继续运行
fn safe_lock<'a, T>(m: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ApiPool {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            strategy: Mutex::new(PoolStrategy::ExpireFirst),
            recent_pick: Mutex::new((String::new(), 0)),
            inflight: Mutex::new(HashMap::new()),
            concurrency_limit: std::sync::atomic::AtomicU32::new(1),
            reserve_credits: std::sync::atomic::AtomicU64::new(0.0f64.to_bits()),
            pool_label: Mutex::new(String::new()),
            cooldown_dirty: std::sync::atomic::AtomicBool::new(false),
            log_ctx: Mutex::new(None),
        }
    }

    /// 装配冷却/禁用标记的日志上下文：池标签 + 数据目录（do_start 启动时调用）；
    /// 未装配（测试/未启动网关）时 note_error 不落 app.log，无副作用。
    /// 池标签同时用于 P1 冷却持久化的 pool_cooldowns 表 pk 前缀
    pub fn set_log_ctx(&self, data_dir: &std::path::Path, label: &str) {
        *safe_lock(&self.log_ctx) = Some((data_dir.to_path_buf(), label.to_string()));
        *safe_lock(&self.pool_label) = label.to_string();
    }

    /// 设置账号并发上限（F-77，pool_set 热应用；0 = 不限）
    pub fn set_concurrency_limit(&self, limit: u32) {
        self.concurrency_limit
            .store(limit, std::sync::atomic::Ordering::Relaxed);
    }

    /// 账号并发上限当前值
    pub fn concurrency_limit(&self) -> u32 {
        self.concurrency_limit.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// 设置账号保留剩余积分阈值（issue #91，dispatch_policy_set 热应用）：
    /// 负值防御归零（0 = 关闭过滤）
    pub fn set_reserve_credits(&self, v: f64) {
        self.reserve_credits
            .store(v.max(0.0).to_bits(), std::sync::atomic::Ordering::Relaxed);
    }

    // ── P1 运行态冷却持久化（pool_cooldowns 表）─────────────────────────────

    /// 冷却运行态发生变化时标脏（note_error/cooldown_until/按模型冷却/手动清除
    /// 的公共尾步）；flusher 只在脏时落库，正常请求零额外开销
    pub fn mark_cooldown_dirty(&self) {
        self.cooldown_dirty.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// 若脏则取出整池冷却快照并清脏（CAS 防并发重复落库）。
    /// 返回 (uid, snapshot) 列表；干净时返回 None（flusher 跳过写库）。
    /// 过期条目（until/hard_until/按模型 ≤ now）过滤掉，表保持瘦身
    pub fn take_cooldown_snapshot(&self) -> Option<Vec<(String, crate::models::PoolCooldownSnapshot)>> {
        if !self
            .cooldown_dirty
            .compare_exchange(
                true,
                false,
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
        {
            return None;
        }
        let now = now_ts();
        let entries = safe_lock(&self.entries);
        let mut out: Vec<(String, crate::models::PoolCooldownSnapshot)> = Vec::new();
        for (uid, e) in entries.iter() {
            let per_model: HashMap<String, i64> = e
                .per_model_cooldowns
                .iter()
                .filter(|(_, until)| **until > now)
                .map(|(m, u)| (m.clone(), *u))
                .collect();
            let soft_alive = e.until > now;
            let hard_alive = e.hard_until > now;
            if !soft_alive && !hard_alive && per_model.is_empty() && e.cb_trips == 0 && e.rate_trips == 0 {
                continue;
            }
            out.push((
                uid.clone(),
                crate::models::PoolCooldownSnapshot {
                    until: if soft_alive { e.until } else { 0 },
                    hard_until: if hard_alive { e.hard_until } else { 0 },
                    cb_trips: e.cb_trips,
                    rate_trips: e.rate_trips,
                    reason: if soft_alive || hard_alive { e.reason.clone() } else { String::new() },
                    err_count: e.err_count,
                    per_model,
                },
            ));
        }
        Some(out)
    }

    /// 落库本池运行态冷却（P1）：脏时取快照 → pool_cooldowns 表；干净即跳过。
    /// 供网关 flusher（每 2s）与 stop 时 flush_pending_writes 调用；
    /// 未装配 log_ctx（测试/未启动）或标签空时不落库
    pub fn flush_cooldown_snapshot(&self) {
        let Some((dir, label)) = safe_lock(&self.log_ctx).clone() else {
            // CAS 清脏仍需处理：未装配时无落库目标，直接返回（不消费脏位，
            // 装配后由下一轮 flusher 补落）
            return;
        };
        if label.is_empty() {
            return;
        }
        let Some(rows) = self.take_cooldown_snapshot() else {
            return; // 干净：本轮无变化
        };
        let store = crate::store::db(&dir);
        if let Err(e) = crate::store::docs::pool_cooldowns_save_pool(&store, &label, &rows) {
            // 写失败恢复脏位，下轮重试（对齐 flush_usage_dirty 的失败语义）
            self.mark_cooldown_dirty();
            crate::fs_utils::app_log(
                &dir,
                &format!("pool cooldown flush failed: pool={label} err={e}"),
            );
        }
    }

    /// 启动回灌：把持久化冷却合并进池中**已存在**的账号（sync_from_* 装配后调用）。
    /// max 合并语义——只延长不缩短（与 sync_from_accounts 已灌入的 Trae until 兼容）。
    /// 重启前被冻结的号重启后立即恢复冻结，无需上游再挨一次 429 重新发现。
    /// 手动清除冷却（clear_cooldowns）会标脏并落空快照，对应行从表中剔除，
    /// 故不会把用户主动清掉的冷却重新灌回。
    pub fn restore_persisted_cooldowns(
        &self,
        snap: &HashMap<String, crate::models::PoolCooldownSnapshot>,
    ) {
        let now = now_ts();
        let mut entries = safe_lock(&self.entries);
        let mut restored = 0usize;
        for (uid, s) in snap {
            let Some(e) = entries.get_mut(uid) else { continue };
            if s.until > now && s.until > e.until {
                e.until = s.until;
                if !s.reason.is_empty() {
                    e.reason = s.reason.clone();
                }
            }
            if s.hard_until > now {
                e.hard_until = e.hard_until.max(s.hard_until);
            }
            e.cb_trips = e.cb_trips.max(s.cb_trips);
            e.rate_trips = e.rate_trips.max(s.rate_trips);
            e.err_count = e.err_count.max(s.err_count);
            for (m, until) in &s.per_model {
                if *until > now {
                    let cur = e.per_model_cooldowns.get(m).copied().unwrap_or(0);
                    if *until > cur {
                        e.per_model_cooldowns.insert(m.clone(), *until);
                    }
                }
            }
            restored += 1;
        }
        drop(entries);
        if restored > 0 {
            if let Some((dir, label)) = safe_lock(&self.log_ctx).clone() {
                crate::fs_utils::app_log(
                    &dir,
                    &format!("pool cooldown restore: pool={label} accounts={restored}"),
                );
            }
        }
    }

    /// 账号保留剩余积分阈值当前值
    pub fn reserve_credits(&self) -> f64 {
        f64::from_bits(self.reserve_credits.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// 是否存在积分快照陈旧/缺时间戳的账号（issue #91 启动保鲜判定）：
    /// 此类账号 reserve 过滤 fail-open 不生效，由 do_start 后台刷新拨新恢复。
    /// reserve 阈值关闭（0）时新鲜度判定无意义（reserve 过滤恒不生效），短路返回
    /// false——避免每次启动都触发全量余额刷新的网络副作用（B2，审查 2026-10-13）
    pub fn has_stale_credits(&self) -> bool {
        if self.reserve_credits() <= 0.0 {
            return false;
        }
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        entries
            .values()
            .any(|e| e.credits.is_some() && !e.reserve_filter_active(now))
    }

    /// Qoder 池启动保鲜判定（issue #102）：是否存在会被 selectable 持续排除的
    /// 钉死快照——零/负余额（零积分过滤无新鲜度门槛，充值后不拨新不恢复）或
    /// 积分已到期（续期后同理）。与 has_stale_credits 的 reserve 新鲜度口径互补：
    /// 后者 reserve=0 时短路 false，覆盖不到本判定的两种场景。
    /// disabled（needs_relogin）账号不参与判定——刷新注定 401 永久失败，
    /// 计入只会让每次启动都触发注定失败的全池刷新（噪音 + 无谓网络请求）
    pub fn has_zero_or_expired_credits(&self) -> bool {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        entries.values().any(|e| {
            !e.disabled
                && (e.credits.is_some_and(|c| c <= 0.0)
                    || e.credits_expire_at.is_some_and(|exp| exp > 0 && exp < now))
        })
    }

    /// 账号在途计数共享句柄（F-77）：InflightGuard 取号后 bind_account 使用；
    /// 不存在则创建（计数从 0 开始，bind 时 +1）
    pub fn inflight_handle(&self, uid: &str) -> Arc<std::sync::atomic::AtomicU32> {
        let mut m = safe_lock(&self.inflight);
        m.entry(uid.to_string())
            .or_default()
            .clone()
    }

    /// 在途计数快照（调度 busy 过滤与 P2C/Weighted 负载因子数据源）
    fn inflight_snapshot(&self) -> HashMap<String, u32> {
        let m = safe_lock(&self.inflight);
        m.iter()
            .map(|(uid, c)| (uid.clone(), c.load(std::sync::atomic::Ordering::Relaxed)))
            .collect()
    }

    /// 设置调度策略（启动时由 api_pool.json 决定）
    pub fn set_strategy(&self, s: PoolStrategy) {
        *safe_lock(&self.strategy) = s;
    }

    /// 健康账号池画像（智能调度因子，dispatch::smart_pool_order 数据源）：
    /// (最早积分到期时间, 健康账号剩余积分总和)；无到期数据 → None。
    /// 积分总和不计「新鲜快照且低于保留阈值」的账号（issue #91）——被 reserve
    /// 过滤的账号实际不可调度；陈旧快照 fail-open 与 selectable 口径一致（计入）
    pub fn stats(&self) -> (Option<i64>, f64) {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let reserve = self.reserve_credits();
        let mut earliest: Option<i64> = None;
        let mut total = 0.0;
        for e in entries.values() {
            if !e.healthy(now) {
                continue;
            }
            if let Some(c) = e.credits {
                if c > reserve || !e.reserve_filter_active(now) {
                    // max(0)：负积分（对账异常快照）不应拉低/拉负总分影响 smart 排序
                    total += c.max(0.0);
                }
            }
            if let Some(exp) = e.credits_expire_at {
                if exp > 0 && earliest.map_or(true, |cur| exp < cur) {
                    earliest = Some(exp);
                }
            }
        }
        (earliest, total)
    }

    /// 从已有账号文件同步池：只加入 enabled_uids 中的账号；
    /// group_ids 非空时仅纳入所选分组的账号（未分组账号不参与，T10）
    #[allow(clippy::too_many_arguments)]
    pub fn sync_from_accounts(
        &self,
        accounts: &[crate::models::RawAccount],
        enabled_uids: &[String],
        group_ids: &[String],
        membership: &HashMap<String, String>,
        cooldowns: &HashMap<String, CooldownEntry>,
        remaining_credits: &HashMap<String, f64>,
        credits_fetched_at: Option<i64>,
        expire_times: &HashMap<String, i64>,
        device_map: &DeviceMap,
    ) {
        let mut entries = safe_lock(&self.entries);
        let enabled: HashSet<&str> = enabled_uids.iter().map(|s| s.as_str()).collect();
        // 增量合并（2026-10-19 修正）：不再 entries.clear()。全量重建会丢掉
        // hard_until（HardCredit 次日 04:00）、cb_trips（Server 熔断记忆）、
        // rate_trips（SoftRate 递增退避）、per_model_cooldowns（按模型冷却）与
        // last_used/successes/failures（加权因子），而本函数在账号导入/改名/
        // 分组变更/OAuth 成功后都会被调用，导致冷却被静默清零、账号提前解冻。
        let group_filter: Option<HashSet<&str>> = if group_ids.is_empty() {
            None
        } else {
            Some(group_ids.iter().map(|s| s.as_str()).collect())
        };
        for a in accounts {
            if let Some(uid) = &a.user_id {
                if !enabled.contains(uid.as_str()) {
                    continue;
                }
                if let Some(filter) = &group_filter {
                    let in_group = membership
                        .get(uid)
                        .map_or(false, |g| filter.contains(g.as_str()));
                    if !in_group {
                        continue;
                    }
                }
                let cd = cooldowns.get(uid).cloned().unwrap_or_default();
                // F-78 批次 3：refresh_token 判定失效的账号同步禁用（需重新 OAuth 登录后恢复）
                let refresh_invalid = a.refresh_token_invalid;
                let disabled = cd.error_type == "SessionDead" || refresh_invalid;
                // 聊天设备身份策略：
                // - machine_id 始终按 uid 确定性派生（seeded_hex(64, uid, "mach")），跨机器一致，
                //   天然满足"一账号一设备指纹"，不与 OAuth 登录层的机器级共享身份（icube 凭证）混用。
                // - device_id 优先取 device_map 中用户显式登记的设备；缺条目时回落 derive_device(uid)
                //   按 uid 派生（与签到伪设备同一派生体系）。绝不发送空 device_id：
                //   "x-device-id: ''" 是显性指纹异常，容易在上游形成聚集特征触发影子风控。
                let (device_id, machine_id) = match device_map.get(uid) {
                    Some(d) => (d.device_id.clone(), seeded_hex(64, uid, "mach")),
                    None => (
                        crate::commands::accounts::derive_device(uid).device_id,
                        seeded_hex(64, uid, "mach"),
                    ),
                };
                let jwt_raw = a.jwt.clone();
                let jwt_clean = jwt_raw
                    .strip_prefix("Cloud-IDE-JWT ")
                    .unwrap_or(&jwt_raw)
                    .trim()
                    .to_string();
                // 增量合并：既有条目保留运行态，只更新快照与持久化冷却字段
                match entries.get_mut(uid) {
                    Some(e) => {
                        e.name = a.name.clone();
                        e.jwt = jwt_clean;
                        e.credits = remaining_credits.get(uid).copied();
                        e.credits_expire_at = expire_times.get(uid).copied();
                        e.credits_fetched_at = credits_fetched_at;
                        e.disabled = disabled;
                        e.err_count = cd.error_count;
                        e.until = cd.until;
                        e.reason = cd.reason;
                        e.device_id = device_id;
                        e.machine_id = machine_id;
                        e.refresh_invalid = refresh_invalid;
                    }
                    None => {
                        entries.insert(uid.clone(), PoolEntry {
                            uid: uid.clone(),
                            name: a.name.clone(),
                            jwt: jwt_clean,
                            credits: remaining_credits.get(uid).copied(),
                            credits_expire_at: expire_times.get(uid).copied(),
                            credits_fetched_at,
                            disabled,
                            err_count: cd.error_count,
                            until: cd.until,
                            reason: cd.reason,
                            device_id,
                            machine_id,
                            refresh_invalid,
                            ..PoolEntry::default()
                        });
                    }
                }
            }
        }
        // 移除本次未入池的账号（保留上方增量合并语义）
        entries.retain(|uid, _| enabled.contains(uid.as_str()));
        // F-77：清理已不在池内的在途计数条目（在途请求经 Arc 独立释放，不受影响）
        {
            let mut inflight = safe_lock(&self.inflight);
            inflight.retain(|uid, _| entries.contains_key(uid));
        }
    }

    /// 运行期积分回写（issue #67）：仅更新池内条目的 credits 快照，
    /// 不触碰冷却/inflight/禁用等运行态（与 sync_from_accounts 全量重建互补——
    /// 后者会重置 hard_until/last_used 等运行状态）。未提及的账号保持原值。
    /// 修复「秃号内存中 credits>0 永远 selectable、策略持续命中同一账号」。
    /// 回写同时拨新快照时间戳（issue #91 缓存保鲜）：新数据即新鲜，
    /// reserve 过滤自动恢复生效
    pub fn update_credits(&self, credits: &HashMap<String, f64>) {
        let mut entries = safe_lock(&self.entries);
        let now = now_ts();
        for (uid, c) in credits {
            if let Some(e) = entries.get_mut(uid) {
                e.credits = Some(*c);
                e.credits_fetched_at = Some(now);
            }
        }
    }

    /// Qoder 池运行期三键拨新（issue #102 运行期保鲜）：credits + credits_expire_at +
    /// credits_fetched_at 一并回写。与 update_credits（Trae/WB 只拨余额+时间戳）不同，
    /// Qoder 的 selectable 到期过滤与 smart 排序键①「先到期先消耗」依赖
    /// credits_expire_at——旧值钉死会让充值/续期后的账号持续被排除直到网关重启。
    /// 未提及的账号保持原值，不触碰冷却/inflight/禁用等运行态（对齐 update_credits 纪律）
    pub fn update_credits_snapshot(&self, snapshot: &HashMap<String, (f64, Option<i64>)>) {
        let mut entries = safe_lock(&self.entries);
        let now = now_ts();
        for (uid, (c, expire)) in snapshot {
            if let Some(e) = entries.get_mut(uid) {
                e.credits = Some(*c);
                e.credits_fetched_at = Some(now);
                e.credits_expire_at = *expire;
            }
        }
    }

    /// Qoder 签到奖励运行期增量回写：credits += delta 并拨新 fetched_at，
    /// expire 以快照为准不动，不触碰冷却/inflight/禁用等运行态（对齐
    /// update_credits_snapshot 的拨新纪律）。修复「签到充值成功后账号仍被
    /// 零积分过滤持续排除到下次快照」（every_hours 2h / daily 档最长约 13.5h
    /// 的排除窗口）；池内不存在的账号静默跳过（与两回写方法同口径）
    pub fn apply_credits_delta(&self, uid: &str, delta: f64) {
        if delta == 0.0 {
            return;
        }
        let mut entries = safe_lock(&self.entries);
        if let Some(e) = entries.get_mut(uid) {
            let base = e.credits.unwrap_or(0.0).max(0.0);
            e.credits = Some((base + delta).max(0.0));
            e.credits_fetched_at = Some(now_ts());
        }
    }

    /// 从 WorkBuddy 账号同步池（T2.1/F-28）：WB 上游账号进同一调度引擎，
    /// 携带区域/企业域信息供上游 headers 使用
    pub fn sync_from_wb(&self, accounts: &[WbSyncAccount], enabled: &[String]) {
        let mut entries = safe_lock(&self.entries);
        let enabled_set: HashSet<&str> = enabled.iter().map(|s| s.as_str()).collect();
        // 增量合并（2026-10-19 修正）：旧版全量重建会把账号运行态
        //（until/hard_until/cb_trips/rate_trips/per_model_cooldowns/last_used/
        // successes/failures）全部清零，而本函数在导入账号、OAuth 登录、改名、
        // 分组变更、JWT 刷新成功后都会被调用（reload_pools_if_running）——
        // 结果是刚被上游限流的模型立刻重新去打、积分耗尽冷却到 04:00 的账号提前
        // 解冻、SoftRate 递增退避与 Server 熔断记忆归零。现改为：保留仍在池内
        // 账号的运行态，只更新快照字段；已不在池内的条目才移除。
        // 防御性保持：不清空非 WB 形态条目（SOLO 与 WB 不同时入池）。
        let mut keep: HashSet<String> = HashSet::new();
        for a in accounts {
            if !enabled_set.contains(a.uid.as_str()) || a.token.is_empty() {
                continue;
            }
            keep.insert(a.uid.clone());
            match entries.get_mut(&a.uid) {
                Some(e) => {
                    e.name = a.name.clone();
                    e.jwt = a.token.clone();
                    e.credits = a.credits;
                    e.credits_expire_at = a.credits_expire_at;
                    e.credits_fetched_at = a.credits_fetched_at;
                    e.disabled = a.needs_relogin;
                    e.domain = a.domain.clone();
                    e.enterprise_id = a.enterprise_id.clone();
                    e.global_region = a.global_region;
                    e.refresh_invalid = false;
                }
                None => {
                    entries.insert(a.uid.clone(), PoolEntry {
                        uid: a.uid.clone(),
                        name: a.name.clone(),
                        jwt: a.token.clone(),
                        credits: a.credits,
                        credits_expire_at: a.credits_expire_at,
                        credits_fetched_at: a.credits_fetched_at,
                        disabled: a.needs_relogin,
                        domain: a.domain.clone(),
                        enterprise_id: a.enterprise_id.clone(),
                        global_region: a.global_region,
                        ..PoolEntry::default()
                    });
                }
            }
        }
        // 仅移除已不在 WB 入池集合内的 WB 形态条目（非 WB 条目不动）
        entries.retain(|_, e| {
            if e.domain.is_empty() && !e.global_region {
                return true;
            }
            keep.contains(&e.uid)
        });
        // F-77：清理已不在池内的在途计数条目（在途请求经 Arc 独立释放，不受影响）
        {
            let mut inflight = safe_lock(&self.inflight);
            inflight.retain(|uid, _| entries.contains_key(uid));
        }
    }

    /// 从 Qoder 账号同步池（p3-3）：Qoder 专用池实例（`ApiSharedState.qoder_pool`），
    /// 全量重建语义同 sync_from_wb。积分/到期快照随 QoderSyncAccount 装配
    ///（issue #102 智能调度三键生效 + issue #91 reserve 保留额过滤）；
    /// machine_id 透传账号记录值（可能为空——请求时由 identity 回调解析
    /// effective 凭证补齐）
    pub fn sync_from_qoder(&self, accounts: &[QoderSyncAccount], enabled: &[String]) {
        let mut entries = safe_lock(&self.entries);
        let enabled_set: HashSet<&str> = enabled.iter().map(|s| s.as_str()).collect();
        // 增量合并（2026-10-19 修正，同 sync_from_wb）：保留池内账号的冷却/
        // 熔断/限频计数/按模型冷却运行态，只更新快照字段。旧版 entries.clear()
        // 会让 HardCredit（次日 04:00）与精确重置时刻冷却在任意一次账号同步后
        // 静默丢失，账号被提前解冻并反复打上游。
        let mut keep: HashSet<String> = HashSet::new();
        for a in accounts {
            if !enabled_set.contains(a.uid.as_str()) || a.access_token.is_empty() {
                continue;
            }
            keep.insert(a.uid.clone());
            match entries.get_mut(&a.uid) {
                Some(e) => {
                    e.name = a.name.clone();
                    e.jwt = a.access_token.clone();
                    e.credits = a.credits;
                    e.credits_expire_at = a.credits_expire_at;
                    e.credits_fetched_at = a.credits_fetched_at;
                    e.disabled = a.needs_relogin;
                    e.machine_id = a.machine_id.clone();
                    e.global_region = a.global_region;
                    e.refresh_invalid = false;
                }
                None => {
                    entries.insert(a.uid.clone(), PoolEntry {
                        uid: a.uid.clone(),
                        name: a.name.clone(),
                        jwt: a.access_token.clone(),
                        credits: a.credits,
                        credits_expire_at: a.credits_expire_at,
                        credits_fetched_at: a.credits_fetched_at,
                        disabled: a.needs_relogin,
                        machine_id: a.machine_id.clone(),
                        global_region: a.global_region,
                        ..PoolEntry::default()
                    });
                }
            }
        }
        entries.retain(|uid, _| keep.contains(uid));
        // F-77：清理已不在池内的在途计数条目（在途请求经 Arc 独立释放，不受影响）
        {
            let mut inflight = safe_lock(&self.inflight);
            inflight.retain(|uid, _| entries.contains_key(uid));
        }
    }

    /// 从 ZCode 账号同步池（z.ai GLM 池）：账号源 = zcode.sqlite（zcode 域管理，
    /// 网关只读），增量合并语义同 sync_from_qoder（保留冷却/熔断/限频运行态）。
    /// 凭证快照仅作入池门槛与兜底——请求期真凭证经 zcode_identity 回调按次解析。
    ///
    /// 套餐门槛（2026-10-23 套餐池改造）：`plan` 随入池一并写入。与历史版本的差别
    /// 只在"已同步画像且模型集为空"的账号——它们**照旧入池但 `selectable` 会挡掉**，
    /// 这样既能立刻停掉 13/15 个无套餐号的无效消耗，又保留 `pool_status` 里对
    /// 这些账号的可见性（用户能看懂为什么它们不可用）。
    pub fn sync_from_zcode(&self, accounts: &[ZCodeSyncAccount], enabled: &[String]) {
        let mut entries = safe_lock(&self.entries);
        let enabled_set: HashSet<&str> = enabled.iter().map(|s| s.as_str()).collect();
        let mut keep: HashSet<String> = HashSet::new();
        for a in accounts {
            if !enabled_set.contains(a.uid.as_str()) || a.access_token.is_empty() {
                continue;
            }
            keep.insert(a.uid.clone());
            match entries.get_mut(&a.uid) {
                Some(e) => {
                    e.name = a.name.clone();
                    e.jwt = a.access_token.clone();
                    e.device_id = a.device_mid.clone();
                    e.refresh_invalid = false;
                    // 画像只在本次带上时覆盖（None = 未同步，保留池内旧快照，
                    // 避免装配层漏传把已生效的套餐门闸擦掉）
                    if a.plan.is_some() {
                        e.plan = a.plan.clone();
                    }
                }
                None => {
                    entries.insert(a.uid.clone(), PoolEntry {
                        uid: a.uid.clone(),
                        name: a.name.clone(),
                        jwt: a.access_token.clone(),
                        device_id: a.device_mid.clone(),
                        plan: a.plan.clone(),
                        ..PoolEntry::default()
                    });
                }
            }
        }
        entries.retain(|uid, _| keep.contains(uid));
        let mut inflight = safe_lock(&self.inflight);
        inflight.retain(|uid, _| entries.contains_key(uid));
    }

    /// 运行期回写套餐画像（监控域画像同步后调用）：只改 plan 字段，不重建池，
    /// 不动白名单/冷却/在途计数。`None` 值表示该账号本次没拿到画像 → 保留旧值。
    pub fn update_zcode_plans(&self, updates: &[(String, Option<PlanGate>)]) {
        if updates.is_empty() {
            return;
        }
        let mut entries = safe_lock(&self.entries);
        for (uid, plan) in updates {
            if let (Some(e), Some(p)) = (entries.get_mut(uid), plan.as_ref()) {
                e.plan = Some(p.clone());
            }
        }
    }

    /// 读取某账号的套餐门闸快照（错误分类用：1113 细分需要知道"是没权益还是没额度"）
    pub fn plan_of(&self, uid: &str) -> Option<PlanGate> {
        safe_lock(&self.entries).get(uid).and_then(|e| e.plan.clone())
    }

    /// 账号区域查询（免费档记账用）：Some(true)=海外、Some(false)=国内、
    /// None=池中无此账号
    pub fn uid_region(&self, uid: &str) -> Option<bool> {
        let entries = safe_lock(&self.entries);
        entries.get(uid).map(|e| e.global_region)
    }

    /// 指定 uid 取号（T2.4 会话粘性）：账号 healthy 时返回其凭证，否则 None
    pub fn pick_by_uid(&self, uid: &str) -> Option<PickedAccount> {
        self.pick_by_uid_region(uid, None)
    }

    /// 指定 uid 取号 + 区域过滤（国内/海外号池隔离）：`want_global` = Some 时
    /// 仅出该区账号（None = 不过滤，历史语义）。粘性绑定区不符 → None（调用方
    /// 回落正常取号——模型区域决定账号区域，粘性不得跨区续用）。
    pub fn pick_by_uid_region(&self, uid: &str, want_global: Option<bool>) -> Option<PickedAccount> {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let tried = HashSet::new();
        let reserve = self.reserve_credits();
        entries
            .get(uid)
            .filter(|e| selectable(e, &tried, now, reserve))
            .filter(|e| want_global.map_or(true, |w| e.global_region == w))
            .map(|e| PickedAccount {
                uid: e.uid.clone(),
                jwt: e.jwt.clone(),
                device_id: e.device_id.clone(),
                machine_id: e.machine_id.clone(),
                domain: e.domain.clone(),
                enterprise_id: e.enterprise_id.clone(),
                global_region: e.global_region,
                // 专一/粘性锁定：忽略 busy 过滤（语义即「锁定即用」），无条件绑定
                over_limit: true,
            })
    }

    /// 每区域各取一个含凭证账号（WB 模型目录刷新用，修复「海外账号模型目录」）：
    /// 两区模型清单由上游各自下发，刷新需按区取号；区内任取一个（目录刷新
    /// 不要求账号健康，积分耗尽账号照样能拉目录）。
    /// 返回 (uid, jwt, domain, enterprise_id, global_region)，区内无凭证则缺该区。
    pub fn pick_one_per_region(&self) -> Vec<(String, String, String, String, bool)> {
        let entries = safe_lock(&self.entries);
        let mut cn: Option<(String, String, String, String, bool)> = None;
        let mut global: Option<(String, String, String, String, bool)> = None;
        for e in entries.values() {
            if e.jwt.is_empty() {
                continue;
            }
            let slot = if e.global_region { &mut global } else { &mut cn };
            if slot.is_none() {
                *slot = Some((
                    e.uid.clone(),
                    e.jwt.clone(),
                    e.domain.clone(),
                    e.enterprise_id.clone(),
                    e.global_region,
                ));
            }
        }
        [cn, global].into_iter().flatten().collect()
    }

    /// 带 Key 约束取号（F-35 子 Key 体系，批次3）：
    /// - `dedicated`：专一模式绑定 uid（healthy 即直接锁定，绕过策略）
    /// - `allowed`：上游白名单过滤（None = 不限；`Some(空集)` = 过滤全部——
    ///   issue #30 混合白名单借空集表达「该池被此 Key 排除」，调用方可有意传入）
    /// - 调度策略沿用池当前策略（子 Key「临期优先」= 池默认 expire_first，
    ///   池策略本身即用户可选的临期/积分/加权等模式；约束仅做过滤与锁定）
    pub fn pick_excluding_constrained(
        &self,
        tried: &HashSet<String>,
        allowed: Option<&HashSet<String>>,
        dedicated: Option<&str>,
    ) -> Option<PickedAccount> {
        self.pick_excluding_constrained_ev(tried, allowed, dedicated)
            .map(|(p, _)| p)
    }

    /// 同 [pick_excluding_constrained]，附带 F-77 调度事件（[SCHED] 日志用），
    /// 并内置防惊群让位（T2.2：100ms 内重复选中同一 uid 且有其他候选时让位）：
    /// - `busy_yield`：候选中存在被并发上限过滤的 busy 账号（让位给空闲账号）
    /// - `busy_fallback`：全部候选 busy，降级取 inflight 最小者（不过载拒绝）
    pub fn pick_excluding_constrained_ev(
        &self,
        tried: &HashSet<String>,
        allowed: Option<&HashSet<String>>,
        dedicated: Option<&str>,
    ) -> Option<(PickedAccount, Option<String>)> {
        self.pick_excluding_constrained_ev_region(tried, allowed, dedicated, None)
    }

    /// 区域过滤版（国内/海外号池隔离）：`want_global` = Some 时只在该区账号中
    /// 取号——国内模型不出海外号、海外模型不出国内号；None = 不过滤（历史语义）。
    /// 专一绑定 uid 区不符时跳过专一锁定（不得跨区续用）。
    pub fn pick_excluding_constrained_ev_region(
        &self,
        tried: &HashSet<String>,
        allowed: Option<&HashSet<String>>,
        dedicated: Option<&str>,
        want_global: Option<bool>,
    ) -> Option<(PickedAccount, Option<String>)> {
        // 专一模式：绑定账号 healthy 且未试错过 → 直接锁定（专一绑定不让位）
        if let Some(uid) = dedicated {
            if !tried.contains(uid) {
                if let Some(p) = self.pick_by_uid_region(uid, want_global) {
                    let mut entries = safe_lock(&self.entries);
                    if let Some(e) = entries.get_mut(uid) {
                        e.last_used = now_ts();
                    }
                    return Some((p, None));
                }
            }
        }
        let mut entries = safe_lock(&self.entries);
        let strategy = *safe_lock(&self.strategy);
        let now = now_ts();
        let reserve = self.reserve_credits();
        let all_cands: Vec<&PoolEntry> = entries
            .values()
            .filter(|e| selectable(e, tried, now, reserve))
            .filter(|e| want_global.map_or(true, |w| e.global_region == w))
            .filter(|e| allowed.map_or(true, |a| a.contains(&e.uid)))
            .collect();
        // F-77 busy 过滤 + 全 busy 降级
        let inflight = self.inflight_snapshot();
        let limit = self.concurrency_limit();
        let (cands, busy_fallback) = busy_filter(&all_cands, &inflight, limit);
        let rand_seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() ^ (d.subsec_nanos() as u64).wrapping_mul(0x9e3779b97f4a7c15))
            .unwrap_or(0);
        let mut picked = if busy_fallback {
            all_cands
                .iter()
                .copied()
                .min_by_key(|e| inflight_of(e, &inflight))
        } else {
            pick_by_strategy(&cands, strategy, rand_seed, now, &inflight)
        }?;

        // 防惊群：100ms 内重复选中同一 uid 且还有其他候选 → 让位（T2.2）；
        // 全 busy 降级路径不参与——min(inflight) 本身即负载分散，策略让位反而
        // 会破坏「取在途最小者」语义（F-77）
        if !busy_fallback && cands.len() > 1 {
            let mut recent = safe_lock(&self.recent_pick);
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if recent.0 == picked.uid && now_ms - recent.1 < 100 {
                if let Some(alt) = pick_by_strategy(
                    &cands
                        .iter()
                        .copied()
                        .filter(|e| e.uid != picked.uid)
                        .collect::<Vec<_>>(),
                    strategy,
                    rand_seed.wrapping_add(1),
                    now,
                    &inflight,
                ) {
                    picked = alt;
                }
            }
            *recent = (picked.uid.clone(), now_ms);
        }
        let account = PickedAccount {
            uid: picked.uid.clone(),
            jwt: picked.jwt.clone(),
            device_id: picked.device_id.clone(),
            machine_id: picked.machine_id.clone(),
            domain: picked.domain.clone(),
            enterprise_id: picked.enterprise_id.clone(),
            global_region: picked.global_region,
            // busy_fallback 降级（全候选 busy 取 in-flight 最小者）保留「不过载拒绝」
            // 既有语义 → 无条件绑定；正常路径交由路由侧 CAS 复核
            over_limit: busy_fallback,
        };
        // F-77⑤ 可观测：busy 让位/降级事件（无 busy 过滤发生则 None）
        let event = if limit > 0 && busy_fallback {
            Some(format!(
                "busy_fallback picked={} inflight={} limit={}",
                picked.uid,
                inflight_of(picked, &inflight),
                limit
            ))
        } else if limit > 0 && cands.len() < all_cands.len() {
            Some(format!(
                "busy_yield filtered={} picked={}",
                all_cands.len() - cands.len(),
                picked.uid
            ))
        } else {
            None
        };
        drop(cands); // 释放 entries 不可变借用后再更新 last_used
        if let Some(e) = entries.get_mut(&account.uid) {
            e.last_used = now;
        }
        Some((account, event))
    }

    /// ZCode 套餐感知取号（2026-10-23 套餐池改造）。
    ///
    /// 与通用 `pick_excluding_constrained_ev_region` 的差别只有三处，其余（专一锁定、
    /// busy 过滤与降级、防惊群、last_used 记账）完全复用同一套语义：
    /// 1. **模型维度过滤**：请求模型不在账号 `servable_models` 内直接跳过，不发请求；
    /// 2. **时段维度过滤**：只有夜间档权益的账号在白天跳过（不冷却、不计错）；
    /// 3. **套餐燃烧顺序**：`new_account > task_card > global_build > start_plan`，
    ///    同形态内先到期先烧，同档内才交给池策略挑——这样临时额度先烧完，
    ///    Start Plan 这类长期订阅留作底座（用户 2026-10-23 定的口径）。
    ///
    /// 画像未知（`plan: None`）的账号 fail-open 仍然可选，但排在所有已知套餐之后，
    /// 避免它们抢走临时额度的消耗顺位。
    pub fn pick_zcode(
        &self,
        tried: &HashSet<String>,
        allowed: Option<&HashSet<String>>,
        dedicated: Option<&str>,
        plan_kinds: Option<&HashSet<String>>,
        model: &str,
        now: i64,
        hour: u32,
    ) -> Option<(PickedAccount, Option<String>)> {
        let model_lc = model.trim().to_lowercase();
        // 专一模式：绑定账号 healthy 且画像允许 → 直接锁定（专一不让位）
        if let Some(uid) = dedicated {
            if !tried.contains(uid) {
                let entries = safe_lock(&self.entries);
                let reserve = self.reserve_credits();
                let no_tried = HashSet::new();
                let hit = entries
                    .get(uid)
                    .filter(|e| selectable(e, &no_tried, now, reserve))
                    .filter(|e| plan_gate_ok(e, &model_lc, plan_kinds, now, hour))
                    .map(|e| PickedAccount {
                        uid: e.uid.clone(),
                        jwt: e.jwt.clone(),
                        device_id: e.device_id.clone(),
                        machine_id: e.machine_id.clone(),
                        domain: e.domain.clone(),
                        enterprise_id: e.enterprise_id.clone(),
                        global_region: e.global_region,
                        over_limit: false,
                    });
                drop(entries);
                if let Some(p) = hit {
                    let mut entries = safe_lock(&self.entries);
                    if let Some(e) = entries.get_mut(uid) {
                        e.last_used = now;
                    }
                    return Some((p, None));
                }
            }
        }
        let mut entries = safe_lock(&self.entries);
        let strategy = *safe_lock(&self.strategy);
        let reserve = self.reserve_credits();
        // 画像/时段过滤在 selectable 之后做：先摘掉无权益账号，再按模型与窗口收窄。
        // plan_blocked 只用于可观测（"有健康号但都不服务这个模型"是最常见的一种 4xx 根因）
        let plan_blocked = entries
            .values()
            .filter(|e| selectable(e, tried, now, reserve))
            .filter(|e| allowed.map_or(true, |a| a.contains(&e.uid)))
            .count();
        let all_cands: Vec<&PoolEntry> = entries
            .values()
            .filter(|e| selectable(e, tried, now, reserve))
            .filter(|e| allowed.map_or(true, |a| a.contains(&e.uid)))
            .filter(|e| plan_gate_ok(e, &model_lc, plan_kinds, now, hour))
            .collect();
        let inflight = self.inflight_snapshot();
        let limit = self.concurrency_limit();
        let (cands, busy_fallback) = busy_filter(&all_cands, &inflight, limit);
        let rand_seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() ^ (d.subsec_nanos() as u64).wrapping_mul(0x9e3779b97f4a7c15))
            .unwrap_or(0);
        let mut picked = if busy_fallback {
            all_cands
                .iter()
                .copied()
                .min_by_key(|e| inflight_of(e, &inflight))
        } else {
            pick_plan_first(&cands, strategy, rand_seed, now, &inflight)
        }?;
        // 防惊群：同 100ms 内重复选中且还有其他候选 → 让位（同档内重新挑）
        if !busy_fallback && cands.len() > 1 {
            let mut recent = safe_lock(&self.recent_pick);
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or(0);
            if recent.0 == picked.uid && now_ms - recent.1 < 100 {
                let others: Vec<&PoolEntry> = cands
                    .iter()
                    .copied()
                    .filter(|e| e.uid != picked.uid)
                    .collect();
                if let Some(alt) =
                    pick_plan_first(&others, strategy, rand_seed.wrapping_add(1), now, &inflight)
                {
                    picked = alt;
                }
            }
            *recent = (picked.uid.clone(), now_ms);
        }
        let account = PickedAccount {
            uid: picked.uid.clone(),
            jwt: picked.jwt.clone(),
            device_id: picked.device_id.clone(),
            machine_id: picked.machine_id.clone(),
            domain: picked.domain.clone(),
            enterprise_id: picked.enterprise_id.clone(),
            global_region: picked.global_region,
            over_limit: busy_fallback,
        };
        // 可观测：套餐过滤/降级事件（无则 None，调用方只在 Some 时记日志）
        let event = if all_cands.is_empty() && plan_blocked > 0 {
            Some(format!(
                "plan_filtered model={} blocked={} (模型/时段/额度/套餐白名单不匹配)",
                model_lc, plan_blocked
            ))
        } else if limit > 0 && busy_fallback {
            Some(format!(
                "busy_fallback picked={} inflight={} limit={}",
                picked.uid,
                inflight_of(picked, &inflight),
                limit
            ))
        } else {
            None
        };
        drop(cands);
        if let Some(e) = entries.get_mut(&account.uid) {
            e.last_used = now;
        }
        Some((account, event))
    }

    /// 池内某模型的可选账号数（ZCode 路由「无可用账号」时报错用）：
    /// 返回 (可服务该模型的健康账号数, 池内健康账号总数)
    pub fn zcode_model_stats(&self, model: &str, now: i64, hour: u32) -> (usize, usize) {
        let entries = safe_lock(&self.entries);
        let reserve = self.reserve_credits();
        let no_tried = HashSet::new();
        let healthy = entries
            .values()
            .filter(|e| selectable(e, &no_tried, now, reserve))
            .count();
        let model_lc = model.trim().to_lowercase();
        let for_model = entries
            .values()
            .filter(|e| selectable(e, &no_tried, now, reserve))
            .filter(|e| plan_gate_ok(e, &model_lc, None, now, hour))
            .count();
        (for_model, healthy)
    }

    /// F-77④ 粘性让位取号：粘性账号 busy（inflight ≥ 并发上限）且池内存在其他
    /// 空闲健康候选时让位改走调度策略（并发健康优先于上游 KV cache 复用）；
    /// 全候选 busy / 未启用并发上限（limit=0）/ 无其他候选时保持粘性锁定。
    /// 返回 (账号, 调度事件)：事件非 None 时由调用方记 [SCHED] 日志
    /// （sticky_yield=让位改选 / sticky_fallback=全 busy 保持粘性）。
    pub fn pick_sticky_yield(
        &self,
        sticky_uid: &str,
        allowed: Option<&HashSet<String>>,
    ) -> Option<(PickedAccount, Option<String>)> {
        self.pick_sticky_yield_region(sticky_uid, allowed, None)
    }

    /// 区域过滤版粘性取号：粘性账号区不符模型区域 → 视同无粘性（None，调用方
    /// 回落正常取号）；让位候选同样只在同区内选（国内/海外号池隔离）。
    pub fn pick_sticky_yield_region(
        &self,
        sticky_uid: &str,
        allowed: Option<&HashSet<String>>,
        want_global: Option<bool>,
    ) -> Option<(PickedAccount, Option<String>)> {
        let sticky = self.pick_by_uid_region(sticky_uid, want_global)?;
        let region_ok = |e: &PoolEntry| want_global.map_or(true, |w| e.global_region == w);
        let limit = self.concurrency_limit();
        let inflight = self.inflight_snapshot();
        let sticky_inflight = inflight.get(sticky_uid).copied().unwrap_or(0);
        if limit == 0 || sticky_inflight < limit {
            // 快照判定空闲：绑定语义交路由侧 CAS 复核（TOCTOU 收紧）；
            // limit=0 未启用上限，CAS 侧按无条件绑定处理
            let mut sticky = sticky;
            sticky.over_limit = false;
            return Some((sticky, None));
        }
        // 粘性账号 busy：判定其他健康候选（同 allowed 约束；首轮无 tried）是否存在空闲
        let reserve = self.reserve_credits();
        let (has_other, has_idle) = {
            let entries = safe_lock(&self.entries);
            let now = now_ts();
            let tried = HashSet::new();
            entries
                .values()
                .filter(|e| e.uid != sticky_uid && selectable(e, &tried, now, reserve))
                .filter(|e| region_ok(e))
                .filter(|e| allowed.map_or(true, |a| a.contains(&e.uid)))
                .fold((false, false), |(any, idle), e| {
                    (any || true, idle || inflight_of(e, &inflight) < limit)
                })
        };
        if !has_other {
            return Some((sticky, None));
        }
        if !has_idle {
            // 全 busy：保持粘性（粘性缓存收益 > 换一个同样 busy 的账号）
            return Some((
                sticky,
                Some(format!(
                    "sticky_fallback uid={sticky_uid} inflight={} limit={limit} all_busy=1",
                    sticky_inflight
                )),
            ));
        }
        // 让位：空闲候选中走调度策略（重取锁；候选在间隙被移除则回退保持粘性）
        let mut entries = safe_lock(&self.entries);
        let now = now_ts();
        let tried = HashSet::new();
        let idle: Vec<&PoolEntry> = entries
            .values()
            .filter(|e| e.uid != sticky_uid && selectable(e, &tried, now, reserve))
            .filter(|e| region_ok(e))
            .filter(|e| allowed.map_or(true, |a| a.contains(&e.uid)))
            .filter(|e| inflight_of(e, &inflight) < limit)
            .collect();
        let strategy = *safe_lock(&self.strategy);
        let rand_seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() ^ (d.subsec_nanos() as u64).wrapping_mul(0x9e3779b97f4a7c15))
            .unwrap_or(0);
        let Some(picked) = pick_by_strategy(&idle, strategy, rand_seed, now, &inflight) else {
            return Some((sticky, None));
        };
        let account = PickedAccount {
            uid: picked.uid.clone(),
            jwt: picked.jwt.clone(),
            device_id: picked.device_id.clone(),
            machine_id: picked.machine_id.clone(),
            domain: picked.domain.clone(),
            enterprise_id: picked.enterprise_id.clone(),
            global_region: picked.global_region,
            // 让位候选取自快照空闲集：绑定语义交路由侧 CAS 复核（TOCTOU 收紧）
            over_limit: false,
        };
        let event = Some(format!(
            "sticky_yield from={sticky_uid} to={} inflight={} limit={}",
            picked.uid, sticky_inflight, limit
        ));
        if let Some(e) = entries.get_mut(&account.uid) {
            e.last_used = now;
        }
        Some((account, event))
    }

    /// 更新账号凭证（T2.6：网关 401 刷新后回填，后续取号即用新 token）
    pub fn update_jwt(&self, uid: &str, jwt: &str) {
        let mut entries = safe_lock(&self.entries);
        if let Some(e) = entries.get_mut(uid) {
            e.jwt = jwt.to_string();
        }
    }

    /// 按 uid 取账号展示名（请求日志用）；不在池中返回空串（日志侧显示 "-"）
    pub fn name_of(&self, uid: &str) -> String {
        safe_lock(&self.entries)
            .get(uid)
            .map(|e| e.name.clone())
            .unwrap_or_default()
    }

    /// F-78 批次 3：refresh_token 判定失效 → 运行时禁用（前端 refresh_jwt 失败联动；
    /// 持久化标记由 record_refresh_failure 写 accounts 文件，重启后经 sync_from_accounts 同步）
    pub fn note_refresh_invalid(&self, uid: &str) {
        let mut entries = safe_lock(&self.entries);
        if let Some(e) = entries.get_mut(uid) {
            e.refresh_invalid = true;
            e.disabled = true;
            e.reason = "refresh_token_invalid".to_string();
        }
    }

    /// 记录错误并冷却（T2.2 错误三态 + 分级冷却）。
    /// None 属请求级错误（如 4001 模型配置缺失），与账号健康无关——在入口
    /// 直接忽略，防止全池账号被同一模型错误连坐熔断（P0-1 审查结论，守卫
    /// 下沉至此，调用点无需逐一重复；Client 未知 4xx 仍保留熔断记忆）。
    /// 冷却/禁用标记发生时同步落一条 app.log（F-80 可观测）：
    /// `account cooldown: pool=<池> acct=<名称> uid=<uid> kind=<错误分类> action=<处置>`，
    /// 排查「账号何时、为何被打冷却→选池 no_healthy_account→跨池回退」链路时可直接定位
    pub fn note_error(&self, uid: &str, kind: ErrKind) {
        if kind == ErrKind::None {
            return;
        }
        let mut entries = safe_lock(&self.entries);
        let mut cooldown_ev: Option<(String, String)> = None;
        if let Some(e) = entries.get_mut(uid) {
            Self::decay_counters(e);
            e.failures = e.failures.saturating_add(1);
            let mut action: Option<String> = None;
            match kind {
                ErrKind::SessionDead | ErrKind::Forbidden => {
                    e.disabled = true;
                    e.reason = kind.as_str().to_string();
                    action = Some("action=disabled".to_string());
                }
                ErrKind::HardCredit => {
                    // 积分耗尽：冷却到下个 04:00（凌晨 00:00–04:00 窗口内为当天，
                    // next_0400 语义），到期自动恢复探测（F-29 v1.2）
                    //
                    // 余额兜底（2026-10-20 事故修复）：号上积分仍 > 0 时，上游那条
                    // 「额度不足」只可能是该模型受限或分类误判，整号冻结一天的后
                    // 果（四个 700/1000 积分的号齐冻到 04:00，客户端持续「重新连接」）
                    // 远大于收益。此时降级为普通冷却：不动 hard_until，按 SoftRate
                    // 退避（60s 起按 trips 翻倍，封顶 30 分钟），接受上游的真拒绝
                    // 而不是伪造一整天的不可用。
                    if e.credits.is_some_and(|c| c > 0.0) {
                        e.rate_trips = e.rate_trips.saturating_add(1);
                        let secs = (60i64 << (e.rate_trips - 1).min(5)).min(30 * 60);
                        let until = now_ts() + secs;
                        e.until = until;
                        e.reason = "hard_credit_downgraded".to_string();
                        e.err_count = 0;
                        action = Some(format!(
                            "action=hard_credit_downgraded credits={:?} secs={secs} until={}",
                            e.credits,
                            fmt_local_ts(until)
                        ));
                    } else {
                        let until = next_0400(now_ts());
                        e.hard_until = until;
                        e.reason = "hard_credit".to_string();
                        e.err_count = 0;
                        action = Some(format!(
                            "action=hard_cooldown until={} (下个04:00)",
                            fmt_local_ts(until)
                        ));
                    }
                }
                ErrKind::PlanLimit | ErrKind::SoftRate | ErrKind::NotFound => {
                    let base_secs = kind.cooldown_duration().as_secs() as i64;
                    // SoftRate 退避递增：60s 固定冷却无法覆盖「日额度耗尽但文案解析
                    // 不出重置时刻」的场景——账号每 60s 解冻一次、又被选中、再 429，
                    // 形成无限循环且每次都占用一次真实上游往返。按连续命中次数
                    // 翻倍（60s→2m→4m→8m→16m），封顶 30 分钟；任何成功即清零。
                    let secs = if kind == ErrKind::SoftRate {
                        e.rate_trips = e.rate_trips.saturating_add(1);
                        let shift = (e.rate_trips - 1).min(5);
                        (base_secs << shift).min(30 * 60)
                    } else {
                        base_secs
                    };
                    let until = now_ts() + secs;
                    e.until = until;
                    e.reason = kind.as_str().to_string();
                    e.err_count = 0;
                    action = Some(format!(
                        "action=cooldown secs={secs} trips={} until={}",
                        e.rate_trips,
                        fmt_local_ts(until)
                    ));
                    // P2 修复10：不重置 cb_trips——熔断记忆只应在 note_success 重置，
                    // 否则 Server 熔断记忆被非 Server 错误意外清零
                }
                _ => {
                    e.err_count += 1;
                    if e.err_count >= 3 {
                        // 熔断器：30m 起指数递增（×2/次），封顶 6h
                        let exp = 30 * 60u64 << e.cb_trips.min(4);
                        let capped = exp.min(6 * 3600);
                        let until = now_ts() + capped as i64;
                        e.until = until;
                        e.reason = "consecutive_errors".to_string();
                        e.err_count = 0;
                        e.cb_trips = e.cb_trips.saturating_add(1);
                        action = Some(format!(
                            "action=circuit_breaker secs={capped} until={}",
                            fmt_local_ts(until)
                        ));
                    }
                }
            }
            if let Some(a) = action {
                cooldown_ev = Some((e.name.clone(), a));
            }
            // P1：运行态冷却字段已变更（until/hard_until/cb_trips/rate_trips/
            // err_count），标脏交 flusher 落库；请求热路径本身不写 SQLite
            self.mark_cooldown_dirty();
        }
        drop(entries);
        // F-80：log_ctx 未装配（测试/未启动）时静默跳过，不产生日志副作用
        if let Some((name, ev)) = cooldown_ev {
            let ctx = safe_lock(&self.log_ctx).clone();
            if let Some((dir, label)) = ctx {
                crate::fs_utils::app_log(
                    &dir,
                    &format!(
                        "account cooldown: pool={label} acct={name} uid={uid} kind={} {ev}",
                        kind.as_str(),
                    ),
                );
            }
        }
    }

    /// 把账号冷却精确延长到上游下发的恢复时刻（6004 日限额「将在 … UTC+8 重置」）。
    /// 取 `max(现有 until, ts)` 只增不减，且不碰 disabled/hard_until——到期由 `healthy`
    /// 自然放开（无恢复探测需求：日额度重置时刻即上游权威时刻）。选号 `selectable` 走
    /// `healthy`（读 `until`），故被冻账号在恢复前不再被取，池内其余健康账号正常顶上。
    pub fn cooldown_until(&self, uid: &str, ts: i64) {
        let now = now_ts();
        if ts <= now {
            return; // 已过/非法恢复时刻：不处理，交由常规冷却
        }
        let mut entries = safe_lock(&self.entries);
        if let Some(e) = entries.get_mut(uid) {
            if ts > e.until {
                e.until = ts;
                e.reason = "quota_reset".to_string();
                e.err_count = 0;
                self.mark_cooldown_dirty();
            }
        }
    }

    /// 按 (uid, model) 精确延长冷却到上游恢复时刻（6004「可切换其他模型继续使用」）。
    /// 与 cooldown_until（整账号）的区别：只挡这一个模型的取号，账号对其他模型
    /// （含积分模型）照常可用——上游文案明确限的是该模型频率，不是整个账号。
    pub fn per_model_cooldown_until(&self, uid: &str, model: &str, ts: i64) {
        let now = now_ts();
        if ts <= now {
            return;
        }
        let mut entries = safe_lock(&self.entries);
        if let Some(e) = entries.get_mut(uid) {
            let cur = e.per_model_cooldowns.get(model).copied().unwrap_or(0);
            if ts > cur {
                e.per_model_cooldowns.insert(model.to_string(), ts);
                self.mark_cooldown_dirty();
            }
        }
    }

    /// 当前在该模型上处于冷却的账号 uid 集合（到期自动剔除）。
    /// 仅供可观测与诊断使用；**不要**把它预注入取号的 tried 集合——那会把
    /// 「该模型对此账号冷却」这个状态误当成「本轮已试过且失败」，池内仅剩
    /// 这一个账号时必然退化成假 503（账号其他模型仍是健康的）。
    /// 选号侧的按模型过滤见 [model_cooling_excluding]。
    pub fn model_cooled_uids(&self, model: &str) -> Vec<String> {
        self.model_cooling_excluding(model, None)
    }

    /// 按模型冷却的 uid 集合，可排除一个账号（`exclude_uid` = 当前候选）。
    /// 语义：若把该账号也算进冷却名单，就永远看不到「除它之外还有没有备选」，
    /// 而「没有备选」恰恰是必须放行的场景——否则等于把账号停了。
    pub fn model_cooling_excluding(&self, model: &str, exclude_uid: Option<&str>) -> Vec<String> {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        entries
            .values()
            .filter(|e| Some(e.uid.as_str()) != exclude_uid)
            .filter(|e| {
                e.per_model_cooldowns
                    .get(model)
                    .copied()
                    .map(|until| now < until)
                    .unwrap_or(false)
            })
            .map(|e| e.uid.clone())
            .collect()
    }

    /// 该账号在此模型上是否仍处冷却（到期自动视为无冷却）。
    pub fn is_model_cooling(&self, uid: &str, model: &str) -> bool {
        let entries = safe_lock(&self.entries);
        entries
            .get(uid)
            .and_then(|e| e.per_model_cooldowns.get(model))
            .map(|&until| now_ts() < until)
            .unwrap_or(false)
    }

    /// 除 `uid` 外，是否还有其他账号可在此模型上顶上（区域/白名单感知）。
    /// 用于「该账号在此模型冷却，但池里没别人了」的判定——此时必须放行，
    /// 让请求真实打到上游，由上游决定是否拒绝，而不是本地伪造 503。
    pub fn has_other_selectable_for_model(
        &self,
        uid: &str,
        allowed: Option<&HashSet<String>>,
        want_global: Option<bool>,
    ) -> bool {
        self.selectable_stats_in_region_excluding(allowed, want_global, Some(uid)).1 > 0
    }

    /// 清除某账号全部按模型冷却（手动清除冷却 / 重新登录时调用）
    pub fn clear_model_cooldowns(&self, uid: &str) {
        if let Some(e) = safe_lock(&self.entries).get_mut(uid) {
            e.per_model_cooldowns.clear();
        }
    }

    /// 记录成功（重置熔断与错误计数）
    pub fn note_success(&self, uid: &str) {
        let mut entries = safe_lock(&self.entries);
        if let Some(e) = entries.get_mut(uid) {
            if e.cb_trips != 0 || e.rate_trips != 0 || e.err_count != 0 {
                // 持久化字段（熔断/限额/连错计数）归零同样要落库，
                // 否则重启后旧的退避记忆复活（P1）
                self.mark_cooldown_dirty();
            }
            e.err_count = 0;
            e.cb_trips = 0;
            e.rate_trips = 0;
            Self::decay_counters(e);
            e.successes = e.successes.saturating_add(1);
        }
    }

    /// 成功/失败计数滚动窗口：总数超过 50 时双双减半（向上取整保 1），
    /// 使 success_rate 反映近期表现而非历史总量——否则账号早期 100 次失败
    /// 会把之后 100 次成功稀释到 0.5，加权调度永久低估恢复后的账号（P2-6）
    fn decay_counters(e: &mut PoolEntry) {
        if e.successes + e.failures >= 50 {
            e.successes = (e.successes + 1) / 2;
            e.failures = (e.failures + 1) / 2;
        }
    }

    /// 清除所有账号的内存冷却状态（不影响 disabled/SessionDead）
    pub fn clear_cooldowns(&self) -> usize {
        let mut entries = safe_lock(&self.entries);
        let now = now_ts();
        let count = entries
            .values()
            .filter(|e| (e.until > 0 && now < e.until) || (e.hard_until > 0 && now < e.hard_until))
            .count();
        for e in entries.values_mut() {
            e.until = 0;
            e.hard_until = 0;
            e.reason.clear();
            e.err_count = 0;
            // 手动清除必须同时清掉限频连续计数：否则下次限频会直接吃到
            // 30 分钟上限，用户点「清除冷却」后仍被长时间冻结
            e.rate_trips = 0;
            // 同理清熔断次数：用户手动清除即宣告「我知道这些号被误伤/上游
            // 已恢复」，若保留 cb_trips，下次 Server 错误直接 1h 起步（30m×2），
            // 与「已清除即可用」的预期相悖
            e.cb_trips = 0;
            e.per_model_cooldowns.clear();
        }
        // P1：清除后的空态也要落库，否则重启会把清除前的冷却快照复活
        self.mark_cooldown_dirty();
        count
    }

    /// 清除单个账号的冷却（cooldown_clear 命令联动，P1）：与 clear_cooldowns
    /// 同语义但只针对 uid；返回是否命中。清除后标脏，由调用方立即 flush 落库，
    /// 保证「用户点了解除冷却」在重启后依然生效（不被 pool_cooldowns 复活）。
    pub fn clear_one(&self, uid: &str) -> bool {
        let mut entries = safe_lock(&self.entries);
        let Some(e) = entries.get_mut(uid) else {
            return false;
        };
        e.until = 0;
        e.hard_until = 0;
        e.reason.clear();
        e.err_count = 0;
        e.rate_trips = 0;
        e.cb_trips = 0;
        e.per_model_cooldowns.clear();
        self.mark_cooldown_dirty();
        true
    }

    /// 返回池状态列表
    pub fn status_list(&self) -> Vec<PoolStatus> {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let inflight = self.inflight_snapshot();
        let mut out: Vec<PoolStatus> = entries
            .values()
            .map(|e| PoolStatus {
                uid: e.uid.clone(),
                name: e.name.clone(),
                global_region: e.global_region,
                credits: e.credits,
                credits_expire_at: e.credits_expire_at,
                cooling: e.until > 0 && now < e.until,
                cooldown_until: if e.until > 0 { Some(e.until) } else { None },
                cooldown_reason: if e.reason.is_empty() { None } else { Some(e.reason.clone()) },
                hard_credit: e.hard_until > 0 && now < e.hard_until,
                disabled: e.disabled,
                err_count: e.err_count,
                state: e.state_str(now).to_string(),
                inflight: inflight.get(&e.uid).copied().unwrap_or(0),
                refresh_invalid: e.refresh_invalid,
                // 按模型冷却快照（已过滤过期）：账号级 cooling/hard_credit 之外的
                // 第二维度——账号健康但某个模型被限。前端据此逐模型展示「哪个模型
                // 没额度、什么时候恢复」，用户才知道该换哪个模型接着用
                per_model_cooldowns: e
                    .per_model_cooldowns
                    .iter()
                    .filter(|(_, until)| **until > now)
                    .map(|(m, u)| (m.clone(), *u))
                    .collect(),
                // 套餐画像（仅 zcode 池有值）：网关页按套餐分组展示、
                // 「为什么这个号不可选」的解释全靠这几个字段
                plan_kind: e.plan.as_ref().map(|p| p.kind.clone()),
                plan_kinds: e
                    .plan
                    .as_ref()
                    .map(|p| {
                        p.kinds
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
                plan_models: e
                    .plan
                    .as_ref()
                    .map(|p| {
                        p.models
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
                plan_period: e
                    .plan
                    .as_ref()
                    .map(|p| p.period.clone())
                    .filter(|s| !s.is_empty()),
                plan_night_only: e.plan.as_ref().map(|p| p.night_only).unwrap_or(false),
                plan_remaining_units: e.plan.as_ref().map(|p| p.remaining_units),
                plan_grant_units: e.plan.as_ref().map(|p| p.grant_units),
                plan_ends_at: e.plan.as_ref().and_then(|p| p.ends_at),
                plan_synced_at: e.plan.as_ref().map(|p| p.synced_at),
            })
            .collect();
        out.sort_by(|a, b| a.uid.cmp(&b.uid));
        out
    }

    pub fn count(&self) -> usize {
        safe_lock(&self.entries).len()
    }

    /// 启动回灌入口（do_start 三池 sync 装配完成后各调一次，P1）：
    /// 按池标签从 pool_cooldowns 表读回上进程的冷却运行态并 max 合并。
    /// 未装配 log_ctx/标签（测试或未启动）时静默跳过
    pub fn restore_cooldowns_from_store(&self) {
        let Some((dir, label)) = safe_lock(&self.log_ctx).clone() else { return };
        if label.is_empty() {
            return;
        }
        let store = crate::store::db(&dir);
        let snap = crate::store::docs::pool_cooldowns_load_pool(&store, &label);
        if snap.is_empty() {
            return;
        }
        self.restore_persisted_cooldowns(&snap);
    }

    /// 池内是否存在可选账号（healthy + 非零积分 + 未过期）。
    /// 统一调度选池健康预检用（§4.1 ④）；不含 Key 级白名单/专一约束——
    /// 那由各执行路径取号时自理，预检仅覆盖"池整体耗尽"场景
    pub fn has_selectable(&self) -> bool {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let tried = HashSet::new();
        let reserve = self.reserve_credits();
        entries.values().any(|e| selectable(e, &tried, now, reserve))
    }

    /// 同 [has_selectable]，附加 Key 级 uid 白名单过滤（issue #25 资源池绑定）：
    /// 绑定池 Key 的健康预检须感知白名单——池内仅剩白名单外账号时视为该池
    /// 对此 Key 不健康，走全局 fallback 开关回退另一池，而非取号阶段才失败。
    /// `allowed`：uid 白名单（None = 不限；`Some(空集)` = 过滤全部，即该池对此
    /// Key 不健康——issue #30 混合白名单的有意传参，语义同 pick_excluding_constrained）
    pub fn has_selectable_in(&self, allowed: Option<&HashSet<String>>) -> bool {
        self.selectable_stats_in(allowed).1 > 0
    }

    /// 区域过滤版健康预检（国内/海外号池隔离）：该区无可选账号即视为不健康，
    /// dispatch 据此跨池回退或显式报错（如海外模型 + 海外池全挂 → 如实
    /// no_healthy，而不是落到国内号被上游拒绝）。None = 不过滤（历史语义）。
    pub fn has_selectable_in_region(
        &self,
        allowed: Option<&HashSet<String>>,
        want_global: Option<bool>,
    ) -> bool {
        self.selectable_stats_in_region(allowed, want_global).1 > 0
    }

    /// 可选账号统计（诊断用，issue #29）：返回 (池内可选总数, 白名单内可选数)。
    /// `allowed` 语义同 has_selectable_in（None = 不限）；供 NoHealthy 错误
    /// 消息携带「池健康数 / 白名单命中数」，帮助用户自查 Key 约束配置
    pub fn selectable_stats_in(&self, allowed: Option<&HashSet<String>>) -> (usize, usize) {
        self.selectable_stats_in_region(allowed, None)
    }

    /// 区域过滤版统计：`want_global` = Some 时只统计该区账号。
    pub fn selectable_stats_in_region(
        &self,
        allowed: Option<&HashSet<String>>,
        want_global: Option<bool>,
    ) -> (usize, usize) {
        self.selectable_stats_in_region_excluding(allowed, want_global, None)
    }

    /// 统计时可排除一个账号（`exclude_uid`）：用于「该账号已命中冷却，池内是否
    /// 还有别的健康号可顶上」。模型级冷却表按裸模型名记账、不区分账号，若不排除
    /// 当前账号就判断，会把刚被限流的号自己算成「可用备选」，得出「模型仍健康」
    /// 的错误结论。
    pub fn selectable_stats_in_region_excluding(
        &self,
        allowed: Option<&HashSet<String>>,
        want_global: Option<bool>,
        exclude_uid: Option<&str>,
    ) -> (usize, usize) {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let tried = HashSet::new();
        let reserve = self.reserve_credits();
        let mut total = 0usize;
        let mut in_allowed = 0usize;
        for e in entries.values() {
            if Some(e.uid.as_str()) == exclude_uid {
                continue;
            }
            if selectable(e, &tried, now, reserve) {
                total += 1;
                if want_global.map_or(true, |w| e.global_region == w)
                    && allowed.map_or(true, |a| a.contains(&e.uid))
                {
                    in_allowed += 1;
                }
            }
        }
        (total, in_allowed)
    }

    /// 诊断：返回所有账号被过滤的原因（用于 "no healthy account" 排查）
    pub fn diagnose(&self) -> Vec<PoolDiagnosis> {
        let entries = safe_lock(&self.entries);
        let now = now_ts();
        let reserve = self.reserve_credits();
        entries
            .values()
            .map(|e| {
                let reason = if e.refresh_invalid {
                    "disabled(refresh_token_invalid)".to_string()
                } else if e.disabled {
                    "disabled(SessionDead)".to_string()
                } else if e.hard_until > 0 && now < e.hard_until {
                    format!("hard_credit(until_0400={}s)", e.hard_until - now)
                } else if e.until > 0 && now < e.until {
                    format!("cooldown(until={} remaining={}s)", e.until, e.until - now)
                } else if let Some(exp) = e.credits_expire_at {
                    if exp > 0 && exp < now {
                        "credits_expired".to_string()
                    } else if exp > 0 {
                        if let Some(c) = e.credits {
                            if c <= 0.0 {
                                "zero_credits".to_string()
                            } else if c <= reserve {
                                // 保留积分阈值过滤（issue #91）：陈旧快照
                                // fail-open 不生效（缓存问题），标注供排查
                                if e.reserve_filter_active(now) {
                                    format!("low_credits(reserve={})", reserve)
                                } else {
                                    "healthy(credits_stale_fail_open)".to_string()
                                }
                            } else {
                                "healthy".to_string()
                            }
                        } else {
                            "healthy(no_credits_info)".to_string()
                        }
                    } else {
                        "healthy(no_expiry)".to_string()
                    }
                } else {
                    "healthy(no_expiry_info)".to_string()
                };
                PoolDiagnosis {
                    uid: e.uid.clone(),
                    name: e.name.clone(),
                    disabled: e.disabled,
                    until: e.until,
                    credits: e.credits,
                    credits_expire_at: e.credits_expire_at,
                    reason,
                }
            })
            .collect()
    }
}

/// 账号池诊断信息
pub struct PoolDiagnosis {
    pub uid: String,
    pub name: String,
    pub disabled: bool,
    pub until: i64,
    pub credits: Option<f64>,
    pub credits_expire_at: Option<i64>,
    pub reason: String,
}

pub struct PickedAccount {
    pub uid: String,
    pub jwt: String,
    pub device_id: String,
    pub machine_id: String,
    // WorkBuddy 上游凭证头字段（T2.1；SOLO 账号为空串/false）
    pub domain: String,
    pub enterprise_id: String,
    pub global_region: bool,
    /// 绑定语义标记（审查 P1-4 TOCTOU 收紧）：true = 按既有语义无条件绑定
    /// （专一/粘性锁定、busy_fallback 降级——拒绝会破坏其锁定/兜底语义）；
    /// false = 路由侧绑定时经 CAS 复核「在途 < 并发上限」，已满则换下一候选
    pub over_limit: bool,
}

/// WorkBuddy 账号入池同步结构（T2.1）
pub struct WbSyncAccount {
    pub uid: String,
    pub name: String,
    pub token: String,
    pub domain: String,
    pub enterprise_id: String,
    pub global_region: bool,
    pub credits: Option<f64>,
    /// 最早积分包到期（Unix 秒，Buddy 不分包类型，issue #28）；None = 无到期约束
    pub credits_expire_at: Option<i64>,
    /// 积分快照采集时间（Unix 秒，issue #91 缓存保鲜）：条目级时间戳
    /// （WB credits_fetch 缓存回写）；None = 无时间信息，reserve 过滤不生效
    pub credits_fetched_at: Option<i64>,
    pub needs_relogin: bool,
    /// 所属 Buddy 分组 id（空 = 未分组）；池分组筛选在装配层按此过滤
    pub group_id: String,
}

/// Qoder 账号入池同步结构（p3-3）：uid 即 Cosy user_id（也是 token store 键）。
///
/// `access_token` 可能是会过期的 24h 作业令牌——池内 jwt 仅作快照展示，
/// 请求时的真凭证由网关 identity 回调按需解析（`ApiSharedState.qoder_identity`，
/// PAT 换令牌 / 刷新链路在闭包内走全防护），与 WB「池内即凭证」的模式不同。
pub struct QoderSyncAccount {
    pub uid: String,
    pub name: String,
    pub access_token: String,
    pub machine_id: String,
    pub needs_relogin: bool,
    /// 余额快照（issue #102 三池对齐）：qoder_credits 任务回写 QoderAccount.credits_balance；
    /// None = 无快照/旧数据，reserve 过滤 fail-open
    pub credits: Option<f64>,
    /// 最早积分包到期（Unix 秒，issue #102）：smart 排序键①「先到期先消耗」与
    /// selectable 到期过滤的数据源；None = 无到期信息
    pub credits_expire_at: Option<i64>,
    /// 余额快照采集时间（Unix 秒）：reserve 过滤新鲜度门槛（issue #91 口径）
    pub credits_fetched_at: Option<i64>,
    /// 账号所属区（2026-10-17 海外接入）：true=海外（api3.qoder.sh 网关 +
    /// openapi.qoder.sh）；池条目 global_region 由此盖章，选号分区过滤依据
    pub global_region: bool,
}

/// ZCode 账号入池同步结构（z.ai GLM 池）：uid = zcode.sqlite accounts.id 字符串化。
/// access_token/device_mid 为入池快照（请求期经 identity 回调重读最新值）。
///
/// `plan` 为套餐画像快照——`None` = 该账号从未同步过画像（fail-open 仍可入池），
/// `Some` 且模型集为空 = 上游明确判定无可用权益（不入池，避免撞 1113）。
pub struct ZCodeSyncAccount {
    pub uid: String,
    pub name: String,
    pub access_token: String,
    pub device_mid: String,
    pub plan: Option<PlanGate>,
}

/// 候选过滤：healthy + 未 tried + 积分未过期 + 非零积分 + 非 hard_credit 冷却
/// `reserve`：账号保留剩余积分阈值（issue #91，≥ 0；0 = 仅过滤零积分）
///
/// ZCode 套餐硬门闸在此处一并生效（模型无关的那一半）：画像已同步且模型集为空
/// = 上游说这号没任何权益，任何模型都不该选它。模型/时段维度在 `pick_zcode` 里判。
fn selectable(e: &PoolEntry, tried: &HashSet<String>, now: i64, reserve: f64) -> bool {
    if tried.contains(&e.uid) || !e.healthy(now) {
        return false;
    }
    if let Some(p) = &e.plan {
        if !p.has_entitlement() {
            return false;
        }
    }
    // 跳过积分已过期的（expire_time=0 视为无过期时间，不跳过）
    if let Some(exp) = e.credits_expire_at {
        if exp > 0 && exp < now {
            return false;
        }
    }
    // 跳过零通用积分账号（通用积分耗尽，llm_utils_chat 无法使用）；
    // 账号保留剩余积分（issue #91）：剩余 ≤ 阈值同样不调度——但仅当积分
    // 快照新鲜。陈旧快照 fail-open（缓存问题）：陈旧「高积分」照样会调度
    // 秃号，fail-closed 只会额外误杀；保鲜由启动刷新与 update_credits 拨新
    // （reserve=0 时等价原零积分过滤）
    if let Some(c) = e.credits {
        if c <= 0.0 || (c <= reserve && e.reserve_filter_active(now)) {
            return false;
        }
    }
    true
}

/// 账号在途数（F-77，快照查表；无记录 = 0）
fn inflight_of(e: &PoolEntry, inflight: &HashMap<String, u32>) -> u32 {
    inflight.get(&e.uid).copied().unwrap_or(0)
}

/// F-77 busy 过滤（纯函数）：inflight < limit 的账号才参与候选；
/// limit = 0 不限（返回原候选集，未降级）。过滤后为空（全部 busy）时
/// 返回 (原候选集, true)，调用方降级取 inflight 最小者（不过载拒绝）
fn busy_filter<'a>(
    cands: &[&'a PoolEntry],
    inflight: &HashMap<String, u32>,
    limit: u32,
) -> (Vec<&'a PoolEntry>, bool) {
    if limit == 0 {
        return (cands.to_vec(), false);
    }
    let idle: Vec<&PoolEntry> = cands
        .iter()
        .copied()
        .filter(|e| inflight_of(e, inflight) < limit)
        .collect();
    if idle.is_empty() {
        (cands.to_vec(), true)
    } else {
        (idle, false)
    }
}

/// ZCode 套餐门闸判定（模型无关 + 模型相关两半合并在这里）。
///
/// `plan: None`（画像从未同步）→ **fail-open 放行**，保持改造前的可用性；
/// 有画像则逐条判：有无权益 → 是否服务该模型 → Key 级套餐白名单 → 夜间窗口
/// → 到期 → 额度耗尽。
fn plan_gate_ok(
    e: &PoolEntry,
    model_lc: &str,
    plan_kinds: Option<&HashSet<String>>,
    now: i64,
    hour: u32,
) -> bool {
    let Some(p) = &e.plan else { return true };
    if !p.has_entitlement() {
        return false;
    }
    if !p.serves(model_lc) {
        return false;
    }
    if !p.kind_allowed(plan_kinds) {
        return false;
    }
    // 夜间档：只有夜间权益的账号在白天跳过（不冷却不计错，到点自然可用）
    if p.night_only && !crate::zcode::plan_profile::in_night_window(hour) {
        return false;
    }
    if let Some(end) = p.ends_at {
        if end > 0 && now >= end {
            return false;
        }
    }
    if p.grant_units > 0 && p.remaining_units <= 0 {
        return false;
    }
    true
}

/// 套餐燃烧顺序排序键：(形态权重, 到期时间)。越小越先烧。
/// 无画像账号归到最后一档（9），免得抢临时额度的顺位。
fn plan_order(e: &PoolEntry) -> (i64, i64) {
    match &e.plan {
        Some(p) => (p.burn_rank(), p.expiry_key()),
        None => (9, i64::MAX),
    }
}

/// 套餐优先挑选：先在「最该烧的那一档」内收窄，再交给池策略挑具体账号。
/// 档内仍保留池策略（expire_first/weighted/…）的均衡与随机性，不会把并发全压到一个号上。
fn pick_plan_first<'a>(
    cands: &[&'a PoolEntry],
    strategy: PoolStrategy,
    rand_seed: u64,
    now: i64,
    inflight: &HashMap<String, u32>,
) -> Option<&'a PoolEntry> {
    if cands.is_empty() {
        return None;
    }
    let best_key = cands.iter().map(|e| plan_order(e)).min()?;
    let tier: Vec<&'a PoolEntry> = cands
        .iter()
        .copied()
        .filter(|e| plan_order(e) == best_key)
        .collect();
    pick_by_strategy(&tier, strategy, rand_seed, now, inflight).or_else(|| tier.first().copied())
}

/// 按策略从候选集中挑选（纯函数，便于单测）；
/// `inflight` 为在途计数快照（F-77 负载因子：Weighted/P2C 偏向空闲账号）
fn pick_by_strategy<'a>(
    cands: &[&'a PoolEntry],
    strategy: PoolStrategy,
    rand_seed: u64,
    now: i64,
    inflight: &HashMap<String, u32>,
) -> Option<&'a PoolEntry> {
    if cands.is_empty() {
        return None;
    }
    match strategy {
        PoolStrategy::Random => Some(cands[(rand_seed as usize) % cands.len()]),
        PoolStrategy::Weighted => pick_weighted(cands, rand_seed, now, inflight),
        PoolStrategy::P2C => pick_p2c(cands, rand_seed, now, inflight),
        PoolStrategy::CreditFirst => cands.iter().copied().max_by(|a, b| {
            a.credits
                .unwrap_or(0.0)
                .partial_cmp(&b.credits.unwrap_or(0.0))
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        PoolStrategy::ExpireFirst => cands.iter().copied().min_by(|a, b| {
            // 有过期时间者优先（现状语义）→ 更早过期优先 → 平手积分多者优先
            let ha = a.credits_expire_at.map_or(false, |t| t > 0);
            let hb = b.credits_expire_at.map_or(false, |t| t > 0);
            hb.cmp(&ha)
                .then_with(|| {
                    a.credits_expire_at
                        .unwrap_or(0)
                        .cmp(&b.credits_expire_at.unwrap_or(0))
                })
                .then_with(|| {
                    b.credits
                        .unwrap_or(0.0)
                        .partial_cmp(&a.credits.unwrap_or(0.0))
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        }),
    }
}

/// 三因子加权随机（T2.2）：积分占比×10 + 闲置补偿（每小时+0.5 封顶 5.0，
/// 从未使用按满额）+ 成功率×3 → Top5 短名单内按得分二次加权随机；
/// F-77 负载因子：每个在途请求扣 4 分（空闲账号天然优先）
fn pick_weighted<'a>(
    cands: &[&'a PoolEntry],
    rand_seed: u64,
    now: i64,
    inflight: &HashMap<String, u32>,
) -> Option<&'a PoolEntry> {
    let total_credits: f64 = cands.iter().filter_map(|e| e.credits).sum();
    let mut scored: Vec<(&'a PoolEntry, f64)> = cands
        .iter()
        .map(|e| {
            let load_penalty = inflight_of(e, inflight) as f64 * 4.0;
            (*e, (e.weighted_score(total_credits, now) - load_penalty).max(0.1))
        })
        .collect();
    // 得分降序，取 Top5 短名单
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(5);
    // 短名单内按得分加权随机（最低权重 0.1 防零分账号永不出场）
    let weights: Vec<f64> = scored.iter().map(|(_, s)| s.max(0.1)).collect();
    let sum: f64 = weights.iter().sum();
    let mut point = (rand_seed % 10_000) as f64 / 10_000.0 * sum;
    for (e, w) in scored.iter().zip(weights.iter()) {
        point -= w;
        if point <= 0.0 {
            return Some(e.0);
        }
    }
    scored.last().map(|(e, _)| *e)
}

/// P2C：随机选二取优（T2.2 v1.2，antigravity-tools 实证延迟优于轮询/加权随机）
/// 「优」= 先比在途数（F-77 负载第一比较键，空闲者胜）再比三因子得分；仅一名候选时直接返回
fn pick_p2c<'a>(
    cands: &[&'a PoolEntry],
    rand_seed: u64,
    now: i64,
    inflight: &HashMap<String, u32>,
) -> Option<&'a PoolEntry> {
    if cands.len() == 1 {
        return Some(cands[0]);
    }
    let total_credits: f64 = cands.iter().filter_map(|e| e.credits).sum();
    let a = cands[(rand_seed as usize) % cands.len()];
    let b = cands[((rand_seed >> 32) as usize) % cands.len()];
    if a.uid == b.uid {
        // 撞号：退化为随机一个
        return Some(cands[(rand_seed as usize) % cands.len()]);
    }
    let ia = inflight_of(a, inflight);
    let ib = inflight_of(b, inflight);
    if ia != ib {
        return Some(if ia < ib { a } else { b });
    }
    let sa = a.weighted_score(total_credits, now);
    let sb = b.weighted_score(total_credits, now);
    Some(if sa >= sb { a } else { b })
}

/// 次日 04:00（本地东八区，与代码库其它处 local_ts 约定一致）的 Unix 秒
pub fn next_0400(now: i64) -> i64 {
    let local = now + 8 * 3600;
    let day_start = local - (local % 86400);
    let today_0400 = day_start + 4 * 3600;
    let target = if local < today_0400 { today_0400 } else { today_0400 + 86400 };
    target - 8 * 3600
}

/// 次日 00:00（东八区）的 Unix 秒：Qoder 免费模型额度重置时刻（上游文案
/// 「明天 00:00 再重试当前免费模型」），(uid, model) 冷却的到期时间
pub fn next_0000(now: i64) -> i64 {
    let local = now + 8 * 3600;
    let day_start = local - (local % 86400);
    day_start + 86400 - 8 * 3600
}

/// 当前 Unix 秒（池内统一时间源；wb_route 模型冷却快照复用）
pub fn now_ts() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Unix 秒 → 固定东八区时间展示（与 next_0400 的 local_ts 约定同源），
/// 供冷却日志 until 可读化；解析失败回退原始数值
pub(crate) fn fmt_local_ts(ts: i64) -> String {
    use chrono::TimeZone;
    chrono::FixedOffset::east_opt(8 * 3600)
        .and_then(|tz| tz.timestamp_opt(ts, 0).single())
        .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// 确定性派生 hex 字符串（与原 device_proxy.py 的 _seeded_stream 算法一致，accounts::derive_device 同源）
/// 用于从 uid 生成 machine_id，保证同一账号始终得到同一设备标识
pub(crate) fn seeded_hex(n: usize, seed: &str, salt: &str) -> String {
    let data = format!("{}:{}", salt, seed);
    let mut out = Vec::new();
    let mut i: u32 = 0;
    while out.len() < (n + 1) / 2 {
        let mut hasher = Sha256::new();
        hasher.update(data.as_bytes());
        hasher.update(i.to_be_bytes());
        out.extend_from_slice(&hasher.finalize());
        i += 1;
    }
    out.truncate((n + 1) / 2);
    out.iter().map(|b| format!("{:02x}", b)).collect::<String>()
        .chars().take(n).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acct(name: &str, uid: &str) -> crate::models::RawAccount {
        crate::models::RawAccount {
            name: name.to_string(),
            user_id: Some(uid.to_string()),
            jwt: String::new(),
            refresh_token: None,
            dc_id: None,
            added_at: None,
            updated_at: None,
            refresh_token_expires_at: None,
            refresh_token_fails: 0,
            refresh_token_invalid: false,
            auth_saved_at: None,
        }
    }

    /// 建池：accounts 为 (uid, credits, expire) 三元组，全部 enabled
    fn build_pool(entries: &[(&str, f64, i64)]) -> ApiPool {
        let pool = ApiPool::new();
        let accounts: Vec<crate::models::RawAccount> = entries
            .iter()
            .map(|(uid, _, _)| acct(uid, uid))
            .collect();
        let enabled: Vec<String> = entries.iter().map(|(uid, _, _)| uid.to_string()).collect();
        let mut credits = HashMap::new();
        let mut expires = HashMap::new();
        for (uid, c, e) in entries {
            credits.insert(uid.to_string(), *c);
            expires.insert(uid.to_string(), *e);
        }
        pool.sync_from_accounts(
            &accounts,
            &enabled,
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &credits,
            Some(now_ts()),
            &expires,
            &HashMap::new(),
        );
        pool
    }

    // ==================== F-80：冷却/禁用标记落 app.log ====================

    #[test]
    fn cooldown_event_logged_when_ctx_assembled() {
        let dir = std::env::temp_dir().join(format!("aiwa_f80_on_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        pool.set_log_ctx(&dir, "trae");
        pool.note_error("uid_a", ErrKind::Forbidden);
        let line = std::fs::read_to_string(dir.join("logs").join("app.log")).unwrap();
        assert!(
            line.contains(
                "account cooldown: pool=trae acct=uid_a uid=uid_a kind=Forbidden action=disabled"
            ),
            "实际日志: {line}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn cooldown_event_not_logged_without_ctx() {
        let dir = std::env::temp_dir().join(format!("aiwa_f80_off_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        // 未装配 log_ctx（测试/未启动网关）：note_error 不产生日志副作用
        pool.note_error("uid_a", ErrKind::Forbidden);
        assert!(
            !dir.join("logs").join("app.log").exists(),
            "未装配 log_ctx 不应产生日志文件"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ==================== P1：运行态冷却落库 ↔ 启动回灌 ====================

    /// 全链路：冷却 → flusher 快照落库 → 模拟重启（新建池 + sync + 回灌）→
    /// 冻结/熔断/限额/按模型冷却全部复活；过期条目不回灌。
    #[test]
    fn cooldown_snapshot_roundtrips_across_restart() {
        let dir = std::env::temp_dir().join(format!("aiwa_p1_rt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // 池 A：装配 log_ctx 后制造多种冷却
        let pool_a = build_pool(&[("uid_a", 0.0, 0)]);
        pool_a.set_log_ctx(&dir, "buddy");
        pool_a.note_error("uid_a", ErrKind::SoftRate); // until + rate_trips
        pool_a.note_error("uid_a", ErrKind::HardCredit); // hard_until（下个04:00）
        // 按模型冷却到未来时刻
        let future = now_ts() + 3600;
        pool_a.per_model_cooldown_until("uid_a", "hy4-preview", future);
        pool_a.flush_cooldown_snapshot();
        // 干净后重复 flush 应无副作用（CAS 消费）
        pool_a.flush_cooldown_snapshot();
        // 模拟重启：全新池实例（同目录同标签）
        let pool_b = build_pool(&[("uid_a", 10.0, 0)]);
        pool_b.set_log_ctx(&dir, "buddy");
        pool_b.restore_cooldowns_from_store();
        {
            let entries = safe_lock(&pool_b.entries);
            let e = entries.get("uid_a").unwrap();
            assert!(e.until > now_ts(), "软冷却应复活");
            assert_eq!(e.rate_trips, 1, "限额退避计数应复活");
            assert!(e.hard_until > now_ts(), "hard_credit 冷却应复活");
            assert_eq!(
                e.per_model_cooldowns.get("hy4-preview").copied(),
                Some(future),
                "按模型冷却应复活"
            );
        }
        // 过期条目不落库：冷却全部到期后 flush → 表内该池行清空 → 回灌无效果
        let pool_c = build_pool(&[("uid_a", 10.0, 0)]);
        pool_c.set_log_ctx(&dir, "buddy");
        pool_c.clear_cooldowns();
        pool_c.flush_cooldown_snapshot();
        let pool_d = build_pool(&[("uid_a", 10.0, 0)]);
        pool_d.set_log_ctx(&dir, "buddy");
        pool_d.restore_cooldowns_from_store();
        {
            let entries = safe_lock(&pool_d.entries);
            let e = entries.get("uid_a").unwrap();
            assert_eq!(e.until, 0, "清除后重灌不得复活");
            assert_eq!(e.hard_until, 0);
            assert_eq!(e.rate_trips, 0, "清除语义须落库（空快照剔除行）");
            assert!(e.per_model_cooldowns.is_empty());
        }
        // 手动清除冷却的账号不再被误恢复——正是 P1 与 clear_cooldowns 的配合点
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 区域隔离：pool_cooldowns 表按 "{pool}:{uid}" 记账，两池同 uid 不串扰
    #[test]
    fn cooldown_snapshot_isolated_per_pool_label() {
        let dir = std::env::temp_dir().join(format!("aiwa_p1_iso_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let trae = build_pool(&[("uid_x", 10.0, 0)]);
        trae.set_log_ctx(&dir, "trae");
        trae.note_error("uid_x", ErrKind::SoftRate);
        trae.flush_cooldown_snapshot();
        // 同 uid 的 buddy 池回灌：trae 的冷却绝不得跨池复活
        let buddy = build_pool(&[("uid_x", 10.0, 0)]);
        buddy.set_log_ctx(&dir, "buddy");
        buddy.restore_cooldowns_from_store();
        let entries = safe_lock(&buddy.entries);
        assert_eq!(entries.get("uid_x").unwrap().until, 0, "冷却不得跨池串扰");
        drop(entries);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn strategy_parse_defaults_to_expire_first() {
        assert_eq!(PoolStrategy::parse(""), PoolStrategy::ExpireFirst);
        assert_eq!(PoolStrategy::parse("unknown"), PoolStrategy::ExpireFirst);
        assert_eq!(PoolStrategy::parse("credit_first"), PoolStrategy::CreditFirst);
        assert_eq!(PoolStrategy::parse("random"), PoolStrategy::Random);
        assert_eq!(PoolStrategy::parse("weighted"), PoolStrategy::Weighted);
        assert_eq!(PoolStrategy::parse("p2c"), PoolStrategy::P2C);
        assert_eq!(PoolStrategy::default(), PoolStrategy::ExpireFirst);
    }

    #[test]
    fn has_selectable_in_respects_whitelist() {
        // issue #25 资源池绑定：健康预检的白名单感知行为
        let pool = build_pool(&[
            ("uid_a", 100.0, 4_000_001_000),
            ("uid_b", 100.0, 4_000_001_000),
        ]);
        // 无白名单：与 has_selectable 等价
        assert!(pool.has_selectable());
        assert!(pool.has_selectable_in(None));
        // 白名单命中任一健康账号即可
        let ok: HashSet<String> = ["uid_b".to_string()].into_iter().collect();
        assert!(pool.has_selectable_in(Some(&ok)));
        // 白名单全在池外 → 视为不健康（dispatch 层据此触发 fallback）
        let miss: HashSet<String> = ["uid_x".to_string()].into_iter().collect();
        assert!(!pool.has_selectable_in(Some(&miss)));
        // 空白名单 = 过滤所有账号：调用方须先过滤空集（与 pick_excluding_constrained 同语义）
        let empty: HashSet<String> = HashSet::new();
        assert!(!pool.has_selectable_in(Some(&empty)));
        // 过期账号即使命中白名单也不可选
        let expired = build_pool(&[("uid_a", 100.0, 1_000)]);
        let ok_a: HashSet<String> = ["uid_a".to_string()].into_iter().collect();
        assert!(!expired.has_selectable_in(Some(&ok_a)));
    }

    #[test]
    fn selectable_stats_in_counts_total_and_whitelisted() {
        // issue #29 修复2：NoHealthy 详情需要池健康计数（总数 + 白名单命中数）
        let pool = build_pool(&[
            ("uid_a", 100.0, 4_000_001_000),
            ("uid_b", 100.0, 4_000_001_000),
            ("uid_c", 0.0, 4_000_001_000), // 零积分 → 不可选
        ]);
        // 无白名单：(可选总数, 同值)
        assert_eq!(pool.selectable_stats_in(None), (2, 2));
        // 白名单命中 uid_b：总数不变，命中=1
        let ok: HashSet<String> = ["uid_b".to_string()].into_iter().collect();
        assert_eq!(pool.selectable_stats_in(Some(&ok)), (2, 1));
        // 白名单全在池外：命中=0
        let miss: HashSet<String> = ["uid_x".to_string()].into_iter().collect();
        assert_eq!(pool.selectable_stats_in(Some(&miss)), (2, 0));
    }

    #[test]
    fn reserve_credits_skips_low_balance_accounts() {
        // issue #91 账号保留剩余积分：剩余 ≤ 阈值的账号不参与取号与健康预检
        let pool = build_pool(&[
            ("uid_low", 30.0, 4_000_001_000),
            ("uid_ok", 100.0, 4_000_001_000),
        ]);
        // 阈值 0（默认）：零积分过滤语义，两账号均可选
        assert_eq!(pool.selectable_stats_in(None), (2, 2));
        pool.set_reserve_credits(50.0);
        // 低余额账号被过滤，仅剩 uid_ok；取号跳过 uid_low
        assert_eq!(pool.selectable_stats_in(None), (1, 1));
        let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
        assert_eq!(picked.uid, "uid_ok");
        assert!(pool.pick_by_uid("uid_low").is_none());
        // 智能调度数据源同步感知：积分总和不计保留阈值以下账号
        assert_eq!(pool.stats().1, 100.0);
        // 诊断输出 low_credits 原因（NoHealthy 排查）
        let low = pool.diagnose().into_iter().find(|d| d.uid == "uid_low").unwrap();
        assert!(low.reason.starts_with("low_credits"));
        // 阈值归零恢复可选
        pool.set_reserve_credits(0.0);
        assert_eq!(pool.selectable_stats_in(None), (2, 2));
    }

    #[test]
    fn reserve_credits_boundary_inclusive() {
        // 语义「低于或等于切换账号」：剩余 == 阈值即过滤
        let pool = build_pool(&[("uid_edge", 50.0, 4_000_001_000)]);
        pool.set_reserve_credits(50.0);
        assert!(!pool.has_selectable());
        // 高于阈值 0.01 即可选
        let pool2 = build_pool(&[("uid_edge", 50.01, 4_000_001_000)]);
        pool2.set_reserve_credits(50.0);
        assert!(pool2.has_selectable());
    }

    #[test]
    fn reserve_credits_stale_snapshot_fail_open() {
        // issue #91 缓存问题：reserve 过滤仅对新鲜积分快照生效。
        // 陈旧快照 fail-open（不误杀），保鲜靠启动刷新与 update_credits 拨新
        let pool = build_pool(&[("uid_low", 30.0, 4_000_001_000)]);
        pool.set_reserve_credits(50.0);
        // 新鲜快照：低余额被过滤
        assert_eq!(pool.selectable_stats_in(None), (0, 0));
        assert!(pool.has_stale_credits() == false, "build_pool 拨新后不应判陈旧");

        // 快照陈旧（>24h）：fail-open 恢复可选；stats 口径同步（计入 total）
        let now = now_ts();
        {
            let mut entries = safe_lock(&pool.entries);
            for e in entries.values_mut() {
                e.credits_fetched_at = Some(now - RESERVE_STALE_SECS - 60);
            }
        }
        assert!(pool.has_stale_credits());
        assert_eq!(pool.selectable_stats_in(None), (1, 1));
        assert!(pool.has_selectable());
        assert_eq!(pool.stats().1, 30.0);
        // 诊断标注陈旧 fail-open（NoHealthy 排查）
        let low = pool.diagnose().into_iter().find(|d| d.uid == "uid_low").unwrap();
        assert!(
            low.reason.contains("credits_stale"),
            "reason={}",
            low.reason
        );

        // 无时间戳（升级前旧数据）同样 fail-open
        {
            let mut entries = safe_lock(&pool.entries);
            for e in entries.values_mut() {
                e.credits_fetched_at = None;
            }
        }
        assert_eq!(pool.selectable_stats_in(None), (1, 1));

        // update_credits 拨新时间戳：过滤自动恢复
        pool.update_credits(&[("uid_low".to_string(), 30.0)].into_iter().collect());
        assert!(!pool.has_stale_credits());
        assert_eq!(pool.selectable_stats_in(None), (0, 0));
    }

    #[test]
    fn resolve_wb_follows_trae_when_wb_empty() {
        // Buddy 池 wb_strategy 空 = 跟随 Trae 池策略（启动/热应用共用语义）
        assert_eq!(PoolStrategy::resolve_wb("weighted", ""), PoolStrategy::Weighted);
        assert_eq!(PoolStrategy::resolve_wb("p2c", ""), PoolStrategy::P2C);
        // Trae 池也为空/未知 → 默认 expire_first
        assert_eq!(PoolStrategy::resolve_wb("", ""), PoolStrategy::ExpireFirst);
        assert_eq!(PoolStrategy::resolve_wb("unknown", ""), PoolStrategy::ExpireFirst);
    }

    #[test]
    fn resolve_wb_prefers_explicit_wb() {
        // Buddy 池显式配置优先于 Trae 池（含 Trae 为空串时仍生效）
        assert_eq!(PoolStrategy::resolve_wb("expire_first", "p2c"), PoolStrategy::P2C);
        assert_eq!(PoolStrategy::resolve_wb("", "weighted"), PoolStrategy::Weighted);
        // 显式未知值回退默认（与 parse 语义一致）
        assert_eq!(PoolStrategy::resolve_wb("weighted", "unknown"), PoolStrategy::ExpireFirst);
    }

    #[test]
    fn expire_first_prefers_soonest_expiry() {
        // 两个都有过期时间（均在未来）：更早过期者胜
        let pool = build_pool(&[
            ("uid_a", 100.0, 4_000_001_000),
            ("uid_b", 100.0, 3_900_000_000),
        ]);
        pool.set_strategy(PoolStrategy::ExpireFirst);
        let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
        assert_eq!(picked.uid, "uid_b");
        // 排除后取另一个
        let mut tried = HashSet::new();
        tried.insert("uid_b".to_string());
        let picked = pool.pick_excluding_constrained(&tried, None, None).unwrap();
        assert_eq!(picked.uid, "uid_a");
    }

    #[test]
    fn expire_first_prefers_has_expiry_and_more_credits_on_tie() {
        // 有过期时间者优先于无过期时间
        let pool = build_pool(&[("uid_a", 999.0, 0), ("uid_b", 10.0, 4_000_000_000)]);
        pool.set_strategy(PoolStrategy::ExpireFirst);
        assert_eq!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid, "uid_b");
        // 过期时间相同：积分多者胜
        let pool = build_pool(&[
            ("uid_a", 50.0, 4_000_000_000),
            ("uid_b", 200.0, 4_000_000_000),
        ]);
        assert_eq!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid, "uid_b");
    }

    #[test]
    fn credit_first_prefers_more_credits() {
        let pool = build_pool(&[
            ("uid_a", 50.0, 3_900_000_000),
            ("uid_b", 300.0, 4_000_000_000),
        ]);
        pool.set_strategy(PoolStrategy::CreditFirst);
        let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
        assert_eq!(picked.uid, "uid_b");
    }

    #[test]
    fn random_strategy_only_picks_candidates() {
        let pool = build_pool(&[
            ("uid_a", 50.0, 3_900_000_000),
            ("uid_b", 300.0, 4_000_000_000),
        ]);
        pool.set_strategy(PoolStrategy::Random);
        // 连续取号（排除已选）能遍历完所有候选后枯竭
        let mut tried = HashSet::new();
        for _ in 0..2 {
            let p = pool.pick_excluding_constrained(&tried, None, None).unwrap();
            tried.insert(p.uid);
        }
        assert!(pool.pick_excluding_constrained(&tried, None, None).is_none());
    }

    #[test]
    fn zero_credit_and_expired_are_skipped() {
        // 零积分账号不参与取号
        let pool = build_pool(&[("uid_a", 0.0, 0), ("uid_b", 10.0, 0)]);
        assert_eq!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid, "uid_b");
        // 积分已过期账号不参与取号（now 之后才会过期的不受影响）
        let pool = build_pool(&[("uid_a", 10.0, 1), ("uid_b", 10.0, 0)]);
        assert_eq!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid, "uid_b");
    }

    #[test]
    fn group_filter_excludes_unselected_groups() {
        let pool = ApiPool::new();
        let accounts = vec![acct("A", "uid_a"), acct("B", "uid_b"), acct("C", "uid_c")];
        let enabled: Vec<String> = vec!["uid_a".into(), "uid_b".into(), "uid_c".into()];
        let mut membership = HashMap::new();
        membership.insert("uid_a".to_string(), "g1".to_string());
        membership.insert("uid_b".to_string(), "g2".to_string());
        // uid_c 未分组
        pool.sync_from_accounts(
            &accounts,
            &enabled,
            &["g2".to_string()],
            &membership,
            &HashMap::new(),
            &HashMap::new(),
            None,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(pool.count(), 1);
        let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
        assert_eq!(picked.uid, "uid_b");
    }

    #[test]
    fn empty_group_filter_keeps_all() {
        let pool = ApiPool::new();
        let accounts = vec![acct("A", "uid_a"), acct("B", "uid_b")];
        let enabled: Vec<String> = vec!["uid_a".into(), "uid_b".into()];
        let mut membership = HashMap::new();
        membership.insert("uid_a".to_string(), "g1".to_string());
        pool.sync_from_accounts(
            &accounts,
            &enabled,
            &[], // 空 = 不限分组（旧配置默认行为）
            &membership,
            &HashMap::new(),
            &HashMap::new(),
            None,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(pool.count(), 2);
    }

    // ==================== F-78 批次 3 refresh_token 失效联动 ====================

    #[test]
    fn refresh_invalid_disables_on_sync() {
        // refresh_token_invalid=true 的账号同步即禁用，不参与取号
        let pool = ApiPool::new();
        let mut a_invalid = acct("A", "uid_a");
        a_invalid.refresh_token_invalid = true;
        let accounts = vec![a_invalid, acct("B", "uid_b")];
        let enabled: Vec<String> = vec!["uid_a".into(), "uid_b".into()];
        let mut credits = HashMap::new();
        credits.insert("uid_a".to_string(), 100.0);
        credits.insert("uid_b".to_string(), 10.0);
        pool.sync_from_accounts(
            &accounts,
            &enabled,
            &[],
            &HashMap::new(),
            &HashMap::new(),
            &credits,
            Some(now_ts()),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(pool.count(), 2);
        let st: Vec<_> = pool.status_list();
        let sa = st.iter().find(|s| s.uid == "uid_a").unwrap();
        assert!(sa.disabled && sa.refresh_invalid);
        assert_eq!(sa.state, "Forbidden");
        // 取号只落到 uid_b
        assert_eq!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid, "uid_b");
        // diagnose 给出 refresh_token_invalid 原因
        let d = pool.diagnose().into_iter().find(|x| x.uid == "uid_a").unwrap();
        assert_eq!(d.reason, "disabled(refresh_token_invalid)");
    }

    #[test]
    fn refresh_invalid_runtime_note_and_recover() {
        // 运行时联动：note_refresh_invalid 禁用 → 恢复路径 = 凭据变更联动热重载
        // reload_pools_if_running → sync_from_accounts 全量重建（commands/api_server.rs），
        // 原单点回填 note_refresh_success 已被其取代并移除
        let pool = build_pool(&[("uid_a", 100.0, 0)]);
        pool.note_refresh_invalid("uid_a");
        let st = &pool.status_list()[0];
        assert!(st.disabled && st.refresh_invalid);
        assert!(pool.pick_excluding_constrained(&HashSet::new(), None, None).is_none());
    }

    // ==================== T2.2 新增 ====================

    #[test]
    fn weighted_prefers_more_credits_and_idle() {
        // 同成功率基线（无历史 0.5）+ 无闲置差（last_used=0 满额）→ 积分多者得分高
        let pool = build_pool(&[
            ("uid_a", 10.0, 0),
            ("uid_b", 900.0, 0),
        ]);
        pool.set_strategy(PoolStrategy::Weighted);
        // Top5 短名单只有两名，uid_b 得分显著更高；多次取样必然覆盖 uid_b
        let mut seen_b = false;
        for i in 0..32u64 {
            let _ = i;
            if pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid == "uid_b" {
                seen_b = true;
                break;
            }
        }
        assert!(seen_b, "weighted 策略应能取到高分账号 uid_b");
        // 闲置补偿：uid_a 刚被用过、uid_b 闲置 3h → uid_b 得分更高
        // 直接对 pick_weighted 纯函数做断言（不依赖随机落点）
        let e_a = crate::api_server::pool::PoolEntry {
            uid: "a".into(), name: String::new(), jwt: String::new(),
            credits: Some(100.0), credits_expire_at: None,
            credits_fetched_at: Some(now_ts()), disabled: false,
            err_count: 0, until: 0, reason: String::new(),
            device_id: String::new(), machine_id: String::new(),
            hard_until: 0, last_used: 1000, successes: 0, failures: 0, cb_trips: 0,
            rate_trips: 0,
            per_model_cooldowns: std::collections::HashMap::new(),
            domain: String::new(), enterprise_id: String::new(), global_region: false,
            refresh_invalid: false,
            plan: None,
        };
        let e_b = crate::api_server::pool::PoolEntry {
            last_used: 1000 - 3 * 3600, // 闲置 3 小时
            ..crate::api_server::pool::PoolEntry {
                uid: "b".into(), name: String::new(), jwt: String::new(),
                credits: Some(100.0), credits_expire_at: None,
                credits_fetched_at: Some(now_ts()), disabled: false,
                err_count: 0, until: 0, reason: String::new(),
                device_id: String::new(), machine_id: String::new(),
                hard_until: 0, last_used: 0, successes: 0, failures: 0, cb_trips: 0,
                rate_trips: 0,
                per_model_cooldowns: std::collections::HashMap::new(),
                domain: String::new(), enterprise_id: String::new(), global_region: false,
                refresh_invalid: false,
                plan: None,
            }
        };
        let now = 1000 + 60;
        let sa = e_a.weighted_score(200.0, now);
        let sb = e_b.weighted_score(200.0, now);
        assert!(sb > sa, "闲置 3h 的账号得分应高于刚用过的账号（{} vs {}）", sb, sa);
    }

    #[test]
    fn p2c_picks_better_of_two() {
        // 纯函数语义：随机选二取优（得分高者胜）；两候选相同时退化为随机
        let e_a = test_entry("a", 10.0, 1000);
        let e_b = test_entry("b", 900.0, 1000);
        let cands = vec![&e_a, &e_b];
        let now = 2000;
        // 两候选不同时（奇数种子 → 索引 (1,0)），恒选得分更高的 b
        for seed in (1..200u64).step_by(2) {
            assert_eq!(pick_p2c(&cands, seed, now, &HashMap::new()).unwrap().uid, "b");
        }
        // 撞号（两索引相同）→ 不 panic，返回任一候选
        for seed in (0..200u64).step_by(2) {
            let picked = pick_p2c(&cands, seed, now, &HashMap::new()).unwrap();
            assert!(picked.uid == "a" || picked.uid == "b");
        }
        // 单候选：直接返回
        let single = vec![&e_b];
        assert_eq!(pick_p2c(&single, 7, now, &HashMap::new()).unwrap().uid, "b");
    }

    /// 测试用 QoderSyncAccount 快速构造
    fn acct_q(uid: &str, name: &str, token: &str, global: bool) -> crate::api_server::pool::QoderSyncAccount {
        crate::api_server::pool::QoderSyncAccount {
            uid: uid.to_string(),
            name: name.to_string(),
            access_token: token.to_string(),
            machine_id: String::new(),
            credits: Some(100.0),
            credits_expire_at: None,
            credits_fetched_at: Some(now_ts()),
            needs_relogin: false,
            global_region: global,
        }
    }

    /// 测试用 PoolEntry 快速构造
    fn test_entry(uid: &str, credits: f64, last_used: i64) -> PoolEntry {
        PoolEntry {
            uid: uid.to_string(),
            name: String::new(),
            jwt: String::new(),
            credits: Some(credits),
            credits_expire_at: None,
            credits_fetched_at: Some(now_ts()),
            disabled: false,
            err_count: 0,
            until: 0,
            reason: String::new(),
            device_id: String::new(),
            machine_id: String::new(),
            hard_until: 0,
            last_used,
            successes: 0,
            failures: 0,
            cb_trips: 0,
            rate_trips: 0,
            per_model_cooldowns: std::collections::HashMap::new(),
            domain: String::new(),
            enterprise_id: String::new(),
            global_region: false,
            refresh_invalid: false,
            plan: None,
        }
    }

    #[test]
    fn hard_credit_cools_until_next_0400() {
        // 余额为 0 才是「真·积分耗尽」：2026-10-20 起有余额的号会降级为短冷却，
        // 整号 04:00 冻结只保留给零余额账号（原用 5 积分账号，语义已变更）
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 0.0, 0)]);
        // 强制注入：模拟积分耗尽（hard_credit 由上游错误触发，不走 selectable 的零积分捷径）
        pool.note_error("uid_b", ErrKind::HardCredit);
        // uid_b 进入 QuotaProtection
        let st = pool.status_list().into_iter().find(|s| s.uid == "uid_b").unwrap();
        assert_eq!(st.state, "QuotaProtection");
        // diagnose 给出 hard_credit 原因
        let d = pool.diagnose().into_iter().find(|x| x.uid == "uid_b").unwrap();
        assert!(d.reason.starts_with("hard_credit"));
        // 恢复探测：把 hard_until 手动置为过去 → 重新可选
        {
            let mut entries = safe_lock(&pool.entries);
            if let Some(e) = entries.get_mut("uid_b") {
                e.hard_until = now_ts() - 1;
            }
        }
        let st = pool.status_list().into_iter().find(|s| s.uid == "uid_b").unwrap();
        assert_eq!(st.state, "Available");
    }

    /// 余额兜底（2026-10-20 事故修复）：号上积分仍 > 0 时不得整号冻结到 04:00——
    /// 四个 700/1000 积分的国内号因一条额度文案齐冻一天，客户端持续「重新连接」。
    /// 降级为普通冷却（不设 hard_until），账号很快回到可调度集合。
    #[test]
    fn hard_credit_with_positive_balance_downgrades() {
        let pool = build_pool(&[("uid_a", 0.0, 0), ("uid_b", 700.0, 0)]);
        pool.note_error("uid_b", ErrKind::HardCredit);
        let status = pool.status_list().into_iter().find(|s| s.uid == "uid_b").unwrap();
        assert!(!status.hard_credit, "有余额的号不应进入 04:00 硬冻结");
        assert_eq!(
            status.cooldown_reason.as_deref(),
            Some("hard_credit_downgraded")
        );
        assert!(status.cooldown_until.is_some(), "降级后仍应有短冷却");
        // 零余额号仍走原口径（真·积分耗尽才冻结到 04:00）
        pool.note_error("uid_a", ErrKind::HardCredit);
        let zero = pool.status_list().into_iter().find(|s| s.uid == "uid_a").unwrap();
        assert!(zero.hard_credit, "零余额号应保持 hard_credit 冻结");
    }

    #[test]
    fn forbidden_disables_account() {
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.note_error("uid_b", ErrKind::Forbidden);
        assert_eq!(pool.status_list()[1].state, "Forbidden");
        assert!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid != "uid_b");
    }

    #[test]
    fn server_circuit_breaker_backs_off_exponentially() {
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        // 三次连续 Server 错误触发第一次熔断（30m）
        for _ in 0..3 {
            pool.note_error("uid_a", ErrKind::Server);
        }
        let s1 = pool.status_list()[0].cooldown_until.unwrap() - now_ts();
        assert!((1799..=1801).contains(&s1), "首次熔断应约 30m，实际 {}s", s1);
        // 手动解除后再触发第二次 → 指数递增到 1h
        pool.clear_cooldowns();
        // clear_cooldowns 重置 until 但 cb_trips 保留（指数递增的记忆）
        {
            let mut entries = safe_lock(&pool.entries);
            entries.get_mut("uid_a").unwrap().cb_trips = 1;
        }
        for _ in 0..3 {
            pool.note_error("uid_a", ErrKind::Server);
        }
        let s2 = pool.status_list()[0].cooldown_until.unwrap() - now_ts();
        assert!((3599..=3601).contains(&s2), "第二次熔断应约 1h，实际 {}s", s2);
        // 成功重置熔断
        pool.clear_cooldowns();
        pool.note_success("uid_a");
        {
            let entries = safe_lock(&pool.entries);
            assert_eq!(entries.get("uid_a").unwrap().cb_trips, 0);
        }
    }

    // ==================== 6004 日限额：账号级冻结（模型级不受累） ====================

    #[test]
    fn cooldown_until_freezes_only_that_account() {
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        let reset = now_ts() + 19 * 3600;
        pool.cooldown_until("uid_a", reset);
        let st = pool.status_list();
        let a = st.iter().find(|s| s.uid == "uid_a").unwrap();
        let b = st.iter().find(|s| s.uid == "uid_b").unwrap();
        assert!(a.cooling, "被限额账号应进入冷却");
        assert_eq!(a.cooldown_until, Some(reset), "冷却截止须为上游恢复时刻");
        assert_eq!(a.cooldown_reason.as_deref(), Some("quota_reset"));
        assert!(!b.cooling, "同池其他账号不受影响");
        // 取号：uid_a 被冻，池内仍可选到 uid_b（这正是「自动换另一个账号」）
        assert!(pool.pick_by_uid("uid_a").is_none(), "被冻账号不可取号");
        assert!(pool.pick_by_uid("uid_b").is_some(), "另一账号应顶上");
        assert!(pool.has_selectable());
    }

    /// 回归（2026-10-18 海外池实测）：模型级冷却表按裸模型名记账、不区分账号，
    /// 一个号撞 429 就会把整个模型判死。判断「是否还有别的号能顶上」时必须排除
    /// 当前命中的账号——否则刚被限流的号会把自己算成可用备选，得出「模型仍健康」
    /// 的错误结论，进而继续把请求打给同一批号。
    /// 回归（2026-10-18 用户实测 6004）：上游「可在其他模型继续使用」语义 =
    /// 冷却必须按 (uid, model) 粒度，账号在该模型被限不影响其他模型调度。
    #[test]
    fn per_model_cooldown_blocks_only_that_model() {
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        let until = now_ts() + 2;
        pool.per_model_cooldown_until("uid_a", "step-5-preview", until);
        // step-5 视角：该号在冷却名单里（路由会并入 tried 排除它）
        assert_eq!(pool.model_cooled_uids("step-5-preview"), vec!["uid_a".to_string()]);
        // deepseek 视角：完全不受影响（积分模型照常可用——用户核心诉求）
        assert!(pool.model_cooled_uids("deepseek-v4.1-flash").is_empty());
        // 整账号 healthy 不受按模型冷却影响
        assert!(pool.has_selectable(), "其他模型不应被整号冻结波及");
        // 到期自动剔除
        std::thread::sleep(std::time::Duration::from_millis(2100));
        assert!(pool.model_cooled_uids("step-5-preview").is_empty(), "到期后必须恢复");
        // 只增不减：更近的过去时刻不缩短；clear 全清
        pool.per_model_cooldown_until("uid_a", "step-5-preview", now_ts() + 3600);
        pool.per_model_cooldown_until("uid_a", "step-5-preview", now_ts() + 60);
        assert_eq!(pool.model_cooled_uids("step-5-preview"), vec!["uid_a".to_string()]);
        pool.clear_model_cooldowns("uid_a");
        assert!(pool.model_cooled_uids("step-5-preview").is_empty());
    }

    /// 回归（2026-10-18 用户关切）：「冻结」必须是**有期限**的，绝不能变成永久封禁。
    /// 覆盖三条恢复路径：① 精确重置时刻到期 ② SoftRate 递增冷却到期 ③ 手动清除。
    /// 到期判断是纯时间比较（`healthy` 里 `now < until`），不存在把账号永久踢出的状态；
    /// 但必须同时确认「解冻后可被重新选中」以及「计数器不会把下次冷却直接顶满」。
    #[test]
    fn frozen_account_recovers_after_every_cooldown_path() {
        // ① cooldown_until 精确到期即恢复（模拟上游给的 03:23:29 到了）
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        pool.cooldown_until("uid_a", now_ts() + 2);
        assert!(!pool.has_selectable(), "冻结期内不可选");
        std::thread::sleep(std::time::Duration::from_millis(2100));
        assert!(pool.has_selectable(), "重置时刻到期后必须重新可用");

        // ② SoftRate 递增冷却到期后恢复，且计数在成功后退回基准
        let pool2 = build_pool(&[("uid_b", 10.0, 0)]);
        for _ in 0..3 {
            pool2.note_error("uid_b", ErrKind::SoftRate);
        }
        // 冷却确实很长（递增生效），但仍是有限值，不是永封
        let st = pool2.status_list();
        let e = st.iter().find(|s| s.uid == "uid_b").unwrap();
        let left = e.cooldown_until.unwrap() - now_ts();
        assert!(left > 0 && left <= 30 * 60, "冷却须有限且在 30 分钟上限内：{left}s");

        // ③ 手动清除冷却后立即可用，且连续计数归零（下次限频不会直接顶满）
        let cleared = pool2.clear_cooldowns();
        assert!(cleared >= 1);
        assert!(pool2.has_selectable(), "手动清除后立即可用");
        pool2.note_error("uid_b", ErrKind::SoftRate); // 模拟成功后又撞一次
        let st = pool2.status_list();
        let e = st.iter().find(|s| s.uid == "uid_b").unwrap();
        assert!(
            e.cooldown_until.unwrap() - now_ts() <= 60,
            "清除后计数应归零，下次限频回到 60s 基准"
        );
    }

    /// 回归（2026-10-19 用户实测 BUG）：池内仅剩一个账号时，不能因为「该模型
    /// 冷却中」就判无可用账号——那等于把账号停了。has_other_selectable_for_model
    /// 排除当前账号后为 false，正是「必须放行」的信号。
    #[test]
    fn single_account_not_blocked_by_its_own_model_cooldown() {
        let pool = build_pool(&[("only_one", 10.0, 0)]);
        // 该账号在某模型上被上游限流（6004「可切换其他模型继续」）
        pool.per_model_cooldown_until("only_one", "hy4-preview", now_ts() + 3600);
        assert!(pool.is_model_cooling("only_one", "hy4-preview"));
        // 关键：排除自己之后没有备选 → has_other_selectable_for_model = false
        // 调用方据此放行，而不是返回 no_healthy_account
        assert!(
            !pool.has_other_selectable_for_model("only_one", None, None),
            "池内仅一个账号时，不得把它自己的模型冷却当成「无可用账号」"
        );
        // 账号整体仍然可选（其他模型照常可用）
        assert!(pool.pick_by_uid("only_one").is_some(), "账号本身必须保持可选");
        // 多账号场景：有备选 → true（此时换号是正确的）
        let pool2 = build_pool(&[("a", 10.0, 0), ("b", 10.0, 0)]);
        pool2.per_model_cooldown_until("a", "hy4-preview", now_ts() + 3600);
        assert!(
            pool2.has_other_selectable_for_model("a", None, None),
            "池内有其他账号时应换号"
        );
        assert!(pool2.pick_by_uid("b").is_some());
    }

    /// 回归（2026-10-19）：sync_from_* 增量合并不丢运行态。旧版全量重建会把
    /// HardCredit/cb_trips/rate_trips/per_model_cooldowns 清零，导致冷却被静默
    /// 丢弃、账号提前解冻并反复打上游。
    #[test]
    fn sync_preserves_runtime_cooldown_state() {
        let pool = ApiPool::new();
        // 首次入池
        pool.sync_from_qoder(
            &[acct_q("qd_a", "A", "tok", true), acct_q("qd_b", "B", "tok", true)],
            &["qd_a".into(), "qd_b".into()],
        );
        // 制造运行态：qd_a 积分耗尽 + 某模型被限流 + 连续限频 2 次
        pool.note_error("qd_a", ErrKind::HardCredit);
        pool.per_model_cooldown_until("qd_a", "qwen3.8-flash", now_ts() + 7200);
        pool.note_error("qd_a", ErrKind::SoftRate);
        pool.note_error("qd_a", ErrKind::SoftRate);
        // 触发一次同步（模拟账号导入/OAuth/改名后的 reload_pools_if_running）
        pool.sync_from_qoder(
            &[acct_q("qd_a", "A-renamed", "tok2", true), acct_q("qd_b", "B", "tok", true)],
            &["qd_a".into(), "qd_b".into()],
        );
        let st = pool.status_list();
        let a = st.iter().find(|s| s.uid == "qd_a").unwrap();
        assert_eq!(a.name, "A-renamed", "快照字段应更新");
        // HardCredit 冷却必须跨同步保留：表现是该账号在 04:00 前取不到号
        assert!(
            pool.pick_by_uid("qd_a").is_none(),
            "HardCredit 冷却必须跨 sync 保留（否则账号提前解冻、反复打上游）"
        );
        // 同池另一个账号不受影响，照常可选
        assert!(pool.pick_by_uid("qd_b").is_some(), "其他账号不得被牵连");
        assert!(pool.is_model_cooling("qd_a", "qwen3.8-flash"), "按模型冷却必须保留");
    }

    /// 回归（2026-10-18）：日额度耗尽但文案解析不出重置时刻时，账号会退化成
    /// SoftRate 60s。若每次撞完就 60s 解冻、又被选中，就是无限循环（每轮还浪费
    /// 一次真实上游往返）。连续命中须递增冷却，成功才清零。
    #[test]
    fn soft_rate_cooldown_escalates_and_resets_on_success() {
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        let mut prev = 0i64;
        for round in 1..=4 {
            pool.note_error("uid_a", ErrKind::SoftRate);
            let st = pool.status_list();
            let e = st.iter().find(|s| s.uid == "uid_a").unwrap();
            let left = e.cooldown_until.unwrap() - now_ts();
            assert!(
                left > prev,
                "第 {round} 次连续限频的冷却应长于上一次（{left}s vs {prev}s）"
            );
            prev = left;
            assert!(e.cooling, "冷却窗口内不可被选中");
        }
        // 成功一次即清零：下一次限频回到 60s 基准
        pool.note_success("uid_a");
        pool.note_error("uid_a", ErrKind::SoftRate);
        let st = pool.status_list();
        let e = st.iter().find(|s| s.uid == "uid_a").unwrap();
        assert!(
            e.cooldown_until.unwrap() - now_ts() <= 60,
            "成功后计数须清零，冷却回到基准 60s"
        );
    }

    /// 回归（P0-1，2026-10-19）：请求级错误（ErrKind::None，如 4001 模型配置
    /// 缺失；ErrKind::Client，如 context_too_long）与账号健康无关。此前 5 处
    /// 调用点缺守卫，同一模型错误让全池每号各计 3 次 → 全部连坐熔断 30 分钟。
    /// 守卫下沉到 note_error 入口后，直传 None/Client 也必须无痕。
    #[test]
    fn request_level_errors_never_touch_account_health() {
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        for _ in 0..5 {
            pool.note_error("uid_a", ErrKind::None);
        }
        let st = pool.status_list();
        let e = st.iter().find(|s| s.uid == "uid_a").unwrap();
        assert_eq!(e.err_count, 0, "None 不得累计连续错误数");
        assert!(!e.cooling, "None 不得触发冷却/熔断");
        assert!(!e.disabled, "None 不得禁用账号");
        assert_eq!(
            pool.selectable_stats_in(None).1,
            1,
            "账号必须仍可被选中"
        );
        // 对照：Client（未知 4xx）保留原熔断记忆语义，仍计入 err_count
        pool.note_error("uid_a", ErrKind::Client);
        assert_eq!(pool.status_list()[0].err_count, 1, "Client 仍应计入连续错误");
    }

    /// 回归（P2-7，2026-10-19）：手动清除冷却即用户宣告「已恢复」，
    /// cb_trips 必须同步清零——否则清除后下次撞墙直接 1h 起步（30m×2^1），
    /// 与「清除后可用」预期相悖。
    #[test]
    fn clear_cooldowns_resets_circuit_breaker_memory() {
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        // 3 次连续 Server 错误触发首个熔断（30m，cb_trips → 1）
        for _ in 0..3 {
            pool.note_error("uid_a", ErrKind::Server);
        }
        let e = pool.status_list().remove(0);
        let first_span = e.cooldown_until.unwrap() - now_ts();
        assert!((29 * 60..=31 * 60).contains(&first_span), "首熔应为 30m，实测 {first_span}s");
        pool.clear_cooldowns();
        // 清除后再撞满 3 次：必须回到 30m 基准而非 60m
        for _ in 0..3 {
            pool.note_error("uid_a", ErrKind::Server);
        }
        let e = pool.status_list().remove(0);
        let second_span = e.cooldown_until.unwrap() - now_ts();
        assert!(
            (29 * 60..=31 * 60).contains(&second_span),
            "清除冷却后熔断应重置为 30m 基准，实测 {second_span}s（若 ≈3600 即 cb_trips 未清）"
        );
    }

    /// 回归（P2-6，2026-10-19）：successes/failures 单调累计会让历史稀释
    /// 近期表现（100 败 + 100 成 = 0.5 永远抬不起来）。窗口衰减后，
    /// 恢复期的连串成功应把 success_rate 拉回高位。
    #[test]
    fn success_fail_counters_use_rolling_window() {
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        // 先制造大量失败
        for _ in 0..60 {
            pool.note_error("uid_a", ErrKind::Server);
        }
        // 再大量成功：衰减必须让失败占比降下来（不衰减则恒定 0.5）
        for _ in 0..80 {
            pool.note_success("uid_a");
        }
        let (succ, fail) = {
            let entries = safe_lock(&pool.entries);
            let e = entries.get("uid_a").unwrap();
            (e.successes, e.failures)
        };
        assert!(succ + fail <= 51, "总数应被窗口封顶（≤51），实测 {}", succ + fail);
        assert!(
            succ > fail * 2,
            "近期成功应显著占优（succ={succ} fail={fail}），否则历史稀释未修"
        );
    }

    #[test]
    fn selectable_excluding_current_account_detects_fallback() {
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        // 三个号里 a 限频冷却，b 健康：排除 a 后仍应看到 b
        pool.note_error("uid_a", ErrKind::SoftRate);
        let (_, in_allowed) =
            pool.selectable_stats_in_region_excluding(None, None, Some("uid_a"));
        assert_eq!(in_allowed, 1, "排除被限流账号后应仍有一个健康号可顶上");
        // 池内仅剩 a（排除后为空）——这才是「确实没有备选」
        let solo = build_pool(&[("uid_a", 10.0, 0)]);
        solo.note_error("uid_a", ErrKind::SoftRate);
        let (_, after) = solo.selectable_stats_in_region_excluding(None, None, Some("uid_a"));
        assert_eq!(after, 0, "池内无其他账号时应判定为无备选");
        // 不排除时口径不变（保持原 has_selectable 语义）
        assert_eq!(pool.selectable_stats_in_region(None, None).1, 1);
    }

    /// 区域作用域备选判定（海外 Qoder 189s 假死修复的池层基座，2026-10-19）：
    /// 排除某号后「是否还有备选」必须只看**同区**账号——此前 qoder 执行路径
    /// 传 (None, None)，国内健康号让海外请求误判「有备选」，排队快退 429
    /// 永不触发，逐号 90s 空转放大成 503。
    #[test]
    fn region_scoped_other_selectable_ignores_cross_region_accounts() {
        let pool = ApiPool::new();
        pool.sync_from_qoder(
            &[
                acct_q("qd-cn", "国内号", "tk", false),
                acct_q("qd-gl", "海外号", "tk", true),
            ],
            &["qd-cn".to_string(), "qd-gl".to_string()],
        );
        // 海外请求视角：排除唯一海外号 → 无备选（国内号不得顶替）
        assert!(
            !pool.has_other_selectable_for_model("qd-gl", None, Some(true)),
            "国内健康号不得被算作海外请求的备选"
        );
        assert_eq!(
            pool.selectable_stats_in_region_excluding(None, Some(true), Some("qd-gl"))
                .1,
            0,
            "海外区排除自身后应为 0"
        );
        // 国内请求视角：排除海外号 → 国内号仍在
        assert!(pool.has_other_selectable_for_model("qd-gl", None, Some(false)));
        // 不带区域过滤（历史语义）仍看到跨区备选——回归保护
        assert!(pool.has_other_selectable_for_model("qd-gl", None, None));
        // has_selectable_in_region（dispatch 健康预检新口径）：区域无号 = false
        pool.note_error("qd-gl", ErrKind::SoftRate);
        assert!(
            !pool.has_selectable_in_region(None, Some(true)),
            "海外号全冷却时海外 Key 应判池不健康"
        );
        assert!(
            pool.has_selectable_in_region(None, Some(false)),
            "国内号不受海外冷却影响"
        );
    }

    #[test]
    fn cooldown_until_never_shortens_and_ignores_past() {
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        let now = now_ts();
        // 已有更长冷却 → 更短的恢复时刻不得缩短
        pool.cooldown_until("uid_a", now + 3600);
        pool.cooldown_until("uid_a", now + 60);
        assert_eq!(pool.status_list()[0].cooldown_until, Some(now + 3600));
        // 过去/非法时刻：不处理（不产生冷却、不改 reason）
        pool.cooldown_until("uid_b", now - 1);
        assert!(pool.pick_by_uid("uid_a").is_some() || pool.status_list()[0].cooling);
    }

    #[test]
    fn next_0400_is_between_1s_and_24h_away() {
        let now = now_ts();
        for offset in [0i64, 3600, 61_200, 86_399] {
            let t = next_0400(now + offset);
            let delta = t - (now + offset);
            assert!(delta > 0, "必须在未来");
            assert!(delta <= 24 * 3600);
            // 目标时刻的本地时钟（东八区）恰好落在 04:00
            let local = t + 8 * 3600;
            assert_eq!(local % 86400, 4 * 3600);
        }
    }

    /// next_0000（Qoder 免费额度重置时刻，2026-10-19）：严格在未来、不超 24h，
    /// 本地时钟恰落 00:00
    #[test]
    fn next_0000_is_midnight_tomorrow() {
        let now = now_ts();
        for offset in [0i64, 3600, 86_399] {
            let t = next_0000(now + offset);
            let delta = t - (now + offset);
            assert!(delta > 0 && delta <= 24 * 3600, "delta={delta}");
            assert_eq!((t + 8 * 3600) % 86400, 0);
        }
        // next_0000 与 next_0400 的先后取决于当前本地时刻：04:00 之后两者都指向
        // 明天（00:00 先于 04:00）；00:00–04:00 之间 next_0400 是今天 04:00 而
        // next_0000 是明天 00:00（前者更早）。原断言恒要求后者更大，凌晨跑必失败
        let local_hour = (now + 8 * 3600) % 86400 / 3600;
        if local_hour >= 4 {
            assert!(next_0000(now) < next_0400(now), "local_hour={local_hour}");
        } else {
            assert!(next_0400(now) < next_0000(now), "local_hour={local_hour}");
        }
    }

    /// Qoder 免费额度按 (uid,model) 冷却：账号对其他模型照常可选，
    /// 单号池「无备选放行」判定与排除同区语义（119 误冻结整号事故回归锁）
    #[test]
    fn qoder_free_model_cooldown_keeps_account_selectable() {
        let pool = ApiPool::new();
        pool.sync_from_qoder(
            &[
                acct_q("qd-a", "号A", "tk", false),
                acct_q("qd-b", "号B", "tk", false),
            ],
            &["qd-a".to_string(), "qd-b".to_string()],
        );
        let reset = next_0000(now_ts());
        pool.per_model_cooldown_until("qd-a", "Qwen3.8-Flash", reset);
        // 账号级健康不受影响（selectable/healthy 仍 true——不整号冻结）
        assert!(pool.is_model_cooling("qd-a", "Qwen3.8-Flash"), "该模型应冷却");
        assert!(!pool.is_model_cooling("qd-a", "其他模型"), "其他模型不受影响");
        // 有同区备选 → 换号信号为 true
        assert!(pool.has_other_selectable_for_model("qd-a", None, Some(false)));
        // 冷却到期自动失效
        pool.per_model_cooldown_until("qd-a", "m-exp", now_ts() - 1);
        assert!(!pool.is_model_cooling("qd-a", "m-exp"), "过期条目视为无冷却");
    }

    // ==================== P2 修复10：cb_trips 不被非 Server 错误重置 ====================

    #[test]
    fn plan_limit_cooldown_keeps_circuit_memory() {
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        // 预置熔断记忆（此前被 PlanLimit/SoftRate/NotFound 分支误重置）
        {
            let mut entries = safe_lock(&pool.entries);
            entries.get_mut("uid_a").unwrap().cb_trips = 2;
        }
        // 三类非 Server 错误均不应清掉 cb_trips（各自冷却时长照常生效）
        pool.note_error("uid_a", ErrKind::PlanLimit);
        assert!(pool.status_list()[0].cooling, "PlanLimit 应进入 12h 冷却");
        {
            let entries = safe_lock(&pool.entries);
            assert_eq!(
                entries.get("uid_a").unwrap().cb_trips,
                2,
                "PlanLimit 不得重置熔断记忆"
            );
        }
        // SoftRate 冷却结束后再验证
        {
            let mut entries = safe_lock(&pool.entries);
            entries.get_mut("uid_a").unwrap().until = 0;
        }
        pool.note_error("uid_a", ErrKind::SoftRate);
        assert!(pool.status_list()[0].cooling, "SoftRate 应进入 60s 冷却");
        {
            let entries = safe_lock(&pool.entries);
            assert_eq!(entries.get("uid_a").unwrap().cb_trips, 2);
        }
        // note_success 仍是唯一重置点
        pool.note_success("uid_a");
        {
            let entries = safe_lock(&pool.entries);
            assert_eq!(entries.get("uid_a").unwrap().cb_trips, 0);
        }
    }

    #[test]
    fn wb_sync_and_pick_carries_region_fields() {
        let pool = ApiPool::new();
        pool.sync_from_wb(
            &[crate::api_server::pool::WbSyncAccount {
                uid: "wb-abc".into(),
                name: "WB账号".into(),
                token: "tk".into(),
                domain: "workbuddy.ai".into(),
                enterprise_id: "e1".into(),
                global_region: true,
                credits: Some(50.0),
                credits_expire_at: None,
                credits_fetched_at: None,
                needs_relogin: false,
                group_id: String::new(),
            }],
            &["wb-abc".to_string()],
        );
        pool.set_strategy(PoolStrategy::CreditFirst);
        let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
        assert_eq!(picked.uid, "wb-abc");
        assert!(picked.global_region);
        assert_eq!(picked.domain, "workbuddy.ai");
        assert_eq!(picked.enterprise_id, "e1");
        // needs_relogin → Forbidden，不参与取号
        let pool2 = ApiPool::new();
        pool2.sync_from_wb(
            &[crate::api_server::pool::WbSyncAccount {
                uid: "wb-x".into(), name: String::new(), token: "tk".into(),
                domain: String::new(), enterprise_id: String::new(),
                global_region: false, credits: None, credits_expire_at: None,
                credits_fetched_at: None, needs_relogin: true,
                group_id: String::new(),
            }],
            &["wb-x".to_string()],
        );
        assert!(pool2.pick_excluding_constrained(&HashSet::new(), None, None).is_none());
    }

    #[test]
    fn qoder_sync_and_pick_carries_machine_id() {
        let pool = ApiPool::new();
        pool.sync_from_qoder(
            &[
                crate::api_server::pool::QoderSyncAccount {
                    uid: "qd-1".into(),
                    name: "Qoder账号".into(),
                    access_token: "tk".into(),
                    machine_id: "mid".into(),
                    needs_relogin: false,
                    credits: None,
                    credits_expire_at: None,
                    credits_fetched_at: None,
                global_region: false,
                },
                crate::api_server::pool::QoderSyncAccount {
                    uid: "qd-dead".into(),
                    name: String::new(),
                    access_token: "tk".into(),
                    machine_id: String::new(),
                    needs_relogin: true,
                    credits: None,
                    credits_expire_at: None,
                    credits_fetched_at: None,
                global_region: false,
                },
                // 空令牌账号不入池（无法签名）
                crate::api_server::pool::QoderSyncAccount {
                    uid: "qd-empty".into(),
                    name: String::new(),
                    access_token: String::new(),
                    machine_id: String::new(),
                    needs_relogin: false,
                    credits: None,
                    credits_expire_at: None,
                    credits_fetched_at: None,
                global_region: false,
                },
            ],
            &["qd-1".to_string(), "qd-dead".to_string(), "qd-empty".to_string()],
        );
        assert_eq!(pool.count(), 2, "空令牌账号不入池");
        let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
        assert_eq!(picked.uid, "qd-1");
        assert_eq!(picked.machine_id, "mid");
        // needs_relogin → disabled，不参与取号（Forbidden 态）
        assert!(pool.pick_by_uid("qd-dead").is_none());
        assert_eq!(pool.status_list().iter().find(|s| s.uid == "qd-dead").unwrap().state, "Forbidden");
        // 全量重建：再次同步仅含 qd-dead（enabled）→ qd-1 消失
        pool.sync_from_qoder(
            &[crate::api_server::pool::QoderSyncAccount {
                uid: "qd-dead".into(),
                name: String::new(),
                access_token: "tk".into(),
                machine_id: String::new(),
                needs_relogin: true,
                credits: None,
                credits_expire_at: None,
                credits_fetched_at: None,
            global_region: false,
            }],
            &["qd-dead".to_string()],
        );
        assert_eq!(pool.count(), 1);
        assert!(pool.pick_excluding_constrained(&HashSet::new(), None, None).is_none());
    }

    #[test]
    fn qoder_sync_carries_credits_snapshot() {
        // issue #102：Qoder 余额/到期/快照时间透传入池——smart 排序三键
        //（最早到期 → 倍率 → 积分和）与 reserve 保留额过滤的数据源
        let pool = ApiPool::new();
        pool.sync_from_qoder(
            &[crate::api_server::pool::QoderSyncAccount {
                uid: "qd-c".into(),
                name: "n".into(),
                access_token: "tk".into(),
                machine_id: String::new(),
                needs_relogin: false,
                credits: Some(42.0),
                credits_expire_at: Some(1_999_999_999),
                credits_fetched_at: Some(1_700_000_000),
            global_region: false,
            }],
            &["qd-c".to_string()],
        );
        let entries = safe_lock(&pool.entries);
        let e = entries.get("qd-c").unwrap();
        assert_eq!(e.credits, Some(42.0));
        assert_eq!(e.credits_expire_at, Some(1_999_999_999));
        assert_eq!(e.credits_fetched_at, Some(1_700_000_000));
    }

    #[test]
    fn qoder_update_credits_snapshot_three_keys_and_runtime_state_kept() {
        // issue #102 运行期拨新：三键同拨；未提及账号不动；运行态（冷却/成功数）不受影响
        let pool = ApiPool::new();
        pool.sync_from_qoder(
            &[
                crate::api_server::pool::QoderSyncAccount {
                    uid: "qd-a".into(), name: String::new(), access_token: "tk".into(),
                    machine_id: String::new(), needs_relogin: false,
                    credits: Some(0.0), credits_expire_at: Some(1), credits_fetched_at: Some(1),
                global_region: false,
                },
                crate::api_server::pool::QoderSyncAccount {
                    uid: "qd-b".into(), name: String::new(), access_token: "tk".into(),
                    machine_id: String::new(), needs_relogin: false,
                    credits: Some(5.0), credits_expire_at: Some(1_999_999_999),
                    credits_fetched_at: Some(1),
                global_region: false,
                },
            ],
            &["qd-a".to_string(), "qd-b".to_string()],
        );
        // 运行态预置：qd-a 软冷却中 + 有成功计数，拨新不得触碰
        {
            let mut entries = safe_lock(&pool.entries);
            let a = entries.get_mut("qd-a").unwrap();
            a.until = 1_234_567_890;
            a.successes = 7;
        }
        let mut m: HashMap<String, (f64, Option<i64>)> = HashMap::new();
        m.insert("qd-a".to_string(), (120.0, Some(1_999_999_999)));
        pool.update_credits_snapshot(&m);
        {
            let entries = safe_lock(&pool.entries);
            let a = entries.get("qd-a").unwrap();
            assert_eq!(a.credits, Some(120.0));
            assert_eq!(a.credits_expire_at, Some(1_999_999_999));
            assert!(a.credits_fetched_at.unwrap() > 1, "快照时间拨新");
            assert_eq!(a.until, 1_234_567_890, "软冷却不被触碰");
            assert_eq!(a.successes, 7, "运行态不被触碰");
            // 未提及账号保持原值
            let b = entries.get("qd-b").unwrap();
            assert_eq!(b.credits, Some(5.0));
            assert_eq!(b.credits_expire_at, Some(1_999_999_999));
            assert_eq!(b.credits_fetched_at, Some(1));
        }
    }

    #[test]
    fn qoder_has_zero_or_expired_credits_detects_pinned_snapshot() {
        let pool = ApiPool::new();
        pool.sync_from_qoder(
            &[crate::api_server::pool::QoderSyncAccount {
                uid: "qd-0".into(), name: String::new(), access_token: "tk".into(),
                machine_id: String::new(), needs_relogin: false,
                credits: Some(0.0), credits_expire_at: None, credits_fetched_at: Some(1),
            global_region: false,
            }],
            &["qd-0".to_string()],
        );
        assert!(pool.has_zero_or_expired_credits(), "零余额快照应触发保鲜");
        // 拨新为正余额 + 未到期 → 不再触发
        let mut m: HashMap<String, (f64, Option<i64>)> = HashMap::new();
        m.insert("qd-0".to_string(), (9.9, Some(1_999_999_999)));
        pool.update_credits_snapshot(&m);
        assert!(!pool.has_zero_or_expired_credits());
        // 已到期快照同样触发（余额为正也被到期过滤排除）
        let mut m2: HashMap<String, (f64, Option<i64>)> = HashMap::new();
        m2.insert("qd-0".to_string(), (9.9, Some(1)));
        pool.update_credits_snapshot(&m2);
        assert!(pool.has_zero_or_expired_credits());
        // disabled（needs_relogin）+ 零余额 → 不触发：刷新注定 401 永久失败，
        // 计入只会让每次启动都跑注定失败的全池刷新
        pool.sync_from_qoder(
            &[crate::api_server::pool::QoderSyncAccount {
                uid: "qd-0".into(), name: String::new(), access_token: "tk".into(),
                machine_id: String::new(), needs_relogin: true,
                credits: Some(0.0), credits_expire_at: Some(1), credits_fetched_at: Some(1),
            global_region: false,
            }],
            &["qd-0".to_string()],
        );
        assert!(!pool.has_zero_or_expired_credits(), "disabled 账号不参与保鲜判定");
    }

    #[test]
    fn wb_sync_carries_credits_expire_at() {
        // issue #28：Buddy 积分包到期透传入池，供 ExpireFirst/smart 排序消费
        let pool = ApiPool::new();
        pool.sync_from_wb(
            &[crate::api_server::pool::WbSyncAccount {
                uid: "wb-e".into(),
                name: "n".into(),
                token: "tk".into(),
                domain: "d".into(),
                enterprise_id: String::new(),
                global_region: false,
                credits: Some(10.0),
                credits_expire_at: Some(1_234_567_890),
                credits_fetched_at: None,
                needs_relogin: false,
                group_id: String::new(),
            }],
            &["wb-e".to_string()],
        );
        let entries = safe_lock(&pool.entries);
        assert_eq!(entries.get("wb-e").unwrap().credits_expire_at, Some(1_234_567_890));
    }

    // ==================== F-77 账号级并发感知调度 ====================

    #[test]
    fn busy_account_yields_to_idle() {
        // 双账号池 + limit=1：uid_a 在途 1 → 取号必落 uid_b（busy 让位 idle）
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.set_concurrency_limit(1);
        let h = pool.inflight_handle("uid_a");
        h.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for _ in 0..8 {
            let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
            assert_eq!(picked.uid, "uid_b", "busy 账号应让位空闲账号");
        }
        // 释放后恢复可选
        h.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        let mut seen_a = false;
        for _ in 0..16 {
            if pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid == "uid_a" {
                seen_a = true;
                break;
            }
        }
        assert!(seen_a, "计数归零后账号应恢复参与调度");
    }

    #[test]
    fn all_busy_falls_back_to_least_inflight() {
        // 全部 busy：不拒绝，取 inflight 最小者（uid_b=1 < uid_a=2）
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.set_concurrency_limit(1);
        pool.inflight_handle("uid_a").fetch_add(2, std::sync::atomic::Ordering::Relaxed);
        pool.inflight_handle("uid_b").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for _ in 0..8 {
            let picked = pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap();
            assert_eq!(picked.uid, "uid_b", "全部 busy 时应取 inflight 最小者");
        }
    }

    #[test]
    fn zero_limit_keeps_original_behavior() {
        // limit=0 = 不限：busy 过滤关闭，在途账号照常参与
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        pool.set_concurrency_limit(0);
        pool.inflight_handle("uid_a").fetch_add(3, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid, "uid_a");
    }

    #[test]
    fn single_busy_account_still_serves() {
        // 单账号池：无让位对象，busy 也照常取号（粘性/单账号行为兜底）
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        pool.set_concurrency_limit(1);
        pool.inflight_handle("uid_a").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(pool.pick_excluding_constrained(&HashSet::new(), None, None).unwrap().uid, "uid_a");
    }

    #[test]
    fn pick_by_uid_ignores_busy_filter() {
        // 粘性取号不受 busy 过滤（粘住 = 上游缓存命中，优先级高于让位）
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.set_concurrency_limit(1);
        pool.inflight_handle("uid_a").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert!(pool.pick_by_uid("uid_a").is_some());
    }

    #[test]
    fn p2c_prefers_lower_inflight_first() {
        // P2C 负载第一比较键：在途数不同时空闲者直接胜出（无视三因子得分）
        let e_a = test_entry("a", 900.0, 1000); // 高分但 busy
        let e_b = test_entry("b", 10.0, 1000); // 低分但空闲
        let cands = vec![&e_a, &e_b];
        let mut inflight = HashMap::new();
        inflight.insert("a".to_string(), 2u32);
        inflight.insert("b".to_string(), 0u32);
        for seed in (1..50u64).step_by(2) {
            assert_eq!(pick_p2c(&cands, seed, 2000, &inflight).unwrap().uid, "b");
        }
    }

    #[test]
    fn status_list_reports_inflight() {
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        assert_eq!(pool.status_list()[0].inflight, 0);
        let h = pool.inflight_handle("uid_a");
        h.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(pool.status_list()[0].inflight, 1);
        h.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(pool.status_list()[0].inflight, 0);
    }

    /// 按模型冷却快照必须外传（前端可见性，2026-10-22）：账号级 cooling 之外
    /// 的第二维度。此前 status_list 不带 per_model_cooldowns，账号在某个模型
    /// 上被冷却到次日 00:00 时前端只显示「就绪」——用户无从知道该换哪个模型。
    /// 过期条目须过滤（前端拿到的都是生效中的）。
    #[test]
    fn status_list_reports_per_model_cooldowns() {
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        let now = now_ts();
        // 生效中的两条 + 已过期的一条
        pool.per_model_cooldown_until("uid_a", "qwen3.8-flash", now + 3600);
        pool.per_model_cooldown_until("uid_a", "deepseek-flash", now + 7200);
        pool.per_model_cooldown_until("uid_a", "stale-model", now - 60);
        let map = &pool.status_list()[0].per_model_cooldowns;
        assert_eq!(map.len(), 2, "过期条目必须过滤，实际 {:?}", map);
        assert_eq!(map.get("qwen3.8-flash"), Some(&(now + 3600)));
        assert_eq!(map.get("deepseek-flash"), Some(&(now + 7200)));
        assert!(!map.contains_key("stale-model"), "已过期不得外传");
        // 无按模型冷却时为空 map（不是 None，前端 ?? {} 兜底同效）
        let pool_b = build_pool(&[("uid_b", 10.0, 0)]);
        assert!(pool_b.status_list()[0].per_model_cooldowns.is_empty());
    }

    #[test]
    fn guard_style_inflight_via_handle() {
        // 模拟 InflightGuard 配对：取号句柄 +1/-1，中途丢弃（Drop 兜底语义）计数归零
        let pool = build_pool(&[("uid_a", 10.0, 0)]);
        {
            let _h = pool.inflight_handle("uid_a");
            _h.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            assert_eq!(pool.status_list()[0].inflight, 1);
        }
        // Arc 丢弃后（guard Drop fetch_sub 语义由调用方保证）：
        // 这里直接验证句柄释放 + 计数递减路径可用
        let h = pool.inflight_handle("uid_a");
        h.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(pool.status_list()[0].inflight, 0);
    }

    // ==================== F-77④ 粘性让位 + busy 调度事件 ====================

    #[test]
    fn sticky_yield_when_busy_and_idle_exists() {
        // 粘性账号 busy（inflight≥limit=1）且存在空闲候选 → 让位改选空闲账号
        let pool = build_pool(&[("uid_sticky", 10.0, 0), ("uid_idle", 10.0, 0)]);
        pool.inflight_handle("uid_sticky").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (p, ev) = pool.pick_sticky_yield("uid_sticky", None).unwrap();
        assert_eq!(p.uid, "uid_idle", "busy 粘性账号应让位给空闲账号");
        let ev = ev.unwrap();
        assert!(
            ev.starts_with("sticky_yield from=uid_sticky to=uid_idle"),
            "让位事件应包含 from/to：{ev}"
        );
    }

    #[test]
    fn sticky_kept_when_all_busy() {
        // 全候选 busy → 保持粘性（sticky_fallback：缓存收益 > 换一个同样 busy 的账号）
        let pool = build_pool(&[("uid_sticky", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.inflight_handle("uid_sticky").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        pool.inflight_handle("uid_b").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (p, ev) = pool.pick_sticky_yield("uid_sticky", None).unwrap();
        assert_eq!(p.uid, "uid_sticky", "全 busy 时应保持粘性");
        assert!(ev.unwrap().starts_with("sticky_fallback"));
    }

    #[test]
    fn sticky_kept_when_idle() {
        // 粘性账号空闲 → 正常锁定，无调度事件
        let pool = build_pool(&[("uid_sticky", 10.0, 0), ("uid_b", 10.0, 0)]);
        let (p, ev) = pool.pick_sticky_yield("uid_sticky", None).unwrap();
        assert_eq!(p.uid, "uid_sticky");
        assert!(ev.is_none());
    }

    #[test]
    fn sticky_yield_respects_allowed() {
        // 让位候选受 allowed 白名单约束：唯一其他候选不在白名单 → 保持粘性
        let pool = build_pool(&[("uid_sticky", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.inflight_handle("uid_sticky").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut allowed = HashSet::new();
        allowed.insert("uid_sticky".to_string());
        let (p, ev) = pool.pick_sticky_yield("uid_sticky", Some(&allowed)).unwrap();
        assert_eq!(p.uid, "uid_sticky", "无白名单内其他候选时保持粘性");
        assert!(ev.is_none());
    }

    #[test]
    fn sticky_yield_no_limit_always_keeps() {
        // limit=0（不限并发）→ 无 busy 概念，始终粘性锁定
        let pool = build_pool(&[("uid_sticky", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.set_concurrency_limit(0);
        pool.inflight_handle("uid_sticky").fetch_add(5, std::sync::atomic::Ordering::Relaxed);
        let (p, ev) = pool.pick_sticky_yield("uid_sticky", None).unwrap();
        assert_eq!(p.uid, "uid_sticky");
        assert!(ev.is_none());
    }

    #[test]
    fn busy_yield_event_on_constrained_pick() {
        // 调度路径：busy 账号被 idle 过滤淘汰 → busy_yield 事件 + 选空闲账号
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.inflight_handle("uid_a").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (p, ev) =
            pool.pick_excluding_constrained_ev(&HashSet::new(), None, None).unwrap();
        assert_eq!(p.uid, "uid_b");
        assert!(ev.unwrap().starts_with("busy_yield"));
    }

    #[test]
    fn busy_fallback_event_when_all_busy() {
        // 全候选 busy → busy_fallback 降级取 inflight 最小者
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.inflight_handle("uid_a").fetch_add(2, std::sync::atomic::Ordering::Relaxed);
        pool.inflight_handle("uid_b").fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (p, ev) =
            pool.pick_excluding_constrained_ev(&HashSet::new(), None, None).unwrap();
        assert_eq!(p.uid, "uid_b", "全 busy 时应取 inflight 最小者");
        assert!(ev.unwrap().starts_with("busy_fallback"));
    }

    #[test]
    fn no_busy_event_when_limit_zero() {
        // limit=0 → 无 busy 过滤，无事件（兼容原语义）
        let pool = build_pool(&[("uid_a", 10.0, 0), ("uid_b", 10.0, 0)]);
        pool.set_concurrency_limit(0);
        pool.inflight_handle("uid_a").fetch_add(3, std::sync::atomic::Ordering::Relaxed);
        let (_, ev) =
            pool.pick_excluding_constrained_ev(&HashSet::new(), None, None).unwrap();
        assert!(ev.is_none());
    }
}
