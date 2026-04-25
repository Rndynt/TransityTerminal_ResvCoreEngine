use anyhow::{anyhow, Result};
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub database_url: String,
    pub redis_url: Option<String>,
    pub hmac_secret: String,
    pub hmac_skew_secs: i64,
    /// Allowlist of accepted `X-Service-Id` values. Empty `Vec` disables
    /// the check (default — back-compat for single-tenant deploys where
    /// any caller with a valid HMAC is authoritative). Configure via
    /// `ALLOWED_SERVICE_IDS=terminal,console`.
    pub allowed_service_ids: Vec<String>,
    pub reaper_interval_secs: u64,
    pub db_min_conn: u32,
    pub db_max_conn: u32,
    /// How often the reaper purges expired idempotency cache rows.
    /// Reuses `reaper_interval_secs` — see `reaper_task::run`.
    pub idempotency_sweep_interval_secs: u64,
    /// Hold TTL for the `short` class (seat-map pick). Mirrors
    /// `HOLD_TTL_SHORT_SECONDS` in the Node Terminal so both sides agree.
    pub ttl_short_secs: i64,
    /// Hold TTL for the `long` class (pending booking). Mirrors
    /// `HOLD_TTL_LONG_SECONDS` in the Node Terminal so both sides agree.
    pub ttl_long_secs: i64,
    /// Retention period before confirmed (booking_id IS NOT NULL) hold rows
    /// are purged by the reaper. Acts as an audit-trail TTL.
    pub confirmed_holds_retention_days: i64,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let port = env::var("PORT").unwrap_or_else(|_| "8090".to_string());
        let host = env::var("HOST").unwrap_or_else(|_| "0.0.0.0".to_string());
        let bind_addr = format!("{host}:{port}");

        // Prefer ENGINE_DATABASE_URL (e.g. Neon) over the workspace's
        // managed DATABASE_URL so the engine can target an external Postgres.
        let database_url = env::var("ENGINE_DATABASE_URL")
            .or_else(|_| env::var("DATABASE_URL"))
            .map_err(|_| anyhow!("ENGINE_DATABASE_URL or DATABASE_URL must be set"))?;

        let redis_url = env::var("REDIS_URL").ok().filter(|s| !s.is_empty());

        let hmac_secret = env::var("RESERVATION_ENGINE_HMAC_SECRET")
            .map_err(|_| anyhow!("RESERVATION_ENGINE_HMAC_SECRET must be set"))?;

        if hmac_secret.len() < 16 {
            return Err(anyhow!(
                "RESERVATION_ENGINE_HMAC_SECRET must be at least 16 characters"
            ));
        }

        Ok(Self {
            bind_addr,
            database_url,
            redis_url,
            hmac_secret,
            hmac_skew_secs: env::var("HMAC_SKEW_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
            allowed_service_ids: env::var("ALLOWED_SERVICE_IDS")
                .ok()
                .map(|s| {
                    s.split(',')
                        .map(|p| p.trim().to_string())
                        .filter(|p| !p.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            reaper_interval_secs: env::var("REAPER_INTERVAL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(60),
            db_min_conn: env::var("DB_MIN_CONN")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(10),
            db_max_conn: env::var("DB_MAX_CONN")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(50),
            idempotency_sweep_interval_secs: env::var("IDEMPOTENCY_SWEEP_INTERVAL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(3600), // hourly is fine — reads filter expired rows anyway
            ttl_short_secs: env::var("HOLD_TTL_SHORT_SECONDS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(300),
            ttl_long_secs: env::var("HOLD_TTL_LONG_SECONDS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1800),
            confirmed_holds_retention_days: env::var("CONFIRMED_HOLDS_RETENTION_DAYS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(30),
        })
    }
}
