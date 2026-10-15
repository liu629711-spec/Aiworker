use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sqlx::SqlitePool;
use tauri::Manager;

use crate::zcode::credentials;
use crate::zcode::db::{models, repo};
use crate::zcode::error::{AppError, AppResult};

/// 从 JWT payload 提取 user_id(字段为 user_id,退化 sub)
fn jwt_user_id(jwt: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v["user_id"]
        .as_str()
        .or_else(|| v["sub"].as_str())
        .map(str::to_string)
}

/// 按客户端 credentials.json 的 user_info 结构构造:{user_id,email,avatar,name}
fn construct_user_info(user_id: String, email: &str, name: &str, avatar: &str) -> String {
    serde_json::json!({
        "user_id": user_id,
        "email": email,
        "avatar": avatar,
        "name": name,
    })
    .to_string()
}

pub async fn list_accounts(db: &SqlitePool) -> AppResult<Vec<models::Account>> {
    Ok(repo::list(db).await?)
}

/// 从本机 ZCode 客户端凭证导入当前登录账号;重复导入(同 user_id 或同 jwt)则刷新其凭证。
/// 多账号录入方式:在 ZCode 客户端切换登录账号后再次导入,或直接用 OAuth 浏览器登录。
pub async fn import_local_account(db: &SqlitePool) -> AppResult<models::Account> {
    let creds = credentials::load_local()?;
    let existing = repo::list(db).await?;

    let dup = existing.iter().find(|a| {
        (!a.jwt.is_empty() && a.jwt == creds.jwt)
            || creds
                .user_id
                .as_deref()
                .is_some_and(|u| a.user_id.as_deref() == Some(u))
    });
    if let Some(d) = dup {
        return Ok(repo::update_credentials(
            db,
            d.id,
            &creds.jwt,
            &creds.refresh_token,
            creds.user_id.as_deref(),
            creds.user_info.as_deref(),
            creds.zai_access_token.as_deref(),
            creds.zai_refresh_token.as_deref(),
        )
        .await?);
    }

    let label = creds
        .display_name
        .clone()
        .unwrap_or_else(|| format!("账号 {}", existing.len() + 1));
    Ok(repo::insert(
        db,
        &models::NewAccount {
            label,
            device_mid: uuid::Uuid::new_v4().to_string(),
            user_id: creds.user_id.clone(),
            jwt: creds.jwt.clone(),
            refresh_token: creds.refresh_token.clone(),
            user_info: creds.user_info.clone(),
            zai_access_token: creds.zai_access_token.clone(),
            zai_refresh_token: creds.zai_refresh_token.clone(),
        },
    )
    .await?)
}

pub async fn delete_account(db: &SqlitePool, id: i64) -> AppResult<()> {
    repo::delete(db, id).await?;
    // 顺手解除绑定:引用此账号的被邀请者置空归属,避免悬空 id
    let _ = sqlx::query("UPDATE accounts SET invited_by = NULL WHERE invited_by = ?1")
        .bind(id)
        .execute(db)
        .await;
    Ok(())
}

/// 同步结果摘要(供前端提示)
#[derive(serde::Serialize)]
pub struct InviteSyncResult {
    /// 官方确认的邀请者数量
    pub inviters: usize,
    /// 自动建立的绑定数量
    pub linked: usize,
    /// 未能匹配到库内账号的打码记录(对应小号尚未导入)
    pub unmatched: Vec<String>,
    /// 各账号请求失败原因(凭证失效等)
    pub errors: Vec<String>,
}

