# TransityTerminal — `holdsAdapter` integration instructions

> **Audience**: an engineer (human or another Replit Agent) who will modify the
> **TransityTerminal repo** to introduce a feature-flagged path between the
> existing Node atomic-hold service and the new Rust reservation engine.
>
> **This document does not modify TT directly.** It is a prescriptive,
> copy-pasteable change set you apply inside the TT codebase. After applying,
> no behavior changes until `RESERVATION_ENGINE_ENABLED=true` is set in an
> operator's `.env`.
>
> **Pre-reqs**: you have read `TRANSITY_TERMINAL_INTEGRATION.md` (especially
> §3 HMAC, §4 engine client, §5 endpoint mapping, §6 strangler-fig phases).

---

## 0. Goals & non-goals

**Goals**
- Introduce a single adapter layer in TT that routes 4 seat operations
  (`hold`, `release`, `confirm`, `cancelSeats`) to either the existing Node
  service or the new engine, based on `process.env.RESERVATION_ENGINE_ENABLED`.
- Default value `false` → **zero behavior change** for any operator that
  doesn't opt in.
- Keep the change reversible: deleting the adapter and switching call sites
  back to the Node service should restore the previous behavior verbatim.

**Non-goals**
- Do **not** change `seatInventoryService`, `tripsService`, or the seatmap
  query path. Those stay 100% in TT.
- Do **not** introduce auto-switching, load-based routing, or traffic
  splitting. The flag is declarative per-operator.
- Do **not** add a circuit breaker in this round. Operational simplicity
  first; add it later only if production data justifies it.
- Do **not** implement a "shadow mode" that dual-writes to the engine while
  also calling the Node service — both writers target the SAME shared
  Postgres (`seat_inventory`, `seat_holds`), and dual writes will produce
  duplicate holds, racing reapers, and false diff signals. See §6 for the
  safer rollout pattern.

---

## 1. Files you will create

```
server/modules/holds/
  ├── engineClient.ts        ← HTTP client + HMAC signer (from integration doc §3, §4)
  ├── engineClient.types.ts  ← Request/response types
  └── holdsAdapter.ts        ← Feature-flag dispatcher (this is the heart)
```

## 2. Files you will modify

| File | Change |
|---|---|
| `server/modules/bookings/bookings.service.ts` | Replace direct calls to `atomicHoldService.atomicHold` / `releaseHoldByRef` with calls to `holdsAdapter`. |
| `server/modules/bookings/booking.helpers.ts` | `confirmSeatsBooked()` either delegates to `holdsAdapter.confirm()` (when flag on) or keeps existing implementation. Extract the inline cancel-seat SQL from `bookings.routes.ts:115` into a helper named `releaseConfirmedSeatsLocal()` and call it from the adapter's flag-off branch. |
| `server/modules/bookings/bookings.routes.ts` | The cancel-seat handler (around line 115) calls `holdsAdapter.cancelSeats()` instead of inline SQL. |
| `server/modules/holds/holds.routes.ts` (if exists) | Public POST/DELETE hold endpoints route through the adapter. |
| `server/scheduler.ts` | When `RESERVATION_ENGINE_ENABLED=true`, **skip** the local reaper (`reapExpiredHolds` and `cleanupOrphanHoldRefs`) — the engine owns reaping. |
| `package.json` | No new runtime deps. (`fetch` is built into Node 20+. `crypto` is stdlib.) |
| `.env.example` | Document the new env vars (`RESERVATION_ENGINE_ENABLED`, `RESERVATION_ENGINE_URL`, `RESERVATION_ENGINE_HMAC_SECRET`). |
| `replit.md` | Add a paragraph about the adapter and the flag semantics. |

---

## 3. Step-by-step

### Step 1 — Create `server/modules/holds/engineClient.ts`

