//! Integration tests validating 1:1 parity with the contract checklist (§10).
//!
//! Requires `DATABASE_URL` pointing at a PostgreSQL instance. Each test uses
//! a uniquely generated `trip_id` (UUID v4) to avoid cross-test interference,
//! so the suite is safe to run in parallel against a shared schema.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use engine_core::{
    atomic_hold, cancel_booking_seats, confirm_booking, expire_holds, get_inventory_snapshot,
    release_hold_by_ref, AtomicHoldResult, EventPublisher, HoldFailureReason, NoopPublisher,
    ReservationEvent, SeatHoldRequest, SeatStatusKind, TtlClass,
};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Test publisher that records every event for ordering assertions.
#[derive(Clone, Default)]
struct RecordingPublisher {
    pub log: Arc<Mutex<Vec<ReservationEvent>>>,
}

#[async_trait::async_trait]
impl EventPublisher for RecordingPublisher {
    async fn publish(&self, event: &ReservationEvent) -> Result<(), engine_core::EngineError> {
        self.log.lock().await.push(event.clone());
        Ok(())
    }
}

async fn pool() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for tests");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .expect("connect to db");
    sqlx::migrate!("../../migrations").run(&pool).await.expect("migrate");
    pool
}

/// Materialize fresh inventory rows for a unique trip.
async fn seed_inventory(pool: &PgPool, seats: &[&str], legs: &[i32]) -> Uuid {
    let trip_id = Uuid::new_v4();
    for seat in seats {
        for leg in legs {
            sqlx::query(
                r#"INSERT INTO seat_inventory (trip_id, seat_no, leg_index, booked, hold_ref)
                   VALUES ($1, $2, $3, false, NULL)"#,
            )
            .bind(trip_id)
            .bind(*seat)
            .bind(*leg)
            .execute(pool)
            .await
            .expect("seed inventory");
        }
    }
    trip_id
}

fn req(trip_id: Uuid, seat: &str, legs: Vec<i32>, ttl: TtlClass) -> SeatHoldRequest {
    SeatHoldRequest {
        trip_id,
        seat_no: seat.to_string(),
        leg_indexes: legs,
        operator_id: "op-test".to_string(),
        ttl_class: ttl,
    }
}

// ───────────────────────────── Hold scenarios ─────────────────────────────

#[tokio::test]
async fn hold_success_single_leg() {
    let pool = pool().await;
    let trip = seed_inventory(&pool, &["1A"], &[0]).await;
    let r = atomic_hold(&pool, &NoopPublisher, req(trip, "1A", vec![0], TtlClass::Short))
        .await
        .unwrap();
    assert!(matches!(r, AtomicHoldResult::Success { .. }));
}

#[tokio::test]
async fn hold_success_multi_leg() {
    let pool = pool().await;
    let trip = seed_inventory(&pool, &["2B"], &[0, 1, 2]).await;
    let r = atomic_hold(
        &pool,
        &NoopPublisher,
        req(trip, "2B", vec![0, 1, 2], TtlClass::Long),
    )
    .await
    .unwrap();
    let AtomicHoldResult::Success { expires_at, .. } = r else {
        panic!("expected success");
    };
    let now = Utc::now();
    let delta = (expires_at - now).num_seconds();
    // TTL exact: 1800s for long. Allow tiny tolerance for clock drift between
    // assignment in `atomic_hold` and the assertion here.
    assert!(
        (1795..=1801).contains(&delta),
        "long TTL should be ~1800s, got {delta}"
    );
}

#[tokio::test]
async fn hold_short_ttl_exact_300() {
    let pool = pool().await;
    let trip = seed_inventory(&pool, &["3C"], &[0]).await;
    let r = atomic_hold(&pool, &NoopPublisher, req(trip, "3C", vec![0], TtlClass::Short))
        .await
        .unwrap();
    let AtomicHoldResult::Success { expires_at, .. } = r else {
        panic!("expected success");
    };
    let delta = (expires_at - Utc::now()).num_seconds();
    assert!((295..=301).contains(&delta), "short TTL should be ~300s, got {delta}");
}

