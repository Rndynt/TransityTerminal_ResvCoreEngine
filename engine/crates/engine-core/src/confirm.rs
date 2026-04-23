//! Promote a hold into a permanent booking.

use sqlx::Row;
use uuid::Uuid;

use crate::error::EngineError;
use crate::events::{EventPublisher, ReservationEvent};
use crate::types::ConfirmResult;

pub async fn confirm_booking<P: EventPublisher + ?Sized>(
    pool: &sqlx::PgPool,
    publisher: &P,
    hold_ref: &str,
    booking_id: &str,
) -> Result<ConfirmResult, EngineError> {
    let mut tx = pool.begin().await?;

    // 1. Validate hold exists and is not expired; lock it.
    let hold = sqlx::query(
        r#"
        SELECT trip_id, seat_no, leg_indexes, booking_id
          FROM seat_holds
         WHERE hold_ref = $1
           AND expires_at > now()
         FOR UPDATE
        "#,
    )
    .bind(hold_ref)
    .fetch_optional(&mut *tx)
    .await?;

    let Some(row) = hold else {
        return Ok(ConfirmResult {
            success: false,
            conflict: Some("HOLD_EXPIRED_OR_MISSING".to_string()),
        });
    };

    let existing_booking: Option<String> = row.try_get("booking_id")?;
    if existing_booking.is_some() {
        return Ok(ConfirmResult {
            success: false,
            conflict: Some("HOLD_ALREADY_CONSUMED".to_string()),
        });
    }

    let trip_id: Uuid = row.try_get("trip_id")?;
    let seat_no: String = row.try_get("seat_no")?;
    let leg_indexes: Vec<i32> = row.try_get("leg_indexes")?;

    // 2. Lock inventory rows held by this hold_ref.
    sqlx::query("SELECT id FROM seat_inventory WHERE hold_ref = $1 FOR UPDATE")
        .bind(hold_ref)
        .fetch_all(&mut *tx)
        .await?;

    // 3. Mark inventory booked, clear hold_ref.
    sqlx::query(
        r#"
        UPDATE seat_inventory
           SET booked = true, hold_ref = NULL
         WHERE hold_ref = $1
        "#,
    )
    .bind(hold_ref)
    .execute(&mut *tx)
    .await?;

    // 4. Mark hold consumed.
    sqlx::query("UPDATE seat_holds SET booking_id = $2 WHERE hold_ref = $1")
        .bind(hold_ref)
        .bind(booking_id)
        .execute(&mut *tx)
        .await?;

    tx.commit().await?;

    let evt = ReservationEvent::inventory_updated(trip_id, seat_no, leg_indexes);
    let _ = publisher.publish(&evt).await;

    Ok(ConfirmResult {
        success: true,
        conflict: None,
    })
}
