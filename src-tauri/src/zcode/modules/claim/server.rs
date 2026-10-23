//! 领取流程的本地 HTTP 服务(127.0.0.1 随机端口,首次领取时启动):
//! - GET /claim              嵌入式领取页(加载阿里云验证码组件)
//! - POST /claim-submit      页面完成验证后提交,Rust 携带 JWT 调 claim 接口
//! 敏感凭证不出 Rust;浏览器页只是"完成验证码 + 展示结果"的壳。

use std::net::TcpListener;

use tauri::{AppHandle, Emitter};

use tiny_http::{Header, Method, Request, Response};

use crate::zcode::api::ZaiClient;
use crate::zcode::db::repo;
use crate::zcode::error::{AppError, AppResult};

const CLAIM_PAGE: &str = include_str!("../../../../assets/claim.html");
const BIND_PAGE: &str = include_str!("../../../../assets/bind.html");

/// 捆绑结果事件(register:progress 之外的独立通道)。
/// 捆绑真正的 bind-code 调用由系统浏览器里的页面异步触发,Rust 在收到结果后
/// 经这个事件广播,前端才能显示真实成败(而不是"已打开页面"的假成功)。
pub const BIND_RESULT_EVENT: &str = "zcode:bind-result";

#[derive(Debug, Clone, serde::Serialize)]
pub struct BindResultPayload {
    pub account_id: i64,
    pub inviter_id: i64,
    pub ok: bool,
    pub message: String,
}

pub fn start(app: AppHandle, db: sqlx::SqlitePool, http: ZaiClient) -> Result<u16, String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    let server =
        tiny_http::Server::from_listener(listener, None).map_err(|e| e.to_string())?;
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            handle(request, &app, &db, &http);
        }
    });
    Ok(port)
}

fn content_header(value: &str) -> Header {
    Header::from_bytes("Content-Type", value).expect("合法头值")
}

fn respond(request: Request, code: u16, body: String, content_type: &str) {
    let response = Response::from_string(body)
        .with_status_code(code)
        .with_header(content_header(content_type));
    let _ = request.respond(response);
}

fn respond_json(request: Request, body: &serde_json::Value) {
    respond(
        request,
        200,
        body.to_string(),
        "application/json; charset=utf-8",
    );
}

/// 从库中取账号(领取页请求到达时解析)
fn resolve_account(
    db: &sqlx::SqlitePool,
    account_id: i64,
) -> AppResult<crate::zcode::db::models::Account> {
    tauri::async_runtime::block_on(async {
        let accounts = repo::list(db).await.map_err(|e| AppError::Other(e.to_string()))?;
        accounts
            .into_iter()
            .find(|a| a.id == account_id)
            .ok_or_else(|| AppError::Other("账号不存在".into()))
    })
}

/// 同时取被邀请账号与主号账号(捆绑页请求到达时解析)
fn resolve_pair(
    db: &sqlx::SqlitePool,
    account_id: i64,
    inviter_id: i64,
) -> AppResult<(crate::zcode::db::models::Account, crate::zcode::db::models::Account)> {
    let account = resolve_account(db, account_id)?;
    let inviter = resolve_account(db, inviter_id)?;
    Ok((account, inviter))
}

