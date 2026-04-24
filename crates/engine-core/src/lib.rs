//! Core domain logic for the TransityTerminal reservation engine.
//!
//! All operations preserve 1:1 parity with the Node reference implementation
//! in `server/modules/bookings/atomicHold.service.ts`.

pub mod cancel;
pub mod confirm;
pub mod error;
pub mod events;
pub mod hold;
pub mod reaper;
pub mod release;
pub mod snapshot;
pub mod types;

pub use cancel::cancel_booking_seats;
pub use confirm::confirm_booking;
pub use error::EngineError;
pub use events::{EventPublisher, NoopPublisher, RedisPublisher, ReservationEvent};
pub use hold::atomic_hold;
pub use reaper::{expire_holds, DEFAULT_CONFIRMED_HOLDS_RETENTION_DAYS};
pub use release::release_hold_by_ref;
pub use snapshot::get_inventory_snapshot;
pub use types::*;

pub type PgPool = sqlx::PgPool;
