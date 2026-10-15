use tauri::{AppHandle, Manager, State};

use crate::zcode::db::models::{self, Account};
use crate::zcode::error::AppResult;
use crate::zcode::state::ZcodeState;

use super::{events, service};

#[tauri::command]
pub async fn zcode_accounts_list(state: State<'_, ZcodeState>) -> AppResult<Vec<Account>> {
    service::list_accounts(&state.db).await
}

/// 浏览器登录 z.ai 并导入新账号(轮询流:系统默认浏览器打开授权页,完成后自动入库)
#[tauri::command]
pub async fn zcode_accounts_add_oauth(
    app: AppHandle,
    state: State<'_, ZcodeState>,
) -> AppResult<Account> {
    let account =
        service::add_account_via_oauth(&state.db, &state.http, &state.oauth, &app).await?;
    events::emit_accounts_changed(&app)?;
    state.monitor.refresh_account(&app, account.id).await;
    Ok(account)
}

/// 取消进行中的 OAuth 轮询登录
#[tauri::command]
pub async fn zcode_accounts_cancel_oauth(state: State<'_, ZcodeState>) -> AppResult<()> {
    state.oauth.cancel();
    Ok(())
}

/// 导入本机 ZCode 客户端当前登录的账号(重复导入即刷新凭证)
#[tauri::command]
pub async fn zcode_accounts_import_local(
    app: AppHandle,
    state: State<'_, ZcodeState>,
) -> AppResult<Account> {
    let account = service::import_local_account(&state.db).await?;
    events::emit_accounts_changed(&app)?;
    // 导入后立即拉一次该账号数据(套餐/用量/活动)
    state.monitor.refresh_account(&app, account.id).await;
    Ok(account)
}

/// 切换到此账号:写回本机 credentials.json 并重启 ZCode 客户端
#[tauri::command]
pub async fn zcode_accounts_switch(
    app: AppHandle,
    state: State<'_, ZcodeState>,
    id: i64,
) -> AppResult<()> {
    service::switch_account_and_restart(&state.db, &state.http, &state.monitor, &app, id).await
}

#[tauri::command]
pub async fn zcode_accounts_delete(
    app: AppHandle,
    state: State<'_, ZcodeState>,
    id: i64,
) -> AppResult<()> {
    service::delete_account(&state.db, id).await?;
    events::emit_accounts_changed(&app)?;
    Ok(())
}

/// 设置账号的邀请关系标识:role 为 "inviter"/"invited",空表示清除;
/// invited_by 为被邀请者归属的邀请者账号 id(仅 invited 时有效)
#[tauri::command]
pub async fn zcode_accounts_set_invite_role(
    app: AppHandle,
    state: State<'_, ZcodeState>,
    id: i64,
    role: Option<String>,
    invited_by: Option<i64>,
) -> AppResult<()> {
    let role = role.filter(|r| matches!(r.as_str(), "inviter" | "invited"));
    // 只有被邀请者才携带归属;邀请者/清除时强制置空,避免悬空绑定
    let invited_by = if role.as_deref() == Some("invited") { invited_by } else { None };
    crate::zcode::db::repo::set_invite_role(&state.db, id, role.as_deref(), invited_by).await?;
    events::emit_accounts_changed(&app)?;
    Ok(())
}

/// 自动同步邀请关系:从官方奖励中心接口拉取每个账号的邀请记录,
/// 自动标记邀请者并按打码 user_id 匹配库内账号建立绑定。返回同步统计。
#[tauri::command]
pub async fn zcode_accounts_sync_invite(
    app: AppHandle,
    state: State<'_, ZcodeState>,
) -> AppResult<serde_json::Value> {
    let result = service::sync_invite_relations(&state.db, &state.http).await?;
    events::emit_accounts_changed(&app)?;
    Ok(serde_json::json!({
        "inviters": result.inviters,
        "linked": result.linked,
        "unmatched": result.unmatched,
        "errors": result.errors,
    }))
}

/// 账密管理:全部条目(含每个已导入账号的行 + 独立录入条目)
#[tauri::command]
pub async fn zcode_secrets_list(state: State<'_, ZcodeState>) -> AppResult<Vec<models::SecretItem>> {
    Ok(crate::zcode::db::repo::secrets_list(&state.db).await?)
}

/// 账密管理:保存某条账密
#[tauri::command]
pub async fn zcode_secrets_save(
    state: State<'_, ZcodeState>,
    id: i64,
    title: String,
    login_name: String,
    password: String,
) -> AppResult<()> {
    crate::zcode::db::repo::secrets_save(&state.db, id, &title, &login_name, &password).await?;
    Ok(())
}

/// 账密管理:新增一条独立账密(不关联已导入账号)
#[tauri::command]
pub async fn zcode_secrets_add(
    state: State<'_, ZcodeState>,
    title: String,
    login_name: String,
    password: String,
) -> AppResult<i64> {
    Ok(crate::zcode::db::repo::secrets_add(&state.db, &title, &login_name, &password).await?)
}

/// 账密管理:删除独立账密条目
#[tauri::command]
pub async fn zcode_secrets_delete(state: State<'_, ZcodeState>, id: i64) -> AppResult<()> {
    crate::zcode::db::repo::secrets_delete(&state.db, id).await?;
    Ok(())
}

