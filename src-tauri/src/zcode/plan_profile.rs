//! 套餐画像：把 `billing/current` 的原始响应归一化成网关可直接消费的账号级画像。
//!
//! 网关侧只关心三件事，其余都是展示用：
//!
//! 1. **入池门槛** —— `servable_models` 为空 = 该账号当前没有任何可用权益，
//!    不该被选号器选中（否则必然撞 1113 白白消耗冷却）。
//! 2. **选号过滤** —— 请求模型必须落在 `servable_models` 内；
//!    `period=daily_night` 的权益只在夜间窗口可选。
//! 3. **燃烧顺序** —— 用户已定口径「临时额度先烧」：
//!    `new_account` > `task_card` > `global_build` > `start_plan`，
//!    同形态内按 `ends_at` 升序（先过期先烧）。
//!
//! 归一化依据（实测 2026-10-23，本机 15 个账号）：
//!
//! | plan_id / name 特征 | plan_kind | period |
//! |---|---|---|
//! | `zcode-v3-start-plan-referral` / `ZCode Global Build` | `global_build` | `daily_night` |
//! | `zcode-v3-start-plan-*` / `ZCode Start Plan` | `start_plan` | `daily` |
//! | 邀请奖励发放（夜间卡/周末卡/周重置卡） | `task_card` | 按发放周期 |
//! | 新注册赠送 | `new_account` | 短期高额度 |
//! | 其余 | `other` | 原样透传 |
//!
//! 注意 `zcode-v3-start-plan-referral` 的前缀也匹配 `zcode-v3-start-plan-*`，
//! 所以判定顺序必须 **先 global_build 再 start_plan**（见 [`classify`]）。

// 本模块是一层"归一化 API"：部分访问器（is_servable / serves_now / label …）目前
// 只被单元测试与监控侧按需调用，留作调度与 UI 的稳定出口，不算死代码。
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::api_server::zcode_catalog;

/// 夜间档窗口（本地时区小时数，左闭右开跨零点）：23:00 → 次日 09:00。
///
/// **实测依据**（2026-10-23 14:42 CST，id=17 的 Global Build 权益）：
/// - `entitlements[].effective_at = 1792767600` = `2026-10-23 23:00:00 CST`（今晚起效）
/// - `plans[].ends_at = 1792803600` = `2026-10-24 09:00:00 CST`（次日早 9 点收）
/// 即夜间档的可服务区间正好是 23:00→09:00，10 小时。
///
/// 这个常量只是**兜底**：首选判据是每条权益自带的 `effective_at`（见
/// [`PlanGrant::usable_at`]）。两者同时校验，避免上游下发过期 `effective_at`
/// 时在白天误选夜间档账号。
pub const NIGHT_START_HOUR: u32 = 23;
pub const NIGHT_END_HOUR: u32 = 9;

/// 当前本地小时是否落在夜间档窗口内。
pub fn in_night_window(local_hour: u32) -> bool {
    if NIGHT_START_HOUR <= NIGHT_END_HOUR {
        // 同日窗口（例如 00:00-08:00）走这里，保持逻辑通用
        local_hour >= NIGHT_START_HOUR && local_hour < NIGHT_END_HOUR
    } else {
        local_hour >= NIGHT_START_HOUR || local_hour < NIGHT_END_HOUR
    }
}

/// 取当前本地小时（拿不到时区信息按 UTC+8 兜底，ZCode 面向国内/亚洲用户）。
pub fn local_hour() -> u32 {
    use chrono::Timelike;
    chrono::Local::now().hour()
}

/// 套餐形态（归一化后）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanKind {
    /// 新账号赠送（短期高额度，到期即失效）
    NewAccount,
    /// 任务卡 / 邀请奖励发放（夜间卡、周末卡、周重置卡）
    TaskCard,
    /// ZCode Global Build（`zcode-v3-start-plan-referral`，夜间档）
    GlobalBuild,
    /// ZCode Start Plan（日内额度，长期订阅底座）
    StartPlan,
    /// 未识别形态（按 period 原样透传）
    Other,
}

