//! 账号健康状态机（2026-10-18 账号态生命周期编排）。
//!
//! 目的：把「账号凭证坏了」从「等用户发现」变成「工具自己判断 + 分级处置」。
//! 判定完全基于本地已有信号（token store 到期时间 / needs_relogin 标记 /
//! 上次成功调用时刻），不做额外网络探测，因此可在列表刷新等热路径调用。
//!
//! 状态语义（单向恶化，只能由刷新成功或 OAuth 重登回升）：
//! - `Healthy`  ：凭证有效、近期未观测到失败
//! - `Expiring` ：access token 24h 内过期——**可自愈**，下轮续期自动换新
//! - `Expired`  ：access token 已过期但 refresh token 尚存——**可自愈**，调用时自动刷新
//! - `Dead`     ：服务端吊销（needs_relogin）——**不可自愈**，唯一出路是 OAuth 重扫
//!
//! 处置分级（`disposition`）：
//! - `AutoRecover`：无视，等自动刷新
//! - `Warn`      ：界面提示，不阻断调度
//! - `Skip`      ：调度跳过该账号（省掉对死凭证的 ~39s 拖延请求）
//! - `Block`     ：彻底移出可用池，需人工介入（OAuth 重扫）——同时推送通知
//!
//! 设计取舍：不引入新的持久化状态字段，避免与既有 needs_relogin 双写打架；
//! 状态由原始信号实时推导，跨进程一致。

use serde::Serialize;

/// 账号可用性状态（对外序列化为 snake_case 字符串，前端直接展示）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountHealth {
    Healthy,
    Expiring,
    Expired,
    Dead,
    /// 无任何凭证（从未入池或凭证被清空）
    NoCredential,
}

/// 该状态下调度层应采取的动作
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Disposition {
    /// 正常参与调度
    Allow,
    /// 可参与调度但界面预警（临期/过期仍有 refresh token）
    Warn,
    /// 调度跳过（死凭证：请求必失败且耗时长）
    Skip,
    /// 需人工介入（用户必须重扫 OAuth）
    Manual,
}

/// 状态判定结果
#[derive(Debug, Clone, Serialize)]
pub struct HealthVerdict {
    pub health: AccountHealth,
    pub disposition: Disposition,
    /// access token 剩余小时数（负值＝已过期；None＝无到期信息）
    pub ttl_hours: Option<f64>,
    /// 面向用户的一句话说明（界面 tooltip / 通知正文）
    pub hint: String,
}

/// 临期阈值（小时）：低于该值提示「即将过期」，但下轮续期会处理
pub const EXPIRING_HOURS: f64 = 24.0;

/// 状态推导（纯函数，便于单测）。
///
/// 参数含义：
/// - `has_credential`：token store 中该账号是否有记录
/// - `needs_relogin`：是否已被标记「服务端吊销」
/// - `expires_at_ms`：access token 到期时刻（Unix 毫秒，None＝未知）
/// - `now_ms`：当前时刻（Unix 毫秒，注入便于测试）
pub fn classify(
    has_credential: bool,
    needs_relogin: bool,
    expires_at_ms: Option<i64>,
    now_ms: i64,
) -> HealthVerdict {
    let ttl_hours = expires_at_ms.map(|ms| (ms - now_ms) as f64 / 3_600_000.0);

    if !has_credential {
        return HealthVerdict {
            health: AccountHealth::NoCredential,
            disposition: Disposition::Manual,
            ttl_hours: None,
            hint: "该账号没有凭证记录，请扫码登录入池".into(),
        };
    }
    // 吊销优先于一切：即使 access token 看起来未过期，服务端已不认
    if needs_relogin {
        return HealthVerdict {
            health: AccountHealth::Dead,
            disposition: Disposition::Manual,
            ttl_hours,
            hint: "登录态已被服务端吊销（常见于在别处/客户端重新登录该账号）；请在账号页点「OAuth登录」重新扫码".into(),
        };
    }
    match ttl_hours {
        // 无到期信息：视为可用（部分凭证不带 expiresIn），交给运行时 401 自愈
        None => HealthVerdict {
            health: AccountHealth::Healthy,
            disposition: Disposition::Allow,
            ttl_hours: None,
            hint: "凭证有效".into(),
        },
        Some(h) if h <= 0.0 => HealthVerdict {
            health: AccountHealth::Expired,
            disposition: Disposition::Warn,
            ttl_hours,
            hint: "access token 已过期；下次调用会自动用 refresh token 换新，无需处理".into(),
        },
        Some(h) if h <= EXPIRING_HOURS => HealthVerdict {
            health: AccountHealth::Expiring,
            disposition: Disposition::Warn,
            ttl_hours,
            hint: format!("凭证约 {:.0} 小时后过期，下轮续期会自动刷新", h),
        },
        Some(_) => HealthVerdict {
            health: AccountHealth::Healthy,
            disposition: Disposition::Allow,
            ttl_hours,
            hint: "凭证有效".into(),
        },
    }
}