/// 自动同步邀请关系(数据源:官方奖励中心接口):
/// 1. 每个账号调 referral/invitees 翻页拉取邀请记录 → 有记录者即邀请者(官方数据,强制标记)
/// 2. 记录的 user_id_masked(前2***后2)与库内账号 user_id 匹配(带横线原样比对;
///    一个 mask 命中多个账号视为歧义跳过) → 命中者标记 invited + invited_by
/// 3. 顺手回填 JWT 解出的 user_id(仅当 DB 缺失)
/// 官方无记录的账号不改动(保留手工标记作为补充)。
pub async fn sync_invite_relations(db: &SqlitePool, http: &crate::zcode::api::ZaiClient) -> AppResult<InviteSyncResult> {
    let accounts = repo::list(db).await?;
    let now = chrono::Utc::now().timestamp();
    let mut result = InviteSyncResult { inviters: 0, linked: 0, unmatched: vec![], errors: vec![] };

    // 账号池:label -> (id, user_id) 供 mask 匹配
    let mut pool: Vec<(String, i64, Option<String>)> = Vec::new();
    for a in &accounts {
        let uid = match &a.user_id {
            Some(u) if !u.is_empty() => Some(u.clone()),
            _ => jwt_user_id(&a.jwt),
        };
        // 回填缺失的 user_id
        if a.user_id.is_none() {
            if let Some(u) = &uid {
                let _ = sqlx::query("UPDATE accounts SET user_id = ?1, updated_at = ?2 WHERE id = ?3")
                    .bind(u)
                    .bind(now)
                    .bind(a.id)
                    .execute(db)
                    .await;
            }
        }
        pool.push((a.label.clone(), a.id, uid));
    }

    // 每个账号拉邀请记录
    for a in &accounts {
        // invitees 翻页(上限 5 页 = 250 条,足够)
        let mut masks: Vec<String> = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..5 {
            let v = match http
                .get_referral_invitees(&a.jwt, &a.device_mid, cursor.as_deref())
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    result.errors.push(format!("{}: {e}", a.label));
                    masks.clear();
                    break;
                }
            };
            for item in v["data"]["invitees"].as_array().into_iter().flatten() {
                if let Some(m) = item["user_id_masked"].as_str() {
                    masks.push(m.to_string());
                }
            }
            cursor = v["data"]["next_cursor"]
                .as_str()
                .filter(|c| !c.is_empty())
                .map(str::to_string);
            if cursor.is_none() {
                break;
            }
        }
        if masks.is_empty() {
            continue; // 无邀请记录:不动(保留手工标记)
        }

        // 官方确认:邀请者
        let _ = repo::set_invite_role(db, a.id, Some("inviter"), None).await;
        result.inviters += 1;

        // mask 匹配小号(去横线比对:官方 mask 作用于 32 位 hex)
        for m in &masks {
            let Some((head, tail)) = m.split_once("***") else { continue };
            let hits: Vec<i64> = pool
                .iter()
                .filter(|(_, _, uid)| {
                    uid.as_deref()
                        .map(|u| {
                            let hex = u.replace('-', "");
                            hex.starts_with(head) && hex.ends_with(tail)
                        })
                        .unwrap_or(false)
                })
                .map(|(_, id, _)| *id)
                .collect();
            if hits.len() == 1 {
                let _ = repo::set_invite_role(db, hits[0], Some("invited"), Some(a.id)).await;
                result.linked += 1;
            } else if hits.is_empty() {
                result.unmatched.push(format!("{m}({})", a.label));
            } else {
                result.unmatched.push(format!("{m}(歧义,跳过)"));
            }
        }
    }
    Ok(result)
}

/// OAuth 轮询登录导入:初始化会话 → 系统默认浏览器打开授权页 → 轮询至 ready → 入库。
/// 全程无自动化浏览器,用户在任意浏览器里登录;可随时取消,超时以服务端 expires_at 为准。
pub async fn add_account_via_oauth(
    db: &SqlitePool,
    http: &crate::zcode::api::ZaiClient,
    cancel: &super::oauth::OAuthCancel,
    app: &tauri::AppHandle,
) -> AppResult<models::Account> {
    // 1. 初始化轮询会话(poll_token = 本地自生成 64 位 hex)
    cancel.reset();
    let poll_token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let init = http.init_oauth_polling(&poll_token).await?;
    if init["code"].as_i64() != Some(0) {
        return Err(AppError::Other(
            init["msg"].as_str().unwrap_or("OAuth 会话初始化失败").to_string(),
        ));
    }
    let data = &init["data"];
    let flow_id = data["flow_id"]
        .as_str()
        .ok_or_else(|| AppError::Other("初始化响应缺少 flow_id".into()))?
        .to_string();
    let authorize_url = data["authorize_url"]
        .as_str()
        .ok_or_else(|| AppError::Other("初始化响应缺少 authorize_url".into()))?;

    // 2. 系统默认浏览器打开授权页(服务端已构造完整参数,原样使用;ShellExecute 保证 URL 原样送达)
    super::oauth::open_in_system_browser(app, authorize_url)?;

    // 3. 轮询至 ready / failed / 过期并入库
    let expires_at = data["expires_at"].as_i64().unwrap_or(0);
    let interval_sec = data["poll_interval_sec"].as_i64().unwrap_or(3).clamp(1, 30);
    poll_and_save_account(db, http, &flow_id, &poll_token, expires_at, interval_sec, || {
        cancel.is_cancelled().then(|| "已取消".to_string())
    })
    .await
}

