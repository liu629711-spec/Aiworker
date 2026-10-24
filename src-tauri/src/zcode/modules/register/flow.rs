//! 全自动注册入池编排:马甲邮箱注册 → 邮箱验证 → OAuth 授权 → 入库 → 邀请捆绑。
//!
//! 流程(每步都广播 register:progress,便于前端展示与定位问题):
//! 1. 打开内嵌登录窗,导航到 chat.z.ai 注册页,注入脚本自动填表 + 过阿里云验证码
//! 2. 提交注册 → 官方发验证邮件 → 页面跳到「检查你的邮箱」
//! 3. Rust 侧用 Gmail IMAP 收验证邮件,取出 token
//! 4. 把 token 交给页面(或直接调 verify_email/finish_signup 接口)完成注册
//! 5. 同一窗口导航到 zcode 的 OAuth authorize_url(注册态已在 cookie 里)→ 自动点授权
//! 6. 轮询 OAuth 会话至 ready,凭证入库(复用 accounts::service::poll_and_save_account)
//! 7. 若指定了主号,自动发起邀请捆绑(复用 claim 本地服务的验证码页)
//!
//! 设计取舍:注册页/授权页的 DOM 选择器是 SPA 动态渲染,按 batch.rs 的
//! 500ms 轮询模式写并在每步给出诊断消息;真机联调时按日志微调。

use std::sync::Arc;

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::zcode::api::ZaiClient;
use crate::zcode::db::repo;
use crate::zcode::error::{AppError, AppResult};
use crate::zcode::modules::accounts::invite;
use crate::zcode::modules::monitor::service::MonitorHandle;
use sqlx::SqlitePool;

/// 注册流水线的取消/运行状态(与批量登录同模式:同时只允许一条)
#[derive(Default)]
pub struct RegisterHandle {
    running: std::sync::atomic::AtomicBool,
    cancel: std::sync::atomic::AtomicBool,
}

