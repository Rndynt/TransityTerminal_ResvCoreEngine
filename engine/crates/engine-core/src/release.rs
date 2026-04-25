//! Release a seat hold by `hold_ref`.
//!
//! Event order MUST be `inventory.updated` first, then `holds.released`
//! (contract §9.7).
//!
//! ## Confirm-aware filter (P1 §10.4)
//!
//! The SELECT below filters `booking_id IS NULL`, i.e. only **active** holds
//! are eligible for release. Once a hold has been promoted to a booking via
//! `confirm_booking`, the row stays in `seat_holds` as an audit trail with
//! `booking_id` set, and the corresponding `seat_inventory.hold_ref` is
//! already cleared. A subsequent `release_hold_by_ref(<consumed-hold-ref>)`
//! must NOT delete the audit row and must NOT emit `inventory.updated` /
//! `holds.released` events (which would mislead subscribers into thinking
//! the seat is back on sale). We therefore treat a confirmed hold the same
//! as a missing hold — return `ReleaseResult { success: false }` silently.

use sqlx::Row;
use uuid::Uuid;

use crate::error::EngineError;
use crate::events::{emit_release_pair, EventPublisher};
use crate::types::ReleaseResult;

pub async fn release_hold_by_ref<P: EventPublisher + ?Sized>(
    pool: &sqlx::PgPool,
    publisher: &P,
    hold_ref: &str,
) -> Result<ReleaseResult, EngineError> {
    let mut tx = pool.begin().await?;

    let hold_row = sqlx::query(
        r#"
        SELECT trip_id, seat_no, leg_indexes
          FROM seat_holds
         WHERE hold_ref = $1
           AND booking_id IS NULL
         FOR UPDATE
        "#,
    )
    .bind(hold_ref)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(row) = hold_row else {
        // No matching active hold — either truly absent OR already consumed
        // by `confirm_booking`. Either way: no row to delete, no events.
        return Ok(ReleaseResult { success: false });
    };

    let trip_id: Uuid = row.try_get("trip_id")?;
    let seat_no: String = row.try_get("seat_no")?;
    let leg_indexes: Vec<i32> = row.try_get("leg_indexes")?;

    sqlx::query("UPDATE seat_inventory SET hold_ref = NULL WHERE hold_ref = $1")
        .bind(hold_ref)
        .execute(&mut *tx)
        .await?;

    sqlx::query("DELETE FROM seat_holds WHERE hold_ref = $1")
        .bind(hold_ref)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    // Side-effect: inventory.updated then holds.released (strict order).
    let _ = emit_release_pair(publisher, trip_id, seat_no, leg_indexes).await;

    Ok(ReleaseResult { success: true })
}
