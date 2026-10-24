//! 注册流水线注入页面的脚本(chat.z.ai 的 SPA 页面)。
//!
//! 三段页面:
//! ① `/auth?action=signup` 注册表单:填邮箱/密码/用户名 → 过阿里云验证码 → 提交
//! ② `/auth/verify` 与 `/auth/verify_email`:邮箱验证(设密码) —— 由 Rust 注入 token 驱动
//! ③ zcode OAuth 授权确认页:勾协议 → 点继续
//!
//! 与 batch.rs 的 autofill_script 同模式:500ms 轮询 DOM,SPA 渲染时序不定。
//! 阶段完成信号走 `location.hash`:脚本把 `#zflow=<flag>:<true|false>` 写进 hash,
//! Rust 侧读 `win.url()` 的 fragment 拿值。
//! (早期版本用 document.title 通道,但 Tauri 的 `win.title()` 读的是 OS 窗口标题栏
//! ——tao 的 GetWindowTextW——收不到 WebView 里的 document.title,通道必然失效;
//! hash 在同源内可写且 `win.url()` 能真实读回,是可行的取数方式。)
//! 真机联调时按窗口内提示与日志微调选择器。

use serde_json::to_string as json_str;

/// 页面侧通用的 flag 上报函数(注入到 window.__zflowReport)。
/// 各段脚本在阶段完成时调用 `window.__zflowReport("flag", true)`。
/// 用 hash 而非 title:hash 同源可写、不会触发页面跳转、Rust 侧 win.url() 能读回。
pub const FLAG_REPORT_HELPER: &str = r##"
window.__zflowReport = window.__zflowReport || function (flag, ok) {
  try {
    var cur = location.hash || "";
    var want = "#zflow=" + flag + ":" + (ok ? "1" : "0");
    if (cur.indexOf(want) !== 0) { location.hash = want; }
  } catch (e) {}
};
"##;

/// 注册表单段脚本:填表 → 触发验证码 → 提交。
/// 邮箱/密码经 serde_json 转义后内插,防注入。
pub fn signup_script(email: &str, password: &str, username: &Option<String>) -> String {
    let email_json = json_str(email).unwrap_or_else(|_| "\"\"".into());
    let password_json = json_str(password).unwrap_or_else(|_| "\"\"".into());
    let username_json = json_str(username).unwrap_or_else(|_| "null".into());
    format!(
        r#"(function () {{
  if (!/chat\.z\.ai/.test(location.href)) return;
  var EMAIL = {email_json};
  var PASSWORD = {password_json};
  var USERNAME = {username_json};
  function setValue(el, v) {{
    var proto = el instanceof HTMLTextAreaElement
      ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    var setter = Object.getOwnPropertyDescriptor(proto, "value").set;
    setter.call(el, v);
    el.dispatchEvent(new Event("input", {{ bubbles: true }}));
    el.dispatchEvent(new Event("change", {{ bubbles: true }}));
  }}
  var filled = false, lastCapClick = 0, lastSubmitClick = 0, submitTries = 0, ticks = 0;
  var timer = setInterval(function () {{
    ticks++;
    if (ticks > 3600) {{ clearInterval(timer); return; }}   // 30 分钟兜底
    var now = Date.now();
    // 已进入验证页/授权页/登录页:本段使命结束(登录页交给 login_script)
    if (location.pathname.indexOf("/auth/verify") === 0 ||
        location.pathname.indexOf("/auth/oauth") === 0 ||
        location.pathname.indexOf("/auth/login") === 0) {{
      if (window.__zflowReport) window.__zflowReport("__zaiSignupSubmitted", true);
      clearInterval(timer);
      return;
    }}
    // 表单三类输入:邮箱 / 密码 / 用户名(注册页才有)
    var email = null, pass = null, name = null;
    document.querySelectorAll("input").forEach(function (el) {{
      var t = (el.getAttribute("type") || "").toLowerCase();
      var ph = el.placeholder || "";
      if (t === "password") {{ if (!pass) pass = el; }}
      else if (!email && (t === "email" || ph.indexOf("邮箱") >= 0 || ph.indexOf("Email") >= 0)) email = el;
      else if (!name && (t === "text" || t === "") && (ph.indexOf("用户名") >= 0 || ph.indexOf("Username") >= 0 || ph.indexOf("昵称") >= 0)) name = el;
    }});
    if (email && pass && !filled) {{
      setValue(email, EMAIL);
      setValue(pass, PASSWORD);
      if (name && USERNAME) setValue(name, USERNAME);
      filled = true;
    }}
    if (!filled) return;
    // 阿里云智能验证按钮(与批量登录同体系):存在时周期性点击,弹滑块由用户手动滑
    var capBtn = null;
    document.querySelectorAll("button, [role=button], div[class*=captcha]").forEach(function (b) {{
      var t = (b.textContent || "").trim();
      if (!capBtn && t && t.length <= 20 &&
          (t.indexOf("开始验证") >= 0 || t.indexOf("点击验证") >= 0 || t.indexOf("验证") >= 0 && t.length <= 6)) capBtn = b;
    }});
    if (capBtn) {{
      if (now - lastCapClick > 5000) {{ lastCapClick = now; capBtn.click(); }}
      return;
    }}
    // 验证已过:点提交(注册/ Sign up / 发送验证码),上限 10 次防死循环
    if (submitTries >= 10) return;
    if (now - lastSubmitClick > 3000) {{
      var submit = null;
      document.querySelectorAll("button").forEach(function (b) {{
        var t = (b.textContent || "").trim();
        if (!submit && !b.disabled && t && t.length <= 12 &&
            (t === "注册" || t === "注 册" || t === "Sign up" || t === "Sign Up" ||
             t.indexOf("发送") >= 0 || t.indexOf("获取验证码") >= 0 || t === "继续" || t === "Continue")) submit = b;
      }});
      if (submit) {{
        lastSubmitClick = now;
        submitTries++;
        submit.click();
      }}
    }}
  }}, 500);
}})();"#
    )
}

