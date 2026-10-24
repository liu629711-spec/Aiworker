//! ZCode 夜间/周末批处理命令（2026-10-23）
//!
//! 命令层只做参数校验与状态转接，业务在 `zcode::batch`。
//! 队列消费由后台协程负责（窗口内自动跑），这里的 `run_now` 是「不等窗口、
//! 立即跑一个」的手动入口（用来白天验通道 / 临时插队）。

use tauri::State;

use crate::zcode::batch::{self, BatchStatus, BatchTask};
use crate::zcode::state::ZcodeState;

/// 批量入队。`prompts` 为多行文本拆出来的任务列表；返回实际入队条数。
#[tauri::command]
pub async fn zcode_batch_enqueue(
    zstate: State<'_, ZcodeState>,
    prompts: Vec<String>,
    model: Option<String>,
    batch_id: Option<String>,
) -> Result<usize, String> {
    let model = model.unwrap_or_else(|| "GLM-5.3".to_string());
    let n = batch::enqueue(&zstate.db, &prompts, &model, batch_id.as_deref()).await?;
    // 入队后唤醒调度器：窗口内会立刻开始消费，窗口外再睡回去
    zstate.batch_rt.wake(false);
    Ok(n)
}

/// 队列列表（按 id 倒序，前端展示最新在上）
#[tauri::command]
pub async fn zcode_batch_list(
    zstate: State<'_, ZcodeState>,
    limit: Option<i64>,
) -> Result<Vec<BatchTask>, String> {
    batch::list(&zstate.db, limit.unwrap_or(200).clamp(1, 1000)).await
}

/// 调度与窗口状态（含 CLI 是否探到、最近一次归因结果）
#[tauri::command]
pub async fn zcode_batch_status(zstate: State<'_, ZcodeState>) -> Result<BatchStatus, String> {
    Ok(batch::status(&zstate.db, zstate.batch_rt.is_running()).await)
}

/// 取消一条 pending 任务
#[tauri::command]
pub async fn zcode_batch_cancel(zstate: State<'_, ZcodeState>, id: i64) -> Result<(), String> {
    batch::cancel(&zstate.db, id).await
}

/// 重跑一条 failed/canceled 任务（打回 pending）
#[tauri::command]
pub async fn zcode_batch_requeue(zstate: State<'_, ZcodeState>, id: i64) -> Result<(), String> {
    batch::requeue(&zstate.db, id).await?;
    zstate.batch_rt.wake(false);
    Ok(())
}

/// 清空已结束任务（done/failed/canceled）
#[tauri::command]
pub async fn zcode_batch_clear(zstate: State<'_, ZcodeState>) -> Result<u64, String> {
    batch::clear_finished(&zstate.db).await
}

/// 立即插队跑一条（忽略时间窗）。
///
/// **不在这里同步等执行完成**：单条任务实测 36–112s，命令挂起会让前端转圈两分钟
/// 且无法观察进度。这里只置「强制唤醒」标志就立刻返回，由后台调度协程执行，
/// 前端靠 5s 轮询 `zcode_batch_list` 看状态从 running → done/failed。
///
/// 返回 false = 队列里没有 pending（没什么可跑的）。
#[tauri::command]
pub async fn zcode_batch_run_now(zstate: State<'_, ZcodeState>) -> Result<bool, String> {
    let pending: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM batch_tasks WHERE status='pending'")
            .fetch_one(&zstate.db)
            .await
            .map_err(|e| e.to_string())?;
    if pending == 0 {
        return Ok(false);
    }
    batch::start_loop(zstate.db.clone(), zstate.batch_rt.clone());
    zstate.batch_rt.wake(true);
    Ok(true)
}

/// 启动/保活后台调度协程（窗口内自动消费队列）。幂等。
#[tauri::command]
pub async fn zcode_batch_start(zstate: State<'_, ZcodeState>) -> Result<bool, String> {
    batch::start_loop(zstate.db.clone(), zstate.batch_rt.clone());
    Ok(zstate.batch_rt.is_running())
}

/// 停止后台调度（已在跑的任务会自然结束）
#[tauri::command]
pub async fn zcode_batch_stop(zstate: State<'_, ZcodeState>) -> Result<bool, String> {
    zstate.batch_rt.stop();
    Ok(zstate.batch_rt.is_running())
}
