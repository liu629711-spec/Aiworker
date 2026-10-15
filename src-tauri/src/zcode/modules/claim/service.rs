use std::collections::HashSet;
use std::sync::Mutex;

use crate::zcode::api::ZaiClient;
use crate::zcode::error::{AppError, AppResult};

use super::server;

/// 领取流程运行时状态(本地服务端口 + 各账号领取流程占用)
#[derive(Default)]
pub struct ClaimRuntime {
    inner: Mutex<ClaimInner>,
}

#[derive(Default)]
struct ClaimInner {
    port: Option<u16>,
    busy: HashSet<i64>,
}

impl ClaimRuntime {
    /// 返回 false 表示该账号已有领取流程在进行中
    pub fn mark_busy(&self, account_id: i64) -> bool {
        self.inner.lock().unwrap().busy.insert(account_id)
    }

    pub fn mark_free(&self, account_id: i64) {
        self.inner.lock().unwrap().busy.remove(&account_id);
    }

    /// 惰性启动本地领取服务并返回端口
    pub fn ensure_server(&self, db: &sqlx::SqlitePool, http: &ZaiClient) -> AppResult<u16> {
        if let Some(p) = self.inner.lock().unwrap().port {
            return Ok(p);
        }
        let port = server::start(db.clone(), http.clone()).map_err(AppError::Other)?;
        self.inner.lock().unwrap().port = Some(port);
        Ok(port)
    }
}
