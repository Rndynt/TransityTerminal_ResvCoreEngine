mod config;
mod error;
mod middleware;
mod reaper_task;
mod routes;
mod state;

use std::time::Duration;

use anyhow::Context;
use axum::Router;
use sqlx::postgres::PgPoolOptions;
use tower_http::trace::TraceLayer;
use tracing::info;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use crate::config::Config;
use crate::state::AppState;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = dotenvy::dotenv();

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(fmt::layer())
        .init();

    let cfg = Config::from_env().context("invalid configuration")?;

    info!(addr = %cfg.bind_addr, "starting reservation engine");

    // Postgres pool — min 10, max 50 per contract §8.
    let pg_pool = PgPoolOptions::new()
        .min_connections(cfg.db_min_conn)
        .max_connections(cfg.db_max_conn)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&cfg.database_url)
        .await
        .context("failed to connect to PostgreSQL")?;

    // Run migrations.
    sqlx::migrate!("../../migrations")
        .run(&pg_pool)
        .await
        .context("failed to run migrations")?;

    // Optional Redis publisher.
    let publisher: std::sync::Arc<dyn engine_core::EventPublisher> = match &cfg.redis_url {
        Some(url) => {
            let pool_cfg = deadpool_redis::Config::from_url(url.clone());
            let pool = pool_cfg
                .create_pool(Some(deadpool_redis::Runtime::Tokio1))
                .context("failed to create redis pool")?;
            std::sync::Arc::new(engine_core::RedisPublisher::new(pool))
        }
        None => {
            info!("REDIS_URL not set — events will be discarded (NoopPublisher)");
            std::sync::Arc::new(engine_core::NoopPublisher)
        }
    };

    let idempotency =
        std::sync::Arc::new(middleware::idempotency::IdempotencyStore::new(cfg.idempotency_max));

    let state = AppState {
        pool: pg_pool.clone(),
        publisher: publisher.clone(),
        hmac_secret: cfg.hmac_secret.clone(),
        hmac_skew_secs: cfg.hmac_skew_secs,
        idempotency,
    };

    // Spawn background reaper.
    let reaper_state = state.clone();
    tokio::spawn(reaper_task::run(reaper_state, cfg.reaper_interval_secs));

    let app: Router = routes::router(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr)
        .await
        .with_context(|| format!("failed to bind {}", cfg.bind_addr))?;

    info!(addr = %cfg.bind_addr, "listening");
    axum::serve(listener, app.layer(TraceLayer::new_for_http())).await?;

    Ok(())
}
