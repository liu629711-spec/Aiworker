use serde::Serialize;
use tauri::{AppHandle, Emitter};

/// 领取流程状态变化(started / finished),主窗口据此更新按钮文案
pub const CLAIM_STATE: &str = "claim:state";

#[derive(Debug, Clone, Serialize)]
pub struct ClaimStatePayload {
    pub account_id: i64,
    pub plan_id: String,
    /// "started" | "finished"
    pub phase: String,
    pub ok: Option<bool>,
    pub message: Option<String>,
}

pub fn emit_started(app: &AppHandle, account_id: i64, plan_id: &str) {
    let _ = app.emit(
        CLAIM_STATE,
        ClaimStatePayload {
            account_id,
            plan_id: plan_id.to_string(),
            phase: "started".into(),
            ok: None,
            message: None,
        },
    );
}

pub fn emit_finished(app: &AppHandle, account_id: i64, plan_id: &str, ok: bool, message: &str) {
    let _ = app.emit(
        CLAIM_STATE,
        ClaimStatePayload {
            account_id,
            plan_id: plan_id.to_string(),
            phase: "finished".into(),
            ok: Some(ok),
            message: Some(message.to_string()),
        },
    );
}
