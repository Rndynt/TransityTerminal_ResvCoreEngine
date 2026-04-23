//! Release a seat hold by `hold_ref`.
//!
//! Event order MUST be `inventory.updated` first, then `holds.released`
//! (contract §9.7).

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
         FOR UPDATE
        "#,
    )
    .bind(hold_ref)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(row) = hold_row else {
        // No matching hold — nothing to do, no events.
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