/// 批量账密登录:把指定 secret 条目逐个经内嵌登录窗口导入(队列在 Rust 后台跑)。
/// 返回接受的条目数;进度经 batch:progress / batch:done 事件广播。
#[tauri::command]
pub async fn zcode_accounts_batch_login(
    app: AppHandle,
    state: State<'_, ZcodeState>,
    secret_ids: Vec<i64>,
) -> AppResult<usize> {
    use super::batch::{run_batch_login, BatchLoginItem};

    if !state.batch.try_start() {
        return Err(crate::zcode::error::AppError::Other(
            "已有批量登录队列在进行中".into(),
        ));
    }

    // 从库里读账密(不信任前端回传的密码),读不到/账密不齐的条目直接剔除
    let mut items = Vec::new();
    for id in secret_ids {
        if let Ok(Some((login, password))) =
            crate::zcode::db::repo::secrets_get_credentials(&state.db, id).await
        {
            if !login.is_empty() && !password.is_empty() {
                items.push(BatchLoginItem {
                    secret_id: id,
                    login_name: login,
                    password,
                });
            }
        }
    }
    let accepted = items.len();
    if accepted == 0 {
        state.batch.finish();
        return Err(crate::zcode::error::AppError::Other(
            "没有可登录的条目(需已填写登录名和密码)".into(),
        ));
    }

    let handle = app.clone();
    let db = state.db.clone();
    let http = state.http.clone();
    let batch = state.batch.clone();
    let monitor = state.monitor.clone();
    tauri::async_runtime::spawn(async move {
        run_batch_login(handle, db, http, batch, monitor, items).await;
    });
    Ok(accepted)
}

/// 取消进行中的批量登录队列(当前账号中止,未开始的不再开始)
#[tauri::command]
pub async fn zcode_accounts_batch_login_cancel(state: State<'_, ZcodeState>) -> AppResult<()> {
    state.batch.cancel();
    Ok(())
}

/// 批量登录是否在进行中(前端窗口重开时恢复按钮态)
#[tauri::command]
pub async fn zcode_accounts_batch_login_status(state: State<'_, ZcodeState>) -> AppResult<bool> {
    Ok(state.batch.is_running())
}

/// 打开账密管理弹窗(已存在则置前)
#[tauri::command]
pub async fn zcode_secrets_open_window(app: AppHandle) -> AppResult<()> {
    use tauri::WebviewUrl;
    if let Some(win) = app.get_webview_window("secrets") {
        let _ = win.show();
        let _ = win.unminimize();
        let _ = win.set_focus();
        return Ok(());
    }
    tauri::WebviewWindowBuilder::new(&app, "secrets", WebviewUrl::App("index.html".into()))
        .title("账密管理")
        .inner_size(560.0, 680.0)
        .min_inner_size(440.0, 420.0)
        .center()
        .build()
        .map_err(|e| crate::zcode::error::AppError::Other(format!("打开账密窗口失败: {e}")))?;
    Ok(())
}

/// 批量导入 CSV 文件（格式：email,password）
#[tauri::command]
pub async fn zcode_accounts_import_from_csv(
    app: AppHandle,
    state: State<'_, ZcodeState>,
    file_content: String,
) -> AppResult<serde_json::Value> {
    use serde_json::json;

    let db = &state.db;
    let existing = match service::list_accounts(db).await {
        Ok(a) => a,
        Err(e) => return Ok(json!({"error": format!("获取账号列表失败：{}", e)})),
    };

    // 解析 CSV（简单实现，处理换行符和逗号）
    let lines: Vec<&str> = file_content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() < 2 {
        return Ok(json!({
            "error": "CSV 文件至少需要一行表头 + 一行数据",
            "imported": 0,
            "skipped": 0
        }));
    }

    let mut imported = 0u64;
    let mut skipped = 0u64;
    let mut errors: Vec<(u64, String)> = Vec::new();

    // 跳过头部，从第二行开始
    for (line_num, line) in lines.iter().enumerate().skip(1) {
        let parts: Vec<&str> = line.split(',').collect();
        if parts.len() < 2 {
            errors.push((line_num as u64 + 1, "格式错误：应为 email,password".to_string()));
            skipped += 1;
            continue;
        }

        let email = parts[0].trim().to_string();
        let password = parts[1].trim().to_string();
        
        if email.is_empty() || password.is_empty() {
            errors.push((line_num as u64 + 1, "邮箱或密码为空".to_string()));
            skipped += 1;
            continue;
        }

        // 检查是否已存在（通过 login_name）
        let secrets = match crate::zcode::db::repo::secrets_list(db).await {
            Ok(s) => s,
            Err(_) => {
                errors.push((line_num as u64 + 1, "查询现有账密失败".to_string()));
                skipped += 1;
                continue;
            }
        };
        let exists = secrets.iter().any(|s| s.login_name == email);
        if exists {
            log::debug!("账号 {} 已存在，跳过", email);
            skipped += 1;
            continue;
        }

        // 先导入账密到 secret_entries
        match crate::zcode::db::repo::secrets_add(
            db, 
            &format!("批量导入{}", existing.len()), 
            &email, 
            &password
        ).await {
            Ok(_secret_id) => {
                imported += 1;
                log::info!("成功导入第 {} 个账号（{}）", email, _secret_id);
            },
            Err(e) => {
                errors.push((line_num as u64 + 1, format!("插入失败：{}", e)));
            }
        }
    }

    // 广播账号列表变化
    let _ = events::emit_accounts_changed(&app);

    // 返回统计信息
    Ok(json!({
        "imported": imported,
        "skipped": skipped,
        "errors": errors
    }))
}
