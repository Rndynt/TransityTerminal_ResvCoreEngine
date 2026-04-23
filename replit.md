# Workspace

## Overview

pnpm workspace monorepo using TypeScript. Each package manages its own dependencies.

## Stack

- **Monorepo tool**: pnpm workspaces (Node side) + Cargo workspace (Rust side, in `engine/`)
- **Node.js version**: 24
- **Package manager**: pnpm
- **TypeScript version**: 5.9
- **API framework**: Express 5
- **Database**: PostgreSQL + Drizzle ORM (Node) / sqlx (Rust)
- **Validation**: Zod (`zod/v4`), `drizzle-zod`
- **API codegen**: Orval (from OpenAPI spec)
- **Build**: esbuild (CJS bundle)
- **Rust toolchain**: 1.88 stable (axum, sqlx, deadpool-redis)

## `engine/` — Reservation Engine (Rust)

1:1 port of TransityTerminal reservation engine per the v1.0 contract
(`attached_assets/RESERVATION_ENGINE_CONTRACT_*.md`). Cargo workspace with
`engine-core` (domain logic) + `engine-server` (Axum HTTP, HMAC, idempotency,
60s reaper task) + `loadtest` (HTTP load test bin). Migrations in
`engine/migrations/`. Full parity test suite in
`engine/crates/engine-core/tests/parity.rs`. See `engine/README.md`.

**Integration**: `engine/docs/TRANSITY_TERMINAL_INTEGRATION.md` — file-by-file
migration guide for the TransityTerminal Node app (engine client, HMAC signer,
WebSocket subscriber, strangler-fig rollout, error mapping).
HMAC signs `{ts_seconds}.{METHOD}.{path}.{sha256(body)}`.

**Load test**: `cargo run -p loadtest -- --scenario hold-release|hold-confirm`.
Reads/writes synthetic seats (prefix `LT-<run>-`) on a discovered trip; reports
p50/p95/p99 + conflict counts. Sanity-checked against live engine, 0 errors,
correct conflict resolution under contention.

**Deployment** (per-operator sidecar): `engine/Dockerfile` + `engine/deploy/`
(docker-compose overlay, `.env` template, README). Each operator runs
TT + engine on the same Docker network sharing one Postgres DB. Engine is
internal-only (`http://engine:8000`). Activation is declarative via
`RESERVATION_ENGINE_ENABLED=true|false` per operator's `.env` (requires TT
restart). Small operators stay on pure Node and don't deploy the engine.

**TT-side adapter**: `engine/docs/TT_HOLDS_ADAPTER_INSTRUCTIONS.md` — prescriptive
change set (not auto-applied) for the agent that will modify the
TransityTerminal repo to introduce `holdsAdapter.ts`, the feature-flag
dispatcher between Node `AtomicHoldService` and the engine, plus shadow-mode
diff logger and scheduler reaper guard.

## Key Commands

- `pnpm run typecheck` — full typecheck across all packages
- `pnpm run build` — typecheck + build all packages
- `pnpm --filter @workspace/api-spec run codegen` — regenerate API hooks and Zod schemas from OpenAPI spec
- `pnpm --filter @workspace/db run push` — push DB schema changes (dev only)
- `pnpm --filter @workspace/api-server run dev` — run API server locally

See the `pnpm-workspace` skill for workspace structure, TypeScript setup, and package details.
