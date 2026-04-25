-- P3 §10.11: document why `seat_holds.booking_id` is `text` instead of
-- the `uuid` used by sibling booking-related columns elsewhere in the
-- TransityTerminal schema.
--
-- The engine wire contract treats `booking_id` as an opaque caller-
-- supplied identifier (see `engine/docs/RESERVATION_ENGINE_CONTRACT.md`
-- §6.3). Multi-operator deployments may use prefix schemes
-- (e.g. `BK-2026-04-001`) or non-UUID namespaces handed in from billing
-- systems, so the column type stays `text` to keep the engine ledger
-- format-agnostic. TransityTerminal stores the same column as `text`
-- in `seat_holds` for parity, even though its own `bookings.id`
-- and FK columns are `uuid`.
--
-- The comment is stored in pg_catalog so `psql \d+ seat_holds` and
-- BI tools surface the rationale alongside the schema.

COMMENT ON COLUMN seat_holds.booking_id IS
  'Caller-supplied opaque booking identifier. Stored as text (not uuid) '
  'because the engine wire contract is format-agnostic — operators may '
  'use UUIDs, prefixed numeric schemes, or external billing IDs. NULL '
  'while the hold is active; set by confirm_booking and retained as '
  'audit trail until the reaper purges per CONFIRMED_HOLDS_RETENTION_DAYS.';

COMMENT ON COLUMN seat_holds.hold_ref IS
  'Engine-generated short identifier returned to the caller. UNIQUE '
  'across the table; used by /v1/holds/:hold_ref/{confirm,DELETE}.';
