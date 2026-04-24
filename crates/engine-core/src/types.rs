use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// TTL class for a hold. `Short` = seat-map pick, `Long` = pending booking.
///
/// The actual TTL in seconds is supplied by the caller (resolved from
/// `HOLD_TTL_SHORT_SECONDS` / `HOLD_TTL_LONG_SECONDS` env vars at the
/// server layer so engine + Terminal stay in sync). The constants below
/// are *defaults* used when no config is plumbed through (tests, loadtest).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TtlClass {
    Short,
    Long,
}

impl TtlClass {
    /// Default TTL in seconds. Production callers should pass the value
    /// resolved from operator config to `atomic_hold` instead of relying
    /// on this constant.
    pub const fn default_seconds(self) -> i64 {
        match self {
            TtlClass::Short => 300,
            TtlClass::Long => 1800,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            TtlClass::Short => "short",
            TtlClass::Long => "long",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SeatHoldRequest {
    pub trip_id: Uuid,
    pub seat_no: String,
    pub leg_indexes: Vec<i32>,
    pub operator_id: String,
    pub ttl_class: TtlClass,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HoldFailureReason {
    IncompleteInventory,
    SeatConflict,
    TransactionError,
}

/// Discriminated union mirroring the Node service result.
/// Serialized with a `success` boolean to match wire format expectations.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AtomicHoldResult {
    Success {
        success: bool, // always true
        hold_ref: Uuid,
        expires_at: DateTime<Utc>,
    },
    Failure {
        success: bool, // always false
        reason: HoldFailureReason,
        conflict_seats: Vec<String>,
    },
}

impl AtomicHoldResult {
    pub fn success(hold_ref: Uuid, expires_at: DateTime<Utc>) -> Self {
        Self::Success {
            success: true,
            hold_ref,
            expires_at,
        }
    }

    pub fn failure(reason: HoldFailureReason, conflict_seats: Vec<String>) -> Self {
        Self::Failure {
            success: false,
            reason,
            conflict_seats,
        }
    }

    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success { .. })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReleaseResult {
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfirmResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conflict: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelResult {
    pub success: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReaperResult {
    pub released_count: usize,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SeatStatusKind {
    Free,
    Held,
    Booked,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LegState {
    pub leg_index: i32,
    pub status: SeatStatusKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hold_expires_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SeatState {
    pub seat_no: String,
    pub leg_states: Vec<LegState>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InventorySnapshot {
    pub trip_id: Uuid,
    pub seats: Vec<SeatState>,
}
