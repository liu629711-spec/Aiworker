use std::sync::Arc;

use tauri::{AppHandle, State};

use crate::zcode::db::repo;
use crate::zcode::error::{AppError, AppResult};
use crate::zcode::state::ZcodeState;

use super::{events, service};

/// 领取入口:系统浏览器打开本地领取页(完成阿里云验证码),Rust 轮询套餐到账后收尾。
/// 不再用 Playwright sidecar(依赖 chromium 安装,报错还撑爆按钮),本地页对任意浏览器可用。
#[tauri::command]
pub async fn zcode_claim_start(
    app: AppHandle,
    state: State<'_, ZcodeState>,
    account_id: i64,
    plan_id: String,
) -> AppResult<()> {
    let account = repo::list(&state.db)
        .await?
        .into_iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| AppError::Other("账号不存在".into()))?;

    let claims: Arc<service::ClaimRuntime> = state.claims.clone();
    if !claims.mark_busy(account_id) {
        return Err(AppError::Other("该账号已有领取流程进行中".into()));
    }
    let port = claims.ensure_server(&state.db, &state.http)?;

    let url = format!(
        "http://127.0.0.1:{port}/claim?account={account_id}&plan={}",
        urlencode(&plan_id)
    );
    events::emit_started(&app, account_id, &plan_id);

    // 系统默认浏览器打开领取页(JWT 不出 Rust,页面只是完成验证码的壳)
    crate::zcode::modules::accounts::oauth::open_in_system_browser(&app, &url)?;

    // 轮询 balance 确认套餐到账(领取成功后 plans 会多出该 plan_id),最长 10 分钟
    let app = app.clone();
    let http = state.http.clone();
    let plan_id = plan_id.clone();
    let jwt = account.jwt;
    let device_mid = account.device_mid;
    tauri::async_runtime::spawn(async move {
        let (mut ok, mut message) = (
            false,
            "未检测到套餐到账:若浏览器里显示已领成功,点「刷新用量」即可同步".to_string(),
        );
        for _ in 0..40 {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            match http.get_balance(&jwt, &device_mid).await {
                Ok(v) => {
                    let arrived = v["data"]["plans"]
                        .as_array()
                        .map(|ps| {
                            ps.iter()
                                .any(|p| p["plan_id"].as_str() == Some(plan_id.as_str()))
                        })
                        .unwrap_or(false);
                    if arrived {
                        ok = true;
                        message = "套餐已到账".into();
                        break;
                    }
                }
                Err(_) => continue, // 网络抖动,继续等
            }
        }
        claims.mark_free(account_id);
        events::emit_finished(&app, account_id, &plan_id, ok, &message);
    });
    Ok(())
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}