#[tokio::test]
async fn hold_fail_when_already_booked() {
    let pool = pool().await;
    let trip = seed_inventory(&pool, &["4D"], &[0]).await;
    sqlx::query("UPDATE seat_inventory SET booked = true WHERE trip_id = $1")
        .bind(trip)
        .execute(&pool)
        .await
        .unwrap();
    let r = atomic_hold(&pool, &NoopPublisher, req(trip, "4D", vec![0], TtlClass::Short))
        .await
        .unwrap();
    match r {
        AtomicHoldResult::Failure { reason, conflict_seats, .. } => {
            assert_eq!(reason, HoldFailureReason::SeatConflict);
            assert_eq!(conflict_seats, vec!["4D".to_string()]);
        }
        _ => panic!("expected failure"),
    }
}

#[tokio::test]
async fn hold_fail_when_already_held() {
    let pool = pool().await;
    let trip = seed_inventory(&pool, &["5E"], &[0]).await;
    let _ = atomic_hold(&pool, &NoopPublisher, req(trip, "5E", vec![0], TtlClass::Short))
        .await
        .unwrap();
    let r2 = atomic_hold(&pool, &NoopPublisher, req(trip, "5E", vec![0], TtlClass::Short))
        .await
        .unwrap();
    match r2 {
        AtomicHoldResult::Failure { reason, .. } => {
            assert_eq!(reason, HoldFailureReason::SeatConflict);
        }
        _ => panic!("expected failure"),
    }
}

#[tokio::test]
async fn hold_fail_incomplete_inventory() {
    let pool = pool().await;
    // Only seed leg 0; ask for legs 0 and 1.
    let trip = seed_inventory(&pool, &["6F"], &[0]).await;
    let r = atomic_hold(
        &pool,
        &NoopPublisher,
        req(trip, "6F", vec![0, 1], TtlClass::Short),
    )
    .await
    .unwrap();
    match r {
        AtomicHoldResult::Failure { reason, conflict_seats, .. } => {
            assert_eq!(reason, HoldFailureReason::IncompleteInventory);
            assert_eq!(conflict_seats, vec!["6F".to_string()]);
        }
        _ => panic!("expected failure"),
    }
}

#[tokio::test]
async fn hold_race_exactly_one_winner() {
    let pool = pool().await;
    let trip = seed_inventory(&pool, &["7G"], &[0]).await;
    // Spawn 8 concurrent holds — exactly one should win.
    let mut handles = Vec::new();
    for _ in 0..8 {
        let p = pool.clone();
        handles.push(tokio::spawn(async move {
            atomic_hold(&p, &NoopPublisher, req(trip, "7G", vec![0], TtlClass::Short))
                .await
                .unwrap()
        }));
    }
    let mut wins = 0;
    let mut conflicts = 0;
    for h in handles {
        match h.await.unwrap() {
            AtomicHoldResult::Success { .. } => wins += 1,
            AtomicHoldResult::Failure { reason, .. } => {
                assert_eq!(reason, HoldFailureReason::SeatConflict);
                conflicts += 1;
            }
        }
    }
    assert_eq!(wins, 1, "exactly one hold must win");
    assert_eq!(conflicts, 7);
}

// ─────────────────────────── Release & events ─────────────────────────────

#[tokio::test]
async fn release_valid_hold_emits_inventory_then_holds_released() {
    let pool = pool().await;
    let trip = seed_inventory(&pool, &["8H"], &[0, 1]).await;
    let publisher = RecordingPublisher::default();

    let hold = atomic_hold(
        &pool,
        &publisher,
        req(trip, "8H", vec![0, 1], TtlClass::Short),
    )
    .await
    .unwrap();
    let AtomicHoldResult::Success { hold_ref, .. } = hold else {
        panic!("expected success");
    };

    publisher.log.lock().await.clear();

    let r = release_hold_by_ref(&pool, &publisher, &hold_ref.to_string())
        .await
        .unwrap();
    assert!(r.success);

    let log = publisher.log.lock().await.clone();
    assert_eq!(log.len(), 2, "release must emit two events");
    assert!(matches!(log[0], ReservationEvent::InventoryUpdated { .. }));
    assert!(matches!(log[1], ReservationEvent::HoldsReleased { .. }));

    // Inventory cleared.
    let row: (Option<String>,) =
        sqlx::query_as("SELECT hold_ref FROM seat_inventory WHERE trip_id = $1 LIMIT 1")
            .bind(trip)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(row.0.is_none());
}

