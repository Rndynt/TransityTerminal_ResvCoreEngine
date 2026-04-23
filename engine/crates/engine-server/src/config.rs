use anyhow::{anyhow, Result};
use std::env;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_addr: String,
    pub database_url: String,
    pub redis_url: Option<String>,
    pub hmac_secret: String,
    pub hmac_skew_secs: i64,
    pub reaper_interval_secs: u64,
    pub db_min_conn: u32,
    pub db_max_conn: u32,
    pub idempotency_max: u64,
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
            idempotency_max: env::var("IDEMPOTENCY_MAX")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(10_000),
        })
    }
}