impl PlanKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanKind::NewAccount => "new_account",
            PlanKind::TaskCard => "task_card",
            PlanKind::GlobalBuild => "global_build",
            PlanKind::StartPlan => "start_plan",
            PlanKind::Other => "other",
        }
    }

    /// 字符串回读（落库字段 → 枚举）。未知值一律 `Other`，保证旧数据可读。
    pub fn parse(s: &str) -> PlanKind {
        match s.trim() {
            "new_account" => PlanKind::NewAccount,
            "task_card" => PlanKind::TaskCard,
            "global_build" => PlanKind::GlobalBuild,
            "start_plan" => PlanKind::StartPlan,
            _ => PlanKind::Other,
        }
    }

    /// 燃烧顺序权重（越小越先烧）。用户口径：临时额度先烧完，长期订阅留底。
    pub fn burn_rank(self) -> i64 {
        match self {
            PlanKind::NewAccount => 0,
            PlanKind::TaskCard => 1,
            PlanKind::GlobalBuild => 2,
            PlanKind::StartPlan => 3,
            PlanKind::Other => 4,
        }
    }

    /// 中文展示名（UI 标签用）
    pub fn label(self) -> &'static str {
        match self {
            PlanKind::NewAccount => "新号赠送",
            PlanKind::TaskCard => "任务卡",
            PlanKind::GlobalBuild => "Global Build",
            PlanKind::StartPlan => "Start Plan",
            PlanKind::Other => "其他套餐",
        }
    }
}

impl Default for PlanKind {
    fn default() -> Self {
        PlanKind::Other
    }
}

/// 单条权益授予（一个套餐 × 一个 entitlement 的展开）。
///
/// 之所以展开到 entitlement 粒度：同一账号的两条权益可能模型不同、时段不同
/// （例如 id=30 同时有 Start Plan 的 GLM-5.3 daily 与 Global Build 的
/// GLM-5.3-Flash daily_night），过滤必须按权益判断而不是按账号判断。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanGrant {
    pub plan_id: String,
    pub plan_name: String,
    pub kind: PlanKind,
    pub entitlement_id: Option<String>,
    /// `daily` / `daily_night` / `once` / `weekly` …
    pub period: String,
    /// 该权益覆盖的模型（catalog 归一化后的 id，例如 `GLM-5.3-Flash`）
    pub models: Vec<String>,
    pub grant_units: i64,
    pub used_units: i64,
    pub remaining_units: i64,
    pub priority: i64,
    pub ends_at: Option<i64>,
    /// 权益生效时刻（unix 秒；0 = 立即可用）。
    /// 夜间档权益实测下发的是「今晚 23:00」，早于该时刻不可用。
    #[serde(default)]
    pub effective_at: i64,
}

impl PlanGrant {
    /// 该权益是否覆盖某模型（入参已是 catalog 归一化 id 或原始请求名均可）
    pub fn serves(&self, model: &str) -> bool {
        let want = canon_model(model);
        self.models.iter().any(|m| canon_model(m) == want)
    }

    /// 该权益当前是否可用：已生效、未过期、额度未耗尽、夜间档在窗口内。
    pub fn usable_at(&self, now_ts: i64, hour: u32) -> bool {
        // 尚未生效（夜间档未到点）：最精确的判据，直接用上游给的 effective_at
        if self.effective_at > 0 && now_ts < self.effective_at {
            return false;
        }
        if let Some(end) = self.ends_at {
            if end > 0 && now_ts >= end {
                return false;
            }
        }
        if self.grant_units > 0 && self.remaining_units <= 0 {
            return false;
        }
        if is_night_period(&self.period) && !in_night_window(hour) {
            return false;
        }
        true
    }
}

/// 账号级套餐画像（`accounts` 表套餐字段的唯一来源）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PlanProfile {
    /// 燃烧顺序最靠前的形态（排序/标签主口径）
    pub plan_kind: PlanKind,
    /// 该账号持有过的全部形态（去重，按 burn_rank 升序）
    pub kinds: Vec<PlanKind>,
    pub plan_ids: Vec<String>,
    pub plan_names: Vec<String>,
    /// 全部权益覆盖的模型并集（catalog 归一化 id）；**空 = 账号不可用于推理**
    pub servable_models: Vec<String>,
    /// 聚合时段口径：全为夜间档 → `daily_night`，含日内额度 → `daily`
    pub quota_period: String,
    /// 是否只能夜间用（只有 daily_night 权益，没有任何日内/一次性权益）
    pub night_only: bool,
    pub grant_units: i64,
    pub used_units: i64,
    pub remaining_units: i64,
    /// 最早到期时间（unix 秒）——燃烧顺序的同形态内排序依据
    pub ends_at: Option<i64>,
    /// 官方给的套餐权重（越大越优先）
    pub priority: i64,
    /// `active` / `expired` / …
    pub status: String,
    /// 展开后的权益明细
    pub grants: Vec<PlanGrant>,
}

