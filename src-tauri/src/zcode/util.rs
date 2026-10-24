//! zcode 域工具函数(移植自 zai-claim lib.rs):app_kv 读写与窗口位置持久化。

use crate::zcode::error::{AppError, AppResult};
use crate::zcode::state::ZcodeState;
use tauri::Manager;

/// 同步上下文 → 异步查询的安全桥接。
///
/// 直接在 async 命令(tokio multi-thread worker)里 `block_on` 会 panic
/// "Cannot start a runtime from within a runtime"(panic.log 2026-10-23 有案底)。
/// - 已在 multi-thread runtime 内 → 先 `block_in_place` 把当前 worker 转成阻塞线程;
/// - current_thread runtime 或普通 OS 线程 → 直接 `block_on`(合法)。
pub fn bridge_block_on<F: std::future::Future>(fut: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| tauri::async_runtime::block_on(fut))
        }
        _ => tauri::async_runtime::block_on(fut),
    }
}

pub async fn kv_get(db: &sqlx::SqlitePool, key: &str) -> Option<String> {
    let (v,): (String,) = sqlx::query_as("SELECT value FROM app_kv WHERE key = ?1")
        .bind(key)
        .fetch_optional(db)
        .await
        .ok()??;
    Some(v)
}

pub async fn kv_set(db: &sqlx::SqlitePool, key: &str, value: &str) {
    let _ = sqlx::query(
        "INSERT INTO app_kv (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
    )
    .bind(key)
    .bind(value)
    .bind(chrono::Utc::now().timestamp())
    .execute(db)
    .await;
}

/// 读取窗口上次保存的位置
pub async fn load_window_pos(db: &sqlx::SqlitePool, label: &str) -> AppResult<(i32, i32)> {
    let row: Option<(i32, i32)> = sqlx::query_as("SELECT x, y FROM window_state WHERE label = ?1")
        .bind(label)
        .fetch_optional(db)
        .await
        .map_err(|e| AppError::Other(format!("读取窗口位置失败: {e}")))?;
    Ok(row.unwrap_or((-1, -1)))
}

/// 异步保存窗口当前位置(失败静默:位置记忆是尽力而为)
pub fn save_window_pos_async(app: &tauri::AppHandle, label: &str, win: &tauri::WebviewWindow) {
    let Ok(pos) = win.outer_position() else { return };
    let app = app.clone();
    let label = label.to_string();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<ZcodeState>();
        let _ = sqlx::query(
            "INSERT INTO window_state (label, x, y, updated_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(label) DO UPDATE SET x = ?2, y = ?3, updated_at = ?4",
        )
        .bind(label)
        .bind(pos.x)
        .bind(pos.y)
        .bind(chrono::Utc::now().timestamp())
        .execute(&state.db)
        .await;
    });
}
