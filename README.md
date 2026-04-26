# Reservation Engine

Rust sidecar implementing the **v1.0 Reservation Engine Contract** for
TransityTerminal. The full source, build instructions, contract spec,
deploy guide, and operational notes live under [`engine/`](./engine):

- [`engine/README.md`](./engine/README.md) — repo layout, build, run, test
- [`engine/docs/TRANSITY_TERMINAL_INTEGRATION.md`](./engine/docs/TRANSITY_TERMINAL_INTEGRATION.md) — wire contract for TT (and any other client)
- [`engine/deploy/`](./engine/deploy) — Docker Compose + `.env.engine.example` for production rollout
- [`engine/migrations/`](./engine/migrations) — engine-owned SQL schema (separate from TT's schema)

CI builds and publishes the production image to `ghcr.io/<owner>/transity-engine`
on every `v*.*.*` tag — see [`.github/workflows/release-engine.yml`](./.github/workflows/release-engine.yml).