/// 邮箱验证段脚本:Rust 收到邮件后注入 token,驱动页面完成 finish_signup。
/// token 经 serde_json 转义;页面跳走到登录态即上报 __zaiSignupVerified。
///
/// 完成判据收紧:离开 `/auth` 还不够(可能被重定向到登录页 = 假阳性),
/// 要求到达 `/` 或 `/c/` 之类登录态页面,或 URL 带上了登录后才会出现的参数。
pub fn verify_script(token: &str) -> String {
    let token_json = json_str(token).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"(function () {{
  if (!/chat\.z\.ai/.test(location.href)) return;
  var TOKEN = {token_json};
  function setValue(el, v) {{
    var proto = el instanceof HTMLTextAreaElement
      ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    var setter = Object.getOwnPropertyDescriptor(proto, "value").set;
    setter.call(el, v);
    el.dispatchEvent(new Event("input", {{ bubbles: true }}));
    el.dispatchEvent(new Event("change", {{ bubbles: true }}));
  }}
  // 若页面是「输入 6 位验证码」形态,把 token 里的数字部分填进去
  var digits = TOKEN.replace(/[^0-9]/g, "");
  var filled = false, lastClick = 0, tries = 0, ticks = 0;
  var timer = setInterval(function () {{
    ticks++;
    if (ticks > 480) {{ clearInterval(timer); return; }}   // 4 分钟兜底
    // 已登录(离开 /auth 且不是登录页):验证完成。
    // 只认「离开 /auth 且路径不是 /auth 下的登录/错误页」,避免被踢回登录页时假阳性。
    var p = location.pathname;
    var leftAuth = p.indexOf("/auth") !== 0;
    var notLogin = p.indexOf("/auth/login") !== 0 && p.indexOf("/login") !== 0;
    if (leftAuth && notLogin) {{
      if (window.__zflowReport) window.__zflowReport("__zaiSignupVerified", true);
      clearInterval(timer);
      return;
    }}
    var code = null;
    document.querySelectorAll("input").forEach(function (el) {{
      var t = (el.getAttribute("type") || "").toLowerCase();
      var ph = el.placeholder || "";
      if (!code && (t === "text" || t === "number" || t === "tel") &&
          (ph.indexOf("验证码") >= 0 || ph.indexOf("code") >= 0 || ph.indexOf("Code") >= 0 ||
           (el.getAttribute("inputmode") || "") === "one-time-code" || el.maxLength === 6)) code = el;
    }});
    if (code && !filled && digits) {{
      setValue(code, digits);
      filled = true;
    }}
    if (!filled) return;
    var nowMs = Date.now();
    if (nowMs - lastClick > 3000 && tries < 10) {{
      var btn = null;
      document.querySelectorAll("button").forEach(function (b) {{
        var t = (b.textContent || "").trim();
        if (!btn && !b.disabled && t && t.length <= 12 &&
            (t === "验证" || t === "确认" || t === "继续" || t === "Continue" || t === "Verify" ||
             t.indexOf("完成") >= 0 || t === "提交")) btn = b;
      }});
      if (btn) {{ lastClick = nowMs; tries++; btn.click(); }}
    }}
  }}, 500);
}})();"#
    )
}