/// 从 token store 记录推导状态（列表/调度共用入口，口径唯一）。
///
/// `record` 为 token store 中该账号的对象（None＝无记录）；
/// 读取 `expires_at_ms`。needs_relogin 由调用方给出（在 pool 文档里）。
pub fn classify_record(
    record: Option<&serde_json::Value>,
    needs_relogin: bool,
    now_ms: i64,
) -> HealthVerdict {
    let expires_at_ms = record
        .and_then(|r| r.get("expires_at_ms"))
        .and_then(serde_json::Value::as_i64);
    classify(record.is_some(), needs_relogin, expires_at_ms, now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;

    #[test]
    fn 无凭证判为需人工() {
        let v = classify(false, false, None, 0);
        assert_eq!(v.health, AccountHealth::NoCredential);
        assert_eq!(v.disposition, Disposition::Manual);
    }

    #[test]
    fn 服务端吊销优先于未过期() {
        // access token 还有 30 天，但已标记 needs_relogin → 必须是 Dead
        let v = classify(true, true, Some(30 * 24 * H), 0);
        assert_eq!(v.health, AccountHealth::Dead);
        assert_eq!(v.disposition, Disposition::Manual);
        assert!(v.ttl_hours.unwrap() > 700.0);
    }

    #[test]
    fn 临期与过期都可自愈只给预警() {
        let expiring = classify(true, false, Some(10 * H), 0);
        assert_eq!(expiring.health, AccountHealth::Expiring);
        assert_eq!(expiring.disposition, Disposition::Warn);

        let expired = classify(true, false, Some(-5 * H), 0);
        assert_eq!(expired.health, AccountHealth::Expired);
        assert_eq!(expired.disposition, Disposition::Warn);
        assert!(expired.hint.contains("无需处理"));
    }

    #[test]
    fn 健康与无到期信息都放行() {
        let healthy = classify(true, false, Some(30 * 24 * H), 0);
        assert_eq!(healthy.health, AccountHealth::Healthy);
        assert_eq!(healthy.disposition, Disposition::Allow);

        let unknown = classify(true, false, None, 0);
        assert_eq!(unknown.health, AccountHealth::Healthy);
        assert_eq!(unknown.disposition, Disposition::Allow);
    }

    #[test]
    fn 记录推导读取到期字段() {
        let rec = serde_json::json!({"expires_at_ms": 10 * H});
        let v = classify_record(Some(&rec), false, 0);
        assert_eq!(v.health, AccountHealth::Expiring);
        // 缺字段 → 视为无到期信息
        let v2 = classify_record(Some(&serde_json::json!({})), false, 0);
        assert_eq!(v2.health, AccountHealth::Healthy);
        // 无记录
        let v3 = classify_record(None, false, 0);
        assert_eq!(v3.health, AccountHealth::NoCredential);
    }
}
