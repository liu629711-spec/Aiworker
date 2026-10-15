use tauri::{AppHandle, State};

use crate::zcode::error::AppResult;
use crate::zcode::state::ZcodeState;

use super::service::Snapshot;

#[tauri::command]
pub async fn zcode_monitor_snapshot(state: State<'_, ZcodeState>) -> AppResult<Snapshot> {
    // 内存快照为空(刚启动)时恢复上次保存的快照,实现重开即显示旧数据
    Ok(state.monitor.snapshot_seeded().await)
}

/// 手动检测单个账号(卡片「检测」按钮):余额 + 活动,完成后广播快照
#[tauri::command]
pub async fn zcode_monitor_check_account(
    app: AppHandle,
    state: State<'_, ZcodeState>,
    account_id: i64,
) -> AppResult<Snapshot> {
    state.monitor.refresh_account(&app, account_id).await;
    Ok(state.monitor.snapshot())
}

#[tauri::command]
pub async fn zcode_monitor_start(app: AppHandle, state: State<'_, ZcodeState>) -> AppResult<()> {
    state.monitor.start(&app);
    Ok(())
}

#[tauri::command]
pub async fn zcode_monitor_stop(state: State<'_, ZcodeState>) -> AppResult<()> {
    state.monitor.stop();
    Ok(())
}

/// 立即刷新全部账号剩余用量
#[tauri::command]
pub async fn zcode_monitor_refresh_now(
    app: AppHandle,
    state: State<'_, ZcodeState>,
) -> AppResult<Snapshot> {
    state.monitor.refresh_all(&app).await;
    Ok(state.monitor.snapshot())
}
