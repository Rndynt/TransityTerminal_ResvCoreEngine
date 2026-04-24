use std::sync::Arc;

use crate::middleware::idempotency::IdempotencyStore;

#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::PgPool,
    pub publisher: Arc<dyn engine_core::EventPublisher>,
    pub hmac_secret: String,
    pub hmac_skew_secs: i64,
    pub idempotency: Arc<IdempotencyStore>,
    pub ttl_short_secs: i64,
    pub ttl_long_secs: i64,
}
