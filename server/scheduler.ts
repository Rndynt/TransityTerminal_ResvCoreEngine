import { BookingsService } from './modules/bookings/bookings.service';
import { storage } from './storage';
import { getConfig } from './config';
import { db, pool } from './db';
import { seatHolds, seatInventory } from '@shared/schema';
import { lt, inArray, sql, and } from 'drizzle-orm';

// S3: per-job advisory lock IDs. Pick stable, project-unique numbers so that
// multi-instance deployments don't run cleanup or snapshot push in parallel.
const LOCK_HOLDS_CLEANUP = 8240_001;
const LOCK_ORPHAN_REFS   = 8240_002;
const LOCK_PENDING_BOOK  = 8240_003;
const LOCK_SNAPSHOT_PUSH = 8240_004;

// IMPORTANT: session-level pg advisory locks are bound to a *single
// connection*. The default `db` client uses a connection pool — two separate
// `db.execute(...)` calls can land on different connections, so the unlock
// will fail (or worse, unlock another session's hold). We therefore pin a
// dedicated client from the pool for the entire acquire/work/release cycle.
async function withAdvisoryLock<T>(lockId: number, fn: () => Promise<T>): Promise<T | null> {
  const client = await pool.connect();
  try {
    const acquired = await client.query('SELECT pg_try_advisory_lock($1) AS got', [lockId]);
    if (!acquired.rows?.[0]?.got) {
      return null;
    }
    try {
      return await fn();
    } finally {
      await client.query('SELECT pg_advisory_unlock($1)', [lockId]);
    }
  } finally {
    client.release();
  }
}
import { webSocketService } from './realtime/ws';
import { buildScheduleSnapshot } from './lib/scheduleSnapshot';
import { fireAndForget } from './lib/consoleWebhook';

const SNAPSHOT_INTERVAL_MS = parseInt(
  process.env.CONSOLE_SNAPSHOT_INTERVAL_MS || `${10 * 60 * 1000}`,
  10
); // default: every 10 minutes
const SNAPSHOT_DAYS_AHEAD = parseInt(
  process.env.CONSOLE_SNAPSHOT_DAYS_AHEAD || '7',
  10
); // default: today + next 7 days

function addDays(yyyyMmDd: string, days: number): string {
  const d = new Date(`${yyyyMmDd}T00:00:00Z`);
  d.setUTCDate(d.getUTCDate() + days);
  return d.toISOString().substring(0, 10);
}

export class Scheduler {
  private bookingsService: BookingsService;
  private intervalId: NodeJS.Timeout | null = null;
  private snapshotIntervalId: NodeJS.Timeout | null = null;

  constructor() {
    this.bookingsService = new BookingsService(storage);
  }

  async cleanupExpiredHolds(): Promise<void> {
    console.log('[SCHEDULER] Running expired holds cleanup...');
    
    try {
      const now = new Date();
      const expiredHolds = await db.transaction(async (tx) => {
        const holds = await tx
          .select()
          .from(seatHolds)
          .where(lt(seatHolds.expiresAt, now))
          .for('update', { skipLocked: true });

        if (holds.length === 0) {
          console.log('[SCHEDULER] No expired holds to clean up');
          return [];
        }

        const expiredRefs = holds.map(h => h.holdRef);

        await tx
          .update(seatInventory)
          .set({ holdRef: null })
          .where(inArray(seatInventory.holdRef, expiredRefs));

        await tx
          .delete(seatHolds)
          .where(and(
            inArray(seatHolds.holdRef, expiredRefs),
            lt(seatHolds.expiresAt, now)
          ));

        console.log(`[SCHEDULER] Cleaned up ${holds.length} expired holds`);
        return holds;
      });

      if (expiredHolds.length > 0) {
        const tripGroups = new Map<string, string[]>();
        for (const hold of expiredHolds) {
          const seats = tripGroups.get(hold.tripId) || [];
          seats.push(hold.seatNo);
          tripGroups.set(hold.tripId, seats);
        }
        for (const [tripId, seats] of tripGroups) {
          webSocketService.emitHoldsReleased(tripId, seats);
        }
      }
    } catch (error) {
      console.error('[SCHEDULER] Error cleaning up expired holds:', error);
    }
  }

