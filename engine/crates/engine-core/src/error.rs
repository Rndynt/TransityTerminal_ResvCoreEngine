use thiserror::Error;

#[derive(Debug, Error)]
pub enum EngineError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),

    #[error("redis error: {0}")]
    Redis(#[from] redis::RedisError),

    #[error("redis pool error: {0}")]
    RedisPool(String),

    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),

    #[error("hold not found or already expired")]
    HoldExpiredOrMissing,

    #[error("internal error: {0}")]
    Internal(String),
}

impl From<deadpool_redis::PoolError> for EngineError {
    fn from(e: deadpool_redis::PoolError) -> Self {
        EngineError::RedisPool(e.to_string())
    }
}
