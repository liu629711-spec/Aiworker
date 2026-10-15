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

        // 一次性迁移:首次使用时,把旧独立版 zai-claim(com.abellee.zaiclaim)的数据整库搬过来,
        // 让之前批量导入的账号/账密/WorkBuddy 会话无缝接管。
        if let Err(e) = migrate_from_legacy_zaiclaim(&db).await {
            log::warn!("[zcode] 旧版 zai-claim 数据迁移跳过/失败(不影响启动): {e}");
        }

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

/// 从旧独立版 zai-claim 数据库迁移数据(幂等:有标记或已有账号则跳过)。
async fn migrate_from_legacy_zaiclaim(db: &SqlitePool) -> AppResult<()> {
    use sqlx::Row;

    // 已迁移过或本地已有账号 → 不动
    let (n_accounts,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM accounts")
        .fetch_one(db)
        .await
        .map_err(|e| AppError::Other(e.to_string()))?;
    let marked = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM app_kv WHERE key = 'migrated_from_zaiclaim'",
    )
    .fetch_one(db)
    .await
    .unwrap_or(0);
    if n_accounts > 0 || marked > 0 {
        return Ok(());
    }

    // 定位旧库:%APPDATA%/com.abellee.zaiclaim/zai-claim.db(app_data_dir 的兄弟目录)
    let Some(appdata) = dirs_data_dir() else {
        return Ok(());
    };
    let legacy = appdata.join("com.abellee.zaiclaim").join("zai-claim.db");
    if !legacy.is_file() {
        log::info!("[zcode] 未发现旧版 zai-claim 数据库,跳过迁移");
        return Ok(());
    }

    let legacy_sql = format!("'{}'", legacy.to_string_lossy().replace('\'', "''"));

    // 关键:ATTACH 只对单个连接生效,连接池可能换连接 —— 全程用同一个连接执行
    let mut conn = db
        .acquire()
        .await
        .map_err(|e| AppError::Other(e.to_string()))?;

    sqlx::query(&format!("ATTACH DATABASE {legacy_sql} AS legacy"))
        .execute(&mut *conn)
        .await
        .map_err(|e| AppError::Other(format!("挂载旧库失败: {e}")))?;

    // 逐表搬运(sqlite_master 动态取旧库用户表,列结构同源,逐表容错)
    let rows = sqlx::query(
        "SELECT name FROM legacy.sqlite_master \
         WHERE type='table' AND name NOT LIKE 'sqlite_%' AND name != '_sqlx_migrations'",
    )
    .fetch_all(&mut *conn)
    .await
    .map_err(|e| AppError::Other(e.to_string()))?;
    let mut moved = Vec::new();
    for r in &rows {
        let t = r.get::<String, _>("name");
        // 标识符不能参数化,表名来自旧库 sqlite_master(同源结构),拼进语句
        let sql = format!(
            "INSERT OR IGNORE INTO main.\"{t}\" SELECT * FROM legacy.\"{t}\""
        );
        match sqlx::query(&sql).execute(&mut *conn).await {
            Ok(res) => moved.push(format!("{t}({})", res.rows_affected())),
            Err(e) => log::warn!("[zcode] 表 {t} 迁移失败(跳过): {e}"),
        }
    }

    sqlx::query("DETACH DATABASE legacy")
        .execute(&mut *conn)
        .await
        .ok();

    // 写迁移标记(即使本次为空库也标记,避免重复挂载)
    sqlx::query(
        "INSERT INTO app_kv (key, value, updated_at) VALUES ('migrated_from_zaiclaim', ?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = ?1, updated_at = ?2",
    )
    .bind(format!("done: {}", moved.join(",")))
    .bind(chrono::Utc::now().timestamp())
    .execute(&mut *conn)
    .await
    .ok();

    log::info!("[zcode] 旧版 zai-claim 数据迁移完成: {}", moved.join(", "));
    Ok(())
}

/// 取 %APPDATA% 目录(Windows: Roaming)。
fn dirs_data_dir() -> Option<std::path::PathBuf> {
    std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .ok()
        .or_else(|| {
            // 非 Windows 兜底
            std::env::var("HOME")
                .ok()
                .map(|h| std::path::PathBuf::from(h).join(".local/share"))
        })
}
