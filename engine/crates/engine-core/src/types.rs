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
///
/// Wire format is **flat** (no tag wrapper) and discriminated by the
/// `success: bool` field:
///
/// ```json
/// // Success
/// {"success": true,  "hold_ref": "...", "expires_at": "..."}
/// // Failure
/// {"success": false, "reason": "SEAT_CONFLICT", "conflict_seats": [...]}
/// ```
///
/// Serialization uses `#[serde(untagged)]` to emit fields flat. **De**-
/// serialization is implemented manually below (P3 §10.12) so the
/// dispatch is by-explicit-discriminator rather than serde's default
/// "try every variant in order" behaviour, which is fragile if a future
/// variant overlaps fields with another.
#[derive(Debug, Clone, Serialize)]
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

impl<'de> Deserialize<'de> for AtomicHoldResult {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;

        // Read into a Value first, then dispatch on `success`. This is
        // O(1) extra work compared to a derived impl and removes the
        // ambiguity of untagged variant ordering.
        let v = serde_json::Value::deserialize(deserializer)?;
        let success = v
            .get("success")
            .and_then(|s| s.as_bool())
            .ok_or_else(|| D::Error::custom("missing or non-bool `success` discriminator"))?;

        if success {
            #[derive(Deserialize)]
            struct OkVariant {
                success: bool,
                hold_ref: Uuid,
                expires_at: DateTime<Utc>,
            }
            let ok: OkVariant = serde_json::from_value(v).map_err(D::Error::custom)?;
            Ok(AtomicHoldResult::Success {
                success: ok.success,
                hold_ref: ok.hold_ref,
                expires_at: ok.expires_at,
            })
        } else {
            #[derive(Deserialize)]
            struct FailVariant {
                success: bool,
                reason: HoldFailureReason,
                #[serde(default)]
                conflict_seats: Vec<String>,
            }
            let f: FailVariant = serde_json::from_value(v).map_err(D::Error::custom)?;
            Ok(AtomicHoldResult::Failure {
                success: f.success,
                reason: f.reason,
                conflict_seats: f.conflict_seats,
            })
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserializes_success_via_discriminator() {
        let json = r#"{"success":true,"hold_ref":"00000000-0000-0000-0000-000000000001","expires_at":"2026-04-25T00:00:00Z"}"#;
        let r: AtomicHoldResult = serde_json::from_str(json).unwrap();
        assert!(matches!(r, AtomicHoldResult::Success { .. }));
    }

    #[test]
    fn deserializes_failure_via_discriminator() {
        let json = r#"{"success":false,"reason":"SEAT_CONFLICT","conflict_seats":["12A"]}"#;
        let r: AtomicHoldResult = serde_json::from_str(json).unwrap();
        match r {
            AtomicHoldResult::Failure {
                reason,
                conflict_seats,
                ..
            } => {
                assert_eq!(reason, HoldFailureReason::SeatConflict);
                assert_eq!(conflict_seats, vec!["12A".to_string()]);
            }
            _ => panic!("expected failure"),
        }
    }

    #[test]
    fn rejects_payload_missing_discriminator() {
        let json = r#"{"reason":"SEAT_CONFLICT","conflict_seats":[]}"#;
        let err = serde_json::from_str::<AtomicHoldResult>(json).unwrap_err();
        assert!(err.to_string().contains("success"));
    }

    #[test]
    fn rejects_failure_payload_missing_reason() {
        let json = r#"{"success":false,"conflict_seats":[]}"#;
        let err = serde_json::from_str::<AtomicHoldResult>(json).unwrap_err();
        // Inner Deserialize error gets wrapped in `D::Error::custom`,
        // so we just assert it surfaced something rather than silently
        // picking the wrong variant.
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn round_trip_success() {
        let original = AtomicHoldResult::success(
            Uuid::nil(),
            chrono::DateTime::from_timestamp(0, 0).unwrap(),
        );
        let json = serde_json::to_string(&original).unwrap();
        let decoded: AtomicHoldResult = serde_json::from_str(&json).unwrap();
        assert!(matches!(decoded, AtomicHoldResult::Success { .. }));
    }

    #[test]
    fn round_trip_failure() {
        let original = AtomicHoldResult::failure(
            HoldFailureReason::IncompleteInventory,
            vec!["1A".to_string()],
        );
        let json = serde_json::to_string(&original).unwrap();
        let decoded: AtomicHoldResult = serde_json::from_str(&json).unwrap();
        match decoded {
            AtomicHoldResult::Failure { reason, .. } => {
                assert_eq!(reason, HoldFailureReason::IncompleteInventory);
            }
            _ => panic!("expected failure"),
        }
    }
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