Copy verbatim from `engine/docs/TRANSITY_TERMINAL_INTEGRATION.md` §4. Make
sure:
- Timestamp is **unix seconds** (`Math.floor(Date.now()/1000)`), not ms.
- The base URL comes from `process.env.RESERVATION_ENGINE_URL`
  (compose default: `http://engine:8000`).
- HMAC secret comes from `process.env.RESERVATION_ENGINE_HMAC_SECRET`.
- Throws a typed `EngineError` with `.status`, `.code`, `.message`.

**API contract reminder** — these are the actual engine payloads (verified
against `engine/crates/engine-server/src/routes.rs`):

| Operation | Method + Path | Body shape |
|---|---|---|
| Hold | `POST /api/v1/holds` | `{ trip_id, seat_no, leg_indexes, operator_id, ttl_class }` |
| Release | `DELETE /api/v1/holds/:hold_ref` | (none) |
| Confirm | `POST /api/v1/holds/:hold_ref/confirm` | `{ booking_id }` |
| Cancel one seat | `POST /api/v1/cancel-seats` | `{ trip_id, seat_no, leg_indexes }` |
| Inventory | `GET /api/v1/inventory/:trip_id` | (none) |

**Important**: `cancel-seats` is **per-seat**, not batched. If TT cancels a
booking with N passengers, the adapter must call the endpoint N times (one
call per seat).

### Step 2 — Create `server/modules/holds/holdsAdapter.ts`

```ts
import { engineClient, EngineError } from "./engineClient";
import {
  AtomicHoldService,
  type SeatHoldRequest,
  type AtomicHoldResult,
} from "@modules/bookings/atomicHold.service";

const isEngineEnabled = () =>
  (process.env.RESERVATION_ENGINE_ENABLED ?? "false").toLowerCase() === "true";

export class HoldsAdapter {
  constructor(private nodeService: AtomicHoldService) {}

  // ────────────────────────────────────────────────────────────
  // HOLD
  // ────────────────────────────────────────────────────────────
  async hold(req: SeatHoldRequest): Promise<AtomicHoldResult> {
    if (!isEngineEnabled()) {
      return this.nodeService.atomicHold(req);
    }
    try {
      const r = await engineClient.hold({
        trip_id: req.tripId,
        seat_no: req.seatNo,
        leg_indexes: req.legIndexes,
        operator_id: req.operatorId,
        ttl_class: req.ttlClass,
      });
      return {
        success: true,
        holdRef: r.hold_ref,
        expiresAt: new Date(r.expires_at),
      };
    } catch (e) {
      if (e instanceof EngineError && e.code === "SEAT_CONFLICT") {
        return {
          success: false,
          reason: "SEAT_CONFLICT",
          conflictSeats: e.details?.conflict_seats ?? [req.seatNo],
        };
      }
      if (e instanceof EngineError && e.code === "INCOMPLETE_INVENTORY") {
        return {
          success: false,
          reason: "INCOMPLETE_INVENTORY",
          conflictSeats: [req.seatNo],
        };
      }
      throw e;
    }
  }

  // ────────────────────────────────────────────────────────────
  // RELEASE
  // ────────────────────────────────────────────────────────────
  async release(holdRef: string): Promise<void> {
    if (!isEngineEnabled()) {
      await this.nodeService.releaseHoldByRef(holdRef);
      return;
    }
    await engineClient.release(holdRef);
  }

  // ────────────────────────────────────────────────────────────
  // CONFIRM (note: engine is per-hold_ref; TT may pass N hold refs)
  // ────────────────────────────────────────────────────────────
  async confirm(holdRefs: string[], bookingId: string): Promise<void> {
    if (!isEngineEnabled()) {
      const { confirmSeatsBooked } = await import(
        "@modules/bookings/booking.helpers"
      );
      await confirmSeatsBooked(holdRefs, bookingId);
      return;
    }
    // Engine confirm is per-hold_ref. Iterate. If any fails partway, throw —
    // the booking row should remain in `pending` status until the caller
    // (bookings.service.ts) decides how to roll back.
    for (const ref of holdRefs) {
      await engineClient.confirm(ref, { booking_id: bookingId });
    }
  }

  // ────────────────────────────────────────────────────────────
  // CANCEL SEATS — release CONFIRMED seats
  // Engine endpoint is PER SEAT, not batched. Iterate.
  // ────────────────────────────────────────────────────────────
  async cancelSeats(input: {
    tripId: string;
    seats: Array<{ seatNo: string; legIndexes: number[] }>;
    operatorId: string;
  }): Promise<void> {
    if (!isEngineEnabled()) {
      const { releaseConfirmedSeatsLocal } = await import(
        "@modules/bookings/booking.helpers"
      );
      await releaseConfirmedSeatsLocal(input);
      return;
    }
    for (const seat of input.seats) {
      await engineClient.cancelSeats({
        trip_id: input.tripId,
        seat_no: seat.seatNo,
        leg_indexes: seat.legIndexes,
      });
    }
  }
}
```

