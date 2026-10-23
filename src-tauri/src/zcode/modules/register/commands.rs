//! 马甲邮箱全自动注册的命令层:发起流水线 / 取消 / 状态查询 / IMAP 配置。

use tauri::{AppHandle, State};

use crate::zcode::error::{AppError, AppResult};
use crate::zcode::state::ZcodeState;

use super::flow::{self, RegisterRequest};
use super::imap::{self, ImapConfig};

/// 发起一条注册流水线:马甲邮箱注册 → 邮箱验证 → OAuth 授权入池 →(可选)邀请捆绑。
/// 进度经 `register:progress` / `register:done` 事件广播,前端面板据此展示。
///
/// 两种指定马甲邮箱的方式:
/// ① `secret_id`:复用账密库里现成的未注册条目(优先,不用手输)
/// ② `email` + `password`:手输(账密库里没有时用)
#[tauri::command]
pub async fn zcode_register_start(
    app: AppHandle,
    state: State<'_, ZcodeState>,
    email: Option<String>,
    password: Option<String>,
    username: Option<String>,
    inviter_id: Option<i64>,
    mail_timeout_sec: Option<u64>,
    secret_id: Option<i64>,
) -> AppResult<()> {
    if !state.register.try_start() {
        return Err(AppError::Other("已有注册流水线在进行中".into()));
    }
    // 先解析出邮箱与密码(优先用库里的现成条目,账密不信任前端回传)
    let (email, password) = match secret_id {
        Some(id) => {
            match crate::zcode::db::repo::secrets_get_credentials(&state.db, id).await {
                Ok(Some((login, pass))) if !login.is_empty() && !pass.is_empty() => (login, pass),
                _ => {
                    state.register.finish();
                    return Err(AppError::Other(
                        "找不到该马甲邮箱的账密(可能已被删除),请重新选择".into(),
                    ));
                }
            }
        }
        None => {
            let email = email.unwrap_or_default().trim().to_string();
            let password = password.unwrap_or_default();
            if !email.contains('@') {
                state.register.finish();
                return Err(AppError::Other("邮箱地址不合法".into()));
            }
            if password.trim().is_empty() {
                state.register.finish();
                return Err(AppError::Other("密码不能为空".into()));
            }
            (email, password)
        }
    };
    // 提前校验 IMAP 配置(缺配置时直接报错,不开窗口)
    imap::ImapConfig::load(&state.db).await.inspect_err(|_| {
        state.register.finish();
    })?;

    let req = RegisterRequest {
        email,
        password,
        username: username.filter(|s| !s.trim().is_empty()),
        inviter_id,
        mail_timeout_sec: mail_timeout_sec.unwrap_or(300).clamp(30, 1800),
        secret_id,
    };

    let app = app.clone();
    let db = state.db.clone();
    let http = state.http.clone();
    let register = state.register.clone();
    let monitor = state.monitor.clone();
    tauri::async_runtime::spawn(async move {
        flow::run_register_flow(app, db, http, register, monitor, req).await;
    });
    Ok(())
}

/// 取消进行中的注册流水线(当前账号中止,已导入的保留)
#[tauri::command]
pub async fn zcode_register_cancel(state: State<'_, ZcodeState>) -> AppResult<()> {
    state.register.cancel();
    Ok(())
}

/// 注册流水线是否在进行中(前端窗口重开时恢复按钮态)
#[tauri::command]
pub async fn zcode_register_status(state: State<'_, ZcodeState>) -> AppResult<bool> {
    Ok(state.register.is_running())
}

/// 列出可复用的现成马甲邮箱(CSV/手工录入但尚未入池的独立条目)。
/// 密码不回显给前端(只给 id 与邮箱);选定时由 start 命令按 id 从库里取账密。
#[tauri::command]
pub async fn zcode_register_mask_pool(
    state: State<'_, ZcodeState>,
) -> AppResult<Vec<serde_json::Value>> {
    let rows = crate::zcode::db::repo::unused_mask_emails(&state.db).await?;
    Ok(rows
        .into_iter()
        .map(|(id, login_name, _)| serde_json::json!({ "id": id, "email": login_name }))
        .collect())
}