/// 注册已完成但会话未建立时的「登录 + 授权确认」段脚本。
///
/// 为什么需要它(实测 2026-10-23 16:25):
/// 邮件验证链接只负责把邮箱标成已验证,验证完官方会**要求重新登录**;
/// 而后续 OAuth 授权页在未登录时会被踢回 `/auth/login`。
/// 旧实现只在授权页 eval 一次 authorize_script,既不会登录也拿不到授权按钮,
/// 表现就是第 4 步超时、账号永远进不了池。
///
/// 本脚本随窗口 initialization_script 注入,**每个 document 都会重跑**,
/// 所以跨多次跳转(登录页 → 授权页)都能各自生效:
/// - 已在登录态(不在 /auth 下)→ 上报 `__zaiSessionReady`
/// - 授权确认页 → 勾协议 + 点继续
/// - 登录页 → 填账密 → 过验证 → 点登录
/// - 验证页 → 让位给 verify_script,不插手
pub fn login_script(email: &str, password: &str) -> String {
    let email_json = json_str(email).unwrap_or_else(|_| "\"\"".into());
    let password_json = json_str(password).unwrap_or_else(|_| "\"\"".into());
    format!(
        r#"(function () {{
  if (!/(^|\.)z\.ai$/i.test(location.hostname)) return;
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
  var filled = false, lastCapClick = 0, lastLoginClick = 0, loginTries = 0;
  var agreed = false, lastAgreeClick = 0, ticks = 0;
  var timer = setInterval(function () {{
    ticks++;
    if (ticks > 1200) {{ clearInterval(timer); return; }}   // 10 分钟兜底
    var p = location.pathname;
    // 已登录:不在 /auth 下,也不在登录页
    if (p.indexOf("/auth") !== 0 && p.indexOf("/login") !== 0) {{
      if (window.__zflowReport) window.__zflowReport("__zaiSessionReady", true);
      clearInterval(timer);
      return;
    }}
    // 邮箱验证页:/auth/verify* 交给 verify_script,这里不抢按钮
    if (p.indexOf("/auth/verify") === 0) return;
    // 注册表单页:交给 signup_script
    if (location.search.indexOf("action=signup") >= 0) return;
    var now = Date.now();
    // ── 授权确认页:勾协议 → 点继续 ──
    var agreeBtn = null;
    document.querySelectorAll("button").forEach(function (b) {{
      var t = (b.textContent || "").trim();
      if (!agreeBtn && t && t.length <= 6 &&
          (t === "继续" || t === "Continue" || t === "授权" || t === "Allow" ||
           t === "允许" || t === "同意")) agreeBtn = b;
    }});
    if (agreeBtn) {{
      document.querySelectorAll("input[type=checkbox]").forEach(function (c) {{
        if (!c.checked) {{
          c.click();
          var setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "checked").set;
          setter.call(c, true);
          c.dispatchEvent(new Event("change", {{ bubbles: true }}));
          c.dispatchEvent(new Event("input", {{ bubbles: true }}));
        }}
      }});
      if (!agreed && !agreeBtn.disabled && now - lastAgreeClick > 2000) {{
        lastAgreeClick = now;
        agreed = true;
        agreeBtn.click();
      }}
      return;
    }}
    // ── 登录页:填账密 → 过验证 → 点登录 ──
    var email = null, pass = null;
    document.querySelectorAll("input").forEach(function (el) {{
      var t = (el.getAttribute("type") || "").toLowerCase();
      var ph = el.placeholder || "";
      if (t === "password") {{ if (!pass) pass = el; }}
      else if (!email && (t === "email" || ph.indexOf("邮箱") >= 0 || ph.indexOf("Email") >= 0)) email = el;
    }});
    if (email && pass && !filled) {{
      setValue(email, EMAIL);
      setValue(pass, PASSWORD);
      filled = true;
    }}
    if (!filled) return;
    var capBtn = null;
    document.querySelectorAll("button, [role=button]").forEach(function (b) {{
      var t = (b.textContent || "").trim();
      if (!capBtn && t && t.length <= 16 && t.indexOf("开始验证") >= 0) capBtn = b;
    }});
    if (capBtn) {{
      if (now - lastCapClick > 5000) {{ lastCapClick = now; capBtn.click(); }}
      return;
    }}
    if (loginTries >= 10) return;
    if (now - lastLoginClick > 3000) {{
      var login = null;
      document.querySelectorAll("button").forEach(function (b) {{
        var t = (b.textContent || "").trim();
        if (!login && !b.disabled && (t === "登录" || t === "登 录" || t === "Sign in" || t === "Sign In")) login = b;
      }});
      if (login) {{ lastLoginClick = now; loginTries++; login.click(); }}
    }}
  }}, 500);
}})();"#
    )
}

