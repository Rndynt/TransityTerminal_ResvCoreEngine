-- Reservation engine schema. Idempotent for shared DB use.
-- Mirrors TransityTerminal contract v1.0.

CREATE EXTENSION IF NOT EXISTS pgcrypto;

DO $$ BEGIN
  CREATE TYPE booking_status AS ENUM
    ('pending', 'confirmed', 'checked_in', 'paid', 'cancelled', 'refunded', 'unseated');
EXCEPTION WHEN duplicate_object THEN null; END $$;

DO $$ BEGIN
  CREATE TYPE ticket_status AS ENUM
    ('active', 'cancelled', 'refunded', 'checked_in', 'no_show', 'unseated');
EXCEPTION WHEN duplicate_object THEN null; END $$;

DO $$ BEGIN
  CREATE TYPE trip_status AS ENUM ('scheduled', 'cancelled', 'closed');
EXCEPTION WHEN duplicate_object THEN null; END $$;

CREATE TABLE IF NOT EXISTS seat_inventory (
  id         uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  trip_id    uuid NOT NULL,
  seat_no    text NOT NULL,
  leg_index  integer NOT NULL,
  booked     boolean NOT NULL DEFAULT false,
  hold_ref   text
);

CREATE UNIQUE INDEX IF NOT EXISTS uniq_seat_inv_trip_seat_leg
  ON seat_inventory (trip_id, seat_no, leg_index);
CREATE INDEX IF NOT EXISTS idx_seat_inv_trip_seat ON seat_inventory (trip_id, seat_no);
CREATE INDEX IF NOT EXISTS idx_seat_inv_trip_id   ON seat_inventory (trip_id);
CREATE INDEX IF NOT EXISTS idx_seat_inv_trip_leg  ON seat_inventory (trip_id, leg_index);

CREATE TABLE IF NOT EXISTS seat_holds (
  id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  hold_ref     text NOT NULL UNIQUE,
  trip_id      uuid NOT NULL,
  seat_no      text NOT NULL,
  leg_indexes  integer[] NOT NULL,
  ttl_class    text NOT NULL,
  operator_id  text NOT NULL,
  booking_id   text,
  expires_at   timestamptz NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_seat_holds_trip_id    ON seat_holds (trip_id);
CREATE INDEX IF NOT EXISTS idx_seat_holds_expires_at ON seat_holds (expires_at);
CREATE INDEX IF NOT EXISTS idx_seat_holds_active
  ON seat_holds (trip_id, expires_at) WHERE booking_id IS NULL;
CREATE INDEX IF NOT EXISTS idx_seat_holds_booking_id
  ON seat_holds (booking_id) WHERE booking_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_seat_holds_trip_seat ON seat_holds (trip_id, seat_no);

-- Bookings header (engine touches only status / pending_expires_at).
-- Created if absent for standalone integration tests; harmless on shared DB.
CREATE TABLE IF NOT EXISTS bookings (
  id                  uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  booking_code        text UNIQUE,
  status              booking_status NOT NULL DEFAULT 'pending',
  trip_id             uuid NOT NULL,
  origin_seq          integer NOT NULL,
  destination_seq     integer NOT NULL,
  pending_expires_at  timestamptz,
  idempotency_key     text
);

CREATE UNIQUE INDEX IF NOT EXISTS uniq_bookings_idempotency_key
  ON bookings (idempotency_key) WHERE idempotency_key IS NOT NULL;