/// 内嵌窗口登录(批量队列专用):与应用内登录窗口(加载 authorize_url + 自动填表脚本)配合。
/// 中止条件:批量取消 / 窗口被用户关闭;超时以服务端 expires_at 为准。
pub async fn login_via_embedded_window(
    app: &tauri::AppHandle,
    db: &SqlitePool,
    http: &crate::zcode::api::ZaiClient,
    batch: &super::batch::BatchLoginHandle,
    script: &str,
) -> AppResult<models::Account> {
    // 1. 初始化轮询会话
    let poll_token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let init = http.init_oauth_polling(&poll_token).await?;
    if init["code"].as_i64() != Some(0) {
        return Err(AppError::Other(
            init["msg"].as_str().unwrap_or("OAuth 会话初始化失败").to_string(),
        ));
    }
    let data = &init["data"];
    let flow_id = data["flow_id"]
        .as_str()
        .ok_or_else(|| AppError::Other("初始化响应缺少 flow_id".into()))?
        .to_string();
    let authorize_url = data["authorize_url"]
        .as_str()
        .ok_or_else(|| AppError::Other("初始化响应缺少 authorize_url".into()))?;
    let expires_at = data["expires_at"].as_i64().unwrap_or(0);
    let interval_sec = data["poll_interval_sec"].as_i64().unwrap_or(3).clamp(1, 30);

    // 2. 复用同名登录窗:先等旧窗销毁(close 是异步的,直接建同名会冲突),再创建新窗
    if let Some(old) = app.get_webview_window("login-flow") {
        let _ = old.close();
        for _ in 0..40 {
            if app.get_webview_window("login-flow").is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    let url: tauri::Url = authorize_url
        .parse()
        .map_err(|e| AppError::Other(format!("非法 authorize_url: {e}")))?;
    // 先开空白页:创建后立即清空浏览数据(cookie/缓存),再导航到授权页。
    // WebView2 的 cookie 在窗口之间共享,若不清,队列中下一个账号会复用上一个
    // 账号的 z.ai 登录态,导致授权页出现错的人、凭证绑错账号。
    let blank: tauri::Url = "about:blank".parse().unwrap();
    let win = tauri::WebviewWindowBuilder::new(
        app,
        "login-flow",
        tauri::WebviewUrl::External(blank),
    )
    .title("z.ai 登录(完成验证后自动继续)")
    .inner_size(480.0, 640.0)
    .center()
    .initialization_script(script)
    .build()
    .map_err(|e| AppError::Other(format!("创建登录窗口失败: {e}")))?;
    if let Err(e) = win.clear_all_browsing_data() {
        log::warn!("清空登录窗口浏览数据失败(可能串号): {e}");
    }
    win.navigate(url)
        .map_err(|e| AppError::Other(format!("导航到授权页失败: {e}")))?;
    let _ = win.set_focus();

    // 3. 轮询至 ready / failed / 过期 / 取消 / 窗口被关,并入库
    poll_and_save_account(db, http, &flow_id, &poll_token, expires_at, interval_sec, || {
        if batch.is_cancelled() {
            Some("队列已取消".into())
        } else if app.get_webview_window("login-flow").is_none() {
            Some("登录窗口被关闭,已跳过".into())
        } else {
            None
        }
    })
    .await
}

/// 批量登录成功后,把独立账密条目关联到新导入的账号:
/// 账号已有关联行 → 先合并账密过去再删独立条目;否则直接把独立条目挂到账号上。
/// 全程静默尽力而为:关联失败不影响登录结果。
pub async fn link_secret_to_account(db: &SqlitePool, secret_id: i64, account_id: i64) {
    let now = chrono::Utc::now().timestamp();
    let _ = sqlx::query(
        "UPDATE secret_entries
         SET login_name = (SELECT login_name FROM secret_entries WHERE id = ?2),
             password = (SELECT password FROM secret_entries WHERE id = ?2),
             updated_at = ?3
         WHERE account_id = ?1",
    )
    .bind(account_id)
    .bind(secret_id)
    .bind(now)
    .execute(db)
    .await;
    let _ = sqlx::query(
        "DELETE FROM secret_entries WHERE id = ?2 AND account_id IS NULL
         AND EXISTS (SELECT 1 FROM secret_entries WHERE account_id = ?1)",
    )
    .bind(account_id)
    .bind(secret_id)
    .execute(db)
    .await;
    let _ = sqlx::query(
        "UPDATE secret_entries SET account_id = ?1, updated_at = ?2
         WHERE id = ?3 AND account_id IS NULL",
    )
    .bind(account_id)
    .bind(now)
    .bind(secret_id)
    .execute(db)
    .await;
}

/// 轮询 OAuth 会话至 ready,提取凭证并去重入库(系统浏览器流与内嵌窗口流共用)。
/// `abort` 返回 Some(原因) 时中止等待(用户取消/窗口被关)。
async fn poll_and_save_account<F>(
    db: &SqlitePool,
    http: &crate::zcode::api::ZaiClient,
    flow_id: &str,
    poll_token: &str,
    expires_at: i64,
    interval_sec: i64,
    abort: F,
) -> AppResult<models::Account>
where
    F: Fn() -> Option<String>,
{
    // 轮询至 ready / failed / 过期(硬上限 10 分钟兜底)
    let deadline = {
        let now = chrono::Utc::now().timestamp();
        let hard_limit = now + 600;
        if expires_at > 0 { expires_at.min(hard_limit) } else { hard_limit }
    };
    let data = loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval_sec as u64)).await;
        if let Some(reason) = abort() {
            return Err(AppError::Other(reason));
        }
        if chrono::Utc::now().timestamp() >= deadline {
            return Err(AppError::Other("登录超时:未在有效期内完成授权".into()));
        }
        let v = match http.poll_oauth(flow_id, poll_token).await {
            Ok(v) => v,
            Err(_) => continue, // 网络抖动重试,直到过期
        };
        if v["code"].as_i64() != Some(0) {
            return Err(AppError::Other(
                v["msg"].as_str().unwrap_or("OAuth 会话查询失败").to_string(),
            ));
        }
        match v["data"]["status"].as_str().unwrap_or("") {
            "pending" => continue,
            "failed" => return Err(AppError::Other("授权失败:登录未完成".into())),
            "ready" => break v["data"].clone(),
            other => {
                return Err(AppError::Other(format!("OAuth 会话状态异常: {other}")));
            }
        }
    };

    // 提取凭证(ready 响应直接携带,无需再换 token)
    let jwt = data["token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::Other("登录完成但响应缺少 token(zcodejwttoken)".into()))?
        .to_string();
    let zai = &data["zai"];
    // 轮询响应里的是 OAuth 生 token,必须再经 z/login 换成业务 token 才是客户端
    // credentials.json 里 oauth:zai:access_token 的真实形态;直接写生 token 会导致
    // 客户端启动校验失败并主动登出清库(表现为切换后要求重新登录)
    let zai_access = match zai["access_token"]
        .as_str()
        .or_else(|| zai["accessToken"].as_str())
        .map(str::to_string)
    {
        Some(raw) => match http.exchange_zai_business_token(&raw).await {
            Ok((business, _)) => Some(business),
            Err(_) => Some(raw), // 换取失败保留生 token,余额/活动查询不受影响
        },
        None => None,
    };
    let zai_refresh = zai["refresh_token"]
        .as_str()
        .or_else(|| zai["refreshToken"].as_str())
        .map(str::to_string);

    // 5. 用户身份:JWT payload 自带 user_id(最可靠),data.user 补充昵称/邮箱
    let user = &data["user"];
    let str_field = |name: &str| user[name].as_str().map(str::to_string);
    let user_id = jwt_user_id(&jwt).or_else(|| {
        str_field("user_id")
            .or_else(|| str_field("sub"))
            .or_else(|| str_field("id"))
    });
    let label = str_field("email")
        .or_else(|| str_field("username"))
        .or_else(|| str_field("displayName"))
        .or_else(|| str_field("name"));
    let display = label.clone().unwrap_or_else(|| "Z.ai 用户".into());
    let user_info = Some(construct_user_info(
        user_id.clone().unwrap_or_default(),
        &str_field("email").unwrap_or_else(|| display.clone()),
        &str_field("name")
            .or_else(|| str_field("username"))
            .unwrap_or(display),
        &str_field("avatar").unwrap_or_default(),
    ));

    let existing = repo::list(db).await?;
    let dup = existing.iter().find(|a| {
        (!a.jwt.is_empty() && a.jwt == jwt)
            || user_id
                .as_deref()
                .is_some_and(|u| a.user_id.as_deref() == Some(u))
    });
    if let Some(d) = dup {
        return Ok(repo::update_credentials(
            db,
            d.id,
            &jwt,
            "",
            user_id.as_deref(),
            user_info.as_deref(),
            zai_access.as_deref(),
            zai_refresh.as_deref(),
        )
        .await?);
    }

    let label = label.unwrap_or_else(|| format!("Z.ai 账号 {}", existing.len() + 1));
    Ok(repo::insert(
        db,
        &models::NewAccount {
            label,
            device_mid: uuid::Uuid::new_v4().to_string(),
            user_id,
            jwt,
            refresh_token: String::new(),
            user_info,
            zai_access_token: zai_access,
            zai_refresh_token: zai_refresh,
        },
    )
    .await?)
}

