use serde::Serializer;

/// 全局统一错误:实现 Serialize 后可直接作为 command 返回值传给前端(序列化为可读字符串)。
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("数据库错误: {0}")]
    Db(#[from] sqlx::Error),

    #[error("迁移错误: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("HTTP 错误: {0}")]
    Http(#[from] reqwest::Error),

    #[error("{0}")]
    Other(String),
}

impl serde::Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
