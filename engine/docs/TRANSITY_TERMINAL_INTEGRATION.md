# TransityTerminal × Reservation Engine — Integration Guide

**Audience**: TransityTerminal backend developers
**Engine version**: v1.0 (Rust, contract `RESERVATION_ENGINE_CONTRACT_1776960925652.md`)
**Status**: Reference — apply changes per the strangler-fig plan in §6

---

## 0. TL;DR

The reservation engine is a **drop-in replacement** for these existing TransityTerminal modules:

| TransityTerminal file (current) | Replaced by engine endpoint |
|---|---|
| `server/modules/bookings/atomicHold.service.ts` → `atomicHold()` | `POST /api/v1/holds` |
| `server/modules/bookings/atomicHold.service.ts` → `releaseHoldByRef()` | `DELETE /api/v1/holds/:hold_ref` |
| `server/modules/bookings/booking.helpers.ts` → `confirmSeatsBooked()` | `POST /api/v1/holds/:hold_ref/confirm` (× N seats) |
| `server/modules/bookings/booking.helpers.ts` → `validateHoldOwnership()` | _no longer needed_ — engine validates atomically inside `confirm` |
| `server/scheduler.ts` → `cleanupExpiredHolds()` | _delete_ — engine's internal reaper does this |
| `server/scheduler.ts` → `cleanupOrphanHoldRefs()` | _delete_ — engine guarantees no orphans by design |
| `server/modules/holds/holds.service.ts` → `releaseSeatHold()` / `releaseHoldByRef()` | `DELETE /api/v1/holds/:hold_ref` |
| Cancel-passenger seat release in `bookings.routes.ts` (PATCH `/api/passengers/:id/cancel`) | `POST /api/v1/cancel-seats` |