/// OAuth 授权确认页脚本:勾协议 → 点继续(与 batch.rs 授权段同款)。
/// 保留给「页面已在授权态、只需点继续」的场景;注册流水线现在统一用
/// [`login_script`](它同时覆盖登录页与授权页),故此处允许未引用。
#[allow(dead_code)]
pub fn authorize_script() -> String {
    r#"(function () {
  var agreed = false, lastAgreeClick = 0, ticks = 0;
  var timer = setInterval(function () {
    ticks++;
    if (ticks > 600) { clearInterval(timer); return; }
    var now = Date.now();
    var agreeBtn = null;
    document.querySelectorAll("button").forEach(function (b) {
      var t = (b.textContent || "").trim();
      if (!agreeBtn && t && t.length <= 6 &&
          (t === "继续" || t === "Continue" || t === "授权" || t === "Allow" || t === "允许")) agreeBtn = b;
    });
    if (agreeBtn) {
      document.querySelectorAll('input[type=checkbox]').forEach(function (c) {
        if (!c.checked) {
          c.click();
          var setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "checked").set;
          setter.call(c, true);
          c.dispatchEvent(new Event("change", { bubbles: true }));
          c.dispatchEvent(new Event("input", { bubbles: true }));
        }
      });
      if (!agreed && !agreeBtn.disabled && now - lastAgreeClick > 2000) {
        lastAgreeClick = now;
        agreed = true;
        agreeBtn.click();
      }
      return;
    }
  }, 500);
})();"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 脚本必须把账号信息以 JSON 字面量形式注入(防注入),且包含三段页面标志位
    #[test]
    fn signup_script_escapes_and_sets_flag() {
        let s = signup_script("a@b.com", "p\"w<d>", &Some("n'ame".into()));
        assert!(s.contains("chat\\.z\\.ai"), "应限定只作用于 chat.z.ai");
        assert!(s.contains("__zaiSignupSubmitted"), "提交后要置标志位");
        assert!(s.contains("a@b.com"));
        // serde_json 转义后的密码(引号/尖括号被转义,不会破坏 JS 字符串)
        assert!(s.contains("p\\\"w<d>") || s.contains("p\\\"w\\u003cd\\u003e"));
        assert!(!s.contains("var PASSWORD = p\""), "密码不能裸插进 JS");
        // 标志位必须走 hash 通道(读 win.url() 能拿到),不能再用 document.title
        assert!(
            !s.contains("document.title"),
            "不能用 document.title 通道:Tauri win.title() 读的是 OS 标题栏"
        );
        assert!(s.contains("__zflowReport"), "应调用 hash 上报助手");
    }

    #[test]
    fn signup_script_handles_missing_username() {
        let s = signup_script("a@b.com", "pw", &None);
        assert!(s.contains("USERNAME = null"), "缺省用户名应为 null");
    }

    #[test]
    fn verify_script_injects_token_and_flag() {
        let s = verify_script("abc-123-XYZ");
        assert!(s.contains("abc-123-XYZ"));
        assert!(s.contains("__zaiSignupVerified"));
        // 数字形 token 要抽出数字部分
        assert!(s.contains("replace(/[^0-9]/g"));
        // 完成判据要排除登录页,避免被踢回登录页时假阳性
        assert!(
            s.contains("/auth/login") && s.contains("/login"),
            "判据需排除登录页"
        );
        assert!(!s.contains("document.title"), "同样不能再用 title 通道");
    }

    #[test]
    fn authorize_script_checks_agreement() {
        let s = authorize_script();
        assert!(s.contains("checkbox"));
        assert!(s.contains("继续"));
    }

    /// hash 上报助手本身:写 #zflow=<flag>:<1|0>
    #[test]
    fn flag_report_helper_writes_hash() {
        assert!(FLAG_REPORT_HELPER.contains("location.hash"));
        assert!(FLAG_REPORT_HELPER.contains("#zflow="));
        assert!(!FLAG_REPORT_HELPER.contains("document.title"));
    }
}
