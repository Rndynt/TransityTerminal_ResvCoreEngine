//! Read-only inventory snapshot for seat-map UIs.

use chrono::{DateTime, Utc};
use sqlx::Row;
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::error::EngineError;
use crate::types::{InventorySnapshot, LegState, SeatState, SeatStatusKind};

pub async fn get_inventory_snapshot(
    pool: &sqlx::PgPool,
    trip_id: Uuid,
) -> Result<InventorySnapshot, EngineError> {
    let rows = sqlx::query(
        r#"
        SELECT i.seat_no,
               i.leg_index,
               i.booked,
               i.hold_ref,
               h.expires_at AS hold_expires_at
          FROM seat_inventory i
          LEFT JOIN seat_holds h ON h.hold_ref = i.hold_ref
         WHERE i.trip_id = $1
         ORDER BY i.seat_no, i.leg_index
        "#,
    )
    .bind(trip_id)
    .fetch_all(pool)
    .await?;

    // Group by seat_no preserving sort order.
    let mut by_seat: BTreeMap<String, Vec<LegState>> = BTreeMap::new();
    for r in rows {
        let seat_no: String = r.try_get("seat_no")?;
        let leg_index: i32 = r.try_get("leg_index")?;
        let booked: bool = r.try_get("booked")?;
        let hold_ref: Option<String> = r.try_get("hold_ref")?;
        let hold_expires_at: Option<DateTime<Utc>> = r.try_get("hold_expires_at")?;

        let status = if booked {
            SeatStatusKind::Booked
        } else if hold_ref.is_some() {
            SeatStatusKind::Held
        } else {
            SeatStatusKind::Free
        };

        by_seat.entry(seat_no).or_default().push(LegState {
            leg_index,
            status,
            hold_expires_at: if matches!(status, SeatStatusKind::Held) {
                hold_expires_at
            } else {
                None
            },
        });
    }

    let seats = by_seat
        .into_iter()
        .map(|(seat_no, leg_states)| SeatState { seat_no, leg_states })
        .collect();

    Ok(InventorySnapshot { trip_id, seats })
}