/// 将该账号写回本机 credentials.json 并重启 ZCode 客户端,使其以此账号身份运行
pub async fn switch_account_and_restart(
    db: &SqlitePool,
    http: &crate::zcode::api::ZaiClient,
    monitor: &crate::zcode::modules::monitor::service::MonitorHandle,
    app: &tauri::AppHandle,
    account_id: i64,
) -> AppResult<()> {
    let mut account = repo::fetch_by_id(db, account_id)
        .await?
        .ok_or_else(|| AppError::Other("账号不存在".into()))?;
    if account.jwt.is_empty() {
        return Err(AppError::Other("该账号没有可用凭证".into()));
    }

    // 自愈:早期导入存的可能是 OAuth 生 token,切换前再换一次业务 token(存的实际是
    // 业务 token 时服务端会拒绝,保留原值即可);换取成功则回填数据库。
    // 注意:z/login 只接受生 token,对业务 token 会报错——换取失败 ≠ token 失效!
    // (此前误判导致切换时移除 active_provider/access_token,ZCode 启动恢复会话时
    // 因 "no active provider" 跳过登录,表现为「连接使用」未登录态)
    // 库里有 access_token 就信任它(批量登录导入的都是新鲜业务 token)并写入完整形态。
    let mut zai_token_valid = false;
    if let Some(raw) = account.zai_access_token.clone().filter(|s| !s.is_empty()) {
        match http.exchange_zai_business_token(&raw).await {
            Ok((business, _)) if business != raw => {
                let _ = sqlx::query(
                    "UPDATE accounts SET zai_access_token = ?1, updated_at = ?2 WHERE id = ?3",
                )
                .bind(&business)
                .bind(chrono::Utc::now().timestamp())
                .bind(account.id)
                .execute(db)
                .await;
                account.zai_access_token = Some(business);
            }
            _ => {}
        }
        zai_token_valid = true;
    }

    // 自愈:早期导入的账号缺 user_info/user_id,用 JWT payload + label 现场构造,并回填数据库
    if account.user_id.is_none() {
        account.user_id = jwt_user_id(&account.jwt);
    }
    if account.user_info.is_none() {
        let email = account.label.clone();
        account.user_info = Some(construct_user_info(
            account.user_id.clone().unwrap_or_default(),
            &email,
            &email,
            "",
        ));
    }
    let _ = sqlx::query(
        "UPDATE accounts SET user_id = ?1, user_info = ?2, updated_at = ?3 WHERE id = ?4",
    )
    .bind(account.user_id.as_deref())
    .bind(account.user_info.as_deref())
    .bind(chrono::Utc::now().timestamp())
    .bind(account.id)
    .execute(db)
    .await;

    // 1. 先写回本机凭证:轮询以"凭证比对"判定使用中,必须让凭证先落位;写失败则不动任何 UI 状态
    let payload = credentials::SwitchPayload {
        jwt: account.jwt,
        user_info: account.user_info,
        zai_access_token: account.zai_access_token,
        zai_refresh_token: account.zai_refresh_token,
        zai_token_valid,
    };
    tauri::async_runtime::spawn_blocking(move || credentials::write_account_switch(&payload))
        .await
        .map_err(|e| AppError::Other(format!("调度切换任务失败: {e}")))??;

    // 2. 凭证落位后立即翻转"使用中"标记(库 + 快照)并广播;在途轮询收尾会重新比对凭证,不会被旧结果覆盖
    let _ = sqlx::query("UPDATE accounts SET is_current = CASE WHEN id = ?1 THEN 1 ELSE 0 END")
        .bind(account.id)
        .execute(db)
        .await;
    monitor.set_current(app, account.id);

    // 3. 重启 ZCode 客户端使其以新账号身份运行
    let app = app.clone();
    tauri::async_runtime::spawn_blocking(move || restart_zcode(&app))
        .await
        .map_err(|e| AppError::Other(format!("调度切换任务失败: {e}")))?
}

