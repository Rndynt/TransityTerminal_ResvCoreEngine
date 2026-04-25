-- P1 §10.3: durable idempotency store backed by Postgres.
--
-- Replaces the in-process Moka cache so cached responses survive engine
-- restart (rolling deploy / crash / OOM kill / sleep). Same 24h TTL as
-- before; planner-friendly index on expires_at lets the periodic sweep
-- evict expired entries without a sequential scan.
--
-- Schema notes:
--  - `key` is the raw `Idempotency-Key` header value sent by the caller.
--  - `body_hash` is sha256(request body) so a same-key/different-body retry
--    can be detected and rejected with 409.
--  - `response_body` stores the engine's exact response bytes so a replay
--    returns byte-identical content (header `x-idempotent-replayed: true`
--    is added at replay time by the middleware).
--  - `expires_at` is set to `created_at + 24h` by the writer.

CREATE TABLE IF NOT EXISTS engine_idempotency_cache (
  key            text        PRIMARY KEY,
  body_hash      text        NOT NULL,
  status_code    integer     NOT NULL,
  response_body  bytea       NOT NULL,
  created_at     timestamptz NOT NULL DEFAULT now(),
  expires_at     timestamptz NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_engine_idempotency_expires_at
  ON engine_idempotency_cache (expires_at);
