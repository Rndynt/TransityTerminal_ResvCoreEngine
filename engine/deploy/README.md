# Engine deployment — per-operator sidecar

This directory contains the deployment template for running the reservation
engine **side-by-side** with TransityTerminal (TT) on a per-operator basis.

The model: each operator runs its own TT container plus an optional engine
container, both on the same Docker network. The engine is not exposed to the
host network — only TT inside the same compose project can reach it via
`http://engine:8000`. All engine ↔ DB traffic goes directly to the operator's
isolated Postgres instance (Neon, RDS, etc.).

```
[Operator VM]
  ├── transity-terminal-<slug>  (Node, port 5000 → bound to 127.0.0.1:HOST_PORT)
  └── transity-engine-<slug>    (Rust, port 8000 → internal only)
        ↓ shared
        ENGINE_DATABASE_URL = DATABASE_URL  (same Neon project as TT)
```

---

## When to deploy the engine

Read `engine/docs/TRANSITY_TERMINAL_INTEGRATION.md` first. The summary:

| Operator profile | Action |
|---|---|
| < 1.000 bookings/day, < 5 concurrent CSO | **Skip the engine.** Leave `RESERVATION_ENGINE_ENABLED=false`. TT's built-in Node atomic-hold service is correct and proven. |
| > 1.000 bookings/day **or** > 10 concurrent CSO **or** multi-replica TT | Deploy the engine sidecar. Soak idle (flag off) for 1–3 days, then cutover per `engine/docs/TT_HOLDS_ADAPTER_INSTRUCTIONS.md` §6. |

The flag is **declarative per-operator**, not per-request. Switching = edit `.env`
+ restart TT (~2 seconds downtime). Do **not** auto-switch based on load —
state asymmetry (idempotency cache, reaper, event emit) makes mid-traffic
switching unsafe.

---

## One-time setup (control plane)

### 1. Build the engine image

From the engine repo root:

```bash
docker build -t transity-engine:v1.0.0 -f Dockerfile .
docker tag transity-engine:v1.0.0 transity-engine:latest
```

If you have a registry, push it:

```bash
docker tag transity-engine:v1.0.0 ghcr.io/<your-org>/transity-engine:v1.0.0
docker push ghcr.io/<your-org>/transity-engine:v1.0.0
```

Then update `ENGINE_IMAGE_TAG` (and the `image:` field in
`docker-compose.engine.yml`) on each operator host accordingly.

### 2. Ensure the shared Docker network exists on every operator host

This is the same network TT already requires:

```bash
docker network inspect transity-terminals-net >/dev/null 2>&1 \
  || docker network create transity-terminals-net
```

---

## Per-operator setup

### 1. Copy the overlay file into the operator's TT directory

The TT repo already contains `docker-compose.yml`. We add a second compose
file alongside it without modifying the original.

```bash
cp engine/deploy/docker-compose.engine.yml /path/to/TransityTerminal/
cp engine/deploy/.env.engine.example /path/to/TransityTerminal/.env.engine.example
```

### 2. Append engine variables to the operator's `.env`

Open `engine/deploy/.env.engine.example`, copy its contents into the operator's
existing `.env` file, then fill in real values. Variable names match exactly
what the engine binary reads (see `engine/crates/engine-server/src/config.rs`).

Critical fields:
- `RESERVATION_ENGINE_ENABLED` — leave `false` initially. Flip to `true` only
  after the cutover plan in `engine/docs/TT_HOLDS_ADAPTER_INSTRUCTIONS.md` §6.
- `RESERVATION_ENGINE_HMAC_SECRET` — generate fresh per operator:
  ```bash
  openssl rand -hex 32
  ```
  Same value used by both TT and engine. The compose file passes it to both
  services from this single env var.
- `ENGINE_IMAGE_TAG` — match the tag you built/pushed.

**Note**: there is **no** dual-write "shadow mode" because both writers
target the same Postgres. The safe rollout is described in
`engine/docs/TT_HOLDS_ADAPTER_INSTRUCTIONS.md` §6.

### 3. Pull image (if using a registry) and start the stack

```bash
cd /path/to/TransityTerminal
docker compose \
  -f docker-compose.yml \
  -f docker-compose.engine.yml \
  pull engine
docker compose \
  -f docker-compose.yml \
  -f docker-compose.engine.yml \
  up -d
```

Both services start in parallel. The engine runs its migrations idempotently
on boot. There is intentionally no `depends_on` between `terminal` and
`engine` — TT must remain startable even when the engine is down or disabled,
so that a degraded engine never blocks TT itself. When `RESERVATION_ENGINE_ENABLED=true`
and the engine is briefly unavailable during cold start, TT requests will
fail fast with a 5xx — operationally easier to diagnose than a stuck startup.

### 4. Smoke-test the engine from inside the network

```bash
docker compose exec terminal wget -qO- http://engine:8000/api/v1/healthz
# Expected: {"status":"ok",...}
```

### 5. Update the operator's `deploy.sh` (optional but recommended)

Edit `deploy.sh` so future deploys layer both compose files:

```bash
docker compose \
  -f docker-compose.yml \
  -f docker-compose.engine.yml \
  up -d --build --remove-orphans
```

---

## Activating the engine for an operator (after pilot)

When ready to cut over traffic to the engine for an operator whose engine
sidecar has been running idle and healthy for 1–3 days, and whose staging
smoke flow passed:

1. Edit the operator's `.env`:
   ```diff
   - RESERVATION_ENGINE_ENABLED=false
   + RESERVATION_ENGINE_ENABLED=true
   ```
2. Restart TT only (engine continues running):
   ```bash
   docker compose -f docker-compose.yml -f docker-compose.engine.yml restart terminal
   ```
3. Verify in TT logs that hold/release/confirm calls now route to the engine.

To **roll back**, do the inverse: set `false` and restart `terminal`. Hold rows
remain in the same DB tables, so in-flight holds stay valid.

---

## Operational notes

- **Memory**: engine idle ~15 MB RSS, under load ~50–80 MB. Negligible per VM.
- **CPU**: idle ~0%, peak ~5% one core at 100 ops/s.
- **Logs**: `docker compose logs -f engine` (structured JSON via tracing).
- **Reaper**: runs internally every 60s. No host cron needed. The reaper takes
  a Postgres advisory lock so multiple engine replicas (if you ever scale)
  cooperate safely.
- **Schema**: engine uses `CREATE TABLE IF NOT EXISTS` and is a no-op against
  TT's existing `seat_inventory` / `seat_holds` tables.
- **DB connection**: engine opens its own pool (default 10 connections). Add
  this to the operator's Neon connection budget if relevant.
- **Image rebuild cadence**: only when the engine repo releases a new tag.
  Routine TT redeploys leave the engine container untouched.

---

## What lives where

| File | Purpose |
|---|---|
| `engine/Dockerfile` | Multi-stage Rust build → minimal Debian runtime image |
| `engine/.dockerignore` | Keeps the build context small (excludes `target/`, docs, etc.) |
| `engine/deploy/docker-compose.engine.yml` | Overlay that adds `engine` service and augments `terminal` env |
| `engine/deploy/.env.engine.example` | Template for the env vars TT operators must add |
| `engine/deploy/README.md` | This file |
| `engine/docs/TRANSITY_TERMINAL_INTEGRATION.md` | Full migration / endpoint mapping guide |
| `engine/docs/TT_HOLDS_ADAPTER_INSTRUCTIONS.md` | Step-by-step instructions for the agent that integrates the adapter into the TT codebase |
