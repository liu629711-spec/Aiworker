//! zcode 域全局状态:独立于主项目 AppState,自持 sqlx SQLite 池与 z.ai API 客户端。

use std::sync::Arc;

use crate::zcode::api::ZaiClient;
use crate::zcode::error::{AppError, AppResult};
use crate::zcode::modules::accounts::batch::BatchLoginHandle;
use crate::zcode::modules::accounts::oauth::OAuthCancel;
use crate::zcode::modules::claim::service::ClaimRuntime;
use crate::zcode::modules::monitor::service::MonitorHandle;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::SqlitePool;
use std::str::FromStr;
use tauri::Manager;

/// zcode 域全局唯一状态:多窗口共享的单一数据源。
pub struct ZcodeState {
    /// SQLite 连接池(异步,内部由后台线程执行,不阻塞 UI)
    pub db: SqlitePool,
    /// z.ai API 客户端(预置逆向所得的客户端同款请求头)
    pub http: ZaiClient,
    /// 检测器(5s 余额 / 10min 活动,后台 tokio 协程)
    pub monitor: MonitorHandle,
    /// 领取流程运行时(本地服务端口 + 窗口占用状态)
    pub claims: Arc<ClaimRuntime>,
    /// OAuth 轮询登录的取消标志
    pub oauth: Arc<OAuthCancel>,
    /// 批量账密登录队列(取消/运行标志)
    pub batch: Arc<BatchLoginHandle>,
}

impl ZcodeState {
    /// 在 setup 阶段调用:建库、跑迁移、构建 API 客户端。
    pub async fn init(app: &tauri::AppHandle) -> AppResult<Self> {
        let dir = app
            .path()
            .app_data_dir()
            .map_err(|e| AppError::Other(format!("无法定位应用数据目录: {e}")))?;
        std::fs::create_dir_all(&dir)?;

        // 与主项目共用 app_data_dir,文件名区分(zcode 域独立库)
        let db_path = dir.join("zcode.sqlite");
        let opts = SqliteConnectOptions::from_str(db_path.to_string_lossy().as_ref())?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true);
        let db = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await?;
        // 迁移目录相对 src-tauri/Cargo.toml
        sqlx::migrate!("src/zcode/migrations").run(&db).await?;

        let http = ZaiClient::new()?;
        let monitor = MonitorHandle::new(db.clone(), http.clone());
        Ok(Self {
            db,
            http,
            monitor,
            claims: Arc::new(ClaimRuntime::default()),
            oauth: Arc::new(OAuthCancel::default()),
            batch: Arc::new(BatchLoginHandle::default()),
        })
    }
}