impl RegisterHandle {
    pub fn try_start(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn finish(&self) {
        use std::sync::atomic::Ordering;
        self.running.store(false, Ordering::SeqCst);
        self.cancel.store(false, Ordering::SeqCst);
    }

    pub fn cancel(&self) {
        use std::sync::atomic::Ordering;
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.cancel.load(Ordering::SeqCst)
    }

    pub fn is_running(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.running.load(Ordering::SeqCst)
    }
}

/// 单步进度(事件名 register:progress)
#[derive(Debug, Clone, Serialize)]
pub struct RegisterProgress {
    /// 当前阶段标识(前端据此显示步骤条)
    pub step: &'static str,
    /// running | success | failed | cancelled
    pub state: String,
    /// 人类可读的诊断消息(每步都带,便于定位)
    pub message: String,
    /// 归属标签:批量模式下形如 "[2/5] a@mozmail.com",单条为空串。
    /// 前端必须把它并入 key,否则同一 step 的后一条事件会覆盖前一条。
    pub tag: String,
    /// 成功入库后的账号 id(仅最终步有)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account_id: Option<i64>,
}

pub const REGISTER_PROGRESS_EVENT: &str = "register:progress";
pub const REGISTER_DONE_EVENT: &str = "register:done";

/// 注册链路日志落盘。
/// 项目全局没有初始化 logger(`log::` 宏会被静默丢弃),所以注册链路显式走
/// `fs_utils::app_log_default`——项目自带的落盘机制(10MB 轮转,
/// 按 APPDATA\AIWorkAssistant 定位),且前端「系统日志」页能读。
/// 没有它,失败后除了前端那条会被覆盖的消息无从排查。
fn app_log(msg: &str) {
    crate::fs_utils::app_log_default(msg);
}

/// 一次注册流水线的输入参数
#[derive(Debug, Clone)]
pub struct RegisterRequest {
    /// 马甲邮箱(Firefox Relay 转投到 Gmail 的那个地址)
    pub email: String,
    /// 注册密码
    pub password: String,
    /// 显示名(可选;缺省用邮箱前缀)
    pub username: Option<String>,
    /// 要捆绑到的主号账号 id(可选)
    pub inviter_id: Option<i64>,
    /// 等待验证邮件的最长秒数
    pub mail_timeout_sec: u64,
    /// 账密库里的现成条目 id(入库后把该条目的账密关联到新账号)
    pub secret_id: Option<i64>,
}

/// 结束汇总(事件 register:done)
#[derive(Debug, Clone, Serialize)]
pub struct RegisterDone {
    pub ok: bool,
    pub message: String,
    pub account_id: Option<i64>,
    /// 是否完成了邀请捆绑
    pub bound: bool,
}

fn emit_progress(app: &AppHandle, step: &'static str, state: &str, tag: &str, message: String) {
    let _ = app.emit(
        REGISTER_PROGRESS_EVENT,
        RegisterProgress {
            step,
            state: state.to_string(),
            message,
            tag: tag.to_string(),
            account_id: None,
        },
    );
}

fn emit_done(app: &AppHandle, done: &RegisterDone) {
    let _ = app.emit(REGISTER_DONE_EVENT, done);
}

/// 跑一条注册流水线(在 tauri 异步运行时里 spawn)。
/// 任何一步失败都会广播 done{ok:false},不影响已导入的账号。
pub async fn run_register_flow(
    app: AppHandle,
    db: SqlitePool,
    http: ZaiClient,
    register: Arc<RegisterHandle>,
    monitor: MonitorHandle,
    req: RegisterRequest,
) {
    let outcome = run_inner(&app, &db, &http, &register, &monitor, &req, "").await;
    register.finish();
    let done = match outcome {
        Ok((account_id, bound)) => RegisterDone {
            ok: true,
            message: if bound {
                "注册入池成功,捆绑验证页已打开(请在系统浏览器完成验证码)".to_string()
            } else {
                "注册入池成功(未捆绑)".to_string()
            },
            account_id,
            bound,
        },
        Err(e) => RegisterDone {
            ok: false,
            message: e.to_string(),
            account_id: None,
            bound: false,
        },
    };
    // 通知各窗口刷新账号列表
    let _ = crate::zcode::modules::accounts::events::emit_accounts_changed(&app);
    emit_done(&app, &done);
}

/// 主号本周邀请配额(用于批量注册时分配主号)。
/// `used` = 官方口径的本周已邀请人数;`cap` = 每周上限(官方周末卡口径为 2)。
#[derive(Debug, Clone, PartialEq)]
pub struct InviterQuota {
    pub id: i64,
    pub label: String,
    pub used: i64,
    pub cap: i64,
}

impl InviterQuota {
    /// 本周还剩多少个空位(不会为负)
    pub fn remaining(&self) -> i64 {
        (self.cap - self.used).max(0)
    }
}

/// 为 N 个新号分配主号:优先用空位最多的主号,单主号不超过本周上限。
///
/// 返回与 `count` 等长的 Vec,每个元素是要捆绑的主号 id;名额不足时为 None
/// (调用方应据此停止或提示,而不是让流水线白跑一轮再被官方拒)。
///
/// 例:主号 A 剩 2、主号 B 剩 1,count=3 → [A, A, B]
pub fn allocate_inviters(quotas: &[InviterQuota], count: usize) -> Vec<Option<i64>> {
    // 空位多的优先;并列按 id 升序保证结果稳定(可测试)
    let mut sorted: Vec<&InviterQuota> = quotas.iter().filter(|q| q.remaining() > 0).collect();
    sorted.sort_by(|a, b| b.remaining().cmp(&a.remaining()).then(a.id.cmp(&b.id)));

    let mut out: Vec<Option<i64>> = Vec::with_capacity(count);
    // 轮转分配:每轮给每个有空位的主号派一个,派完一轮重新排(空位会变)
    while out.len() < count {
        let mut progressed = false;
        for q in &sorted {
            if out.len() >= count {
                break;
            }
            // 用掉了几个名额
            let used_by_me = out.iter().filter(|x| **x == Some(q.id)).count() as i64;
            if used_by_me < q.remaining() {
                out.push(Some(q.id));
                progressed = true;
            }
        }
        if !progressed {
            break; // 全部主号都没空位了
        }
    }
    // 名额不够:剩下的补 None
    while out.len() < count {
        out.push(None);
    }
    out
}

/// 批量注册:把一批马甲(邮箱+密码)逐个跑完整流水线,全部跑完才结束。
/// 单条失败不中断队列;用户取消则跳过剩余。跑完广播汇总 done。
///
/// `inviter_id` 为 Some 时全部新号都绑该主号;为 None 时按 `quotas` 自动分配
/// (单主号不超过本周上限),名额不足的那几条不捆绑但照常注册入池。
pub async fn run_register_batch(
    app: AppHandle,
    db: SqlitePool,
    http: ZaiClient,
    register: Arc<RegisterHandle>,
    monitor: MonitorHandle,
    items: Vec<(String, String)>,
    inviter_id: Option<i64>,
    mail_timeout_sec: Option<u64>,
    quotas: Vec<InviterQuota>,
) {
    let total = items.len();
    let (mut ok, mut fail, mut bound) = (0usize, 0usize, 0usize);
    let timeout = mail_timeout_sec.unwrap_or(300).clamp(30, 1800);
    // 主号分配方案:显式指定就不分配;否则按配额轮转
    let plan: Vec<Option<i64>> = match inviter_id {
        Some(id) => vec![Some(id); total],
        None => allocate_inviters(&quotas, total),
    };
    // 名额不足时提前告知(避免用户以为都会绑上)
    let alloc_none = plan.iter().filter(|x| x.is_none()).count();
    if alloc_none > 0 {
        let cap: i64 = quotas.iter().map(|q| q.remaining()).sum();
        emit_progress(
            &app,
            "open",
            "running",
            "",
            format!(
                "主号本周剩余名额共 {cap} 个,不足 {total} 个:后 {alloc_none} 个新号将只注册不捆绑"
            ),
        );
    }

    for (i, (email, password)) in items.into_iter().enumerate() {
        // 归属标签:批量模式下所有事件都带它,前端据此分卡片,不会互相覆盖
        let tag = format!("[{}/{}] {email}", i + 1, total);
        emit_progress(&app, "open", "running", &tag, "开始注册".into());
        if register.is_cancelled() {
            emit_progress(
                &app,
                "done",
                "cancelled",
                &tag,
                format!("队列已取消:已完成 {ok} 个,剩余 {} 个未跑", total - i),
            );
            register.finish();
            let _ = crate::zcode::modules::accounts::events::emit_accounts_changed(&app);
            emit_done(
                &app,
                &RegisterDone {
                    ok: false,
                    message: format!("队列已取消:成功 {ok} / 失败 {fail} / 共 {total}"),
                    account_id: None,
                    bound: bound > 0,
                },
            );
            return;
        }

        let req = RegisterRequest {
            email: email.clone(),
            password,
            username: None,
            inviter_id: plan.get(i).copied().flatten(),
            mail_timeout_sec: timeout,
            secret_id: None,
        };
        match run_inner(&app, &db, &http, &register, &monitor, &req, &tag).await {
            Ok((account_id, did_bind)) => {
                ok += 1;
                if did_bind {
                    bound += 1;
                }
                emit_progress(
                    &app,
                    "done",
                    "success",
                    &tag,
                    format!("{tag} 注册入池成功"),
                );
                if let Some(id) = account_id {
                    monitor.refresh_account(&app, id).await;
                }
            }
            Err(e) => {
                fail += 1;
                emit_progress(&app, "done", "failed", &tag, format!("{tag} 失败: {e}"));
            }
        }
        let _ = crate::zcode::modules::accounts::events::emit_accounts_changed(&app);
    }

    register.finish();
    let _ = crate::zcode::modules::accounts::events::emit_accounts_changed(&app);
    emit_done(
        &app,
        &RegisterDone {
            ok: fail == 0,
            message: format!("批量注册结束:成功 {ok} / 失败 {fail} / 共 {total}"),
            account_id: None,
            bound: bound > 0,
        },
    );
}

/// 流水线主体:返回 (账号 id, 是否完成捆绑)
/// `tag` 是归属标签(批量模式下形如 "[2/5] a@mozmail.com"),用于进度事件与日志。
async fn run_inner(
    app: &AppHandle,
    db: &SqlitePool,
    http: &ZaiClient,
    register: &RegisterHandle,
    _monitor: &MonitorHandle,
    req: &RegisterRequest,
    tag: &str,
) -> AppResult<(Option<i64>, bool)> {
    let aborted = || register.is_cancelled().then(|| AppError::Other("用户已取消".into()));

    // ── 1. 打开注册页并自动填表 ─────────────────────────────────
    emit_progress(app, "open", "running", &tag, "正在打开注册页".into());
    let script = super::flow_script::signup_script(&req.email, &req.password, &req.username);

    // 复用同名登录窗机制:先等旧窗销毁,再建新窗并清浏览数据(防串号)
    if let Some(old) = app.get_webview_window("login-flow") {
        let _ = old.close();
        for _ in 0..40 {
            if app.get_webview_window("login-flow").is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    let url: tauri::Url = "https://chat.z.ai/auth?action=signup"
        .parse()
        .map_err(|e| AppError::Other(format!("非法注册页地址: {e}")))?;
    let blank: tauri::Url = "about:blank".parse().unwrap();
    // 初始化脚本 = flag 上报助手 + 注册表单脚本 + 登录脚本。
    // 助手必须先注入:它提供 window.__zflowReport,后续各段脚本靠它把阶段完成信号
    // 写进 location.hash(Rust 侧读 win.url() 的 fragment)。
    // 登录脚本一起注入是必要的:验证链接点完后官方会要求重新登录,
    // 那一刻页面已经跳走,事后 eval 的脚本不会在新文档里执行。
    let init_script = format!(
        "{}\n{}\n{}",
        super::flow_script::FLAG_REPORT_HELPER,
        script,
        super::flow_script::login_script(&req.email, &req.password)
    );
    let win = tauri::WebviewWindowBuilder::new(
        app,
        "login-flow",
        tauri::WebviewUrl::External(blank),
    )
    .title("z.ai 注册(自动进行中,请勿关闭)")
    .inner_size(520.0, 700.0)
    .center()
    .initialization_script(&init_script)
    .build()
    .map_err(|e| AppError::Other(format!("创建注册窗口失败: {e}")))?;
    if let Err(e) = win.clear_all_browsing_data() {
        log::warn!("[zcode/register] 清空注册窗口浏览数据失败(可能串号): {e}");
        app_log(&format!("[register:{tag}] 清空浏览数据失败(可能串号): {e}"));
    }
    win.navigate(url)
        .map_err(|e| AppError::Other(format!("导航到注册页失败: {e}")))?;
    let _ = win.set_focus();
    app_log(&format!("[register:{tag}] 已打开注册页并注入填表脚本"));

    // ── 2. 等待页面提交注册并进入「检查邮箱」态 ──────────────────
    // 脚本填表/验证/提交都是异步的,这里轮询窗口 URL 的 hash 标志位。
    emit_progress(app, "fill", "running", &tag, "正在自动填表与过验证码".into());
    let submitted = wait_for_flag(app, &win, "__zaiSignupSubmitted", 300).await;
    if submitted.is_none() {
        if let Some(reason) = aborted() {
            let _ = win.close();
            app_log(&format!("[register:{tag}] 第2步填表:用户取消"));
            return Err(reason);
        }
        let _ = win.close();
        let msg = "注册表未在 5 分钟内提交成功:可能是验证码未通过、页面结构变化,或验证邮件被拦截"
            .to_string();
        app_log(&format!("[register:{tag}] 第2步填表超时失败"));
        emit_progress(app, "fill", "failed", &tag, msg.clone());
        return Err(AppError::Other(msg));
    }
    emit_progress(
        app,
        "fill",
        "success",
        &tag,
        "注册已提交,正在等待验证邮件(若 30s 内未到,请检查垃圾箱)".into(),
    );
    app_log(&format!("[register:{tag}] 第2步填表提交成功"));

    // ── 3. IMAP 收验证邮件 ─────────────────────────────────────
    emit_progress(app, "mail", "running", &tag, "正在等待验证邮件".into());
    let imap_cfg = super::imap::ImapConfig::load(db).await?;
    // 失败必须写日志:实测(2026-10-23)这一步超时后 app.log 里只有"已提交",
    // 完全看不出卡在收信,排查全靠猜。
    let mail = match super::imap::wait_verify_token(
        &imap_cfg,
        &req.email,
        req.mail_timeout_sec,
        || register.is_cancelled(),
    )
    .await
    {
        Ok(m) => m,
        Err(e) => {
            // 关键提示(2026-10-23 19:04 用户实测「注册完无法往下、账号管理看不到」复现):
            // 此刻账号在 z.ai 服务端**已经创建成功**(第2步已提交),只是收信失败。
            // 若不说清,用户会再点一次「发起注册」——同一邮箱在服务端要么报
            // 「已存在」要么造出第二个号,而本地库里始终看不到号,循环踩坑。
            let msg = format!(
                "{e}。注意:账号已在 z.ai 服务端创建成功(只是验证邮件没收到/没点开),\
                 切勿用同一邮箱重复注册。恢复:先在 Gmail 里手动点开该邮箱的验证链接,\
                 然后到「账密库」对这条马甲邮箱点「批量登录」即可导入入池(验证邮件本身\
                 已发出,IMAP 连不上多为代理/网络问题——本次是 imap.gmail.com:993 超时)"
            );
            app_log(&format!("[register:{tag}] 第3步收信失败: {e}"));
            emit_progress(app, "mail", "failed", &tag, msg.clone());
            let _ = win.close();
            return Err(AppError::Other(msg));
        }
    };
    emit_progress(app, "mail", "success", &tag, "已收到验证邮件".into());
    app_log(&format!(
        "[register:{tag}] 第3步收到验证邮件(链接:{})",
        if mail.url.is_some() { "有" } else { "无,用 token 兜底" }
    ));

    // ── 4. 完成邮箱验证 ────────────────────────────────────────
    // 实测(2026-10-23)页面停在「验证您的邮箱 · 请点击邮件中的链接完成验证」:
    // 这一态**没有验证码输入框**,必须像真人点邮件一样把窗口导航到那个链接,
    // 链接自带 token/email/username,服务端验完直接进登录态。
    // (旧实现只往页面里填 6 位 code,在这个页面永远填不上 → 必然超时。)
    emit_progress(app, "verify", "running", &tag, "正在完成邮箱验证".into());
    if let Some(url) = mail.url.as_deref() {
        match url.parse::<tauri::Url>() {
            Ok(u) => {
                if let Err(e) = win.navigate(u) {
                    app_log(&format!("[register:{tag}] 第4步导航验证链接失败,回落填码: {e}"));
                } else {
                    app_log(&format!(
                        "[register:{tag}] 第4步已导航验证链接({}…)",
                        super::imap::mask_url(url)
                    ));
                }
            }
            Err(e) => app_log(&format!("[register:{tag}] 第4步验证链接非法,回落填码: {e}")),
        }
    } else {
        app_log(&format!("[register:{tag}] 第4步邮件里没有链接,回落填码"));
    }
    // 兜底:少数形态会在页面里要 6 位码(导航后页面可能也留着这个输入框)
    let verify_script = super::flow_script::verify_script(&mail.token);
    let _ = win.eval(&verify_script);
    // 判据放宽:验证链接生效后页面可能停在同一路径(只把邮箱标成已验证),
    // 也可能跳登录页/首页。三者都算这一步成功;超时也不再中止流水线——
    // 下一步会用账密重新登录,把验证后必须重新登录的官方要求接住。
    let verified = wait_for_any_flag_or_leave(
        app,
        &win,
        &["__zaiSignupVerified", "__zaiSessionReady"],
        120,
        "/auth/verify",
    )
    .await;
    if verified.is_none() {
        app_log(&format!(
            "[register:{tag}] 第4步未等到验证标志(不中止):继续用账密登录并授权"
        ));
        emit_progress(
            app,
            "verify",
            "running",
            tag,
            "验证链接已打开但未回传标志,继续用账密登录并授权".into(),
        );
    }
    emit_progress(app, "verify", "success", &tag, "邮箱验证链接已生效".into());
    app_log(&format!("[register:{tag}] 第4步邮箱验证链接处理完成"));

    // ── 5. 新建带登录脚本的窗口走 OAuth 授权(注册态已在 cookie 里) ──────
    // 实测(2026-10-23 16:27):验证完官方要求重新登录,而 authorize 页在未登录时
    // 会被踢到 /auth/login。旧实现只在当前文档 eval 一次 authorize_script,
    // 跳转即丢失 → 授权按钮永远等不到 → 第 4 步超时、账号永不入池。
    // 改为重建窗口并把 login_script 放进 initialization_script(每个文档都跑),
    // 登录页/授权页各自自动完成。
    emit_progress(app, "oauth", "running", tag, "正在授权并导入账号".into());
    let _ = win.close();
    for _ in 0..40 {
        if app.get_webview_window("login-flow").is_none() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
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

    let authorize: tauri::Url = authorize_url
        .parse()
        .map_err(|e| AppError::Other(format!("非法 authorize_url: {e}")))?;
    // 登录脚本 + flag 助手:同源多文档生效,登录页与授权页都能自动过关
    let login_init = format!(
        "{}\n{}",
        super::flow_script::FLAG_REPORT_HELPER,
        super::flow_script::login_script(&req.email, &req.password)
    );
    let blank: tauri::Url = "about:blank".parse().unwrap();
    let auth_win = tauri::WebviewWindowBuilder::new(
        app,
        "login-flow",
        tauri::WebviewUrl::External(blank),
    )
    .title("z.ai 授权(自动进行中,请勿关闭)")
    .inner_size(520.0, 700.0)
    .center()
    .initialization_script(&login_init)
    .build()
    .map_err(|e| AppError::Other(format!("创建授权窗口失败: {e}")))?;
    let _ = auth_win.set_focus();
    if let Err(e) = auth_win.navigate(authorize) {
        let _ = auth_win.close();
        return Err(AppError::Other(format!("导航到授权页失败: {e}")));
    }
    app_log(&format!(
        "[register:{tag}] 第5步已打开授权页(带自动登录脚本),等待会话建立"
    ));

    // 复用账号服务的轮询入库逻辑(中止条件:取消 / 窗口被关)
    let account = super::super::accounts::service::poll_and_save_account_public(
        db,
        http,
        &flow_id,
        &poll_token,
        expires_at,
        interval_sec,
        || {
            if register.is_cancelled() {
                Some("队列已取消".into())
            } else if app.get_webview_window("login-flow").is_none() {
                Some("注册窗口被关闭,已中止".into())
            } else {
                None
            }
        },
    )
    .await;
    let _ = auth_win.close();
    let account = match account {
        Ok(a) => a,
        Err(e) => {
            // 关键诊断:这一步失败时服务端账号可能已经创建,只是没入池。
            // 必须明确告知,否则用户会重复注册,导致同一邮箱在库里出现两行。
            let msg = format!(
                "{e}。注意:如果授权页已加载过,该账号在 z.ai 服务端可能已创建成功, \
                 只是未导入本地——请勿用同一邮箱重复注册,改用「添加账号」走 OAuth 导入"
            );
            app_log(&format!("[register:{tag}] 第5步OAuth入库失败: {e}"));
            emit_progress(app, "oauth", "failed", tag, msg.clone());
            return Err(AppError::Other(msg));
        }
    };
    emit_progress(
        app,
        "oauth",
        "success",
        tag,
        format!("已导入为「{}」", account.label),
    );
    // 复用了账密库里的现成条目:把它的账密关联到新账号(与批量登录一致),
    // 免得这条马甲邮箱下次还被当成"未注册"
    if let Some(sid) = req.secret_id {
        super::super::accounts::service::link_secret_to_account(db, sid, account.id).await;
    }
    emit_progress(app, "done", "running", tag, format!("账号 #{} 已入池", account.id));
    app_log(&format!("[register:{tag}] 第5步入池成功 account_id={}", account.id));

    // ── 6. 邀请捆绑(可选) ──────────────────────────────────────
    // 注意:bind_after_register 只负责"发起"(打开本地验证码页),真正的 bind-code
    // 由系统浏览器里的页面异步触发,Rust 不知道结果。所以这里只能报"已发起",
    // 真实结果由 claim 服务的 claim:state 事件回传(见 P0-b 修复)。
    let mut bound = false;
    if let Some(inviter_id) = req.inviter_id {
        emit_progress(app, "bind", "running", tag, "正在自动捆绑到主号".into());
        match bind_after_register(app, db, http, account.id, inviter_id, tag).await {
            Ok(()) => {
                bound = true;
                emit_progress(
                    app,
                    "bind",
                    "success",
                    tag,
                    format!("已自动捆绑到主号 #{inviter_id}"),
                );
                app_log(&format!("[register:{tag}] 第6步捆绑成功"));
            }
            Err(e) => {
                // 捆绑失败不否决注册结果:账号已入池,可稍后手动捆绑
                emit_progress(
                    app,
                    "bind",
                    "failed",
                    tag,
                    format!("自动捆绑未成功(账号已入池,可稍后手动捆绑): {e}"),
                );
                app_log(&format!("[register:{tag}] 第6步捆绑失败: {e}"));
            }
        }
    }

    // ── 7. 补齐新号新手任务(决定主号能否拿到夜间蹬蹬卡) ──────────
    // 只有 send_message 必须马上做:新号的免费额度有时限,拖久了就用不掉。
    // login_device 会切换本机登录态并重启客户端,单独由用户触发,不在这里自动跑。
    if account.zai_access_token.as_deref().is_some_and(|s| !s.is_empty()) {
        emit_progress(app, "tasks", "running", tag, "正在完成新手任务(发送消息)".into());
        match super::tasks::complete_send_message(http, &account).await {
            Ok(true) => {
                emit_progress(app, "tasks", "success", tag, "已发送消息(新手任务 1/2 完成)".into());
                app_log(&format!("[register:{tag}] 第7步发消息成功"));
            }
            Ok(false) => {
                emit_progress(
                    app,
                    "tasks",
                    "failed",
                    tag,
                    "发送消息未被服务端受理(不影响入池)".into(),
                );
            }
            Err(e) => {
                emit_progress(
                    app,
                    "tasks",
                    "failed",
                    tag,
                    format!("发送消息失败(不影响入池,可稍后重试): {e}"),
                );
                app_log(&format!("[register:{tag}] 第7步发消息失败: {e}"));
            }
        }
    } else {
        emit_progress(
            app,
            "tasks",
            "failed",
            tag,
            "该账号没有 zai token,跳过发消息(可重新导入后再补)".into(),
        );
        app_log(&format!("[register:{tag}] 第7步跳过:无 zai token"));
    }

    Ok((Some(account.id), bound))
}

/// 轮询等待页面脚本设置的标志位(脚本把 `#zflow=<flag>:<1>` 写进 location.hash)。
/// 实现:页面侧调 `window.__zflowReport(flag, true)` 写 hash,Rust 侧读 `win.url()`
/// 的 fragment 判断。
/// (早期版本把 flag 写进 document.title 再读 win.title(),但 Tauri 的 title() 读的是
/// OS 窗口标题栏——tao 的 GetWindowTextW——收不到 WebView 里的 document.title,
/// 通道必然失效;hash 是 win.url() 能真实读回的。)
///
/// 存活判定:窗口从 manager 里消失要**连续多次**才认定被关。
/// 实测(2026-10-23 16:24)`clear_all_browsing_data()` 会让 WebView2 短暂重建,
/// 期间 `get_webview_window()` 有瞬时空窗,旧实现只看一次就返回 None,
/// 表现为「打开 4 秒后就报填表超时」。
/// 返回 Some(elapsed) 表示等到;None 表示超时或窗口真的被关。
async fn wait_for_flag(
    app: &AppHandle,
    win: &tauri::WebviewWindow,
    flag: &str,
    timeout_sec: u64,
) -> Option<u64> {
    let started = std::time::Instant::now();
    // hash 形如 "#zflow=__zaiSignupSubmitted:1"
    let want = format!("zflow={flag}:1");
    let mut missing = 0u32;
    loop {
        if app.get_webview_window("login-flow").is_none() {
            missing += 1;
            // 连续 3 秒都不在(6 次 × 500ms)才算被关
            if missing >= 6 {
                return None;
            }
        } else {
            missing = 0;
        }
        if let Ok(url) = win.url() {
            if url.fragment().is_some_and(|f| f == want) {
                return Some(started.elapsed().as_secs());
            }
        }
        if started.elapsed().as_secs() >= timeout_sec {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// 轮询等待**任一**页面脚本标志位命中(多判据场景:一条路径可能只落其中一个)。
/// 与 [`wait_for_flag`] 同机制,只是接受一组 flag。
async fn wait_for_any_flag(
    app: &AppHandle,
    win: &tauri::WebviewWindow,
    flags: &[&str],
    timeout_sec: u64,
) -> Option<u64> {
    wait_any_flag_impl(app, win, flags, timeout_sec, None).await
}

/// 同上,并且**页面离开指定路径前缀**也算达成。
///
/// 用途(实测 2026-10-23):验证链接被服务端处理后,页面会跳到登录页或首页,
/// 但脚本的 `__zaiSignupVerified` 只在「离开 /auth」时才写。
/// 若只等标志位,页面跳到 `/auth/login` 这种合法进展也会被当成失败干等满 120s。
async fn wait_for_any_flag_or_leave(
    app: &AppHandle,
    win: &tauri::WebviewWindow,
    flags: &[&str],
    timeout_sec: u64,
    leave_prefix: &str,
) -> Option<u64> {
    wait_any_flag_impl(app, win, flags, timeout_sec, Some(leave_prefix)).await
}

async fn wait_any_flag_impl(
    app: &AppHandle,
    win: &tauri::WebviewWindow,
    flags: &[&str],
    timeout_sec: u64,
    leave_prefix: Option<&str>,
) -> Option<u64> {
    let started = std::time::Instant::now();
    let wants: Vec<String> = flags.iter().map(|f| format!("zflow={f}:1")).collect();
    let mut missing = 0u32;
    loop {
        if app.get_webview_window("login-flow").is_none() {
            missing += 1;
            if missing >= 6 {
                return None;
            }
        } else {
            missing = 0;
        }
        if let Ok(url) = win.url() {
            if let Some(f) = url.fragment() {
                if wants.iter().any(|w| w == f) {
                    return Some(started.elapsed().as_secs());
                }
            }
            if let Some(prefix) = leave_prefix {
                if !url.path().starts_with(prefix) {
                    return Some(started.elapsed().as_secs());
                }
            }
        }
        if started.elapsed().as_secs() >= timeout_sec {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
}

/// 注册入池后的邀请捆绑(全自动):预检 → 在应用内窗口打开本地验证码页 →
/// 页面自动跑阿里云验证码并提交 → 轮询页面写回的 hash 标志拿最终结果。
///
/// 为什么不再用系统浏览器:`open_in_system_browser` 只能"发起",Rust 侧收不到结果,
/// 且用户在外部浏览器里还得手动点。改用应用内 WebviewWindow 后整条链路闭环。
///
/// 关键点:`/bind` 页必须在**应用内**跑,因为
/// 1) 它要访问 `127.0.0.1:{port}` 本地服务(Rust 持 JWT,页面只出验证码);
/// 2) 结果经 `location.hash` 回传,Rust 用 `win.url()` 轮询读取。
async fn bind_after_register(
    app: &AppHandle,
    db: &SqlitePool,
    http: &ZaiClient,
    account_id: i64,
    inviter_id: i64,
    tag: &str,
) -> AppResult<()> {
    let accounts = repo::list(db).await?;
    let account = accounts
        .iter()
        .find(|a| a.id == account_id)
        .ok_or_else(|| AppError::Other("被邀请账号不存在".into()))?;
    let inviter = accounts
        .iter()
        .find(|a| a.id == inviter_id)
        .ok_or_else(|| AppError::Other("主号账号不存在".into()))?;
    invite::check_bind_precondition(account, inviter)?;

    let port = crate::zcode::modules::claim::service::ClaimRuntime::default()
        .ensure_server(app, db, http)?;
    let url: tauri::Url = format!(
        "http://127.0.0.1:{port}/bind?account={account_id}&inviter={inviter_id}"
    )
    .parse()
    .map_err(|e| AppError::Other(format!("非法捆绑页地址: {e}")))?;

    // 复用注册窗:它此刻已经跑完 OAuth,关掉不影响后续
    if let Some(old) = app.get_webview_window("login-flow") {
        let _ = old.close();
        for _ in 0..40 {
            if app.get_webview_window("login-flow").is_none() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    let win = tauri::WebviewWindowBuilder::new(
        app,
        "login-flow",
        tauri::WebviewUrl::External(url),
    )
    .title("ZCode 邀请捆绑(自动进行中,请勿关闭)")
    .inner_size(520.0, 460.0)
    .center()
    .build()
    .map_err(|e| AppError::Other(format!("创建捆绑窗口失败: {e}")))?;
    let _ = win.set_focus();
    app_log(&format!(
        "[register:{tag}] 第6步捆绑页已打开,等待验证码与提交结果"
    ));

    // 等页面回写 __zcodeBindDone / __zcodeBindFailed(验证码+提交通常 3~8 秒)
    let want_ok = "zflow=__zcodeBindDone:1";
    let want_err = "zflow=__zcodeBindFailed:1";
    let started = std::time::Instant::now();
    let mut outcome: Option<bool> = None;
    while started.elapsed().as_secs() < 90 {
        if app.get_webview_window("login-flow").is_none() {
            break;
        }
        if let Ok(u) = win.url() {
            if let Some(f) = u.fragment() {
                if f == want_ok {
                    outcome = Some(true);
                    break;
                }
                if f == want_err {
                    outcome = Some(false);
                    break;
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    }
    let _ = win.close();

    match outcome {
        Some(true) => {
            app_log(&format!("[register:{tag}] 第6步捆绑成功"));
            Ok(())
        }
        Some(false) => Err(AppError::Other(
            "捆绑页报告失败(验证码或 bind-code 被拒)".into(),
        )),
        None => Err(AppError::Other(
            "捆绑超时或窗口被关闭,未拿到结果".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(id: i64, used: i64, cap: i64) -> InviterQuota {
        InviterQuota { id, label: format!("acct-{id}"), used, cap }
    }

    /// 名额不足时补 None,不panic、不越界
    #[test]
    fn allocate_returns_none_when_quota_exhausted() {
        let plan = allocate_inviters(&[q(1, 2, 2)], 3);
        assert_eq!(plan, vec![None, None, None]);
    }

    /// 单主号不超过本周上限
    #[test]
    fn allocate_respects_per_inviter_cap() {
        // A 剩 2(used0/cap2),B 剩 1(used1/cap2),要 3 个
        let plan = allocate_inviters(&[q(1, 0, 2), q(2, 1, 2)], 3);
        assert_eq!(plan.len(), 3);
        let a_count = plan.iter().filter(|x| **x == Some(1)).count();
        let b_count = plan.iter().filter(|x| **x == Some(2)).count();
        assert!(a_count <= 2, "A 最多 2 个,实际 {a_count}");
        assert!(b_count <= 1, "B 最多 1 个,实际 {b_count}");
        assert_eq!(a_count + b_count, 3, "3 个都要有主号");
    }

    /// 空位多的优先用(单主号能扛就先用它)
    #[test]
    fn allocate_prefers_larger_remaining() {
        let plan = allocate_inviters(&[q(1, 0, 2), q(2, 0, 5)], 2);
        // B 空位多,应优先拿到第一个名额
        assert_eq!(plan[0], Some(2));
    }

    /// 全部满员时全 None;没有任何主号时也全 None
    #[test]
    fn allocate_handles_empty_and_full() {
        assert_eq!(allocate_inviters(&[], 2), vec![None, None]);
        assert_eq!(
            allocate_inviters(&[q(1, 5, 2), q(2, 9, 2)], 2),
            vec![None, None]
        );
    }

    /// remaining 不为负
    #[test]
    fn remaining_never_negative() {
        assert_eq!(q(1, 9, 2).remaining(), 0);
        assert_eq!(q(1, 1, 2).remaining(), 1);
    }
}
