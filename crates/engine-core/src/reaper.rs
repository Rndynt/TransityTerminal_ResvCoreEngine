//! Background reaper: release expired holds.
//!
//! Mirrors `server/scheduler.ts::cleanupExpiredHolds`. Uses
//! `pg_try_advisory_lock(hashtext('reservation_reaper'))` to keep multi-instance
//! deployments single-runner. Holds with `booking_id IS NOT NULL` are NEVER
//! released.

use chrono::{Duration, Utc};
use sqlx::{Acquire, Row};
use uuid::Uuid;

use crate::error::EngineError;
use crate::events::{EventPublisher, ReservationEvent};
use crate::types::ReaperResult;

const REAPER_LOCK_KEY: &str = "reservation_reaper";
const BATCH_LIMIT: i64 = 500;
/// Default retention for confirmed (booking_id IS NOT NULL) hold rows when
/// the caller doesn't supply an explicit value (tests, loadtest). Production
/// callers should pass `CONFIRMED_HOLDS_RETENTION_DAYS` from operator env.
pub const DEFAULT_CONFIRMED_HOLDS_RETENTION_DAYS: i64 = 30;

pub async fn expire_holds<P: EventPublisher + ?Sized>(
    pool: &sqlx::PgPool,
    publisher: &P,
    confirmed_retention_days: i64,
) -> Result<ReaperResult, EngineError> {
    let mut conn = pool.acquire().await?;

    let lock_acquired: bool =
        sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtext($1))")
            .bind(REAPER_LOCK_KEY)
            .fetch_one(&mut *conn)
            .await?;

    if !lock_acquired {
        return Ok(ReaperResult { released_count: 0 });
    }

    let result = run_reaper(&mut conn, confirmed_retention_days).await;

    // Always release the advisory lock.
    let _: bool = sqlx::query_scalar("SELECT pg_advisory_unlock(hashtext($1))")
        .bind(REAPER_LOCK_KEY)
        .fetch_one(&mut *conn)
        .await
        .unwrap_or(false);

    let expired = result?;

    // Emit holds.released per seat (per Node behaviour each released hold = one event).
    for (trip_id, seat_no, _legs) in &expired {
        let evt = ReservationEvent::holds_released(*trip_id, vec![seat_no.clone()]);
        let _ = publisher.publish(&evt).await;
    }

    Ok(ReaperResult {
        released_count: expired.len(),
    })
}

async fn run_reaper(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Postgres>,
    confirmed_retention_days: i64,
) -> Result<Vec<(Uuid, String, Vec<i32>)>, sqlx::Error> {
    // Audit-trail purge: confirmed holds (booking_id IS NOT NULL) are kept
    // for `confirmed_retention_days` after expiry then deleted so the table
    // doesn't grow forever. Done in its own statement (not the main
    // transaction) so a slow purge can't hold the reaper transaction open.
    if confirmed_retention_days > 0 {
        let cutoff = Utc::now() - Duration::days(confirmed_retention_days);
        let purged = sqlx::query(
            r#"
            DELETE FROM seat_holds
             WHERE booking_id IS NOT NULL
               AND expires_at < $1
            "#,
        )
        .bind(cutoff)
        .execute(&mut **conn)
        .await?;
        if purged.rows_affected() > 0 {
            tracing::info!(
                purged = purged.rows_affected(),
                retention_days = confirmed_retention_days,
                "purged old confirmed hold rows"
            );
        }
    }

    let mut tx = conn.begin().await?;

    let expired_rows = sqlx::query(
        r#"
        SELECT hold_ref, trip_id, seat_no, leg_indexes
          FROM seat_holds
         WHERE expires_at <= now()
           AND booking_id IS NULL
         FOR UPDATE SKIP LOCKED
         LIMIT $1
        "#,
    )
    .bind(BATCH_LIMIT)
    .fetch_all(&mut *tx)
    .await?;

    if expired_rows.is_empty() {
        tx.commit().await?;
        return Ok(Vec::new());
    }

    let mut refs: Vec<String> = Vec::with_capacity(expired_rows.len());
    let mut released: Vec<(Uuid, String, Vec<i32>)> = Vec::with_capacity(expired_rows.len());
    for r in &expired_rows {
        let hr: String = r.try_get("hold_ref")?;
        let tid: Uuid = r.try_get("trip_id")?;
        let seat: String = r.try_get("seat_no")?;
        let legs: Vec<i32> = r.try_get("leg_indexes")?;
        refs.push(hr);
        released.push((tid, seat, legs));
    }

    sqlx::query("UPDATE seat_inventory SET hold_ref = NULL WHERE hold_ref = ANY($1)")
        .bind(&refs)
        .execute(&mut *tx)
        .await?;

    sqlx::query(
        r#"
        DELETE FROM seat_holds
         WHERE hold_ref = ANY($1)
           AND booking_id IS NULL
        "#,
    )
    .bind(&refs)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(released)
}
