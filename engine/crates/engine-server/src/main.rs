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

    // Schema fail-fast. The engine shares its database with TransityTerminal
    // in per-operator deployments, so the most common deployment mistake is
    // pointing DATABASE_URL at the wrong Neon project (e.g. an empty DB or
    // the central control-plane DB). A bad URL would silently let the
    // engine boot, then return cryptic 500s on the first /v1/holds call.
    //
    // We probe the two tables this engine touches with a zero-row SELECT
    // listing the columns it expects. Any error → exit 1 with a clear
    // message so the sidecar restart loop surfaces the problem to the
    // operator immediately instead of after first traffic.
    if let Err(e) = sqlx::query(
        "SELECT id, hold_ref, trip_id, seat_no, leg_indexes, operator_id, expires_at, ttl_class, booking_id FROM seat_holds LIMIT 0",
    )
    .execute(&pg_pool)
    .await
    {
        anyhow::bail!(
            "startup schema check failed on seat_holds: {e:#}. \
             Verify DATABASE_URL points at the correct operator's Neon DB \
             with TransityTerminal migrations applied."
        );
    }
    if let Err(e) = sqlx::query(
        "SELECT trip_id, seat_no, leg_index, booked, hold_ref FROM seat_inventory LIMIT 0",
    )
    .execute(&pg_pool)
    .await
    {
        anyhow::bail!(
            "startup schema check failed on seat_inventory: {e:#}. \
             Verify DATABASE_URL points at the correct operator's Neon DB \
             with TransityTerminal migrations applied."
        );
    }
    info!("schema fail-fast probe OK (seat_holds, seat_inventory present)");

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

    // P1 §10.3: Postgres-backed idempotency store survives restart. The
    // store is unbounded — capacity is governed by the reaper sweep
    // interval (rows expire after 24h and are physically removed by the
    // periodic DELETE in `reaper_task::run`).
    let idempotency =
        std::sync::Arc::new(middleware::idempotency::IdempotencyStore::new(pg_pool.clone()));

    if !cfg.allowed_service_ids.is_empty() {
        info!(
            allowlist = ?cfg.allowed_service_ids,
            "X-Service-Id allowlist enabled"
        );
    } else {
        info!("X-Service-Id allowlist disabled (ALLOWED_SERVICE_IDS unset)");
    }

    let state = AppState {
        pool: pg_pool.clone(),
        publisher: publisher.clone(),
        hmac_secret: cfg.hmac_secret.clone(),
        hmac_skew_secs: cfg.hmac_skew_secs,
        allowed_service_ids: std::sync::Arc::new(cfg.allowed_service_ids.clone()),
        idempotency,
        ttl_short_secs: cfg.ttl_short_secs,
        ttl_long_secs: cfg.ttl_long_secs,
    };

    info!(
        ttl_short = cfg.ttl_short_secs,
        ttl_long = cfg.ttl_long_secs,
        retention_days = cfg.confirmed_holds_retention_days,
        "hold ttl + retention configuration loaded"
    );

    // Spawn background reaper.
    let reaper_state = state.clone();
    tokio::spawn(reaper_task::run(
        reaper_state,
        cfg.reaper_interval_secs,
        cfg.confirmed_holds_retention_days,
    ));

    // Spawn idempotency cache sweep (P1 §10.3).
    let idem_state = state.clone();
    tokio::spawn(reaper_task::run_idempotency_sweep(
        idem_state,
        cfg.idempotency_sweep_interval_secs,
    ));

    let app: Router = routes::router(state);

    let listener = tokio::net::TcpListener::bind(&cfg.bind_addr)
        .await
        .with_context(|| format!("failed to bind {}", cfg.bind_addr))?;

    info!(addr = %cfg.bind_addr, "listening");
    axum::serve(listener, app.layer(TraceLayer::new_for_http())).await?;

    Ok(())
}
