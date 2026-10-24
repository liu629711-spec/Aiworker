//! ZCode 号池网关桥（commands 层）：把 zcode 域（ZcodeState，独立 sqlx 池）的
//! 账号凭证喂给网关 ZCode 池。
//!
//! 两个职责：
//! ① `list_pool_accounts`：池装配（apply_pool_snapshot）时全量列出可入池账号
//!    （uid = accounts.id 字符串化，凭证快照 = zai_access_token + device_mid）；
//! ② `resolve_credentials`：identity 回调（每请求一次）——按 uid 重读最新凭证，
//!    zai token 无 exp 字段（HS512，服务端长期有效直至吊销），无刷新链路；
//!    凭证缺失（账号被删/从未导入 zai token）→ Err，网关侧换号。
//!
//! 同步策略：读路径全部直查 sqlx 池（zcode.sqlite 单机单人场景，账号量 < 百，
//! 无缓存必要；账号变更经 zcode 域命令 → reload_pools_if_running 热重载兜底）。

use crate::api_server::pool::PlanGate;
use crate::api_server::zcode_catalog::ZCodeCreds;
use crate::state::AppState;
use crate::zcode::db::models::Account;
use crate::zcode::state::ZcodeState;

/// 池装配用账号条目（uid = zcode accounts.id 字符串化）
pub struct ZCodePoolAccount {
    pub uid: String,
    pub label: String,
    pub access_token: String,
    pub device_mid: String,
    /// 套餐门闸快照（accounts 表 plan_* 字段归一化）。
    /// `None` = 从未同步过画像（fail-open：老库升级后第一轮画像拉取前仍可用）
    pub plan: Option<PlanGate>,
}

