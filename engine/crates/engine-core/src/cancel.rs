//! Cancel seats for a passenger / booking — clear `booked` and `hold_ref`.

use uuid::Uuid;

use crate::error::EngineError;
use crate::events::{EventPublisher, ReservationEvent};
use crate::types::CancelResult;

pub async fn cancel_booking_seats<P: EventPublisher + ?Sized>(
    pool: &sqlx::PgPool,
    publisher: &P,
    trip_id: Uuid,
    seat_no: &str,
    leg_indexes: &[i32],
) -> Result<CancelResult, EngineError> {
    let mut tx = pool.begin().await?;

    sqlx::query(
        r#"
        UPDATE seat_inventory
           SET booked = false, hold_ref = NULL
         WHERE trip_id = $1
           AND seat_no = $2
           AND leg_index = ANY($3)
        "#,
    )
    .bind(trip_id)
    .bind(seat_no)
    .bind(leg_indexes)
    .execute(&mut *tx)
    .await?;

    tx.commit().await?;

    let evt =
        ReservationEvent::inventory_updated(trip_id, seat_no.to_string(), leg_indexes.to_vec());
    let _ = publisher.publish(&evt).await;

    Ok(CancelResult { success: true })
}
