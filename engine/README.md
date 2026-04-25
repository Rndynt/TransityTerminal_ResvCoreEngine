# Reservation Engine (Rust)

Rust port of the TransityTerminal reservation engine. Implements the
**v1.0 Reservation Engine Contract** with 1:1 parity against the Node
reference (`server/modules/bookings/atomicHold.service.ts` et al.).

## Layout

```
engine/
├── Cargo.toml                     # Workspace
├── migrations/0001_init.sql       # Schema (idempotent on shared DB)
└── crates/
    ├── engine-core/               # Domain logic (lib)
    │   └── src/
    │       ├── types.rs           # SeatHoldRequest, AtomicHoldResult, …
    │       ├── hold.rs            # atomic_hold (FOR UPDATE + insert)
    │       ├── release.rs         # release_hold_by_ref
    │       ├── confirm.rs         # confirm_booking (hold → booked)
    │       ├── cancel.rs          # cancel_booking_seats
    │       ├── reaper.rs          # expire_holds (advisory-locked)
    │       ├── snapshot.rs        # get_inventory_snapshot
    │       ├── events.rs          # Redis Pub/Sub publisher
    │       └── error.rs
    └── engine-server/             # Axum HTTP server (bin)
        └── src/
            ├── main.rs            # Boot, pool, reaper task spawn
            ├── config.rs          # Env-driven Config
            ├── routes.rs          # /api/v1/* routes
            ├── middleware/
            │   ├── hmac.rs        # X-Service-Id / X-Timestamp / X-Signature
            │   └── idempotency.rs # 24h LRU keyed by Idempotency-Key
            ├── reaper_task.rs     # tokio interval (default 60s)
            └── error.rs
```

## Endpoints (internal, HMAC-signed)

| Method | Path                                    | Engine fn               |
|--------|-----------------------------------------|-------------------------|
| GET    | `/api/v1/healthz`                       | (bypasses HMAC)         |
| POST   | `/api/v1/holds`                         | `atomic_hold`           |
| DELETE | `/api/v1/holds/:hold_ref`               | `release_hold_by_ref`   |
| POST   | `/api/v1/holds/:hold_ref/confirm`       | `confirm_booking`       |
| POST   | `/api/v1/cancel-seats`                  | `cancel_booking_seats`  |
| GET    | `/api/v1/inventory/:trip_id`            | `get_inventory_snapshot`|

`POST` / `PUT` / `PATCH` / `DELETE` honor the `Idempotency-Key` header per
contract §6 (24h TTL, body-hash compared, 409 on collision).

## Auth (contract §7)

Every request (except `/healthz`) must carry:

```
X-Service-Id: terminal
X-Timestamp:  1714056000
X-Signature:  hex(HMAC-SHA256(secret, "{ts}.{METHOD}.{path}.{body_sha256}"))
```

`|now - ts| > HMAC_SKEW_SECS` (default 30s) → 401. Constant-time signature
comparison via `subtle`.

## Environment

| Var                                | Required | Default          | Notes                                   |
|------------------------------------|----------|------------------|-----------------------------------------|
| `DATABASE_URL`                     | yes      | —                | PostgreSQL connection string            |
| `RESERVATION_ENGINE_HMAC_SECRET`   | yes      | —                | ≥16 chars                               |
| `REDIS_URL`                        | no       | —                | Falls back to NoopPublisher if unset    |
| `PORT`, `HOST`                     | no       | `8090`, `0.0.0.0`|                                          |
| `HMAC_SKEW_SECS`                   | no       | `30`             |                                          |
| `REAPER_INTERVAL_SECS`             | no       | `60`             |                                          |
| `DB_MIN_CONN`, `DB_MAX_CONN`       | no       | `10`, `50`       | Per contract §8                         |
| `IDEMPOTENCY_SWEEP_INTERVAL_SECS`  | no       | `3600`           | Period between sweeps that purge expired idempotency rows. Reads filter `expires_at > now()` regardless. |

## Build & Run

```bash
cd engine
cargo build --release
DATABASE_URL=... RESERVATION_ENGINE_HMAC_SECRET=... ./target/release/engine-server
```

Migrations run automatically on boot via `sqlx::migrate!`.

## Tests

The full parity checklist (contract §10) is implemented in
`crates/engine-core/tests/parity.rs`. Each test seeds a unique `trip_id`
to allow safe parallel execution against a shared database.

```bash
DATABASE_URL=postgres://... cargo test --workspace
```

Coverage:

| §10 item                                         | Test                                                |
|--------------------------------------------------|------------------------------------------------------|
| Hold sukses 1 seat 1 leg                         | `hold_success_single_leg`                            |
| Hold sukses multi-leg                            | `hold_success_multi_leg`                             |
| TTL exact 300 / 1800                             | `hold_short_ttl_exact_300`, `hold_success_multi_leg` |
| Hold gagal: sudah booked                         | `hold_fail_when_already_booked`                      |
| Hold gagal: sudah di-hold                        | `hold_fail_when_already_held`                        |
| Hold gagal: leg_index tidak ada                  | `hold_fail_incomplete_inventory`                     |
| Race 2+ concurrent → 1 menang                    | `hold_race_exactly_one_winner`                       |
| Release valid → inventory clear, event terbit    | `release_valid_hold_emits_inventory_then_holds_released` |
| Release tidak ada → `{success:false}`, no event  | `release_missing_hold_returns_false_no_event`        |
| Confirm valid → booked + booking_id              | `confirm_valid_hold_marks_booked`                    |
| Confirm expired → fail, no inventory change      | `confirm_expired_hold_fails_no_inventory_change`     |
| Reaper releases expired                          | `reaper_releases_expired_holds`                      |
| Reaper skips `booking_id IS NOT NULL`            | `reaper_skips_holds_with_booking_id`                 |
| Cancel passenger: inventory free                 | `cancel_clears_booked_and_hold_ref`                  |
| Snapshot Free/Held/Booked                        | `snapshot_reports_free_held_booked_correctly`        |

## Redis Event Schema (channel `reservation.events`)

```json
{ "type": "inventory.updated", "trip_id": "uuid", "seat_no": "1A", "leg_indexes": [0,1,2], "ts": "2026-04-23T15:00:00Z" }
{ "type": "holds.released",    "trip_id": "uuid", "seat_nos": ["1A"],                       "ts": "2026-04-23T15:00:00Z" }
```

Release flows always emit `inventory.updated` **before** `holds.released`
(contract §9.7). Publish failures are logged but never roll back the
transaction — the database is the source of truth.

## Migration Strategy

This crate is engineered to slot in via per-operator sidecar deployment with a
declarative feature flag in TT (`RESERVATION_ENGINE_ENABLED`). See
`docs/TRANSITY_TERMINAL_INTEGRATION.md` §6 and
`docs/TT_HOLDS_ADAPTER_INSTRUCTIONS.md` §6 for the full rollout sequence.

1. **Idle deploy** — engine sidecar runs alongside TT, flag off, 1–3 day soak.
2. **Staging cutover** — flag on against staging DB, run smoke flow.
3. **Production cutover** — flag on per operator during low-traffic window.
4. **Cleanup** — months later, delete the Node atomic-hold path from TT.

Note: traditional dual-write shadow mode is **not** safe here because both
implementations write to the same Postgres tables. See the integration doc.