impl PlanProfile {
    /// 是否有任何可用于推理的权益（入池门槛）
    pub fn is_servable(&self) -> bool {
        !self.servable_models.is_empty()
    }

    /// 该账号能否在当前时刻服务某模型（选号过滤器）
    pub fn serves_now(&self, model: &str, now_ts: i64, hour: u32) -> bool {
        let want = canon_model(model);
        self.grants
            .iter()
            .any(|g| canon_model_list(&g.models).contains(&want) && g.usable_at(now_ts, hour))
    }

    /// 当前时刻可服务的模型列表（UI「可用模型」展示，随夜间窗口变化）
    pub fn servable_now(&self, now_ts: i64, hour: u32) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for g in &self.grants {
            if !g.usable_at(now_ts, hour) {
                continue;
            }
            for m in &g.models {
                if !out.iter().any(|x| canon_model(x) == canon_model(m)) {
                    out.push(m.clone());
                }
            }
        }
        out
    }

    /// 同形态内的次级排序键：先过期先烧；无到期时间排最后。
    pub fn expiry_key(&self) -> i64 {
        self.ends_at.filter(|v| *v > 0).unwrap_or(i64::MAX)
    }

    /// 逗号分隔的模型串（落库用）
    pub fn servable_models_str(&self) -> String {
        self.servable_models.join(",")
    }

    /// 展示/门闸用的形态串。
    ///
    /// `none`（没有任何套餐）与 `other`（有套餐但没识别出来）必须分开：
    /// 前者是"上游说这号什么都没有"，网关一定跳过；后者只是我们没认出来，
    /// 权益可能仍然可用。混成一个值会让 UI 无法解释"为什么这个号不可选"。
    pub fn kind_tag(&self) -> &'static str {
        if self.grants.is_empty() {
            "none"
        } else {
            self.plan_kind.as_str()
        }
    }
}

// ---------------------------------------------------------------------------
// 归一化实现
// ---------------------------------------------------------------------------

fn canon_model(s: &str) -> String {
    s.trim().to_lowercase()
}

fn canon_model_list(models: &[String]) -> Vec<String> {
    models.iter().map(|m| canon_model(m)).collect()
}

fn is_night_period(period: &str) -> bool {
    period.eq_ignore_ascii_case("daily_night") || period.eq_ignore_ascii_case("night")
}

/// 把任意模型名归一化成 catalog 里的正式 id；不在目录内则保留原始拼写。
fn normalize_model(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(id) = zcode_catalog::resolve(s) {
        return Some(id);
    }
    // capabilities 形如 `model:glm-5.3-flash` / `model:GLM-5.3`
    let stripped = s
        .strip_prefix("model:")
        .or_else(|| s.strip_prefix("model/"))
        .unwrap_or(s);
    if let Some(id) = zcode_catalog::resolve(stripped) {
        return Some(id);
    }
    Some(stripped.trim().to_string())
}

/// 单条套餐记录 → 形态判定。
pub fn classify(plan_id: &str, name: &str) -> PlanKind {
    let pid = plan_id.to_lowercase();
    let pname = name.to_lowercase();

    // 先判 Global Build：它的 plan_id 同样以 `zcode-v3-start-plan-` 开头
    if pid.contains("referral") || pname.contains("global build") {
        return PlanKind::GlobalBuild;
    }
    if pid.contains("start-plan") || pid.contains("start_plan") || pname.contains("start plan") {
        return PlanKind::StartPlan;
    }
    // 任务卡 / 邀请奖励：客户端与官方文案里出现的关键词
    // 实测文案是「夜间蹬蹬卡 / 周末蹬蹬卡 / 周重置卡」，所以"蹬蹬卡"必须单独命中
    for kw in [
        "task", "任务", "night_card", "夜间卡", "夜间蹬蹬卡", "周末卡", "周末蹬蹬卡",
        "蹬蹬卡", "weekend", "reset", "重置卡", "打卡",
    ] {
        if pid.contains(kw) || pname.contains(kw) {
            return PlanKind::TaskCard;
        }
    }
    // 新号赠送
    for kw in ["new_user", "newuser", "new-account", "new_account", "welcome", "新号", "新手", "新用户"] {
        if pid.contains(kw) || pname.contains(kw) {
            return PlanKind::NewAccount;
        }
    }
    PlanKind::Other
}

