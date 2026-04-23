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
60s reaper task). Migrations in `engine/migrations/`. Full parity test suite
in `engine/crates/engine-core/tests/parity.rs`. See `engine/README.md`.

## Key Commands

- `pnpm run typecheck` — full typecheck across all packages
- `pnpm run build` — typecheck + build all packages
- `pnpm --filter @workspace/api-spec run codegen` — regenerate API hooks and Zod schemas from OpenAPI spec
- `pnpm --filter @workspace/db run push` — push DB schema changes (dev only)
- `pnpm --filter @workspace/api-server run dev` — run API server locally

See the `pnpm-workspace` skill for workspace structure, TypeScript setup, and package details.