What stays in TransityTerminal:
- All **booking** logic: `bookings` table, passenger rows, payments, promos, idempotency, fare quoting, snapshots
- Pending bookings (long-hold `bookingId` linkage uses the engine; row metadata stays in TransityTerminal's `bookings` table)
- WebSocket fan-out to clients (engine emits to Redis Pub/Sub; TransityTerminal listens and re-broadcasts to its Socket.io rooms)
- RBAC, auth, rate limiting, all routes
- `precomputeInventory()` — engine does NOT seed inventory; TransityTerminal still owns trip/leg/layout knowledge

What leaves TransityTerminal:
- Direct writes to `seat_inventory.booked` / `seat_inventory.hold_ref`
- Direct writes / deletes on `seat_holds`
- The advisory-lock reaper loop

---

## 1. Architecture Overview

```
┌─────────────────────────────────────────────┐
│           TransityTerminal (Node)           │
│                                             │
│  Booking flow  ──────► Engine HTTP client   │
│  Passenger cancel ──►  (HMAC-signed)        │
│  CSO seat hold ──────►                      │
│                                             │
│  Inventory precompute ──── direct DB write  │ ──┐
│  WebSocket (Socket.io) ◄── Redis subscriber │   │
└─────────────────────────────────────────────┘   │
                  │                               │
                  │ HTTP/JSON + HMAC              │
                  ▼                               │ Postgres
┌─────────────────────────────────────────────┐   │ (shared schema)
│         Reservation Engine (Rust)           │ ──┤
│                                             │   │
│  POST   /api/v1/holds            (atomic)   │   │
│  DELETE /api/v1/holds/:ref                  │   │
│  POST   /api/v1/holds/:ref/confirm          │   │
│  POST   /api/v1/cancel-seats                │   │
│  GET    /api/v1/inventory/:trip_id          │   │
│  GET    /api/v1/healthz                     │   │
│                                             │   │
│  Internal reaper loop (60s)                 │ ──┘
│  Redis Pub/Sub publisher ─► engine.events.* ──► (TransityTerminal subscribes)
└─────────────────────────────────────────────┘
```

**Both processes share the same Postgres database**. The engine owns writes to `seat_inventory` and `seat_holds`. TransityTerminal owns everything else.

---

## 2. Engine Configuration

Set these env vars on the engine process (already done in this Replit project):

| Var | Required | Example | Notes |
|---|---|---|---|
| `ENGINE_DATABASE_URL` | yes (or `DATABASE_URL`) | `postgresql://user:pass@ep-xxx.neon.tech/db?sslmode=require` | Same DB as TransityTerminal |
| `RESERVATION_ENGINE_HMAC_SECRET` | yes | `035402f8...` (≥16 chars) | TransityTerminal uses the **same** secret to sign requests |
| `REDIS_URL` | optional | `redis://...` | Without it, events are silently dropped (single-instance OK for dev) |
| `PORT` | no (default 8090) | `8000` | |
| `HOST` | no (default 0.0.0.0) | | |
| `HMAC_SKEW_SECS` | no (default 30) | | Max clock skew tolerated |
| `REAPER_INTERVAL_SECS` | no (default 60) | | |
| `DB_MIN_CONN` / `DB_MAX_CONN` | no (10 / 50) | | Per contract §8 |
| `IDEMPOTENCY_MAX` | no (10000) | | LRU cache size |

The engine runs migrations on startup (`engine/migrations/0001_init.sql`). The schema is **identical** to TransityTerminal's `seat_inventory` / `seat_holds` tables — no schema changes needed if TransityTerminal already has them.

---

## 3. HMAC Authentication (REQUIRED for all `/api/v1/*` except `/healthz`)

Every request must include three headers:

| Header | Value |
|---|---|
| `X-Service-Id` | Service name, e.g. `transity-terminal` |
| `X-Timestamp` | Unix **seconds** (not ms), must be within ±30 s of engine clock |
| `X-Signature` | Hex-encoded HMAC-SHA256 (lowercase) |

**Signing string** (exact format, no spaces):
```
{ts}.{METHOD}.{path}.{body_sha256}
```

Where:
- `ts` = the same value sent in `X-Timestamp`
- `METHOD` = uppercase HTTP method (`GET`, `POST`, `DELETE`)
- `path` = request path **only**, no query string, no host (e.g. `/api/v1/holds`)
- `body_sha256` = hex-encoded SHA-256 of the raw request body bytes (use empty string `""` for GET / no-body)

### Reference TypeScript helper (drop into `server/modules/engineClient/sign.ts`)

```typescript
import crypto from 'crypto';

export function signRequest(opts: {
  secret: string;
  method: string;
  path: string;
  body?: string;        // raw JSON string or '' for empty body
  serviceId?: string;
}): Record<string, string> {
  const ts = Math.floor(Date.now() / 1000).toString();  // unix seconds
  const bodySha = crypto
    .createHash('sha256')
    .update(opts.body ?? '')
    .digest('hex');
  const signingString = `${ts}.${opts.method.toUpperCase()}.${opts.path}.${bodySha}`;
  const signature = crypto
    .createHmac('sha256', opts.secret)
    .update(signingString)
    .digest('hex');

  return {
    'X-Service-Id': opts.serviceId ?? 'transity-terminal',
    'X-Timestamp': ts,
    'X-Signature': signature,
  };
}
```

---

## 4. Engine Client (drop into TransityTerminal)

Create `server/modules/engineClient/index.ts`:

```typescript
import { signRequest } from './sign';
import { randomUUID } from 'crypto';

const ENGINE_URL = process.env.RESERVATION_ENGINE_URL || 'http://localhost:8000';
const ENGINE_SECRET = process.env.RESERVATION_ENGINE_HMAC_SECRET!;
const ENGINE_TIMEOUT_MS = parseInt(process.env.RESERVATION_ENGINE_TIMEOUT_MS || '5000', 10);

if (!ENGINE_SECRET) throw new Error('RESERVATION_ENGINE_HMAC_SECRET must be set');

export class EngineError extends Error {
  constructor(public status: number, public code: string, message: string, public body?: any) {
    super(message);
  }
}

async function call<T>(method: 'GET' | 'POST' | 'DELETE', path: string, body?: any, idemKey?: string): Promise<T> {
  const bodyStr = body == null ? '' : JSON.stringify(body);
  const headers: Record<string, string> = {
    'Content-Type': 'application/json',
    ...signRequest({ secret: ENGINE_SECRET, method, path, body: bodyStr }),
  };
  if (idemKey) headers['Idempotency-Key'] = idemKey;

  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), ENGINE_TIMEOUT_MS);

  try {
    const res = await fetch(ENGINE_URL + path, {
      method,
      headers,
      body: bodyStr || undefined,
      signal: ctrl.signal,
    });
    const text = await res.text();
    const json = text ? JSON.parse(text) : null;

    if (!res.ok) {
      throw new EngineError(res.status, json?.code ?? 'UNKNOWN', json?.message ?? res.statusText, json);
    }
    return json as T;
  } finally {
    clearTimeout(timer);
  }
}

// ─── Public API matching contract §3 ──────────────────────────

export interface HoldRequest {
  trip_id: string;
  seat_no: string;
  leg_indexes: number[];
  operator_id: string;
  ttl_class: 'short' | 'long';
}

export interface HoldOk {
  hold_ref: string;            // lowercase UUID
  expires_at: string;          // ISO-8601
}

export interface HoldFail {
  reason: 'INCOMPLETE_INVENTORY' | 'SEAT_CONFLICT';
  conflict_seats: string[];    // always single-element array
}

export const engineClient = {
  /** §3.1 — atomic seat hold */
  hold: (req: HoldRequest, idemKey = randomUUID()) =>
    call<HoldOk>('POST', '/api/v1/holds', req, idemKey),

  /** §3.2 — release a hold by hold_ref. 204 No Content on success, 404 if not found. */
  release: (holdRef: string) =>
    call<void>('DELETE', `/api/v1/holds/${holdRef}`),

  /** §3.3 — confirm hold → booking. Sets booked=true, links booking_id to hold. */
  confirm: (holdRef: string, body: { booking_id: string; operator_id: string }, idemKey = randomUUID()) =>
    call<{ confirmed: true; seat_no: string; leg_indexes: number[] }>(
      'POST', `/api/v1/holds/${holdRef}/confirm`, body, idemKey,
    ),

  /** §3.4 — cancel booked seats, returns them to inventory pool */
  cancelSeats: (req: { trip_id: string; seats: { seat_no: string; leg_indexes: number[] }[]; reason: string }, idemKey = randomUUID()) =>
    call<{ released: number }>('POST', '/api/v1/cancel-seats', req, idemKey),

  /** §3.5 — read-only snapshot of seat inventory for a trip */
  inventory: (tripId: string) =>
    call<{ trip_id: string; seats: Array<{ seat_no: string; leg_index: number; booked: boolean; hold_ref: string | null }> }>(
      'GET', `/api/v1/inventory/${tripId}`,
    ),

  health: () => call<{ service: string; status: string }>('GET', '/api/v1/healthz'),
};
```

Add `RESERVATION_ENGINE_URL` and `RESERVATION_ENGINE_HMAC_SECRET` to TransityTerminal's env (read in `server/config.ts`).

---

## 5. Endpoint-by-Endpoint Mapping

### 5.1 Hold a seat

**Was** (`atomicHold.service.ts:27` `atomicHold()`):
```ts
const result = await this.atomicHoldService.atomicHold({
  tripId, seatNo, legIndexes, operatorId, ttlClass: 'short',
});
```

**Becomes**:
```ts
try {
  const { hold_ref, expires_at } = await engineClient.hold({
    trip_id: tripId,
    seat_no: seatNo,
    leg_indexes: legIndexes,
    operator_id: operatorId,
    ttl_class: 'short',
  });
  return { ok: true, holdRef: hold_ref, expiresAt: new Date(expires_at).getTime(), ownedByYou: true };
} catch (e) {
  if (e instanceof EngineError && e.status === 409) {
    return { ok: false, reason: e.body?.reason, conflictSeats: e.body?.conflict_seats };
  }
  throw e;
}
```

**Engine behavior** (contract §3.1):
- HTTP 201 on success with `{ hold_ref, expires_at }`
- HTTP 409 on `SEAT_CONFLICT` with `{ reason, conflict_seats: [seat_no] }`
- HTTP 422 on `INCOMPLETE_INVENTORY` with `{ reason, conflict_seats: [seat_no] }`
- TTL: `short` = 300 s, `long` = 1800 s — **fixed by contract**

### 5.2 Release a hold

**Was** (`atomicHold.service.ts:109` or `holds.service.ts:125`):
```ts
await this.atomicHoldService.releaseHoldByRef(holdRef);
```

**Becomes**:
```ts
try {
  await engineClient.release(holdRef);
} catch (e) {
  if (!(e instanceof EngineError && e.status === 404)) throw e;
  // 404 = already gone; treat as no-op
}
```

The engine emits `inventory.updated` **before** `holds.released` to Redis (contract §9.7). TransityTerminal's WebSocket layer should preserve this order when forwarding to clients.

### 5.3 Confirm a booking — replaces `confirmSeatsBooked()`

**Was** (`booking.helpers.ts:181`): one giant transaction that locked rows, validated holds, set `booked=true`, and deleted holds. Called once per booking with N seats × M legs.

**Becomes** (in `bookings.service.ts → createBooking()`):

```ts
// Inside the booking transaction, after inserting bookingsTable + passengers:
const seatHoldRefs = await getSeatHoldRefsForOperator(tx, bookingData.tripId, seatNos, operatorId);
// (Look up the hold_refs the operator created earlier via createHold)

// IMPORTANT: confirm via engine *outside* the DB tx, because the engine
// runs its own DB tx internally. If the engine call fails, roll back the
// booking by throwing.
await tx.commit();  // commit the booking row first

try {
  for (const { holdRef, seatNo } of seatHoldRefs) {
    await engineClient.confirm(holdRef, {
      booking_id: newBooking.id,
      operator_id: operatorId,
    }, /* idemKey = */ `${idempotencyKey}:${seatNo}`);
  }
} catch (e) {
  // Compensating action: cancel the booking we just wrote.
  await db.update(bookingsTable).set({ status: 'cancelled' }).where(eq(bookingsTable.id, newBooking.id));
  throw e;
}
```

> **Recommended pattern instead**: introduce a `pending` booking status that becomes `paid` only after all confirms succeed. This is what TransityTerminal already does for `createPendingBooking()` — extend the same pattern to all bookings to avoid compensating writes.

The engine's `confirm` is idempotent per `Idempotency-Key` for 24 hours.

### 5.4 Cancel passenger seats — replaces inline seat release in `bookings.routes.ts:115`

**Was**:
```ts
await tx.update(seatInventory)
  .set({ booked: false, holdRef: null })
  .where(and(
    eq(seatInventory.tripId, booking.tripId),
    eq(seatInventory.seatNo, passengerRow.seatNo),
    inArray(seatInventory.legIndex, legIndexes)
  ));
```

**Becomes**:
```ts
// After updating the passenger / booking status rows in TransityTerminal's tx:
await engineClient.cancelSeats({
  trip_id: booking.tripId,
  seats: [{ seat_no: passengerRow.seatNo, leg_indexes: legIndexes }],
  reason: reason.trim(),
});
```

This works for all cancel paths: `unseatPassenger`, `unseatAllPassengers`, `cancelTicket`, `releasePendingBooking`, `cleanupExpiredPendingBookings`.

### 5.5 Inventory snapshot (optional, useful for debug pages)

```ts
const snap = await engineClient.inventory(tripId);
// snap.seats: [{ seat_no, leg_index, booked, hold_ref }, ...]
```

---

## 6. Migration Strategy (strangler-fig, contract §12)

Do this over **3 deploys**, not one big-bang. Engine and Node code coexist throughout.

### Phase 1 — Shadow (no behavior change)

1. Deploy engine pointing at TransityTerminal's Postgres.
2. Add the engine client (§4) but keep all existing Node code as the source of truth.
3. After every Node `atomicHold()` / `releaseHoldByRef()` / `confirmSeatsBooked()` call, **also** call the corresponding engine endpoint and **diff the result asynchronously** (log mismatches, do not throw):

```ts
const nodeResult = await this.atomicHoldService.atomicHold(req);
queueMicrotask(async () => {
  try {
    const engineResult = await engineClient.hold(...);
    diffAndLog('hold', nodeResult, engineResult);
  } catch (e) { logShadowError(e); }
});
return nodeResult;
```

Run for 24–72 h, watch logs for mismatches. **Expect zero** — if any appear, root-cause before proceeding.

### Phase 2 — Canary cutover

1. Add a feature flag `RESERVATION_ENGINE_ENABLED=true` (gate per-route or per-operator).
2. When the flag is on, call the engine **as the primary**, skip the Node version.
3. Keep the scheduler's `cleanupExpiredHolds()` and `cleanupOrphanHoldRefs()` **disabled** when the flag is on (engine has its own reaper).
4. Roll out to 5% of traffic → 50% → 100%.

### Phase 3 — Cleanup

1. Delete `atomicHold.service.ts`, the seat-related parts of `holds.service.ts`, and the cleanup methods in `scheduler.ts`.
2. Remove the feature flag.
3. Engine becomes the only writer to `seat_inventory` and `seat_holds`.

---

## 7. WebSocket / Realtime Events

The engine publishes to Redis Pub/Sub on these channels (contract §9):

| Channel | Payload | When |
|---|---|---|
| `engine.inventory.updated` | `{ trip_id, seat_no, leg_indexes }` | After every successful `hold` / `release` / `confirm` / `cancel` / reaper-release |
| `engine.holds.released` | `{ trip_id, seat_nos: string[] }` | After release / reaper-release / pending-cancel |
| `engine.holds.confirmed` | `{ trip_id, seat_no, hold_ref, booking_id }` | After confirm |
| `engine.bookings.cancelled` | `{ trip_id, seat_nos, reason }` | After cancel-seats |

**Order is guaranteed**: `inventory.updated` → `holds.released` for every release operation (contract §9.7).

### TransityTerminal subscriber (drop into `server/realtime/engineSubscriber.ts`)

```typescript
import { createClient } from 'redis';
import { webSocketService } from './ws';

export async function startEngineEventSubscriber() {
  const sub = createClient({ url: process.env.REDIS_URL });
  await sub.connect();

  await sub.pSubscribe('engine.*', (raw, channel) => {
    try {
      const msg = JSON.parse(raw);
      switch (channel) {
        case 'engine.inventory.updated':
          webSocketService.emitInventoryUpdated(msg.trip_id, msg.seat_no, msg.leg_indexes);
          break;
        case 'engine.holds.released':
          webSocketService.emitHoldsReleased(msg.trip_id, msg.seat_nos);
          break;
        case 'engine.holds.confirmed':
          // optional: forward as-is, or trigger UI refresh
          break;
        case 'engine.bookings.cancelled':
          webSocketService.emitHoldsReleased(msg.trip_id, msg.seat_nos);
          break;
      }
    } catch (e) { console.error('[engineSub] bad message', e); }
  });
}
```

Call `startEngineEventSubscriber()` in `server/index.ts` startup.

**Stop emitting WS events directly from `atomicHold.service.ts` / `holds.service.ts`** once the cutover is done — the engine is the single source of truth for these events.

---

## 8. Error Code Mapping

| Engine HTTP | Engine `code` | TransityTerminal user-visible mapping |
|---|---|---|
| 201 | — | success |
| 204 | — | success (release) |
| 400 | `BAD_REQUEST` | 400 — validation error |
| 401 | `INVALID_SIGNATURE` / `STALE_TIMESTAMP` | 500 — internal (config bug) |
| 404 | `HOLD_NOT_FOUND` | for release: treat as no-op; for confirm: 410 GONE |
| 409 | `SEAT_CONFLICT` | 409 — `Kursi sedang dipegang oleh agen lain` |
| 409 | `IDEMPOTENCY_BODY_MISMATCH` | 500 — internal (caller bug, key reused with different body) |
| 410 | `HOLD_EXPIRED` | 410 — `Hold telah kadaluarsa, silakan hold ulang` |
| 422 | `INCOMPLETE_INVENTORY` | 422 — `Inventori belum diinisialisasi, jalankan precompute` |
| 5xx | `INTERNAL` | 500 — generic |

---

## 9. Backward-Compat Checklist for TransityTerminal Devs

Before merging the cutover PR:

- [ ] `RESERVATION_ENGINE_URL` and `RESERVATION_ENGINE_HMAC_SECRET` set in all envs (dev, staging, prod)
- [ ] Engine binary deployed and `/api/v1/healthz` returns 200 from TransityTerminal's network
- [ ] `engineClient` wired into all 5 call sites (hold, release, confirm, cancel-seats, optional: inventory)
- [ ] Redis subscriber (`startEngineEventSubscriber`) running and forwarding events to Socket.io
- [ ] `scheduler.ts` `cleanupExpiredHolds()` + `cleanupOrphanHoldRefs()` disabled (engine reaper handles both)
- [ ] Direct `seat_inventory` / `seat_holds` writes in non-engine code: zero (grep `seat_inventory\\.` and `seat_holds\\.` — should only return reads after cutover)
- [ ] Feature flag `RESERVATION_ENGINE_ENABLED` rolled out gradually
- [ ] Shadow-diff log shows zero mismatches over the canary window
- [ ] `precomputeInventory()` continues to seed rows; engine reads but does not seed

---

## 10. Operational Notes

- **Database connections**: engine uses a pool of 10–50 (configurable). Add this to your Neon plan budget.
- **Engine restarts**: safe at any time. In-flight HTTP requests will fail with connection-reset; caller should retry idempotently. The internal reaper resumes automatically.
- **Idempotency**: every write endpoint accepts `Idempotency-Key`. Replays within 24 h with the same body return the cached response; same key + different body returns 409. Use UUIDv4 from TransityTerminal.
- **Clock skew**: engine rejects requests with `|ts_now - X-Timestamp| > 30 s`. NTP your servers.
- **Schema migration**: engine ships its own migrations (`engine/migrations/0001_init.sql`) and runs them on startup with `IF NOT EXISTS` guards. Safe to point at a DB that already has the tables — engine will skip.
- **Multi-instance engine**: safe. Reaper uses `pg_try_advisory_lock(hashtext('reservation_reaper'))` so only one instance reaps at a time. HTTP traffic load-balances normally.

---

## 11. Quick smoke-test (run after cutover)

```bash
# 1. health
curl http://engine:8000/api/v1/healthz

# 2. (from TransityTerminal Node REPL)
const { engineClient } = require('./server/modules/engineClient');

// hold
const { hold_ref } = await engineClient.hold({
  trip_id: 'real-trip-uuid',
  seat_no: '1A',
  leg_indexes: [1, 2],
  operator_id: 'test-operator',
  ttl_class: 'short',
});

// inventory should now show hold_ref set
console.log(await engineClient.inventory('real-trip-uuid'));

// release
await engineClient.release(hold_ref);
```

---

## 12. Reference

- Full contract: `attached_assets/RESERVATION_ENGINE_CONTRACT_1776960925652.md`
- Engine source: `engine/crates/engine-core/` (domain) + `engine/crates/engine-server/` (HTTP)
- Engine README: `engine/README.md`
- Load test: `engine/loadtest/` — see §13 below

---

## 13. Performance & Load Testing

A reproducible load test lives at `engine/loadtest/`. It exercises hold → confirm and hold → release flows against a live engine over HTTP.

Run from the engine repo root:

```bash
# Required: engine running, ENGINE_DATABASE_URL set
SEATS=200 CONCURRENCY=64 OPS=5000 \
  cargo run --release -p loadtest -- \
    --engine-url http://localhost:8000 \
    --hmac-secret "$RESERVATION_ENGINE_HMAC_SECRET"
```

Output:
```
== Reservation Engine Load Test ==
seats=200  concurrency=64  ops=5000  scenario=hold-release
seeded trip 0190xxxx... with 200 seats × 1 leg

[done] 5000 ops in 4.31s = 1160 req/s
hold:    p50=12ms  p95=34ms  p99=58ms   ok=4823  conflict=177
release: p50=8ms   p95=22ms  p99=41ms   ok=4823
errors:  0
```

Tweak `--scenario hold-confirm` to test confirm path, or `--seats 50 --concurrency 200` to force heavy contention.