/// 重启 ZCode 客户端:先定位安装路径(兼容任意安装盘),结束进程后重新拉起
/// 重启 ZCode 客户端:先定位安装路径(兼容任意安装盘),结束进程后重新拉起。
/// 拉起走 ShellExecute(opener):与用户双击体验一致,不受本进程环境继承影响。
fn restart_zcode(_app: &tauri::AppHandle) -> AppResult<()> {
    // 必须在杀进程前定位:首选运行中进程的真实路径(用户可能装在任意盘)
    let exe = locate_zcode_exe().ok_or_else(|| {
        AppError::Other(
            "未找到 ZCode 客户端:进程未运行,注册表与常见安装目录中也没有,请确认已安装".into(),
        )
    })?;

    let mut kill = std::process::Command::new("taskkill");
    kill.args(["/IM", "ZCode.exe", "/F"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        kill.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let _ = kill.output(); // 未运行时忽略错误
    std::thread::sleep(std::time::Duration::from_secs(3));

    // 启动:继承本进程环境,但必须剔除"毒变量"——
    // ELECTRON_RUN_AS_NODE=1 会让 Electron 客户端(ZCode)退化成无界面 Node 进程秒退;
    // NODE_OPTIONS 会向其注入外部 require shim。ShellExecute 亦会继承父环境块,躲不开,
    // 所以这里用 spawn + env_remove 定向清除,无论 zai-claim 本身从哪种环境启动都能拉起。
    let mut cmd = std::process::Command::new(&exe);
    cmd.env_remove("ELECTRON_RUN_AS_NODE").env_remove("NODE_OPTIONS");
    let poisoned: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("ELECTRON_"))
        .collect();
    for k in poisoned {
        cmd.env_remove(&k);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0000_0008); // DETACHED_PROCESS:与父进程生命周期解耦
    }
    cmd.spawn()
        .map_err(|e| AppError::Other(format!("启动 ZCode 失败({}): {e}", exe.display())))?;
    Ok(())
}

