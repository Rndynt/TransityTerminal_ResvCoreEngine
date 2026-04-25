-- P3 §10.10: schema version marker so engine can fail fast on boot
-- if the deployed binary expects a schema version different from the
-- one applied to the database. This is the engine's analogue of an
-- application "schema_migrations" table — separate from sqlx's own
-- _sqlx_migrations to avoid coupling the integrity check to the
-- migration runner's internals.
--
-- The version stored here is the *engine schema contract version*,
-- bumped whenever the engine adds a hard requirement on a new
-- column/index/table. It does NOT track every migration — only ones
-- that change the wire-visible schema contract.

CREATE TABLE IF NOT EXISTS engine_schema_meta (
  key        text        PRIMARY KEY,
  value      text        NOT NULL,
  updated_at timestamptz NOT NULL DEFAULT now()
);

INSERT INTO engine_schema_meta (key, value)
VALUES ('engine_schema_version', '1')
ON CONFLICT (key) DO UPDATE
  SET value      = EXCLUDED.value,
      updated_at = now();

COMMENT ON TABLE  engine_schema_meta IS
  'Engine schema contract metadata. Updated by hand-written migrations '
  'whenever a new engine release requires a stricter schema; checked '
  'at boot in main.rs. Distinct from _sqlx_migrations.';

COMMENT ON COLUMN engine_schema_meta.key IS
  'Metadata key — e.g. engine_schema_version, last_breaking_migration.';