  async cleanupOrphanHoldRefs(): Promise<void> {
    try {
      const result = await db.execute(sql`
        UPDATE seat_inventory
        SET hold_ref = NULL
        WHERE hold_ref IS NOT NULL
          AND NOT EXISTS (
            SELECT 1 FROM seat_holds
            WHERE seat_holds.hold_ref = seat_inventory.hold_ref
          )
        RETURNING id
      `);

      const count = result.rows?.length || 0;
      if (count > 0) {
        console.log(`[SCHEDULER] Cleaned up ${count} orphan holdRefs in seat_inventory`);
      }
    } catch (error) {
      console.error('[SCHEDULER] Error cleaning up orphan holdRefs:', error);
    }
  }

  async pushScheduleSnapshot(): Promise<void> {
    // Only push if Console is configured. Without these env vars the webhook
    // emitter would skip anyway, so don't waste DB queries building snapshots.
    if (
      !process.env.CONSOLE_URL ||
      !process.env.CONSOLE_OPERATOR_SLUG ||
      !process.env.CONSOLE_WEBHOOK_SECRET
    ) {
      return;
    }

    try {
      const today = new Date().toISOString().substring(0, 10);
      const allTrips = [];
      for (let i = 0; i < SNAPSHOT_DAYS_AHEAD; i++) {
        const day = addDays(today, i);
        const trips = await buildScheduleSnapshot(storage, day);
        if (trips.length > 0) allTrips.push(...trips);
      }
      if (allTrips.length === 0) {
        return;
      }
      // Emit a single combined snapshot. fireAndForget will retry on failure
      // and coalesce repeated snapshot pushes if Console stays offline.
      fireAndForget({
        event: 'schedule.snapshot',
        trips: allTrips,
        emittedAt: new Date().toISOString(),
      });
      console.log(
        `[SCHEDULER] Pushed schedule snapshot to Console (${allTrips.length} trips, next ${SNAPSHOT_DAYS_AHEAD} days)`
      );
    } catch (error) {
      console.error('[SCHEDULER] Error pushing schedule snapshot:', error);
    }
  }

  start(): void {
    const config = getConfig();
    
    // Run cleanup every 60 seconds. Wrap each job in a Postgres advisory lock
    // so multi-instance deployments do not run the same cleanup multiple times.
    this.intervalId = setInterval(async () => {
      try {
        await withAdvisoryLock(LOCK_HOLDS_CLEANUP, () => this.cleanupExpiredHolds());
        await withAdvisoryLock(LOCK_ORPHAN_REFS, () => this.cleanupOrphanHoldRefs());

        if (config.pendingBookingAutoRelease) {
          await withAdvisoryLock(LOCK_PENDING_BOOK, async () => {
            console.log('[SCHEDULER] Running expired pending bookings cleanup...');
            await this.bookingsService.cleanupExpiredPendingBookings();
          });
        }
      } catch (error) {
        console.error('[SCHEDULER] Error during cleanup:', error);
      }
    }, 60 * 1000); // Every 1 minute

    console.log('[SCHEDULER] Cleanup scheduler started (runs every 1 minute)');

    // Run immediately on start
    this.cleanupExpiredHolds();

    // Periodic schedule snapshot push to Console — this is the safety net so
    // that even if every fire-and-forget delivery fails (e.g. Console is down
    // for an extended period), Console will reconverge on its own once it
    // comes back, without an operator having to press "Sync".
    if (SNAPSHOT_INTERVAL_MS > 0) {
      this.snapshotIntervalId = setInterval(() => {
        void withAdvisoryLock(LOCK_SNAPSHOT_PUSH, () => this.pushScheduleSnapshot());
      }, SNAPSHOT_INTERVAL_MS);
      console.log(
        `[SCHEDULER] Schedule snapshot push started (every ${Math.round(
          SNAPSHOT_INTERVAL_MS / 1000
        )}s, ${SNAPSHOT_DAYS_AHEAD} days ahead)`
      );
      // Kick once on startup so a freshly-restarted Terminal pushes immediately.
      void withAdvisoryLock(LOCK_SNAPSHOT_PUSH, () => this.pushScheduleSnapshot());
    }
  }

  stop(): void {
    if (this.intervalId) {
      clearInterval(this.intervalId);
      this.intervalId = null;
      console.log('[SCHEDULER] Cleanup scheduler stopped');
    }
    if (this.snapshotIntervalId) {
      clearInterval(this.snapshotIntervalId);
      this.snapshotIntervalId = null;
      console.log('[SCHEDULER] Schedule snapshot push stopped');
    }
  }
}

export const scheduler = new Scheduler();
