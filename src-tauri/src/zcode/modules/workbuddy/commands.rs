//! WorkBuddy 模块的 Tauri command 层:只做参数提取与调用转发。

use tauri::{Manager, State};

use crate::zcode::error::AppResult;
use crate::zcode::state::ZcodeState;

use super::service::{self, WbAccount, WbStatus};

#[tauri::command]
pub async fn zcode_wb_status(state: State<'_, ZcodeState>) -> AppResult<WbStatus> {
    service::status(&state.db).await
}

#[tauri::command]
pub async fn zcode_wb_list(state: State<'_, ZcodeState>) -> AppResult<Vec<WbAccount>> {
    service::list_accounts(&state.db).await
}

/// 捕获 WorkBuddy 当前登录的账号(整份会话快照入库,重复捕获即刷新)
#[tauri::command]
pub async fn zcode_wb_capture(state: State<'_, ZcodeState>) -> AppResult<WbAccount> {
    service::capture_current(&state.db).await
}

/// 切换到此账号:退出 WorkBuddy → 替换会话文件 → 重新启动。
/// 海外会话注入海外 ACC 配置(否则客户端以国内产品域配置启动,海外登录态连不上)。
#[tauri::command]
pub async fn zcode_wb_switch(state: State<'_, ZcodeState>, app: tauri::AppHandle, id: i64) -> AppResult<()> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| crate::zcode::error::AppError::Other(format!("无法定位数据目录: {e}")))?;
    service::switch_account(&state.db, id, &dir).await
}

#[tauri::command]
pub async fn zcode_wb_remove(state: State<'_, ZcodeState>, id: i64) -> AppResult<()> {
    service::remove_account(&state.db, id).await
}

/// 重命名账号备注
#[tauri::command]
pub async fn zcode_wb_rename(state: State<'_, ZcodeState>, id: i64, label: Option<String>) -> AppResult<()> {
    service::rename_account(&state.db, id, label.as_deref()).await
}

/// 手动指定 WorkBuddy.exe 路径(自动探测失败时)
#[tauri::command]
pub async fn zcode_wb_set_exe_path(state: State<'_, ZcodeState>, path: String) -> AppResult<()> {
    service::set_exe_path(&state.db, &path).await
}

/// 海外登录向导:退出 WorkBuddy → 官方登出标记 → 以海外配置(登录页指 workbuddy.ai)重启。
/// 前提:当前登录账号必须已捕获(否则拒绝执行,防止会话丢失)。
#[tauri::command]
pub async fn zcode_wb_launch_oversea_login(
    state: State<'_, ZcodeState>,
    app: tauri::AppHandle,
) -> AppResult<()> {
    let dir = app
        .path()
        .app_data_dir()
        .map_err(|e| crate::zcode::error::AppError::Other(format!("无法定位数据目录: {e}")))?;
    service::launch_oversea_login(&state.db, &dir).await
}
