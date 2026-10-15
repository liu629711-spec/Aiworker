use tauri::{AppHandle, Emitter};

use crate::zcode::error::{AppError, AppResult};

/// 账号列表发生任何变化(增/删/改)后广播,前端各窗口据此重新拉取
pub const ACCOUNTS_CHANGED: &str = "accounts:changed";

pub fn emit_accounts_changed(app: &AppHandle) -> AppResult<()> {
    app.emit(ACCOUNTS_CHANGED, ())
        .map_err(|e| AppError::Other(format!("广播 {ACCOUNTS_CHANGED} 失败: {e}")))
}