### Step 3 — Wire the adapter into call sites

`bookings.service.ts` (and any other place that currently does
`new AtomicHoldService(storage).atomicHold(...)`):

```diff
- import { AtomicHoldService } from "./atomicHold.service";
+ import { AtomicHoldService } from "./atomicHold.service";
+ import { HoldsAdapter } from "@modules/holds/holdsAdapter";

  // …
- private atomicHold = new AtomicHoldService(this.storage);
+ private holds = new HoldsAdapter(new AtomicHoldService(this.storage));

- const r = await this.atomicHold.atomicHold({ ... });
+ const r = await this.holds.hold({ ... });
```

Same pattern for `release`, `confirm`, `cancelSeats`.

### Step 4 — Disable the local reaper when the engine owns it

`server/scheduler.ts`:

```ts
const engineEnabled =
  (process.env.RESERVATION_ENGINE_ENABLED ?? "false").toLowerCase() === "true";

if (engineEnabled) {
  console.log("[scheduler] engine enabled — local hold reaper disabled");
} else {
  setInterval(reapExpiredHolds, 60_000);
  setInterval(cleanupOrphanHoldRefs, 5 * 60_000);
}
```

This is critical. If both reapers run, you get duplicate `holds.released`
events and possible races on the same expired row.

### Step 5 — Update `.env.example`

Append:

```
# Reservation Engine sidecar (see engine/deploy/.env.engine.example)
RESERVATION_ENGINE_ENABLED=false
RESERVATION_ENGINE_URL=http://engine:8000
RESERVATION_ENGINE_HMAC_SECRET=
```

### Step 6 — Update `replit.md`

Add a short section:

```markdown
## Reservation engine (optional sidecar)

For high-volume operators, hold/release/confirm/cancel-seats can be routed to
the Rust reservation engine sidecar. Toggle via `RESERVATION_ENGINE_ENABLED`
(declarative per-operator, requires TT restart). When disabled, TT uses the
built-in `AtomicHoldService` and the local scheduler reaper. See
`engine/docs/TRANSITY_TERMINAL_INTEGRATION.md` and
`engine/deploy/README.md` for the full deployment guide.
```

---

## 4. Verification checklist (run in order, do not skip)

For each step, the test must pass before proceeding to the next.

- [ ] **Flag off (default), no engine running**: existing TT test suite passes
      unchanged. The 16 atomic-hold parity tests must all pass.
- [ ] **Flag off, engine running**: same as above. Engine container is idle;
      no traffic should reach it. Verify with `docker compose logs engine`
      (no `POST /api/v1/holds` lines).
- [ ] **Flag on, single dev seat**: enable flag in a staging env, restart TT,
      hold one seat via the UI. Verify the engine logs the hold and TT
      proceeds normally. Verify TT's local reaper is silent (`grep -i reap`
      in TT logs returns nothing new).
