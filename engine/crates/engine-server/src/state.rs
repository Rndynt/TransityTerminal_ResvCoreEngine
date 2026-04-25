use std::sync::Arc;

use crate::middleware::idempotency::IdempotencyStore;

#[derive(Clone)]
pub struct AppState {
    pub pool: sqlx::PgPool,
    pub publisher: Arc<dyn engine_core::EventPublisher>,
    pub hmac_secret: String,
    pub hmac_skew_secs: i64,
    /// Allowlist for `X-Service-Id` (P3 §10.13). Empty vec = check
    /// disabled (any service id passes as long as HMAC is valid).
    pub allowed_service_ids: Arc<Vec<String>>,
    pub idempotency: Arc<IdempotencyStore>,
    pub ttl_short_secs: i64,
    pub ttl_long_secs: i64,
}
