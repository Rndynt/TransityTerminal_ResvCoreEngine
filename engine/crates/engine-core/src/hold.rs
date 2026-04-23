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

pub async fn atomic_hold<P: EventPublisher + ?Sized>(
    pool: &sqlx::PgPool,
    publisher: &P,
    req: SeatHoldRequest,
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
    let expires_at = Utc::now() + Duration::seconds(ttl_class.seconds());

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

    // 1. Lock inventory rows (row-level, blocking).
    //    ORDER BY leg_index gives deterministic lock acquisition order across
    //    concurrent holds touching overlapping leg ranges, avoiding deadlocks.
    let rows = sqlx::query(
        r#"
        SELECT booked, hold_ref
          FROM seat_inventory
         WHERE trip_id = $1
           AND seat_no = $2
           AND leg_index = ANY($3)
         ORDER BY leg_index
         FOR UPDATE
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

    // 2b. Conflict check.
    for r in &rows {
        let booked: bool = r.try_get("booked")?;
        let existing_hold: Option<String> = r.try_get("hold_ref")?;
        if booked || existing_hold.is_some() {
            return Ok(Err(HoldFailureReason::SeatConflict));
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
