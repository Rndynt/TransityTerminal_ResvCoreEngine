use async_trait::async_trait;
use chrono::Utc;
use deadpool_redis::Pool as RedisPool;
use redis::AsyncCommands;
use serde::Serialize;
use tracing::warn;
use uuid::Uuid;

use crate::error::EngineError;

pub const CHANNEL: &str = "reservation.events";

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
pub enum ReservationEvent {
    #[serde(rename = "inventory.updated")]
    InventoryUpdated {
        trip_id: Uuid,
        seat_no: String,
        leg_indexes: Vec<i32>,
        ts: String,
    },
    #[serde(rename = "holds.released")]
    HoldsReleased {
        trip_id: Uuid,
        seat_nos: Vec<String>,
        ts: String,
    },
}

impl ReservationEvent {
    pub fn inventory_updated(trip_id: Uuid, seat_no: String, leg_indexes: Vec<i32>) -> Self {
        Self::InventoryUpdated {
            trip_id,
            seat_no,
            leg_indexes,
            ts: Utc::now().to_rfc3339(),
        }
    }

    pub fn holds_released(trip_id: Uuid, seat_nos: Vec<String>) -> Self {
        Self::HoldsReleased {
            trip_id,
            seat_nos,
            ts: Utc::now().to_rfc3339(),
        }
    }
}

#[async_trait]
pub trait EventPublisher: Send + Sync + 'static {
    async fn publish(&self, event: &ReservationEvent) -> Result<(), EngineError>;
}

/// No-op publisher for tests / standalone runs.
#[derive(Clone, Default)]
pub struct NoopPublisher;

#[async_trait]
impl EventPublisher for NoopPublisher {
    async fn publish(&self, _event: &ReservationEvent) -> Result<(), EngineError> {
        Ok(())
    }
}

/// Redis Pub/Sub publisher. Failure to publish is logged but never fails the
/// caller — the DB transaction is the source of truth.
#[derive(Clone)]
pub struct RedisPublisher {
    pool: RedisPool,
}

impl RedisPublisher {
    pub fn new(pool: RedisPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl EventPublisher for RedisPublisher {
    async fn publish(&self, event: &ReservationEvent) -> Result<(), EngineError> {
        let payload = serde_json::to_string(event)?;
        match self.pool.get().await {
            Ok(mut conn) => {
                if let Err(e) = conn.publish::<_, _, i64>(CHANNEL, &payload).await {
                    warn!(error = %e, "redis publish failed (event dropped, DB is source of truth)");
                }
            }
            Err(e) => {
                warn!(error = %e, "redis pool unavailable (event dropped)");
            }
        }
        Ok(())
    }
}

/// Helper for routines that emit multiple events; mirrors Node behaviour where
/// `inventory.updated` MUST precede `holds.released` for release flows.
pub async fn emit_release_pair<P: EventPublisher + ?Sized>(
    publisher: &P,
    trip_id: Uuid,
    seat_no: String,
    leg_indexes: Vec<i32>,
) -> Result<(), EngineError> {
    let inv = ReservationEvent::inventory_updated(trip_id, seat_no.clone(), leg_indexes);
    publisher.publish(&inv).await?;
    let rel = ReservationEvent::holds_released(trip_id, vec![seat_no]);
    publisher.publish(&rel).await?;
    Ok(())
}