/// Relay 状态:能否读到本机浏览器的登录态、账号邮箱、现有马甲数与免费上限。
#[tauri::command]
pub async fn zcode_relay_status() -> AppResult<serde_json::Value> {
    let client = super::relay::RelayClient::from_local_browser()?;
    let email = client.account_email().await?;
    let masks = client.list_masks().await?;
    Ok(serde_json::json!({
        "ok": true,
        "account_email": email,
        "mask_count": masks.len(),
        "free_limit": 50,
    }))
}

/// 列出 Relay 里的全部马甲,并标注哪些已入池(本地有对应账号)。
/// 供「马甲管理」面板展示与清理:未入池的可以放心删(腾额度),
/// 已入池的删了不影响本地凭证,但该地址以后无法再收信/找回。
#[tauri::command]
pub async fn zcode_relay_list_masks(state: State<'_, ZcodeState>) -> AppResult<serde_json::Value> {
    let client = super::relay::RelayClient::from_local_browser()?;
    let email = client.account_email().await?;
    let masks = client.list_masks().await?;
    // 已入池判定:账号 label 或 user_info.email 命中该马甲地址
    let accounts = crate::zcode::db::repo::list(&state.db).await?;
    let pooled: std::collections::HashSet<String> = accounts
        .iter()
        .flat_map(|a| {
            let mut s = vec![a.label.clone()];
            if let Some(raw) = a.user_info.as_deref() {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
                    if let Some(e) = v["email"].as_str() {
                        s.push(e.to_string());
                    }
                }
            }
            s
        })
        .collect();
    let items: Vec<serde_json::Value> = masks
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "address": m.full_address,
                "enabled": m.enabled,
                "pooled": pooled.contains(&m.full_address),
            })
        })
        .collect();
    Ok(serde_json::json!({
        "account_email": email,
        "total": masks.len(),
        "free_limit": 50,
        "masks": items,
    }))
}

/// 删除一个马甲(腾出免费额度)。
#[tauri::command]
pub async fn zcode_relay_delete_mask(mask_id: i64) -> AppResult<()> {
    let client = super::relay::RelayClient::from_local_browser()?;
    client.delete_mask(mask_id).await
}

/// 自动生成 N 个新马甲(直接调 Relay 接口,不需要人工过验证码)。
/// 返回新建的完整地址;超免费上限会报错并说明。
#[tauri::command]
pub async fn zcode_relay_create_masks(count: u32) -> AppResult<Vec<String>> {
    let count = count.clamp(1, 20);
    let client = super::relay::RelayClient::from_local_browser()?;
    let existing = client.list_masks().await?;
    if existing.len() as u32 + count > 50 {
        return Err(AppError::Other(format!(
            "免费账号最多 50 个马甲,已有 {} 个,再生成 {} 个会超限",
            existing.len(),
            count
        )));
    }
    let mut created = Vec::new();
    for i in 0..count {
        let mask = client
            .create_mask(&format!("AI Work Studio 自动注册 {}", existing.len() + i as usize + 1))
            .await?;
        created.push(mask.full_address);
    }
    Ok(created)
}

