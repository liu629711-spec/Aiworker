//! z.ai OAuth 轮询登录流(逆向自 ZCode 客户端 startOAuthWithPolling):
//! 1. init:POST `https://zcode.z.ai/api/v1/oauth/cli/init`,Bearer 为本地自生成的 64 位 hex,
//!    body `{"provider":"zai"}` → data:{flow_id, authorize_url, expires_at, poll_interval_sec}
//!    authorize_url 服务端已构造好(含轮询流专用 redirect_uri),原样打开即可
//! 2. 用系统默认浏览器打开 authorize_url,用户在任意浏览器完成登录
//! 3. 按 poll_interval_sec 轮询 `GET /api/v1/oauth/cli/poll/{flow_id}`(Bearer 同上):
//!    data.status = pending 继续等 / ready 直接取 data.token(zcodejwttoken)+ data.zai + data.user / failed 报错

/// 取消标志:关闭浏览器 tab 无法被感知,由用户手动「取消」解除等待
#[derive(Default)]
pub struct OAuthCancel(pub std::sync::Arc<std::sync::atomic::AtomicBool>);

impl OAuthCancel {
    pub fn cancel(&self) {
        use std::sync::atomic::Ordering;
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        use std::sync::atomic::Ordering;
        self.0.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        use std::sync::atomic::Ordering;
        self.0.store(false, Ordering::SeqCst);
    }
}

/// 用系统默认浏览器打开 URL。
/// 必须走 ShellExecute(opener 插件):explorer.exe / cmd start 传含 `&` 的 URL 时
/// 命令行解析会把 URL 拦腰截断(实测浏览器收到 `...callback/zai&state` 这类残缺地址导致 404),
/// ShellExecuteW 以单参数原样交给 URL 关联程序,无此问题。
pub fn open_in_system_browser<R: tauri::Runtime>(
    app: &tauri::AppHandle<R>,
    url: &str,
) -> crate::zcode::error::AppResult<()> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(url, None::<&str>)
        .map_err(|e| crate::zcode::error::AppError::Other(format!("打开系统浏览器失败: {e}")))
}