fn handle(mut request: Request, app: &AppHandle, db: &sqlx::SqlitePool, http: &ZaiClient) {
    let url = request.url().to_string();
    let path = url.split('?').next().unwrap_or("").to_string();
    let query = url
        .split_once('?')
        .map(|(_, q)| q.to_string())
        .unwrap_or_default();
    let param = |name: &str| {
        query.split('&').find_map(|kv| {
            kv.split_once('=')
                .filter(|(k, _)| *k == name)
                .map(|(_, v)| v.to_string())
        })
        .unwrap_or_default()
    };

    match (request.method().clone(), path.as_str()) {
        (Method::Get, "/claim") => {
            respond(
                request,
                200,
                CLAIM_PAGE.to_string(),
                "text/html; charset=utf-8",
            );
        }
        (Method::Get, "/bind") => {
            respond(
                request,
                200,
                BIND_PAGE.to_string(),
                "text/html; charset=utf-8",
            );
        }
        (Method::Get, "/claim-config") => {
            let account_id: i64 = param("account").parse().unwrap_or(-1);
            let result: AppResult<serde_json::Value> = (|| {
                let account = resolve_account(db, account_id)?;
                let configs = tauri::async_runtime::block_on(async {
                    http.get_client_configs(&account.jwt, &account.device_mid)
                        .await
                })
                .map_err(|e| AppError::Other(e.to_string()))?;
                Ok(serde_json::json!({
                    "ok": true,
                    "label": account.label,
                    "captcha": configs["data"]["configs"]["captcha"].clone(),
                }))
            })();
            let body = match result {
                Ok(v) => v,
                Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
            };
            respond_json(request, &body);
        }
        (Method::Post, "/claim-submit") => {
            let mut buf = String::new();
            let _ = request.as_reader().read_to_string(&mut buf);
            let payload: serde_json::Value =
                serde_json::from_str(&buf).unwrap_or(serde_json::Value::Null);
            let account_id = payload["account"].as_i64().unwrap_or(-1);
            let plan_id = payload["plan_id"].as_str().unwrap_or("").to_string();
            let cvp = payload["captcha_verify_param"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let region = payload["region"].as_str().map(str::to_string);

            let result: AppResult<serde_json::Value> = (|| {
                let account = resolve_account(db, account_id)?;
                tauri::async_runtime::block_on(async {
                    http.post_claim(
                        &account.jwt,
                        &account.device_mid,
                        &plan_id,
                        &cvp,
                        region.as_deref(),
                    )
                    .await
                })
            })()
            .map(|v| {
                let code = v["code"].as_i64().unwrap_or(-1);
                let msg = v["msg"].as_str().unwrap_or("");
                let message = if code == 0 {
                    if msg.is_empty() { "领取成功".to_string() } else { msg.to_string() }
                } else if msg.is_empty() {
                    format!("领取失败(code={code})")
                } else {
                    msg.to_string()
                };
                serde_json::json!({ "ok": code == 0, "message": message })
            });

            let body = match result {
                Ok(v) => v,
                Err(e) => serde_json::json!({ "ok": false, "message": e.to_string() }),
            };
            respond_json(request, &body);
        }
        (Method::Get, "/bind-config") => {
            let account_id: i64 = param("account").parse().unwrap_or(-1);
            let inviter_id: i64 = param("inviter").parse().unwrap_or(-1);
            let result: AppResult<serde_json::Value> = (|| {
                let (account, inviter) = resolve_pair(db, account_id, inviter_id)?;
                // 预检:自己绑自己/已绑定/主号冷却/主号无码,直接给页面可展示的原因
                super::super::accounts::invite::check_bind_precondition(&account, &inviter)
                    .map_err(|e| AppError::Other(e.to_string()))?;
                let code = inviter
                    .invite_code
                    .as_deref()
                    .and_then(super::super::accounts::invite::normalize_code)
                    .ok_or_else(|| AppError::Other("主号没有可用邀请码".into()))?;
                let configs = tauri::async_runtime::block_on(async {
                    http.get_client_configs(&account.jwt, &account.device_mid).await
                })
                .map_err(|e| AppError::Other(e.to_string()))?;
                Ok(serde_json::json!({
                    "ok": true,
                    "account_label": account.label,
                    "inviter_label": inviter.label,
                    "invite_code": code,
                    "captcha": configs["data"]["configs"]["captcha"].clone(),
                }))
            })();
            let body = match result {
                Ok(v) => v,
                Err(e) => serde_json::json!({ "ok": false, "error": e.to_string() }),
            };
            respond_json(request, &body);
        }
        (Method::Post, "/bind-submit") => {
            let mut buf = String::new();
            let _ = request.as_reader().read_to_string(&mut buf);
            let payload: serde_json::Value =
                serde_json::from_str(&buf).unwrap_or(serde_json::Value::Null);
            let account_id = payload["account"].as_i64().unwrap_or(-1);
            let inviter_id = payload["inviter"].as_i64().unwrap_or(-1);
            let cvp = payload["captcha_verify_param"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let region = payload["region"].as_str().map(str::to_string);

            let result: AppResult<serde_json::Value> = (|| {
                let (account, inviter) = resolve_pair(db, account_id, inviter_id)?;
                super::super::accounts::invite::check_bind_precondition(&account, &inviter)
                    .map_err(|e| AppError::Other(e.to_string()))?;
                let code = inviter
                    .invite_code
                    .as_deref()
                    .and_then(super::super::accounts::invite::normalize_code)
                    .ok_or_else(|| AppError::Other("主号没有可用邀请码".into()))?;
                let v = tauri::async_runtime::block_on(async {
                    http.bind_invite_code(
                        &account.jwt,
                        &account.device_mid,
                        &code,
                        &cvp,
                        region.as_deref(),
                    )
                    .await
                })?;
                // 官方判定:code==0 且 data.bound===true 才算成功
                let bound = super::super::accounts::invite::is_bind_success(&v);
                if bound {
                    // 落库:标记为被邀请者并记归属(邀请者同步标 inviter,分组视图立即可见)
                    let now = chrono::Utc::now().timestamp();
                    tauri::async_runtime::block_on(async {
                        let _ = sqlx::query(
                            "UPDATE accounts SET invite_role = 'invited', invited_by = ?1, updated_at = ?2 WHERE id = ?3",
                        )
                        .bind(inviter.id)
                        .bind(now)
                        .bind(account.id)
                        .execute(db)
                        .await;
                        let _ = sqlx::query(
                            "UPDATE accounts SET invite_role = 'inviter', invited_by = NULL, updated_at = ?1 WHERE id = ?2",
                        )
                        .bind(now)
                        .bind(inviter.id)
                        .execute(db)
                        .await;
                    });
                    Ok(serde_json::json!({
                        "ok": true,
                        "message": format!("已捆绑到「{}」", inviter.label),
                    }))
                } else {
                    let code = v["code"].as_i64();
                    Ok(serde_json::json!({
                        "ok": false,
                        "message": super::super::accounts::invite::bind_error_message(code),
                    }))
                }
            })();

            let body = match result {
                Ok(v) => {
                    // 真实结果广播:浏览器页面已展示过,但注册流水线与账号页需要知道成败
                    let ok = v["ok"].as_bool().unwrap_or(false);
                    let msg = v["message"].as_str().unwrap_or("").to_string();
                    let _ = app.emit(
                        BIND_RESULT_EVENT,
                        BindResultPayload {
                            account_id,
                            inviter_id,
                            ok,
                            message: msg.clone(),
                        },
                    );
                    v
                }
                Err(e) => {
                    let _ = app.emit(
                        BIND_RESULT_EVENT,
                        BindResultPayload {
                            account_id,
                            inviter_id,
                            ok: false,
                            message: e.to_string(),
                        },
                    );
                    serde_json::json!({ "ok": false, "message": e.to_string() })
                }
            };
            respond_json(request, &body);
        }
        _ => {
            respond(
                request,
                404,
                "not found".to_string(),
                "text/plain; charset=utf-8",
            );
        }
    }
}