/// 批量注册:自动生成马甲(或复用账密库里现成的)后逐个跑完整注册流水线,
/// 全部跑完才结束(单条失败不中断)。进度经 register:progress / register:done 广播。
///
/// 参数:`count` 要注册多少个新号;`source` 为 "generate"(自动生成新马甲)或
/// "pool"(用账密库里现成的);`inviter_id` 要捆绑的主号账号 id(可选)。
#[tauri::command]
pub async fn zcode_register_batch(
    app: AppHandle,
    state: State<'_, ZcodeState>,
    count: u32,
    source: String,
    inviter_id: Option<i64>,
    mail_timeout_sec: Option<u64>,
) -> AppResult<u32> {
    if !state.register.try_start() {
        return Err(AppError::Other("已有注册流水线在进行中".into()));
    }
    let count = count.clamp(1, 20);

    // 先备好这一批的马甲账密(邮箱 + 密码),失败则直接结束
    let batch: Vec<(String, String)> = match source.as_str() {
        "generate" => {
            let client = super::relay::RelayClient::from_local_browser().inspect_err(|_| {
                state.register.finish();
            })?;
            let existing = client.list_masks().await.inspect_err(|_| {
                state.register.finish();
            })?;
            if existing.len() as u32 + count > 50 {
                state.register.finish();
                return Err(AppError::Other(format!(
                    "免费账号最多 50 个马甲,已有 {} 个,不够再生成 {} 个",
                    existing.len(),
                    count
                )));
            }
            let mut out = Vec::new();
            for i in 0..count {
                match client
                    .create_mask(&format!("AI Work Studio 自动注册 {}", existing.len() + i as usize + 1))
                    .await
                {
                    Ok(m) => {
                        // 密码由工具生成并写回账密库,后续换号/找回都有据可查
                        let password = random_password();
                        let _ = crate::zcode::db::repo::secrets_add(
                            &state.db,
                            &format!("自动注册{}", existing.len() + i as usize + 1),
                            &m.full_address,
                            &password,
                        )
                        .await;
                        out.push((m.full_address, password));
                    }
                    Err(e) => {
                        state.register.finish();
                        return Err(AppError::Other(format!(
                            "生成第 {} 个马甲失败: {e}(已成功生成 {} 个)",
                            i + 1,
                            out.len()
                        )));
                    }
                }
            }
            out
        }
        _ => {
            // 复用账密库里现成的未注册马甲
            let rows = crate::zcode::db::repo::unused_mask_emails(&state.db)
                .await
                .inspect_err(|_| {
                    state.register.finish();
                })?;
            if rows.len() < count as usize {
                state.register.finish();
                return Err(AppError::Other(format!(
                    "账密库里只有 {} 个未注册马甲,不够 {} 个",
                    rows.len(),
                    count
                )));
            }
            rows.into_iter().take(count as usize).map(|(_, e, p)| (e, p)).collect()
        }
    };

    // IMAP 配置提前校验
    super::imap::ImapConfig::load(&state.db).await.inspect_err(|_| {
        state.register.finish();
    })?;

    // 主号本周配额:显式指定主号时不分配(沿用旧行为);否则按各主号本周剩余名额
    // 自动分摊,避免流水线白跑一轮后才被官方以"无邀请资格"拒绝。
    // 数据来自最近一次「同步资格」落的库(invite_state / invited_weekly / invite_cap)。
    let quotas: Vec<flow::InviterQuota> = if inviter_id.is_some() {
        Vec::new()
    } else {
        let accounts = crate::zcode::db::repo::list(&state.db).await.unwrap_or_default();
        accounts
            .into_iter()
            .filter(|a| a.invite_state.as_deref() == Some("active") && a.invite_code.is_some())
            // 官方已判本期封顶(capped)的主号不再分配
            .filter(|a| a.invite_capped.unwrap_or(0) == 0)
            .map(|a| flow::InviterQuota {
                id: a.id,
                label: a.label.clone(),
                // 官方口径(实测 2026-10-22):weekly 是本周已邀数,reward_cap 是本期限额。
                // 旧实现误用累计 invited_count + 写死 cap=2,会把累计邀过 2 人的主号
                // 直接判成"名额已满",自动分配永远跳过它们。未同步过资格时按 0 计,
                // 宁可让流水线去试一次,也不要凭累计数误杀。
                used: a.invited_weekly.unwrap_or(0),
                cap: a.invite_cap.unwrap_or(crate::zcode::modules::accounts::invite::DEFAULT_INVITE_CAP),
            })
            .collect()
    };
    if inviter_id.is_none() && quotas.is_empty() {
        state.register.finish();
        return Err(AppError::Other(
            "没有可用的主号:需要至少一个账号已获得邀请资格(👑 可邀请)。\
             请先在「账号管理」页点「同步资格」刷新,或指定一个主号。"
                .into(),
        ));
    }

    let accepted = batch.len() as u32;
    let app = app.clone();
    let db = state.db.clone();
    let http = state.http.clone();
    let register = state.register.clone();
    let monitor = state.monitor.clone();
    tauri::async_runtime::spawn(async move {
        flow::run_register_batch(
            app,
            db,
            http,
            register,
            monitor,
            batch,
            inviter_id,
            mail_timeout_sec,
            quotas,
        )
        .await;
    });
    Ok(accepted)
}