/// 从 `billing/current`（也兼容 `billing/balance`）响应里挑出所有「套餐对象」。
///
/// 官方两种接口的包裹层级不完全一致（`data.plans[]` / `data` 单对象 / 顶层数组），
/// 这里做宽松抽取，避免因为多包一层就整块失效。
fn collect_plan_objects(v: &Value) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();

    fn push_all(out: &mut Vec<Value>, node: &Value) {
        match node {
            Value::Array(items) => {
                for it in items {
                    push_all(out, it);
                }
            }
            Value::Object(_) => {
                // 只把"看起来像套餐"的对象收进来：必须有 plan_id 或 entitlements
                if node.get("plan_id").is_some() || node.get("entitlements").is_some() {
                    out.push(node.clone());
                }
            }
            _ => {}
        }
    }

    for candidate in [
        v.get("data").and_then(|d| d.get("plans")),
        v.get("data").and_then(|d| d.get("plan")),
        v.get("data"),
        v.get("plans"),
        Some(v),
    ]
    .into_iter()
    .flatten()
    {
        push_all(&mut out, candidate);
    }

    // 去重（同一对象可能被多条路径命中）
    let mut seen: Vec<String> = Vec::new();
    out.retain(|p| {
        let id = p
            .get("plan_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let key = format!("{id}#{}", p.get("entitlements").map(|e| e.to_string()).unwrap_or_default());
        if seen.contains(&key) {
            false
        } else {
            seen.push(key);
            true
        }
    });
    out
}

/// `billing/balance` 的 `data.balances[]` 索引：entitlement_id → (used, remaining)。
///
/// 为什么不自己算 `grant - used`：实测 `plans[].entitlements[]` 里**没有** used 字段，
/// 真实消耗只出现在 `balances[]`（按 entitlement_id 对齐，还带 `available_units`）。
/// 缺这一步会把已烧掉的额度当成没用过，出现"账上还有 5M 实际 0"的假可用。
fn balance_index(v: &Value) -> std::collections::HashMap<String, (i64, i64)> {
    let mut map = std::collections::HashMap::new();
    let Some(rows) = v["data"]["balances"].as_array() else {
        return map;
    };
    for b in rows {
        let Some(eid) = b["entitlement_id"].as_str() else {
            continue;
        };
        let used = b["used_units"].as_i64().unwrap_or(0);
        let remaining = b["remaining_units"].as_i64().unwrap_or(0);
        map.insert(eid.to_string(), (used, remaining));
    }
    map
}