#[tokio::test]
async fn release_missing_hold_returns_false_no_event() {
    let pool = pool().await;
    let publisher = RecordingPublisher::default();
    let r = release_hold_by_ref(&pool, &publisher, &Uuid::new_v4().to_string())
        .await
        .unwrap();
    assert!(!r.success);
    assert!(publisher.log.lock().await.is_empty());
}

// ───────────────────────────── Confirm flow ───────────────────────────────

#[tokio::test]
async fn confirm_valid_hold_marks_booked() {
    let pool = pool().await;
    let trip = seed_inventory(&pool, &["9I"], &[0]).await;
    let hold = atomic_hold(&pool, &NoopPublisher, req(trip, "9I", vec![0], TtlClass::Short))
        .await
        .unwrap();
    let AtomicHoldResult::Success { hold_ref, .. } = hold else {
        panic!("expected success");
    };

    let r = confirm_booking(&pool, &NoopPublisher, &hold_ref.to_string(), "BK-9I-TEST")
        .await
        .unwrap();
    assert!(r.success);

    let row: (bool, Option<String>) =
        sqlx::query_as("SELECT booked, hold_ref FROM seat_inventory WHERE trip_id = $1")
            .bind(trip)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(row.0);
    assert!(row.1.is_none());

    let bk: (Option<String>,) =
        sqlx::query_as("SELECT booking_id FROM seat_holds WHERE hold_ref = $1")
            .bind(hold_ref.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(bk.0.as_deref(), Some("BK-9I-TEST"));
}

#[tokio::test]
async fn confirm_expired_hold_fails_no_inventory_change() {
    let pool = pool().await;
    let trip = seed_inventory(&pool, &["10J"], &[0]).await;
    let hold = atomic_hold(
        &pool,
        &NoopPublisher,
        req(trip, "10J", vec![0], TtlClass::Short),
    )
    .await
    .unwrap();
    let AtomicHoldResult::Success { hold_ref, .. } = hold else {
        panic!("expected success");
    };

    // Force-expire.
    sqlx::query("UPDATE seat_holds SET expires_at = now() - interval '1 second' WHERE hold_ref = $1")
        .bind(hold_ref.to_string())
        .execute(&pool)
        .await
        .unwrap();

    let r = confirm_booking(&pool, &NoopPublisher, &hold_ref.to_string(), "BK-FAIL")
        .await
        .unwrap();
    assert!(!r.success);

    let row: (bool, Option<String>) =
        sqlx::query_as("SELECT booked, hold_ref FROM seat_inventory WHERE trip_id = $1")
            .bind(trip)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!row.0, "inventory must NOT be booked");
    // hold_ref still points to the (now expired) hold until the reaper sweeps it.
    assert!(row.1.is_some());
}

// ─────────────────────────────── Reaper ───────────────────────────────────

#[tokio::test]
async fn reaper_releases_expired_holds() {
    let pool = pool().await;
    let trip = seed_inventory(&pool, &["11K"], &[0]).await;
    let hold = atomic_hold(
        &pool,
        &NoopPublisher,
        req(trip, "11K", vec![0], TtlClass::Short),
    )
    .await
    .unwrap();
    let AtomicHoldResult::Success { hold_ref, .. } = hold else {
        panic!("expected success");
    };
    sqlx::query("UPDATE seat_holds SET expires_at = now() - interval '1 second' WHERE hold_ref = $1")
        .bind(hold_ref.to_string())
        .execute(&pool)
        .await
        .unwrap();

    let publisher = RecordingPublisher::default();
    let r = expire_holds(&pool, &publisher).await.unwrap();
    assert!(r.released_count >= 1);

    let row: (Option<String>,) =
        sqlx::query_as("SELECT hold_ref FROM seat_inventory WHERE trip_id = $1")
            .bind(trip)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(row.0.is_none(), "inventory hold_ref must be cleared");

    let log = publisher.log.lock().await.clone();
    assert!(
        log.iter().any(|e| matches!(e, ReservationEvent::HoldsReleased { .. })),
        "reaper must emit holds.released"
    );
}

