import { randomUUID } from "crypto";
import { getConfig } from "@server/config";
import { db } from "@server/db";
import { seatHolds, seatInventory } from "@shared/schema";
import { eq, and, lt, inArray } from "drizzle-orm";
import { webSocketService } from "@server/realtime/ws";

interface SeatHoldOwner {
  operatorId: string;
  bookingId?: string;
}

interface SeatHold {
  holdRef: string;
  tripId: string;
  seatNo: string;
  legIndexes: number[];
  expiresAt: number;
  ttlClass: 'short' | 'long';
  owner: SeatHoldOwner;
}

export class HoldsService {
  constructor() {
  }

  async createSeatHold(
    tripId: string,
    seatNo: string,
    legIndexes: number[],
    ttlClass: 'short' | 'long' = 'short',
    owner: SeatHoldOwner
  ): Promise<{ ok: boolean; holdRef?: string; expiresAt?: number; ownedByYou?: boolean; reason?: string }> {
    const config = getConfig();
    const now = Date.now();
    const ttlSeconds = ttlClass === 'short' ? config.holdTtlShortSeconds : config.holdTtlLongSeconds;
    const expiresAt = new Date(now + (ttlSeconds * 1000));
    const holdRef = randomUUID();

    try {
      const result = await db.transaction(async (tx) => {
        const existingRows = await tx.select()
          .from(seatInventory)
          .where(and(
            eq(seatInventory.tripId, tripId),
            eq(seatInventory.seatNo, seatNo),
            inArray(seatInventory.legIndex, legIndexes)
          ));

        if (existingRows.some(r => r.booked)) {
          return { ok: false as const, reason: 'already-booked', ownedByYou: false };
        }

        const activeHoldRefs = [...new Set(existingRows.map(r => r.holdRef).filter(Boolean))] as string[];
        if (activeHoldRefs.length > 0) {
          const activeHolds = await tx.select()
            .from(seatHolds)
            .where(inArray(seatHolds.holdRef, activeHoldRefs));

          const now = new Date();
          for (const h of activeHolds) {
            if (new Date(h.expiresAt) > now) {
              if (h.operatorId === owner.operatorId) {
                return { ok: false as const, reason: 'already-held-by-you', ownedByYou: true };
              } else {
                return { ok: false as const, reason: 'held-by-other', ownedByYou: false };
              }
            }
          }
        }

        await tx.insert(seatHolds).values({
          holdRef,
          tripId,
          seatNo,
          legIndexes,
          operatorId: owner.operatorId,
          bookingId: owner.bookingId || null,
          ttlClass,
          expiresAt
        });

        await tx.update(seatInventory)
          .set({ holdRef })
          .where(and(
            eq(seatInventory.tripId, tripId),
            eq(seatInventory.seatNo, seatNo),
            inArray(seatInventory.legIndex, legIndexes)
          ));

        return {
          ok: true as const,
          holdRef,
          expiresAt: expiresAt.getTime(),
          ownedByYou: true
        };
      });

      if (result.ok) {
        webSocketService.emitInventoryUpdated(tripId, seatNo, legIndexes);
      }

      return result;
    } catch (error) {
      console.error(`[HOLDS] Failed to create hold for ${tripId}:${seatNo}:`, error);
      return { ok: false, reason: 'internal-error' };
    }
  }

  async releaseSeatHold(tripId: string, seatNo: string, legIndexes: number[]): Promise<void> {
    const [inventoryRow] = await db.select()
      .from(seatInventory)
      .where(and(
        eq(seatInventory.tripId, tripId),
        eq(seatInventory.seatNo, seatNo),
        eq(seatInventory.legIndex, legIndexes[0])
      ))
      .limit(1);

    if (inventoryRow?.holdRef) {
      await this.releaseHoldByRef(inventoryRow.holdRef);
    }
  }

  async releaseHoldByRef(holdRef: string): Promise<void> {
    try {
      const released = await db.transaction(async (tx) => {
        const [dbHold] = await tx.select()
          .from(seatHolds)
          .where(eq(seatHolds.holdRef, holdRef))
          .limit(1);

        if (!dbHold) return null;

        await tx.update(seatInventory)
          .set({ holdRef: null })
          .where(eq(seatInventory.holdRef, holdRef));

        await tx.delete(seatHolds)
          .where(eq(seatHolds.holdRef, holdRef));

        return dbHold;
      });

      if (released) {
        webSocketService.emitInventoryUpdated(released.tripId, released.seatNo, released.legIndexes as number[]);
        webSocketService.emitHoldsReleased(released.tripId, [released.seatNo]);
      }
    } catch (dbError) {
      console.error(`[HOLDS] Failed to release hold ${holdRef}:`, dbError);
      throw dbError;
    }
  }

