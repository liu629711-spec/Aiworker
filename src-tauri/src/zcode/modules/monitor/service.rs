use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::SqlitePool;
use tauri::AppHandle;

use crate::zcode::api::ZaiClient;
use crate::zcode::credentials::{self, LocalCredentials};
use crate::zcode::db::{models, repo};

use super::events::{self, ClaimDetectedPayload};

/// 余额轮询间隔(需求:每 5 秒刷新各账号剩余用量)
pub const BALANCE_INTERVAL: Duration = Duration::from_secs(5);
/// 活动检测间隔(需求:每 10 分钟检测一次 claimable)
pub const PREVIEW_INTERVAL: Duration = Duration::from_secs(600);
/// 多账号连续请求之间的小错峰,避免同刻并发打接口
const PER_ACCOUNT_GAP: Duration = Duration::from_millis(500);

/// 探测顺序与卡片展示排序一致:使用中 → 有套餐(剩余总量多者靠前) → 无套餐,同级按 id。
/// 排名依据共享快照里已有的数据(启动时为恢复的旧快照),快照里没有的账号排最后。
fn sort_probe_order(shared: &MonitorShared, accounts: &mut [models::Account]) {
    let snap = shared.snapshot.lock().unwrap().clone();
    let rank = |id: i64| -> (u8, i64, i64) {
        match snap.accounts.iter().find(|a| a.id == id) {
            Some(live) => {
                let remaining: i64 = live.balances.iter().map(|b| b.remaining_units).sum();
                let r = if live.is_current {
                    0
                } else if !live.plans.is_empty() {
                    1
                } else {
                    2
                };
                (r, -remaining, id)
            }
            None => (2, 0, id),
        }
    };
    accounts.sort_by_key(|a| rank(a.id));
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BalanceItem {
    pub show_name: String,
    /// 归属套餐的 entitlement(跨套餐同名模型靠它区分;旧快照无此字段)
    #[serde(default)]
    pub entitlement_id: Option<String>,
    #[serde(default)]
    pub bucket_id: Option<String>,
    pub total_units: i64,
    pub used_units: i64,
    pub remaining_units: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntitlementInfo {
    #[serde(default)]
    pub entitlement_id: Option<String>,
    pub show_name: String,
    pub grant_units: i64,
    pub period: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanInfo {
    pub plan_id: String,
    pub name: String,
    /// 套餐到期时间(unix 秒;活动套餐多为当夜 24 点,日套餐为当日)
    #[serde(default)]
    pub ends_at: Option<i64>,
    pub entitlements: Vec<EntitlementInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaimablePlan {
    pub plan_id: String,
    pub name: String,
}

/// 单个账号的实时状态(快照的一行)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountLive {
    pub id: i64,
    pub label: String,
    /// 自定义昵称(user_info.name,与邮箱不同时才有)
    pub nickname: Option<String>,
    /// 登录邮箱(user_info.email;早期数据缺失时退化用 label 本身是邮箱的情况)
    #[serde(default)]
    pub email: Option<String>,
    /// 邀请关系标识:inviter/invited,未标记为 None(旧快照缺省)
    #[serde(default)]
    pub invite_role: Option<String>,
    /// 被邀请者归属的邀请者账号 id(绑定关系;旧快照缺省)
    #[serde(default)]
    pub invited_by: Option<i64>,
    pub is_current: bool,
    /// 套餐(来自 balance 接口的 data.plans)
    pub plans: Vec<PlanInfo>,
    /// 各模型用量(来自 balance 接口的 data.balances)
    pub balances: Vec<BalanceItem>,
    /// 当前可领活动(来自 preview 接口)
    pub claimables: Vec<ClaimablePlan>,
    pub balance_error: Option<String>,
    pub updated_at: Option<i64>,
}

/// 全局实时快照:两条轮询协程更新,事件广播给所有窗口
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Snapshot {
    pub accounts: Vec<AccountLive>,
    /// 本机当前登录账号显示名(即使未导入也展示,悬浮窗提示用)
    pub current_label: Option<String>,
    pub last_balance_at: Option<i64>,
    pub last_preview_at: Option<i64>,
}

/// 检测器句柄:随 AppState 全局共享,轮询协程挂在 tokio 上,窗口关闭不影响运行。
#[derive(Clone)]
pub struct MonitorHandle {
    shared: Arc<MonitorShared>,
}

struct MonitorShared {
    db: SqlitePool,
    http: ZaiClient,
    snapshot: Mutex<Snapshot>,
    /// 已提醒过的活动 plan_id,用于识别"新"活动
    known_claimables: Mutex<HashMap<i64, HashSet<String>>>,
    running: AtomicBool,
    /// 一轮余额刷新是否进行中(防重入,也供前端按钮 loading)
    refreshing: AtomicBool,
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// app_kv 中快照的存储键
const KV_SNAPSHOT_KEY: &str = "monitor_snapshot";

/// 内存快照为空时从 app_kv 恢复并回种。轮询协程启动早于前端首取快照,
/// 探测排序需要排名数据,所以每轮开头都调它(空才恢复,常态零开销)
async fn ensure_seeded(shared: &MonitorShared) {
    {
        let s = shared.snapshot.lock().unwrap();
        if !s.accounts.is_empty() {
            return;
        }
    }
    if let Some(snap) = load_snapshot(&shared.db).await {
        let mut s = shared.snapshot.lock().unwrap();
        if s.accounts.is_empty() {
            *s = snap;
        }
    }
}

/// 把当前快照 UPSERT 进 app_kv(每次数据变更后调用;退出时再兜底一次)
async fn persist_snapshot(shared: &MonitorShared) {
    let snap = shared.snapshot.lock().unwrap().clone();
    let Ok(json) = serde_json::to_string(&snap) else { return };
    let _ = sqlx::query(
        "INSERT INTO app_kv (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
    )
    .bind(KV_SNAPSHOT_KEY)
    .bind(json)
    .bind(now())
    .execute(&shared.db)
    .await;
}

/// 从 app_kv 读取上次保存的快照,过滤掉已删除的账号
async fn load_snapshot(db: &SqlitePool) -> Option<Snapshot> {
    let (json,): (String,) = sqlx::query_as("SELECT value FROM app_kv WHERE key = ?1")
        .bind(KV_SNAPSHOT_KEY)
        .fetch_optional(db)
        .await
        .ok()??;
    let mut snap: Snapshot = serde_json::from_str(&json).ok()?;
    let live_ids: HashSet<i64> = repo::list(db)
        .await
        .ok()?
        .into_iter()
        .map(|a| a.id)
        .collect();
    snap.accounts.retain(|a| live_ids.contains(&a.id));
    Some(snap)
}

fn account_matches(a: &models::Account, l: &LocalCredentials) -> bool {
    if !a.jwt.is_empty() && a.jwt == l.jwt {
        return true;
    }
    l.user_id
        .as_deref()
        .is_some_and(|u| a.user_id.as_deref() == Some(u))
}

/// 从账号的 user_info JSON 提取昵称;与 label 相同或缺失时返回 None
fn nickname_of(account: &models::Account) -> Option<String> {
    let raw = account.user_info.as_deref()?;
    let v: Value = serde_json::from_str(raw).ok()?;
    let name = v["name"].as_str()?.trim();
    if name.is_empty() || name == account.label {
        return None;
    }
    Some(name.to_string())
}

/// 从账号的 user_info JSON 提取登录邮箱;user_info 缺失/无 email 时,
/// 退化用 label 本身是邮箱的情况(早期导入的 label 常直接是邮箱)
fn email_of(account: &models::Account) -> Option<String> {
    if let Some(raw) = account.user_info.as_deref() {
        if let Ok(v) = serde_json::from_str::<Value>(raw) {
            if let Some(e) = v["email"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
                return Some(e.to_string());
            }
        }
    }
    account
        .label
        .contains('@')
        .then(|| account.label.clone())
}

impl MonitorHandle {
    pub fn new(db: SqlitePool, http: ZaiClient) -> Self {
        Self {
            shared: Arc::new(MonitorShared {
                db,
                http,
                snapshot: Mutex::new(Snapshot::default()),
                known_claimables: Mutex::new(HashMap::new()),
                running: AtomicBool::new(false),
                refreshing: AtomicBool::new(false),
            }),
        }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.shared.snapshot.lock().unwrap().clone()
    }

    /// 启动取快照:内存快照为空时恢复上次保存的(并回种内存,供增量轮询在此基础上合并),
    /// 实现"重开应用立即显示上次数据,后台再逐账号探测刷新"
    pub async fn snapshot_seeded(&self) -> Snapshot {
        ensure_seeded(&self.shared).await;
        self.snapshot()
    }

    /// 把当前内存快照写入 app_kv(退出兜底用)
    pub async fn persist_now(&self) {
        persist_snapshot(&self.shared).await;
    }

    /// 启动两条轮询协程(幂等):余额每 5s,活动每 10min
    pub fn start(&self, app: &AppHandle) {
        if self.shared.running.swap(true, Ordering::SeqCst) {
            return;
        }
        let shared = Arc::clone(&self.shared);
        let app_balance = app.clone();
        tauri::async_runtime::spawn(async move {
            let mut ticker = tokio::time::interval(BALANCE_INTERVAL);
            // 一轮耗时超过间隔时跳过错过的 tick,不让轮次背靠背连发
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // 仅启动首轮广播刷新状态(按钮 loading/进度);之后 5s 自动轮询静默
            let mut first_round = true;
            loop {
                ticker.tick().await;
                if !shared.running.load(Ordering::SeqCst) {
                    break;
                }
                refresh_balances(&shared, &app_balance, first_round).await;
                first_round = false;
            }
        });
        let shared = Arc::clone(&self.shared);
        let app_preview = app.clone();
        tauri::async_runtime::spawn(async move {
            let mut ticker = tokio::time::interval(PREVIEW_INTERVAL);
            loop {
                ticker.tick().await;
                if !shared.running.load(Ordering::SeqCst) {
                    break;
                }
                refresh_previews(&shared, &app_preview).await;
            }
        });
    }

    pub fn stop(&self) {
        self.shared.running.store(false, Ordering::SeqCst);
    }

    /// 立即刷新全部账号余额(工具栏「刷新用量」按钮);逐账号探完即广播,无需等整轮
    pub async fn refresh_all(&self, app: &AppHandle) {
        refresh_balances(&self.shared, app, true).await;
    }

    /// 立即把指定账号标记为"使用中"(切换账号写回凭证后调用,不等下轮轮询),并广播快照
    pub fn set_current(&self, app: &AppHandle, account_id: i64) {
        {
            let mut s = self.shared.snapshot.lock().unwrap();
            for a in s.accounts.iter_mut() {
                a.is_current = a.id == account_id;
            }
        }
        let snapshot = self.shared.snapshot.lock().unwrap().clone();
        events::emit_snapshot(app, &snapshot);
        let shared = Arc::clone(&self.shared);
        tauri::async_runtime::spawn(async move { persist_snapshot(&shared).await });
    }

    /// 手动检测单个账号(卡片「检测」按钮/导入后):余额 + 活动,识别新活动并广播快照
    pub async fn refresh_account(&self, app: &AppHandle, account_id: i64) {
        let accounts = match repo::list(&self.shared.db).await {
            Ok(a) => a,
            Err(_) => return,
        };
        let Some(account) = accounts.iter().find(|a| a.id == account_id) else {
            return;
        };
        let local = credentials::load_local().ok();
        let current_id = local
            .as_ref()
            .and_then(|l| accounts.iter().find(|a| account_matches(a, l)).map(|a| a.id));

        let mut live = probe_account(&self.shared, account, current_id).await;

        if let Ok(v) = self
            .shared
            .http
            .get_preview(&account.jwt, &account.device_mid)
            .await
        {
            let plans = parse_claimables(&v);
            let detected = diff_and_remember(&self.shared, account, &plans);
            live.claimables = plans;
            for d in &detected {
                events::emit_claim_detected(app, d);
            }
        }

        {
            let mut s = self.shared.snapshot.lock().unwrap();
            match s.accounts.iter_mut().find(|a| a.id == account_id) {
                Some(slot) => *slot = live,
                None => {
                    s.accounts.push(live);
                    s.accounts.sort_by_key(|a| a.id);
                }
            }
            // 应用前重新比对一次本机凭证:探测长达数秒,期间可能发生切换,开头比对结果已过期
            let local = credentials::load_local().ok();
            let current_id = local
                .as_ref()
                .and_then(|l| accounts.iter().find(|a| account_matches(a, l)).map(|a| a.id));
            s.current_label = local.as_ref().and_then(|l| l.display_name.clone());
            // 归一化全量 is_current,以最新凭证比对为准(也修掉手动刷新非当前账号时误清"使用中"的问题)
            for a in s.accounts.iter_mut() {
                a.is_current = Some(a.id) == current_id;
            }
        }
        emit(&self.shared, app);
        persist_snapshot(&self.shared).await;
    }
}

fn emit(shared: &MonitorShared, app: &AppHandle) {
    let snapshot = shared.snapshot.lock().unwrap().clone();
    events::emit_snapshot(app, &snapshot);
}

/// 查单个账号余额;保留快照里已有的活动列表
async fn probe_account(
    shared: &MonitorShared,
    account: &models::Account,
    current_id: Option<i64>,
) -> AccountLive {
    let mut live = AccountLive {
        id: account.id,
        label: account.label.clone(),
        nickname: nickname_of(account),
        email: email_of(account),
        invite_role: account.invite_role.clone(),
        invited_by: account.invited_by,
        is_current: Some(account.id) == current_id,
        plans: vec![],
        balances: vec![],
        claimables: vec![],
        balance_error: None,
        updated_at: None,
    };
    match shared
        .http
        .get_balance(&account.jwt, &account.device_mid)
        .await
    {
        Ok(v) => {
            live.plans = parse_plans(&v);
            live.balances = parse_balances(&v);
            live.updated_at = Some(now());
        }
        Err(e) => live.balance_error = Some(e.to_string()),
    }
    live.claimables = shared
        .snapshot
        .lock()
        .unwrap()
        .accounts
        .iter()
        .find(|a| a.id == account.id)
        .map(|a| a.claimables.clone())
        .unwrap_or_default();
    live
}

/// 与已知活动比对,记录新 plan_id 并返回需要提醒的新活动
fn diff_and_remember(
    shared: &MonitorShared,
    account: &models::Account,
    plans: &[ClaimablePlan],
) -> Vec<ClaimDetectedPayload> {
    let mut known = shared.known_claimables.lock().unwrap();
    let entry = known.entry(account.id).or_default();
    let new_plans: Vec<ClaimablePlan> = plans
        .iter()
        .filter(|p| !entry.contains(&p.plan_id))
        .cloned()
        .collect();
    for p in plans {
        entry.insert(p.plan_id.clone());
    }
    if new_plans.is_empty() {
        return vec![];
    }
    vec![ClaimDetectedPayload {
        account_id: account.id,
        label: account.label.clone(),
        plans: new_plans,
    }]
}

/// report=false 时(后台 5s 轮询)不广播刷新状态,按钮不受影响
async fn refresh_balances(shared: &MonitorShared, app: &AppHandle, report: bool) {
    // 防重入:上一轮(含启动首轮)未结束时忽略新的刷新请求
    if shared
        .refreshing
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    if report {
        events::emit_refreshing(app, true, 0, 0);
    }
    refresh_balances_round(shared, app, report).await;
    shared.refreshing.store(false, Ordering::SeqCst);
    if report {
        events::emit_refreshing(app, false, 0, 0);
    }
}

async fn refresh_balances_round(shared: &MonitorShared, app: &AppHandle, report: bool) {
    // 排序前先确保快照已恢复(首轮探测早于前端首取快照,否则排名数据为空)
    ensure_seeded(shared).await;
    let mut accounts = match repo::list(&shared.db).await {
        Ok(a) => a,
        Err(_) => return,
    };
    sort_probe_order(shared, &mut accounts);

    // 增量模式:按序探完一个账号就合并进快照、广播、落库,前端随即更新对应卡片
    for (i, account) in accounts.iter().enumerate() {
        let mut item = probe_account(shared, account, None).await;

        // 每次合并前重新比对本机凭证:探测长达数秒,期间可能发生账号切换,
        // 凭证比对是"使用中"的唯一真相,不能沿用本轮开头的结果覆盖
        let local = credentials::load_local().ok();
        let current_id = local
            .as_ref()
            .and_then(|l| accounts.iter().find(|a| account_matches(a, l)).map(|a| a.id));
        item.is_current = Some(item.id) == current_id;
        // 标记当前账号入库,accounts_list 与悬浮窗都依赖
        if let Some(id) = current_id {
            let _ =
                sqlx::query("UPDATE accounts SET is_current = CASE WHEN id = ?1 THEN 1 ELSE 0 END")
                    .bind(id)
                    .execute(&shared.db)
                    .await;
        }

        {
            let mut s = shared.snapshot.lock().unwrap();
            match s.accounts.iter_mut().find(|a| a.id == item.id) {
                Some(slot) => *slot = item,
                None => {
                    s.accounts.push(item);
                    s.accounts.sort_by_key(|a| a.id);
                }
            }
            s.current_label = local.as_ref().and_then(|l| l.display_name.clone());
            s.last_balance_at = Some(now());
        }
        emit(shared, app);
        persist_snapshot(shared).await;
        if report {
            events::emit_refreshing(app, true, i + 1, accounts.len());
        }

        if i + 1 < accounts.len() {
            tokio::time::sleep(PER_ACCOUNT_GAP).await;
        }
    }

    // 收尾:清掉已删除账号的残留卡片;补探测期间新导入账号的骨架行(防其卡片闪失)
    if let Ok(all) = repo::list(&shared.db).await {
        let live_ids: HashSet<i64> = all.iter().map(|a| a.id).collect();
        let mut changed = false;
        {
            let mut s = shared.snapshot.lock().unwrap();
            let before = s.accounts.len();
            s.accounts.retain(|a| live_ids.contains(&a.id));
            changed |= before != s.accounts.len();
            for a in &all {
                if !s.accounts.iter().any(|x| x.id == a.id) {
                    let nickname = nickname_of(a);
                    s.accounts.push(AccountLive {
                        id: a.id,
                        label: a.label.clone(),
                        nickname,
                        email: email_of(a),
                        invite_role: a.invite_role.clone(),
                        invited_by: a.invited_by,
                        is_current: false,
                        plans: vec![],
                        balances: vec![],
                        claimables: vec![],
                        balance_error: None,
                        updated_at: None,
                    });
                    changed = true;
                }
            }
            s.accounts.sort_by_key(|a| a.id);
        }
        if changed {
            emit(shared, app);
            persist_snapshot(shared).await;
        }
    }
}

async fn refresh_previews(shared: &MonitorShared, app: &AppHandle) {
    ensure_seeded(shared).await;
    let mut accounts = match repo::list(&shared.db).await {
        Ok(a) => a,
        Err(_) => return,
    };
    sort_probe_order(shared, &mut accounts);

    // 快照里缺失的账号先补骨架行,保证 claimables 有落点
    {
        let mut s = shared.snapshot.lock().unwrap();
        for a in &accounts {
            if !s.accounts.iter().any(|x| x.id == a.id) {
                s.accounts.push(AccountLive {
                    id: a.id,
                    label: a.label.clone(),
                    nickname: nickname_of(a),
                    email: email_of(a),
                    invite_role: a.invite_role.clone(),
                    invited_by: a.invited_by,
                    is_current: false,
                    plans: vec![],
                    balances: vec![],
                    claimables: vec![],
                    balance_error: None,
                    updated_at: None,
                });
            }
        }
    }

    let mut detected_all: Vec<ClaimDetectedPayload> = Vec::new();
    for (i, account) in accounts.iter().enumerate() {
        if let Ok(v) = shared
            .http
            .get_preview(&account.jwt, &account.device_mid)
            .await
        {
            let plans = parse_claimables(&v);
            detected_all.extend(diff_and_remember(shared, account, &plans));
            {
                let mut s = shared.snapshot.lock().unwrap();
                if let Some(slot) = s.accounts.iter_mut().find(|x| x.id == account.id) {
                    slot.claimables = plans;
                }
            }
            // 探完一个广播+落库一个
            emit(shared, app);
            persist_snapshot(shared).await;
        }
        if i + 1 < accounts.len() {
            tokio::time::sleep(PER_ACCOUNT_GAP).await;
        }
    }

    shared.snapshot.lock().unwrap().last_preview_at = Some(now());
    emit(shared, app);
    persist_snapshot(shared).await;
    for d in &detected_all {
        events::emit_claim_detected(app, d);
    }
}

fn parse_balances(v: &Value) -> Vec<BalanceItem> {
    v["data"]["balances"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|b| {
                    Some(BalanceItem {
                        show_name: b["show_name"].as_str()?.to_string(),
                        entitlement_id: b["entitlement_id"]
                            .as_str()
                            .map(str::to_string),
                        bucket_id: b["bucket_id"].as_str().map(str::to_string),
                        total_units: b["total_units"].as_i64().unwrap_or(0),
                        used_units: b["used_units"].as_i64().unwrap_or(0),
                        remaining_units: b["remaining_units"].as_i64().unwrap_or(0),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_plans(v: &Value) -> Vec<PlanInfo> {
    v["data"]["plans"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    Some(PlanInfo {
                        plan_id: p["plan_id"].as_str()?.to_string(),
                        name: p["name"].as_str().unwrap_or("?").to_string(),
                        ends_at: p["ends_at"].as_i64(),
                        entitlements: p["entitlements"]
                            .as_array()
                            .map(|es| {
                                es.iter()
                                    .filter_map(|e| {
                                        Some(EntitlementInfo {
                                            entitlement_id: e["entitlement_id"]
                                                .as_str()
                                                .map(str::to_string),
                                            show_name: e["show_name"].as_str()?.to_string(),
                                            grant_units: e["grant_units"].as_i64().unwrap_or(0),
                                            period: e["period"].as_str().unwrap_or("?").to_string(),
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn parse_claimables(v: &Value) -> Vec<ClaimablePlan> {
    v["data"]["plans"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    Some(ClaimablePlan {
                        plan_id: p["plan_id"].as_str()?.to_string(),
                        name: p["name"].as_str().unwrap_or("限时活动").to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}