#[tokio::test]
async fn reaper_skips_holds_with_booking_id() {
    let pool = pool().await;
    let trip = seed_inventory(&pool, &["12L"], &[0]).await;
    let hold = atomic_hold(
        &pool,
        &NoopPublisher,
        req(trip, "12L", vec![0], TtlClass::Short),
    )
    .await
    .unwrap();
    let AtomicHoldResult::Success { hold_ref, .. } = hold else {
        panic!("expected success");
    };
    confirm_booking(&pool, &NoopPublisher, &hold_ref.to_string(), "BK-12L-PROTECT")
        .await
        .unwrap();
    // Force-expire after confirm.
    sqlx::query("UPDATE seat_holds SET expires_at = now() - interval '1 second' WHERE hold_ref = $1")
        .bind(hold_ref.to_string())
        .execute(&pool)
        .await
        .unwrap();

    let _ = expire_holds(&pool, &NoopPublisher).await.unwrap();

    let still_there: (i64,) =
        sqlx::query_as("SELECT count(*) FROM seat_holds WHERE hold_ref = $1")
            .bind(hold_ref.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still_there.0, 1, "consumed holds must NEVER be reaped");

    let booked: (bool,) =
        sqlx::query_as("SELECT booked FROM seat_inventory WHERE trip_id = $1")
            .bind(trip)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(booked.0);
}

// ─────────────────────────────── Cancel ───────────────────────────────────

#[tokio::test]
async fn cancel_clears_booked_and_hold_ref() {
    let pool = pool().await;
    let trip = seed_inventory(&pool, &["13M"], &[0, 1]).await;
    let hold = atomic_hold(
        &pool,
        &NoopPublisher,
        req(trip, "13M", vec![0, 1], TtlClass::Short),
    )
    .await
    .unwrap();
    let AtomicHoldResult::Success { hold_ref, .. } = hold else {
        panic!("expected success");
    };
    confirm_booking(&pool, &NoopPublisher, &hold_ref.to_string(), "BK-13M")
        .await
        .unwrap();

    let r = cancel_booking_seats(&pool, &NoopPublisher, trip, "13M", &[0, 1])
        .await
        .unwrap();
    assert!(r.success);

    let rows: Vec<(bool, Option<String>)> = sqlx::query_as(
        "SELECT booked, hold_ref FROM seat_inventory WHERE trip_id = $1 ORDER BY leg_index",
    )
    .bind(trip)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 2);
    for r in rows {
        assert!(!r.0);
        assert!(r.1.is_none());
    }
}

// ─────────────────────────────── Snapshot ─────────────────────────────────

#[tokio::test]
async fn snapshot_reports_free_held_booked_correctly() {
    let pool = pool().await;
    let trip = seed_inventory(&pool, &["14N", "15O", "16P"], &[0]).await;

    // Hold 15O.
    let _ = atomic_hold(
        &pool,
        &NoopPublisher,
        req(trip, "15O", vec![0], TtlClass::Short),
    )
    .await
    .unwrap();
    // Book 16P via hold + confirm.
    let h = atomic_hold(
        &pool,
        &NoopPublisher,
        req(trip, "16P", vec![0], TtlClass::Short),
    )
    .await
    .unwrap();
    let AtomicHoldResult::Success { hold_ref, .. } = h else {
        panic!("expected success");
    };
    confirm_booking(&pool, &NoopPublisher, &hold_ref.to_string(), "BK-16P")
        .await
        .unwrap();

    let snap = get_inventory_snapshot(&pool, trip).await.unwrap();
    let mut m = std::collections::HashMap::new();
    for s in snap.seats {
        m.insert(s.seat_no.clone(), s.leg_states[0].status);
    }
    assert_eq!(m.get("14N"), Some(&SeatStatusKind::Free));
    assert_eq!(m.get("15O"), Some(&SeatStatusKind::Held));
    assert_eq!(m.get("16P"), Some(&SeatStatusKind::Booked));
}

// Smoke test: ensure the test pool actually reaches the DB before deeper tests run.
#[tokio::test]
async fn smoke_db_reachable() {
    let pool = pool().await;
    let _: (i32,) = sqlx::query_as("SELECT 1").fetch_one(&pool).await.unwrap();
    // Briefly delay to surface any panicking init in other tests.
    tokio::time::sleep(Duration::from_millis(10)).await;
}