  async isSeatHeld(tripId: string, seatNo: string, legIndexes: number[]): Promise<boolean> {
    const hold = await this.getSeatHoldInfoFromDb(tripId, seatNo, legIndexes[0]);
    return hold !== null;
  }

  async getHoldInfo(holdRef: string): Promise<SeatHold | null> {
    const [dbHold] = await db.select()
      .from(seatHolds)
      .where(eq(seatHolds.holdRef, holdRef))
      .limit(1);

    if (dbHold && new Date(dbHold.expiresAt) > new Date()) {
      return {
        holdRef: dbHold.holdRef,
        tripId: dbHold.tripId,
        seatNo: dbHold.seatNo,
        legIndexes: dbHold.legIndexes as number[],
        expiresAt: new Date(dbHold.expiresAt).getTime(),
        ttlClass: dbHold.ttlClass as 'short' | 'long',
        owner: {
          operatorId: dbHold.operatorId,
          bookingId: dbHold.bookingId || undefined
        }
      };
    }

    return null;
  }

  async getSeatHoldInfo(tripId: string, seatNo: string, legIndex: number): Promise<SeatHold | null> {
    return this.getSeatHoldInfoFromDb(tripId, seatNo, legIndex);
  }

  private async getSeatHoldInfoFromDb(tripId: string, seatNo: string, legIndex: number): Promise<SeatHold | null> {
    try {
      const [inventoryRow] = await db.select()
        .from(seatInventory)
        .where(and(
          eq(seatInventory.tripId, tripId),
          eq(seatInventory.seatNo, seatNo),
          eq(seatInventory.legIndex, legIndex)
        ))
        .limit(1);

      if (!inventoryRow || !inventoryRow.holdRef) {
        return null;
      }

      return this.getHoldInfo(inventoryRow.holdRef);
    } catch (error) {
      console.error(`[HOLDS] Error getting seat hold info from DB:`, error);
      return null;
    }
  }

  async extendHold(holdRef: string, ttlClass: 'short' | 'long'): Promise<boolean> {
    const config = getConfig();
    const now = Date.now();
    const ttlSeconds = ttlClass === 'short' ? config.holdTtlShortSeconds : config.holdTtlLongSeconds;
    const newExpiry = new Date(now + (ttlSeconds * 1000));

    const result = await db.update(seatHolds)
      .set({ expiresAt: newExpiry, ttlClass })
      .where(eq(seatHolds.holdRef, holdRef));

    return true;
  }

  async convertHoldsToLong(operatorId: string, bookingId: string): Promise<void> {
    const config = getConfig();
    const now = Date.now();
    const newExpiry = new Date(now + (config.holdTtlLongSeconds * 1000));
    const { isNull } = await import("drizzle-orm");

    await db.update(seatHolds)
      .set({ bookingId, ttlClass: 'long', expiresAt: newExpiry })
      .where(and(
        eq(seatHolds.operatorId, operatorId),
        isNull(seatHolds.bookingId)
      ));
  }

  async releaseHoldsByOwner(operatorId: string, bookingId?: string): Promise<void> {
    const condition = bookingId
      ? and(eq(seatHolds.operatorId, operatorId), eq(seatHolds.bookingId, bookingId))
      : eq(seatHolds.operatorId, operatorId);

    const holdsToRelease = await db.select().from(seatHolds).where(condition);
    if (holdsToRelease.length === 0) return;

    const holdRefs = holdsToRelease.map(h => h.holdRef);

    await db.transaction(async (tx) => {
      await tx.update(seatInventory)
        .set({ holdRef: null })
        .where(inArray(seatInventory.holdRef, holdRefs));

      await tx.delete(seatHolds)
        .where(inArray(seatHolds.holdRef, holdRefs));
    });

    const tripGroups = new Map<string, string[]>();
    for (const hold of holdsToRelease) {
      const seats = tripGroups.get(hold.tripId) || [];
      seats.push(hold.seatNo);
      tripGroups.set(hold.tripId, seats);
    }
    for (const [tripId, seats] of tripGroups) {
      webSocketService.emitHoldsReleased(tripId, seats);
    }
    for (const hold of holdsToRelease) {
      webSocketService.emitInventoryUpdated(hold.tripId, hold.seatNo, hold.legIndexes as number[]);
    }
  }
}
