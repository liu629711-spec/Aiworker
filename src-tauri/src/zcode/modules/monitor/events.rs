use serde::Serialize;
use tauri::{AppHandle, Emitter};

use super::service::{ClaimablePlan, Snapshot};

/// 全量快照广播(所有窗口订阅,含账号套餐/用量/活动/当前账号标记)
pub const SNAPSHOT: &str = "monitor:snapshot";

/// 检测到新的 claimable 活动时广播(主窗口据此弹提醒 + 领取按钮)
pub const CLAIM_DETECTED: &str = "monitor:claim-detected";

/// 一轮余额刷新开始/每探完一个账号/结束时广播,前端据此切换按钮 loading 与进度;
/// 启动首轮与手动「刷新用量」走同一条路,都会触发
pub const REFRESHING: &str = "monitor:refreshing";

#[derive(Debug, Clone, Serialize)]
pub struct RefreshingPayload {
    pub active: bool,
    /// 已探完的账号数(开始时 0,结束时归零)
    pub done: usize,
    /// 本轮账号总数
    pub total: usize,
}

pub fn emit_refreshing(app: &AppHandle, active: bool, done: usize, total: usize) {
    let _ = app.emit(REFRESHING, RefreshingPayload { active, done, total });
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaimDetectedPayload {
    pub account_id: i64,
    pub label: String,
    pub plans: Vec<ClaimablePlan>,
}

pub fn emit_snapshot(app: &AppHandle, snapshot: &Snapshot) {
    let _ = app.emit(SNAPSHOT, snapshot);
}

pub fn emit_claim_detected(app: &AppHandle, payload: &ClaimDetectedPayload) {
    let _ = app.emit(CLAIM_DETECTED, payload);
}