/// 归一化入口：任意 billing 响应 → 账号级画像。
///
/// 数据来源已实测（2026-10-23）：
/// - `data.plans[].entitlements[]` —— 套餐/权益/`capabilities`/`period`/`grant_units`/
///   `priority`/`status`/`ends_at`/`effective_at`（模型-账号匹配的唯一权威口径）
/// - `data.balances[]` —— 按 `entitlement_id` 对齐的真实 used/remaining
///
/// `billing/current` 与 `billing/balance` 的 `plans[]` 结构完全一致，但 `current`
/// 没有 `balances[]` 且 `effective_at` 恒为 0——所以**画像只认 balance**，
/// `current` 仅作调试/兜底。
pub fn normalize(v: &Value) -> PlanProfile {
    let plans = collect_plan_objects(v);
    let balances = balance_index(v);
    let mut profile = PlanProfile::default();
    let mut kinds: Vec<PlanKind> = Vec::new();
    let mut grants: Vec<PlanGrant> = Vec::new();
    let mut periods: Vec<String> = Vec::new();

    for p in &plans {
        let plan_id = p
            .get("plan_id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let plan_name = p
            .get("name")
            .and_then(|x| x.as_str())
            .unwrap_or("?")
            .to_string();
        let kind = classify(&plan_id, &plan_name);
        let plan_priority = p.get("priority").and_then(|x| x.as_i64()).unwrap_or(0);
        let plan_ends_at = p
            .get("ends_at")
            .and_then(|x| x.as_i64())
            .filter(|v| *v > 0);
        let plan_status = p
            .get("status")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();

        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
        if !plan_id.is_empty() && !profile.plan_ids.contains(&plan_id) {
            profile.plan_ids.push(plan_id.clone());
        }
        if !profile.plan_names.contains(&plan_name) {
            profile.plan_names.push(plan_name.clone());
        }
        if plan_priority > profile.priority {
            profile.priority = plan_priority;
        }
        if !plan_status.is_empty() && profile.status.is_empty() {
            profile.status = plan_status;
        }
        match (plan_ends_at, profile.ends_at) {
            (Some(e), Some(cur)) => profile.ends_at = Some(cur.min(e)),
            (Some(e), None) => profile.ends_at = Some(e),
            _ => {}
        }

        let Some(ents) = p.get("entitlements").and_then(|x| x.as_array()) else {
            // 没有 entitlement 明细：至少记录形态，模型集留空（不可用于推理）
            continue;
        };

        for e in ents {
            let show_name = e.get("show_name").and_then(|x| x.as_str()).unwrap_or("");
            let period = e
                .get("period")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let grant_units = e.get("grant_units").and_then(|x| x.as_i64()).unwrap_or(0);
            // used/remaining 优先取 balances[]（按 entitlement_id 对齐）；没有对应的
            // 余额行（如夜间档尚未生效时 balances 为空）→ 视为"未动用全额可用"
            let eid = e
                .get("entitlement_id")
                .and_then(|x| x.as_str())
                .unwrap_or("");
            let (used_units, remaining_units) = match balances.get(eid) {
                Some((u, r)) => (*u, *r),
                None => {
                    // 没有对应余额行（夜间档尚未生效时 balances 为空）：按权益自带的
                    // used/remaining 推算，都没有就视为"未动用全额可用"
                    let u = e.get("used_units").and_then(|x| x.as_i64()).unwrap_or(0);
                    let r = e
                        .get("remaining_units")
                        .and_then(|x| x.as_i64())
                        .unwrap_or((grant_units - u).max(0));
                    (u, r)
                }
            };

            // 模型集：capabilities 优先，其次 show_name
            let mut models: Vec<String> = Vec::new();
            if let Some(caps) = e.get("capabilities").and_then(|x| x.as_array()) {
                for c in caps {
                    if let Some(s) = c.as_str() {
                        if let Some(m) = normalize_model(s) {
                            if !models.iter().any(|x: &String| canon_model(x) == canon_model(&m)) {
                                models.push(m);
                            }
                        }
                    }
                }
            }
            if models.is_empty() {
                if let Some(m) = normalize_model(show_name) {
                    models.push(m);
                }
            }

            if !period.is_empty() && !periods.contains(&period) {
                periods.push(period.clone());
            }
            profile.grant_units += grant_units;
            profile.used_units += used_units;
            profile.remaining_units += remaining_units;

            let ent_priority = e.get("priority").and_then(|x| x.as_i64()).unwrap_or(plan_priority);
            grants.push(PlanGrant {
                plan_id: plan_id.clone(),
                plan_name: plan_name.clone(),
                kind,
                entitlement_id: e
                    .get("entitlement_id")
                    .and_then(|x| x.as_str())
                    .map(str::to_string),
                period,
                models,
                grant_units,
                used_units,
                remaining_units,
                priority: ent_priority,
                ends_at: plan_ends_at,
                effective_at: e.get("effective_at").and_then(|x| x.as_i64()).unwrap_or(0),
            });
        }
    }

    // 模型并集
    let mut models: Vec<String> = Vec::new();
    for g in &grants {
        for m in &g.models {
            if !models.iter().any(|x| canon_model(x) == canon_model(m)) {
                models.push(m.clone());
            }
        }
    }
    profile.servable_models = models;

    // 形态排序：燃烧顺序最靠前的当主口径
    kinds.sort_by_key(|k| k.burn_rank());
    profile.plan_kind = kinds.first().copied().unwrap_or(PlanKind::Other);
    profile.kinds = kinds;

    // 时段口径：只有夜间档 → night_only；否则按日内
    let has_day = periods
        .iter()
        .any(|p| !is_night_period(p) && !p.is_empty());
    let has_night = periods.iter().any(|p| is_night_period(p));
    profile.night_only = has_night && !has_day;
    profile.quota_period = if profile.night_only {
        "daily_night".to_string()
    } else if has_day {
        "daily".to_string()
    } else {
        periods.first().cloned().unwrap_or_default()
    };

    profile.grants = grants;
    profile
}

/// 画像 → 网关池内门闸（`api_server::pool::PlanGate`）。
///
/// 唯一转换点：池内只存"判定所需的最小集合"，不持有整个画像 JSON，
/// 每轮 5s 更新也只拷这几个字段。
pub fn gate_of(p: &PlanProfile) -> crate::api_server::pool::PlanGate {
    crate::api_server::pool::PlanGate {
        kind: p.kind_tag().to_string(),
        kinds: p
            .kinds
            .iter()
            .map(|k| k.as_str().to_string())
            .collect::<Vec<_>>()
            .join(","),
        models: p
            .servable_models
            .iter()
            .map(|m| m.trim().to_lowercase())
            .collect::<Vec<_>>()
            .join(","),
        period: p.quota_period.clone(),
        night_only: p.night_only,
        grant_units: p.grant_units,
        remaining_units: p.remaining_units,
        ends_at: p.ends_at,
        synced_at: chrono::Utc::now().timestamp(),
    }
}

/// 仅凭 `billing/balance` 的 `data.plans[]` 归一化（无 capabilities 时的降级路径）。
/// 模型集由 entitlement 的 `show_name` 推断（"GLM-5.3" / "GLM-5.3-Flash"）。
pub fn normalize_balance(v: &Value) -> PlanProfile {
    normalize(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classify_referral_is_global_build_not_start_plan() {
        // 关键陷阱：referral 的 plan_id 也以 zcode-v3-start-plan- 开头
        assert_eq!(
            classify("zcode-v3-start-plan-referral", "ZCode Global Build"),
            PlanKind::GlobalBuild
        );
        assert_eq!(
            classify("zcode-v3-start-plan-0817", "ZCode Start Plan"),
            PlanKind::StartPlan
        );
        assert_eq!(classify("whatever", "夜間蹬蹬卡"), PlanKind::TaskCard);
        assert_eq!(classify("promo-new_user-gift", "新号见面礼"), PlanKind::NewAccount);
    }

    #[test]
    fn burn_order_matches_user_choice() {
        assert!(PlanKind::NewAccount.burn_rank() < PlanKind::TaskCard.burn_rank());
        assert!(PlanKind::TaskCard.burn_rank() < PlanKind::GlobalBuild.burn_rank());
        assert!(PlanKind::GlobalBuild.burn_rank() < PlanKind::StartPlan.burn_rank());
    }

    #[test]
    fn night_window_wraps_midnight() {
        // 实测窗口 23:00 → 次日 09:00
        assert!(in_night_window(23));
        assert!(in_night_window(0));
        assert!(in_night_window(8));
        assert!(!in_night_window(9));
        assert!(!in_night_window(12));
        assert!(!in_night_window(14)); // 实测取证时刻 14:42 CST
        assert!(!in_night_window(22));
    }

    #[test]
    fn normalize_start_plan_sample() {
        // 实测样本（docs/pending/ZCode套餐池接入方案.md §3.5）
        let v = json!({"code":0,"data":{
            "plan_id":"zcode-v3-start-plan-0817","name":"ZCode Start Plan",
            "priority":90,"status":"active","ends_at":1792803600i64,
            "entitlements":[{"entitlement_id":"ent_2_0817_glm_5p3","show_name":"GLM-5.3",
              "meter":"model_usage","unit_type":"token",
              "capabilities":["model:glm-5.3-flash"],
              "grant_units":3000000i64,"period":"daily","priority":100}]
        }});
        let p = normalize(&v);
        assert_eq!(p.plan_kind, PlanKind::StartPlan);
        assert_eq!(p.servable_models, vec!["GLM-5.3-Flash".to_string()]);
        assert_eq!(p.grant_units, 3_000_000);
        assert_eq!(p.quota_period, "daily");
        assert!(!p.night_only);
        assert!(p.serves_now("glm-5.3-flash", 0, 12));
        // 只覆盖 Flash，不能服务 GLM-5.3
        assert!(!p.serves_now("GLM-5.3", 0, 12));
    }

    #[test]
    fn normalize_multi_plan_account_merges_grants() {
        // id=30 的真实形态：Start Plan(daily, GLM-5.3) + Global Build(daily_night, Flash)
        let v = json!({"data":{"plans":[
            {"plan_id":"zcode-v3-start-plan-0817","name":"ZCode Start Plan","ends_at":1792803600i64,
             "entitlements":[{"show_name":"GLM-5.3","grant_units":3000000i64,"period":"daily",
                              "capabilities":["model:glm-5.3"]}]},
            {"plan_id":"zcode-v3-start-plan-referral","name":"ZCode Global Build","ends_at":1792803600i64,
             "entitlements":[{"show_name":"GLM-5.3-Flash","grant_units":100000000i64,"period":"daily_night",
                              "capabilities":["model:glm-5.3-flash"]}]}
        ]}});
        let p = normalize(&v);
        assert_eq!(p.plan_ids.len(), 2);
        assert_eq!(p.plan_kind, PlanKind::GlobalBuild);
        assert!(p.kinds.contains(&PlanKind::StartPlan));
        assert_eq!(p.servable_models.len(), 2);
        assert!(!p.night_only);
        // 白天：只有 GLM-5.3 可用（夜间档的 Flash 不在窗口内）
        assert!(p.serves_now("GLM-5.3", 0, 12));
        assert!(!p.serves_now("GLM-5.3-Flash", 0, 12));
        // 夜间：两者都行
        assert!(p.serves_now("GLM-5.3-Flash", 0, 23));
    }

    #[test]
    fn no_plan_means_not_servable() {
        let v = json!({"code":0,"data":{"balances":[],"plans":[]}});
        let p = normalize(&v);
        assert!(!p.is_servable());
        assert!(p.servable_models.is_empty());
        assert_eq!(p.plan_kind, PlanKind::Other);
    }

    #[test]
    fn exhausted_grant_is_skipped() {
        let v = json!({"data":{"plans":[{"plan_id":"zcode-v3-start-plan-0817","name":"ZCode Start Plan",
            "entitlements":[{"show_name":"GLM-5.3","grant_units":3000000i64,"used_units":3000000i64,
                             "period":"daily","capabilities":["model:glm-5.3"]}]}]}});
        let p = normalize(&v);
        assert!(p.is_servable());
        assert_eq!(p.remaining_units, 0);
        // 额度耗尽 → 当前不可选
        assert!(!p.serves_now("GLM-5.3", 0, 12));
    }

    #[test]
    fn expired_plan_is_skipped() {
        let v = json!({"data":{"plans":[{"plan_id":"zcode-v3-start-plan-0817","name":"ZCode Start Plan",
            "ends_at":1000i64,
            "entitlements":[{"show_name":"GLM-5.3","grant_units":3000000i64,
                             "period":"daily","capabilities":["model:glm-5.3"]}]}]}});
        let p = normalize(&v);
        assert!(p.serves_now("GLM-5.3", 0, 12));
        assert!(!p.serves_now("GLM-5.3", 2000, 12));
    }

    /// 真实响应回归（id=30，2026-10-23 14:42 CST 抓取，原文裁剪）
    #[test]
    fn normalize_real_id30_balance() {
        let v = json!({"code":0,"data":{"server_time":1792737762i64,"plans":[
            {"plan_id":"zcode-v3-start-plan-referral","name":"ZCode Global Build",
             "priority":110,"status":"active","starts_at":1792733648i64,"ends_at":1792803600i64,
             "entitlements":[{"entitlement_id":"ent-zcode-v3-start-plan-referral-1",
               "show_name":"GLM-5.3-Flash","meter":"model_usage","unit_type":"token",
               "capabilities":["model:glm-5.3-flash"],"grant_units":100000000i64,
               "period":"daily_night","priority":100,"effective_at":1792767600i64}]},
            {"plan_id":"zcode-v3-start-plan-0817","name":"ZCode Start Plan",
             "priority":90,"status":"active","starts_at":1792733606i64,"ends_at":1793116799i64,
             "entitlements":[
               {"entitlement_id":"ent_2_0817_glm_5p3","show_name":"GLM-5.3",
                "capabilities":["model:glm-5.3"],"grant_units":3000000i64,
                "period":"daily","priority":110,"effective_at":0},
               {"entitlement_id":"ent_2_0817_glm_5p3f","show_name":"GLM-5.3-Flash",
                "capabilities":["model:glm-5.3-flash"],"grant_units":5000000i64,
                "period":"daily","priority":80,"effective_at":0}]}],
          "balances":[
            {"plan_id":"zcode-v3-start-plan-0817","entitlement_id":"ent_2_0817_glm_5p3",
             "total_units":3000000i64,"used_units":0,"remaining_units":3000000i64,
             "period_start":1792684800i64,"period_end":1792771199i64},
            {"plan_id":"zcode-v3-start-plan-0817","entitlement_id":"ent_2_0817_glm_5p3f",
             "total_units":5000000i64,"used_units":160730i64,"remaining_units":4839270i64,
             "period_start":1792684800i64,"period_end":1792771199i64}]}});

        let p = normalize(&v);
        assert_eq!(p.plan_ids.len(), 2);
        assert_eq!(p.plan_kind, PlanKind::GlobalBuild);
        assert_eq!(p.kinds, vec![PlanKind::GlobalBuild, PlanKind::StartPlan]);
        assert_eq!(p.servable_models.len(), 2);
        assert!(!p.night_only);
        assert_eq!(p.quota_period, "daily");
        // balances[] 对齐：Start Plan 的 Flash 已用 160,730
        assert_eq!(p.used_units, 160_730);
        // 夜间档未生效（effective_at=23:00）→ 剩余按全额算，合计 1e8 + 3e6 + 4,839,270
        assert_eq!(p.remaining_units, 100_000_000 + 3_000_000 + 4_839_270);
        // 白天 14:42：GLM-5.3 走 Start Plan 的日内权益，可服务
        assert!(p.serves_now("GLM-5.3", 1792737762, 14));
        // Flash 白天同样可用——不是因为夜间档（未到点），而是因为 Start Plan 里
        // 也有一条 Flash 的日内权益；这正是「按 entitlement 而不是按账号判定」的意义
        assert!(p.serves_now("glm-5.3-flash", 1792737762, 14));
        // 夜间：两者都行
        assert!(p.serves_now("GLM-5.3-Flash", 1792737762, 23));
    }

    /// 真实响应回归（id=17：只有夜间档，且 effective_at 未到 → 白天完全不可用）
    #[test]
    fn normalize_real_id17_night_only() {
        let v = json!({"code":0,"data":{"server_time":1792737763i64,"plans":[
            {"plan_id":"zcode-v3-start-plan-referral","name":"ZCode Global Build",
             "priority":110,"status":"active","ends_at":1792803600i64,
             "entitlements":[{"entitlement_id":"ent-zcode-v3-start-plan-referral-1",
               "show_name":"GLM-5.3-Flash","capabilities":["model:glm-5.3-flash"],
               "grant_units":100000000i64,"period":"daily_night","priority":100,
               "effective_at":1792767600i64}]}],
          "balances":[]}});
        let p = normalize(&v);
        assert_eq!(p.plan_kind, PlanKind::GlobalBuild);
        assert!(p.night_only);
        assert_eq!(p.quota_period, "daily_night");
        assert!(p.is_servable()); // 权益是有的，只是时段没到
        // 白天 14:42：不可用（effective_at=23:00 未到）
        assert!(!p.serves_now("GLM-5.3-Flash", 1792737762, 14));
        // 生效后 23:00：可用
        assert!(p.serves_now("GLM-5.3-Flash", 1792767600, 23));
        // 次日 09:00 到期后：不可用
        assert!(!p.serves_now("GLM-5.3-Flash", 1792803600, 23));
    }
}