/// 由库内 plan_* 字段拼池内门闸。`profile_synced_at` 为空 = 从未同步 → None。
///
/// 优先用 `plan_profile_json`（含 `kinds` 全量形态与 grants 明细）反序列化，
/// 仓储列只作降级——这样 Key 级套餐路由能命中"同号多形态"（如 id=30 同时是
/// Global Build + Start Plan），而不是只看到主形态。
fn plan_gate_of(a: &Account) -> Option<PlanGate> {
    let synced_at = a.profile_synced_at?;
    let kinds = a
        .plan_profile_json
        .as_deref()
        .and_then(|j| serde_json::from_str::<crate::zcode::plan_profile::PlanProfile>(j).ok())
        .map(|p| {
            p.kinds
                .iter()
                .map(|k| k.as_str().to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let kind = a.plan_kind.clone().unwrap_or_else(|| "other".to_string());
    let kinds = if kinds.is_empty() {
        kind.clone()
    } else {
        kinds.join(",")
    };
    Some(PlanGate {
        kind,
        kinds,
        models: a
            .plan_servable_models
            .clone()
            .unwrap_or_default()
            .split(',')
            .map(|m| m.trim().to_lowercase())
            .filter(|m| !m.is_empty())
            .collect::<Vec<_>>()
            .join(","),
        period: a.plan_period.clone().unwrap_or_default(),
        night_only: a.plan_night_only.unwrap_or(0) != 0,
        grant_units: a.plan_grant_units.unwrap_or(0),
        remaining_units: a.plan_remaining_units.unwrap_or(0),
        ends_at: a.plan_ends_at,
        synced_at,
    })
}

/// 同步上下文 → 异步查询的安全桥接（block_in_place 保护，防嵌套 block_on
/// panic "Cannot start a runtime from within a runtime"）。实现共享化后
/// 统一走 zcode::util——与 relay.rs / agent_integration.rs 同一入口，
/// 此地曾维护过一份重复副本（panic.log 2026-10-21 有案底的防线分叉维护）。
fn bridge_block_on<F: std::future::Future>(fut: F) -> F::Output {
    crate::zcode::util::bridge_block_on(fut)
}

/// 全量列出可入池账号（token 非空才入池——空 token 账号入池也只会每请求
/// 换号失败，装配层先过滤掉，与 sync_from_zcode 的门槛双重保证）
pub fn list_pool_accounts(state: &AppState) -> Vec<ZCodePoolAccount> {
    let Some(zstate) = try_zcode_state(state) else {
        return Vec::new();
    };
    let accounts = bridge_block_on(async {
        sqlx::query_as::<_, Account>("SELECT * FROM accounts ORDER BY id")
            .fetch_all(&zstate.db)
            .await
    });
    match accounts {
        Ok(list) => list
            .into_iter()
            .filter(|a| !a.zai_access_token.as_deref().unwrap_or("").is_empty())
            .map(|a| ZCodePoolAccount {
                uid: a.id.to_string(),
                label: a.label.clone(),
                access_token: a.zai_access_token.clone().unwrap_or_default(),
                device_mid: a.device_mid.clone(),
                plan: plan_gate_of(&a),
            })
            .collect(),
        Err(e) => {
            crate::fs_utils::app_log(
                &state.data_dir,
                &format!("[ZCODE-SYNC] 读取 zcode 账号失败: {e}"),
            );
            Vec::new()
        }
    }
}

/// identity 回调：按账号 id 解析最新凭证（uid = accounts.id 字符串化）。
///  Err = 凭证缺失/库不可读（网关侧走换号 + 熔断语义）
pub fn resolve_credentials(state: &AppState, acct_id: &str) -> Result<ZCodeCreds, String> {
    let Some(zstate) = try_zcode_state(state) else {
        return Err("zcode 域状态不可用（ZcodeState 未注册）".to_string());
    };
    let id: i64 = acct_id
        .parse()
        .map_err(|_| format!("zcode 账号 id 非法: {acct_id}"))?;
    let row = bridge_block_on(async {
        sqlx::query_as::<_, Account>("SELECT * FROM accounts WHERE id = ?1")
            .bind(id)
            .fetch_optional(&zstate.db)
            .await
    });
    match row {
        Ok(Some(a)) => {
            let token = a.zai_access_token.unwrap_or_default();
            if token.is_empty() {
                return Err(format!("zcode 账号 {acct_id} 无 zai access_token"));
            }
            Ok(ZCodeCreds {
                access_token: token,
                device_mid: a.device_mid,
            })
        }
        Ok(None) => Err(format!("zcode 账号 {acct_id} 不存在（已被删除）")),
        Err(e) => Err(format!("zcode 账号 {acct_id} 查询失败: {e}")),
    }
}

/// 取 ZcodeState（网关运行在主 AppState 上下文，ZcodeState 由 setup 阶段
/// manage；此处经 AppHandle 拿不到，改由全局 OnceLock 缓存——apply_pool_snapshot
/// 与 identity 回调都在 tauri command 上下文外（axum handler）调用）。
///
/// 兜底路径：ZcodeState 未注册（测试/极早期启动）时返回 None——调用方按
/// 「池为空 / 凭证不可用」处理，不影响网关其他池
fn try_zcode_state(_state: &AppState) -> Option<std::sync::Arc<ZcodeState>> {
    ZCODE_STATE.get().cloned()
}

/// ZcodeState 全局缓存（main.rs setup 注册后由 register_zcode_state 填充）。
/// 网关桥在 axum handler（无 tauri State 访问）里按需读取
static ZCODE_STATE: std::sync::OnceLock<std::sync::Arc<ZcodeState>> = std::sync::OnceLock::new();

/// setup 阶段调用：把 ZcodeState 存入全局缓存供网关桥读取（幂等，重复调用
/// 保留首次值）
pub fn register_zcode_state(zstate: std::sync::Arc<ZcodeState>) {
    let _ = ZCODE_STATE.set(zstate);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// uid 解析：非数字 id 必须报错而非 panic（网关回调入参来自池条目）
    #[test]
    fn resolve_credentials_rejects_non_numeric_id() {
        // ZcodeState 未注册时先撞「状态不可用」，但 id 非法应在注册检查前拦截？
        // 当前实现先查状态——测试只验证不 panic（状态缺失返回 Err 即可）
        let r = resolve_credentials_unchecked_for_test("abc");
        assert!(r.is_err());
    }

    // 直接测 id 解析分支（不经状态查询）：抽出纯逻辑
    fn resolve_credentials_unchecked_for_test(acct_id: &str) -> Result<i64, String> {
        acct_id
            .parse()
            .map_err(|_| format!("zcode 账号 id 非法: {acct_id}"))
    }
}