/// 补齐某账号缺失的新手任务(登录客户端 + 发送消息)。
///
/// 注意:`login_device` 会把本机当前登录账号切走并重启 ZCode 客户端——这是官方的
/// 判定方式(客户端以此身份启动过即记到登录),调用前请确认这是有意为之。
/// `send_message` 无副作用,任何时候都能补。
/// `include_login` 为 false 时只发消息,不打断当前登录态。
#[tauri::command]
pub async fn zcode_register_complete_tasks(
    app: AppHandle,
    state: State<'_, ZcodeState>,
    account_id: i64,
    include_login: bool,
) -> AppResult<serde_json::Value> {
    let (mut did_login, did_msg, mut skipped) = if include_login {
        super::tasks::complete_pending_tasks(
            &state.db,
            &state.http,
            &state.monitor,
            &app,
            account_id,
        )
        .await?
    } else {
        // 只发消息
        let account = crate::zcode::db::repo::fetch_by_id(&state.db, account_id)
            .await?
            .ok_or_else(|| AppError::Other("账号不存在".into()))?;
        let msg = super::tasks::complete_send_message(&state.http, &account).await;
        match msg {
            Ok(true) => (false, true, vec![]),
            Ok(false) => (false, false, vec!["发送消息未受理".to_string()]),
            Err(e) => (false, false, vec![format!("send_message 失败: {e}")]),
        }
    };
    if !include_login {
        did_login = false;
    }
    // 刷新该账号数据,把最新的任务完成度同步进快照
    state.monitor.refresh_account(&app, account_id).await;
    if skipped.is_empty() {
        skipped.push("已完成".to_string());
    }
    Ok(serde_json::json!({
        "login_device": did_login,
        "send_message": did_msg,
        "message": skipped.join("; "),
    }))
}

/// 新马甲账号统一密码。
///
/// 用户要求所有新注册马甲的密码固定为 `lxl575595LXL!`(末尾半角感叹号),
/// 便于人工在官方网页兜底登录。不要再改回随机生成。
pub(crate) const MASK_PASSWORD: &str = "lxl575595LXL!";

/// 返回新马甲账号使用的密码(固定值,见 [`MASK_PASSWORD`])。
fn random_password() -> String {
    MASK_PASSWORD.to_string()
}

/// 保存马甲邮箱(Gmail)IMAP 配置并测试连接。
/// App Password 不会回显给前端(只返回连接结果)。
#[tauri::command]
pub async fn zcode_register_save_imap(
    state: State<'_, ZcodeState>,
    host: String,
    port: u16,
    user: String,
    pass: String,
) -> AppResult<String> {
    let cfg = ImapConfig {
        host: host.trim().to_string(),
        port,
        user: user.trim().to_string(),
        pass: pass.trim().to_string(),
    };
    if cfg.host.is_empty() || cfg.user.is_empty() || cfg.pass.is_empty() {
        return Err(AppError::Other("主机、邮箱地址与 App Password 均不能为空".into()));
    }
    // 先测连通,通过才落库(避免把错配置写进去)
    let result = imap::test_connection(&cfg).await?;
    cfg.save(&state.db).await;
    Ok(result)
}

/// 读取已保存的 IMAP 配置(密码不回显,只告诉前端"是否已配置")
#[tauri::command]
pub async fn zcode_register_imap_state(state: State<'_, ZcodeState>) -> AppResult<serde_json::Value> {
    let configured = imap::ImapConfig::load(&state.db).await.is_ok();
    let host = crate::zcode::util::kv_get(&state.db, "imap_host").await;
    let user = crate::zcode::util::kv_get(&state.db, "imap_user").await;
    Ok(serde_json::json!({
        "configured": configured,
        "host": host.unwrap_or_else(|| "imap.gmail.com".to_string()),
        "user": user,
    }))
}
