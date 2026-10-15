//! 批量账密登录:把账密库里未关联账号的条目逐个走「内嵌登录窗口 + OAuth 轮询」导入。
//! 窗口加载 zcode 的 authorize_url(307 到 chat.z.ai 登录页),initialization_script
//! 自动填邮箱密码、点验证与登录;验证码为阿里云智能验证(与 claim 同体系),通常无感通过,
//! 弹滑块时用户在窗口里手动滑一下即可。登录完成 z.ai 自动跳回 authorize → callback,
//! zcode 侧 flow 变 ready,轮询拿凭证入库,随后自动开始队列中的下一个账号。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

/// 批量任务句柄:同一时间只允许一个批量队列,可随时取消
#[derive(Default)]
pub struct BatchLoginHandle {
    running: AtomicBool,
    cancel: AtomicBool,
}

impl BatchLoginHandle {
    /// 尝试占用:已有队列在跑时返回 false
    pub fn try_start(&self) -> bool {
        self.running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn finish(&self) {
        self.running.store(false, Ordering::SeqCst);
        self.cancel.store(false, Ordering::SeqCst);
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }
}

/// 单条进度事件(事件名 batch:progress)
#[derive(Debug, Clone, Serialize)]
pub struct BatchProgress {
    pub index: usize,
    pub total: usize,
    pub secret_id: i64,
    pub login_name: String,
    /// running | success | failed | cancelled
    pub state: String,
    pub message: String,
}

pub const BATCH_PROGRESS_EVENT: &str = "batch:progress";
pub const BATCH_DONE_EVENT: &str = "batch:done";

fn emit_progress(app: &AppHandle, p: &BatchProgress) {
    let _ = app.emit(BATCH_PROGRESS_EVENT, p);
}

/// 队列条目:来自 secret_entries 的一行
pub struct BatchLoginItem {
    pub secret_id: i64,
    pub login_name: String,
    pub password: String,
}

/// 批量驱动:逐个登录,每条开始/结束都广播进度;取消后立即停止队列。
/// 单个失败不中断队列(账密错误/超时等),最后汇总广播 batch:done。
pub async fn run_batch_login(
    app: AppHandle,
    db: sqlx::SqlitePool,
    http: crate::zcode::api::ZaiClient,
    batch: Arc<BatchLoginHandle>,
    monitor: crate::zcode::modules::monitor::service::MonitorHandle,
    items: Vec<BatchLoginItem>,
) {
    let total = items.len();
    let (mut ok, mut fail) = (0usize, 0usize);

    for (i, item) in items.into_iter().enumerate() {
        if batch.is_cancelled() {
            emit_progress(
                &app,
                &BatchProgress {
                    index: i,
                    total,
                    secret_id: item.secret_id,
                    login_name: item.login_name,
                    state: "cancelled".into(),
                    message: "队列已取消".into(),
                },
            );
            continue;
        }

        emit_progress(
            &app,
            &BatchProgress {
                index: i,
                total,
                secret_id: item.secret_id,
                login_name: item.login_name.clone(),
                state: "running".into(),
                message: "正在打开登录窗口".into(),
            },
        );

        // 账密经 serde_json 转义后内插进脚本,防注入
        let email = serde_json::to_string(&item.login_name).unwrap_or_else(|_| "\"\"".into());
        let password = serde_json::to_string(&item.password).unwrap_or_else(|_| "\"\"".into());
        let script = autofill_script(&email, &password);

        let result =
            super::service::login_via_embedded_window(&app, &db, &http, &batch, &script).await;

        match result {
            Ok(account) => {
                super::service::link_secret_to_account(&db, item.secret_id, account.id).await;
                crate::zcode::modules::accounts::events::emit_accounts_changed(&app).ok();
                monitor.refresh_account(&app, account.id).await;
                ok += 1;
                emit_progress(
                    &app,
                    &BatchProgress {
                        index: i,
                        total,
                        secret_id: item.secret_id,
                        login_name: item.login_name,
                        state: "success".into(),
                        message: format!("已导入为「{}」", account.label),
                    },
                );
            }
            Err(e) => {
                fail += 1;
                emit_progress(
                    &app,
                    &BatchProgress {
                        index: i,
                        total,
                        secret_id: item.secret_id,
                        login_name: item.login_name,
                        state: "failed".into(),
                        message: e.to_string(),
                    },
                );
            }
        }

        // 兜底关窗(正常流程 login_via_embedded_window 内部已关)
        if let Some(win) = app.get_webview_window("login-flow") {
            let _ = win.close();
        }
    }

    batch.finish();
    let _ = app.emit(
        BATCH_DONE_EVENT,
        serde_json::json!({ "total": total, "ok": ok, "fail": fail }),
    );
}

/// 自动填表脚本:注入登录窗口,覆盖三段页面:
/// ① chat.z.ai 登录页:填账密 → 点验证 → 点登录
/// ② OAuth 授权确认页(「ZCode 想要访问您的 Z.ai 账号」):勾协议 → 点继续
/// ③ 其他页面:只观察,等待 Rust 端轮询到凭证后自动关窗进下一个
/// initialization_script 在每个页面(document start)都会执行;
/// SPA 渲染时序不定,用 500ms 轮询 DOM 直到目标出现。
///
/// 时序设计:
/// - 填表一次(filled 标志),此后不再动输入框(避免覆盖用户手动修改)
/// - 「点击开始验证」按钮存在时每 5s 点一次(无感验证通常 1~2s 自动过,按钮消失即成功)
/// - 验证按钮消失后每 3s 点一次「登录」,最多 10 次(防密码错误时无限重试)
/// - 授权页每 2s 检查一次:勾协议 + 点「继续」(只点一次,点击后页面即跳转)
/// - 总时长 5 分钟兜底,超时后停止自动操作,窗口留给用户手动救场(如邮箱二次验证)
fn autofill_script(email_json: &str, password_json: &str) -> String {
    format!(
        r#"(function () {{
  if (!/chat\.z\.ai/.test(location.href)) return;
  var EMAIL = {email_json};
  var PASSWORD = {password_json};
  function setValue(el, v) {{
    var proto = el instanceof HTMLTextAreaElement
      ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    var setter = Object.getOwnPropertyDescriptor(proto, "value").set;
    setter.call(el, v);
    el.dispatchEvent(new Event("input", {{ bubbles: true }}));
    el.dispatchEvent(new Event("change", {{ bubbles: true }}));
  }}
  var filled = false, lastCapClick = 0, lastLoginClick = 0, loginTries = 0, ticks = 0;
  var agreed = false, lastAgreeClick = 0;
  var timer = setInterval(function () {{
    ticks++;
    if (ticks > 600) {{ clearInterval(timer); return; }}
    var now = Date.now();
    // ── 授权确认页(「ZCode 想要访问您的 Z.ai 账号」):勾协议 → 点继续 ──
    var agreeBtn = null;
    document.querySelectorAll("button").forEach(function (b) {{
      var t = (b.textContent || "").trim();
      if (!agreeBtn && t && t.length <= 6 &&
          (t === "继续" || t === "Continue" || t === "授权" || t === "Allow" || t === "允许")) agreeBtn = b;
    }});
    if (agreeBtn) {{
      // 勾选所有未勾选的 checkbox(该页面只有协议一个)
      document.querySelectorAll('input[type=checkbox]').forEach(function (c) {{
        if (!c.checked) {{
          c.click();
          var setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "checked").set;
          setter.call(c, true);
          c.dispatchEvent(new Event("change", {{ bubbles: true }}));
          c.dispatchEvent(new Event("input", {{ bubbles: true }}));
        }}
      }});
      // 协议生效前按钮 disabled;可点时只点一次(点了页面即跳转)
      if (!agreed && !agreeBtn.disabled && now - lastAgreeClick > 2000) {{
        lastAgreeClick = now;
        agreed = true;
        agreeBtn.click();
      }}
      return;
    }}
    var email = null, pass = null;
    document.querySelectorAll("input").forEach(function (el) {{
      var t = (el.getAttribute("type") || "").toLowerCase();
      if (t === "password") {{ if (!pass) pass = el; }}
      else if (!email && (t === "email" || (el.placeholder || "").indexOf("邮箱") >= 0)) email = el;
    }});
    if (email && pass && !filled) {{
      setValue(email, EMAIL);
      setValue(pass, PASSWORD);
      filled = true;
    }}
    if (!filled) return;
    // 验证按钮:文本含「开始验证」;存在时周期性点击(弹滑块时用户手动滑,滑完按钮消失)
    var capBtn = null;
    document.querySelectorAll("button, [role=button]").forEach(function (b) {{
      var t = (b.textContent || "").trim();
      if (!capBtn && t && t.length <= 16 && t.indexOf("开始验证") >= 0) capBtn = b;
    }});
    if (capBtn) {{
      if (now - lastCapClick > 5000) {{ lastCapClick = now; capBtn.click(); }}
      return;
    }}
    // 验证已完成(按钮消失),周期性点登录,10 次后放弃自动重试
    if (loginTries >= 10) return;
    if (now - lastLoginClick > 3000) {{
      var login = null;
      document.querySelectorAll("button").forEach(function (b) {{
        var t = (b.textContent || "").trim();
        if (!login && t === "登录" && !b.disabled) login = b;
      }});
      if (login) {{
        lastLoginClick = now;
        loginTries++;
        login.click();
      }}
    }}
  }}, 500);
}})();"#
    )
}
