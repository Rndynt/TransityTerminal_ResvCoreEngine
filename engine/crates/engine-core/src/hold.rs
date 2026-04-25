//! Atomic seat hold operation.
//!
//! Mirrors `atomicHold.service.ts::atomicHold` behaviour. The transaction
//! locks inventory rows with `FOR UPDATE`, validates completeness and
//! conflicts, marks the rows with the new `hold_ref`, and inserts a single
//! `seat_holds` row. Conflict reporting returns `[seat_no]` (single element)
//! to match the reference implementation.

use chrono::{Duration, Utc};
use sqlx::Row;
use uuid::Uuid;

use crate::error::EngineError;
use crate::events::{EventPublisher, ReservationEvent};
use crate::types::{AtomicHoldResult, HoldFailureReason, SeatHoldRequest};

/// Acquire an atomic seat hold.
///
/// `ttl_seconds` is the lifetime of the hold in seconds and **must** be
/// resolved by the caller from operator configuration (env vars
/// `HOLD_TTL_SHORT_SECONDS` / `HOLD_TTL_LONG_SECONDS` in production) so
/// that engine and Node Terminal agree on the same expiry. The class on
/// `req` is still used for the audit column and event payloads.
pub async fn atomic_hold<P: EventPublisher + ?Sized>(
    pool: &sqlx::PgPool,
    publisher: &P,
    req: SeatHoldRequest,
    ttl_seconds: i64,
) -> Result<AtomicHoldResult, EngineError> {
    let SeatHoldRequest {
        trip_id,
        seat_no,
        leg_indexes,
        operator_id,
        ttl_class,
    } = req;

    let hold_ref = Uuid::new_v4();
    let hold_ref_str = hold_ref.to_string(); // lowercase canonical (per contract §9.2)
    let expires_at = Utc::now() + Duration::seconds(ttl_seconds);

    let result = run_hold_txn(
        pool,
        trip_id,
        &seat_no,
        &leg_indexes,
        &operator_id,
        ttl_class.as_str(),
        &hold_ref_str,
        expires_at,
    )
    .await;

    match result {
        Ok(Ok(())) => {
            // Side-effect after commit: emit inventory.updated.
            let evt = ReservationEvent::inventory_updated(trip_id, seat_no.clone(), leg_indexes);
            // Best-effort publish.
            let _ = publisher.publish(&evt).await;
            Ok(AtomicHoldResult::success(hold_ref, expires_at))
        }
        Ok(Err(reason)) => Ok(AtomicHoldResult::failure(reason, vec![seat_no])),
        Err(e) => {
            tracing::error!(error = %e, "atomic_hold transaction error");
            Ok(AtomicHoldResult::failure(
                HoldFailureReason::TransactionError,
                vec![seat_no],
            ))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_hold_txn(
    pool: &sqlx::PgPool,
    trip_id: Uuid,
    seat_no: &str,
    leg_indexes: &[i32],
    operator_id: &str,
    ttl_class: &str,
    hold_ref: &str,
    expires_at: chrono::DateTime<Utc>,
) -> Result<Result<(), HoldFailureReason>, sqlx::Error> {
    let mut tx = pool.begin().await?;

    // 1. Lock inventory rows (row-level, blocking) and pull the
    //    referenced hold's expiry + booking_id in the same query.
    //    ORDER BY leg_index gives deterministic lock acquisition order
    //    across concurrent holds touching overlapping leg ranges,
    //    avoiding deadlocks. `FOR UPDATE OF i` only locks the
    //    seat_inventory rows (not seat_holds) — holds rows are read-
    //    only here, the reaper / release path is the only writer.
    //
    // P2 §10.7 — without the LEFT JOIN, an expired hold whose
    // `hold_ref` is still pinned on inventory (because the reaper
    // hasn't swept yet, max 60s window) would surface as a false
    // SEAT_CONFLICT and the user would see "kursi sudah di-hold"
    // for a seat nobody is actually holding any more.
    let rows = sqlx::query(
        r#"
        SELECT i.booked,
               i.hold_ref,
               h.expires_at  AS hold_expires_at,
               h.booking_id  AS hold_booking_id
          FROM seat_inventory i
          LEFT JOIN seat_holds h ON h.hold_ref = i.hold_ref
         WHERE i.trip_id = $1
           AND i.seat_no = $2
           AND i.leg_index = ANY($3)
         ORDER BY i.leg_index
         FOR UPDATE OF i
        "#,
    )
    .bind(trip_id)
    .bind(seat_no)
    .bind(leg_indexes)
    .fetch_all(&mut *tx)
    .await?;

    // 2a. Inventory completeness check.
    if rows.len() != leg_indexes.len() {
        // Roll back implicitly on drop.
        return Ok(Err(HoldFailureReason::IncompleteInventory));
    }

    // 2b. Conflict check (P2 §10.7 — expired-aware).
    //
    // A leg conflicts iff:
    //   (a) it is already booked, OR
    //   (b) it has a hold_ref pointing at an *active* seat_holds row,
    //       i.e. one that has either not expired yet (h.expires_at >
    //       now()) or has been confirmed (h.booking_id IS NOT NULL).
    //
    // An orphaned hold_ref (no matching seat_holds row, or one whose
    // expires_at has lapsed without a booking_id) is treated as a
    // tombstone the reaper will sweep — we proceed with the new hold
    // and overwrite it.
    let now = Utc::now();
    for r in &rows {
        let booked: bool = r.try_get("booked")?;
        if booked {
            return Ok(Err(HoldFailureReason::SeatConflict));
        }
        let existing_hold: Option<String> = r.try_get("hold_ref")?;
        if existing_hold.is_some() {
            let hold_expires_at: Option<chrono::DateTime<Utc>> = r.try_get("hold_expires_at")?;
            let hold_booking_id: Option<String> = r.try_get("hold_booking_id")?;
            let active = match (hold_expires_at, hold_booking_id.as_deref()) {
                // Confirmed hold (booking_id set) — always treat as
                // active regardless of expiry; release path nulls
                // hold_ref when the booking is cancelled.
                (_, Some(_)) => true,
                // Unconfirmed hold still inside its TTL window.
                (Some(exp), None) => exp > now,
                // hold_ref without a matching seat_holds row, or with
                // expires_at NULL — treat as orphan, not a conflict.
                (None, None) => false,
            };
            if active {
                return Ok(Err(HoldFailureReason::SeatConflict));
            }
        }
    }

    // 5. Mark inventory.
    sqlx::query(
        r#"
        UPDATE seat_inventory
           SET hold_ref = $4
         WHERE trip_id = $1
           AND seat_no = $2
           AND leg_index = ANY($3)
        "#,
    )
    .bind(trip_id)
    .bind(seat_no)
    .bind(leg_indexes)
    .bind(hold_ref)
    .execute(&mut *tx)
    .await?;

    // 6. Insert hold record.
    sqlx::query(
        r#"
        INSERT INTO seat_holds
          (hold_ref, trip_id, seat_no, leg_indexes, ttl_class, operator_id, expires_at)
        VALUES ($1, $2, $3, $4, $5, $6, $7)
        "#,
    )
    .bind(hold_ref)
    .bind(trip_id)
    .bind(seat_no)
    .bind(leg_indexes)
    .bind(ttl_class)
    .bind(operator_id)
    .bind(expires_at)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Ok(()))
}