/// 定位 ZCode 客户端 exe:
/// ① 运行中进程的真实路径(最准,进程在时必中);
/// ② 注册表卸载表(HKCU/HKLM,含 WOW6432Node)的 InstallLocation/DisplayIcon;
/// ③ 常见安装位置逐个试探。
fn locate_zcode_exe() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let no_window = 0x0800_0000u32;

        // ① 运行中进程的真实路径
        let script = "(Get-Process ZCode -ErrorAction SilentlyContinue | \
              Select-Object -First 1).Path";
        if let Ok(out) = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", script])
            .creation_flags(no_window)
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if out.status.success() && !s.is_empty() {
                let p = PathBuf::from(&s);
                if p.is_file() {
                    return Some(p);
                }
            }
        }

        // ② 注册表卸载表:DisplayName 含 ZCode 的项,取 InstallLocation 或 DisplayIcon
        let script = "\
$k = 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*',\
'HKLM:\\Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*',\
'HKLM:\\Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\*';\
$e = Get-ItemProperty $k -ErrorAction SilentlyContinue | \
Where-Object { $_.DisplayName -like '*ZCode*' } | Select-Object -First 1; \
if ($e -and $e.InstallLocation) { Join-Path $e.InstallLocation 'ZCode.exe' } \
elseif ($e -and $e.DisplayIcon) { ($e.DisplayIcon -split ',')[0] }";
        if let Ok(out) = std::process::Command::new("powershell")
            .args(["-NoProfile", "-Command", script])
            .creation_flags(no_window)
            .output()
        {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                let p = PathBuf::from(&s);
                // DisplayIcon 可能指向卸载器,退到同目录的 ZCode.exe
                if p.is_file() && p.file_name().is_some_and(|n| n.eq_ignore_ascii_case("ZCode.exe")) {
                    return Some(p);
                }
                let sibling = p.parent().map(|d| d.join("ZCode.exe"));
                if let Some(sib) = sibling {
                    if sib.is_file() {
                        return Some(sib);
                    }
                }
            }
        }
    }

    // ③ 常见安装位置
    let candidates = [
        ("LOCALAPPDATA", r"Programs\ZCode\ZCode.exe"),
        ("PROGRAMFILES", r"ZCode\ZCode.exe"),
        ("ProgramFiles(x86)", r"ZCode\ZCode.exe"),
        ("LOCALAPPDATA", r"ZCode\ZCode.exe"),
    ];
    for (var, tail) in candidates {
        if let Ok(base) = std::env::var(var) {
            let p = PathBuf::from(base).join(tail);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实测邀请关系自动同步:把备份库复制到临时目录,在其上跑同步并打印结果。
    /// 依赖备份文件存在与真实接口,平时忽略;--ignored 显式运行。
    #[tokio::test]
    #[ignore]
    async fn sync_invite_live() {
        let src = std::env::var("APPDATA").unwrap()
            .replace(std::path::MAIN_SEPARATOR, "/")
            + "/com.abellee.zaiclaim/zai-claim.db.backup-20261015";
        let tmp = std::env::temp_dir().join("zai-claim-sync-test.db");
        std::fs::copy(&src, &tmp).unwrap();
        let url = format!("sqlite://{}", tmp.display().to_string().replace('\\', "/"));
        let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
        // 备份库是旧 schema:补上 0007 的两个列(幂等处理)
        let _ = sqlx::query("ALTER TABLE accounts ADD COLUMN invite_role TEXT").execute(&pool).await;
        let _ = sqlx::query("ALTER TABLE accounts ADD COLUMN invited_by INTEGER").execute(&pool).await;

        let http = crate::zcode::api::ZaiClient::new().unwrap();
        let r = sync_invite_relations(&pool, &http).await.unwrap();
        println!(
            "inviters={} linked={} unmatched={:?} errors={:?}",
            r.inviters, r.linked, r.unmatched, r.errors
        );
        let rows: Vec<(String, Option<String>, Option<i64>)> =
            sqlx::query_as("SELECT label, invite_role, invited_by FROM accounts WHERE invite_role IS NOT NULL ORDER BY id")
                .fetch_all(&pool)
                .await
                .unwrap();
        for row in rows {
            println!("{row:?}");
        }
        // 至少:2 个主号(0cykqg0n5/011abaaq8),1 个自动绑定(x1tcjr1c5)
        assert!(r.inviters >= 2, "应至少识别 2 个邀请者");
        assert!(r.linked >= 1, "应至少自动绑定 1 个小号");
    }
}
