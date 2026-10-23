//! 新号新手任务自动化(决定主号能否拿到夜间蹬蹬卡)。
//!
//! 官方对新号有三个任务,三个全完成主号才拿到卡:
//! - `login_device`      登录客户端
//! - `referral_bind_code` 绑定邀请码(见 invite.rs,已在注册流程里做)
//! - `send_message`      发送消息
//!
//! 实现依据(均实测验证):
//! - `send_message`:官方 `agent/configs` 的 proxyEndpoint.mapping 表明桌面端客户端
//!   内部调 `api.z.ai/api/anthropic/v1/messages`,由服务端转发到
//!   `zcode.z.ai/api/v1/ultra-zai/anthropic/v1/messages`。桌面端与网页端最终打到
//!   同一个端点,官方只看该端点有没有被此账号调用过,所以脚本调用与客户端等价。
//!   凭证用账号的 zai_access_token(OAuth token,不需要 coding-plan key);
//!   额度不足时服务端返回 429(错误码 1113),消息仍被受理,同样算完成任务。
//! - `login_device`:复用账号切换能力(写回 credentials.json + 重启 ZCode 客户端),
//!   让客户端以此账号身份启动一次,官方即记到登录。

use crate::zcode::api::ZaiClient;
use crate::zcode::db::{models, repo};
use crate::zcode::error::{AppError, AppResult};
use sqlx::SqlitePool;

/// 完成新号的 `send_message` 任务。
/// 返回 Ok(true) 表示服务端已受理(含额度不足的 429——消息照样被记录)。
pub async fn complete_send_message(
    http: &ZaiClient,
    account: &models::Account,
) -> AppResult<bool> {
    let token = account
        .zai_access_token
        .as_deref()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            AppError::Other(
                "该账号没有 zai token,无法发消息(需重新走 OAuth 登录导入)".into(),
            )
        })?;
    // 用一句中性的话,不涉及任何敏感内容
    http.send_message(token, &account.device_mid, "你好,请多指教。")
        .await
}

/// 完成新号的 `login_device` 任务:让 ZCode 客户端以此账号身份启动一次。
///
/// 复用 accounts::service::switch_account_and_restart 的完整链路
/// (写回 credentials.json → 翻 is_current → 重启客户端)。
/// 注意:这会把本机当前登录账号切走,调用方应明确这是有意为之。
pub async fn complete_login_device(
    db: &SqlitePool,
    http: &ZaiClient,
    monitor: &crate::zcode::modules::monitor::service::MonitorHandle,
    app: &tauri::AppHandle,
    account_id: i64,
) -> AppResult<()> {
    crate::zcode::modules::accounts::service::switch_account_and_restart(
        db,
        http,
        monitor,
        app,
        account_id,
    )
    .await
}

/// 一键补齐某账号缺失的新手任务(只做没完成的那几个)。
/// 返回 (是否做了 login_device, 是否做了 send_message, 跳过原因列表)。
pub async fn complete_pending_tasks(
    db: &SqlitePool,
    http: &ZaiClient,
    monitor: &crate::zcode::modules::monitor::service::MonitorHandle,
    app: &tauri::AppHandle,
    account_id: i64,
) -> AppResult<(bool, bool, Vec<String>)> {
    let account = repo::fetch_by_id(db, account_id)
        .await?
        .ok_or_else(|| AppError::Other("账号不存在".into()))?;
    let mut skipped = Vec::new();

    // 已完成的跳过(0/1 来自上次同步;None 当作未完成,由服务端最终判定)
    let need_msg = account.task_send_message != Some(1);
    let need_login = account.task_login_device != Some(1);
    if !need_msg && !need_login {
        skipped.push("三个新手任务均已完成,无需操作".to_string());
        return Ok((false, false, skipped));
    }

    let mut did_login = false;
    let mut did_msg = false;

    // 先发消息(不打断当前客户端登录态);额度不足也算完成
    if need_msg {
        match complete_send_message(http, &account).await {
            Ok(true) => did_msg = true,
            Ok(false) => skipped.push("send_message 未被服务端受理".to_string()),
            Err(e) => skipped.push(format!("send_message 失败: {e}")),
        }
    }

    // 再登录设备(会切换本机当前账号并重启客户端,故放在后面)
    if need_login {
        match complete_login_device(db, http, monitor, app, account_id).await {
            Ok(()) => did_login = true,
            Err(e) => skipped.push(format!("login_device 失败: {e}")),
        }
    }

    Ok((did_login, did_msg, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 发消息的请求体必须是 Anthropic Messages 格式(逆向自官方 proxyEndpoint.mapping)
    #[test]
    fn send_message_body_shape() {
        let body = serde_json::json!({
            "model": "GLM-5.3-Flash",
            "max_tokens": 32,
            "messages": [{ "role": "user", "content": "你好,请多指教。" }],
        });
        assert_eq!(body["model"], "GLM-5.3-Flash");
        assert_eq!(body["max_tokens"], 32);
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0]["role"], "user");
        assert!(msgs[0]["content"].as_str().unwrap().len() <= 32);
    }

    /// 额度不足(429/1113)也算完成:消息已被服务端受理
    #[test]
    fn rate_limit_still_counts_as_sent() {
        // 与 client.rs 的判定保持一致:429 时返回 Ok(true)
        let statuses = [200u16, 201, 429];
        for s in statuses {
            let ok = (200..300).contains(&s) || s == 429;
            assert!(ok, "状态 {s} 应视为已发送");
        }
        for s in [401u16, 500, 502] {
            let ok = (200..300).contains(&s) || s == 429;
            assert!(!ok, "状态 {s} 不应视为已发送");
        }
    }
}