- [ ] **Flag on, full smoke flow**: hold → confirm → release → cancel-seats,
      each operation, in staging. All four operations succeed and `seat_holds`
      / `seat_inventory` end states match expectations.
- [ ] **Rollback drill**: set flag back to `false`, restart TT. In-flight
      holds (created via engine) remain valid because they live in the same
      `seat_holds` table. TT can release/confirm them via the Node path
      because the data is identical.

---

## 5. What you must NOT do

- ❌ Do not modify `engine-core` or `engine-server` from the TT side.
  The engine is a separate codebase with its own contract (v1.0).
- ❌ Do not add a circuit breaker in this iteration. Add it only if production
  metrics show engine unavailability events worth recovering from.
- ❌ Do not split traffic 50/50 or do load-based routing. State asymmetry
  (idempotency cache, reaper, event emit) makes mid-traffic switching unsafe.
- ❌ Do not implement dual-write shadow mode. Both Node and engine target the
  same `seat_holds` table; calling both for the same operation will create
  duplicate holds, race the reapers, and pollute event streams.
- ❌ Do not call `seatInventoryService.precomputeInventory` from the engine
  side. That logic stays 100% in TT — the engine has no knowledge of layouts,
  vehicles, or trip legs.
- ❌ Do not change the seatmap endpoint to call the engine. Seatmap composes
  data from 5+ TT-owned tables. Adding an HTTP hop makes it slower with zero
  benefit.

---

## 6. Rollout sequence (safe pattern for shared-DB topology)

The standard "shadow mode" (call both, diff the results) is **not safe** when
both implementations write to the same Postgres. Instead:

1. **Apply this changeset to TT.** Default flag off. Deploy to staging.
   Existing tests pass unchanged.
2. **Pick one pilot operator.** Build/push the engine image. Add the engine
   sidecar to that operator's compose stack. Flag stays `false` — engine runs
   idle, healthchecks green, no traffic.
3. **Soak for 1–3 days** with the engine container running idle next to TT
   to confirm the deployment topology, healthcheck, log shipping, and resource
   footprint are stable.
4. **Stage cutover in dev/staging.** Point a non-prod copy of the operator's
   stack at a non-prod DB. Set flag `true`. Run TT's existing test suite plus
   the smoke flow in §4. Reproduce a few real booking scenarios manually.
5. **Production cutover for the pilot.** Choose a low-traffic window. Set
   `RESERVATION_ENGINE_ENABLED=true` in the operator's `.env`. Restart TT
   (~2s downtime). Monitor for 7 days: error rate, p95 latency, `seat_holds`
   row count drift, double-book reports.
6. **Roll out to other large operators** that need the engine. Small operators
   stay on the Node path with `RESERVATION_ENGINE_ENABLED=false` and don't
   need the engine container at all.
7. **Phase-3 cleanup** (optional, months later): once all relevant operators
   are stable on the engine, the Node `atomicHold.service.ts` and the local
   reaper can be deleted from TT. Keep the adapter as the single entry point.

If you really want a Phase-1 shadow signal before cutover: add a **read-only**
audit. After every successful Node hold/release/confirm, asynchronously call
`GET /api/v1/inventory/:trip_id` on the engine and verify the engine's view of
the changed seats matches the DB. This is racy but informational, and will not
double-write. **Do not** call engine write endpoints in shadow.

---

## 7. Where to ask questions

- Engine API contract: `engine/docs/TRANSITY_TERMINAL_INTEGRATION.md` §5, §8
- HMAC signing details: `engine/docs/TRANSITY_TERMINAL_INTEGRATION.md` §3
- Engine source of truth: `engine/crates/engine-core/src/{hold,release,confirm,cancel,reaper}.rs`
- Engine env vars (verified): `engine/crates/engine-server/src/config.rs`
- Engine route shapes (verified): `engine/crates/engine-server/src/routes.rs`
- Deployment: `engine/deploy/README.md`
