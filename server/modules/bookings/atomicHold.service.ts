import { IStorage } from "@server/storage.interface";
import { db } from "@server/db";
import { seatInventory, seatHolds } from "@shared/schema";
import { eq, and, inArray } from "drizzle-orm";
import { randomUUID } from "crypto";
import { webSocketService } from "@server/realtime/ws";

export interface SeatHoldRequest {
  tripId: string;
  seatNo: string;
  legIndexes: number[];
  operatorId: string;
  ttlClass: 'short' | 'long';
}

export interface AtomicHoldResult {
  success: boolean;
  holdRef?: string;
  conflictSeats?: string[];
  reason?: string;
  expiresAt?: Date;
}

export class AtomicHoldService {
  constructor(private storage: IStorage) {}

  async atomicHold(request: SeatHoldRequest): Promise<AtomicHoldResult> {
    const { tripId, seatNo, legIndexes, operatorId, ttlClass } = request;
    const holdRef = randomUUID();
    const ttlSeconds = ttlClass === 'short' ? 300 : 1800;
    const expiresAt = new Date(Date.now() + ttlSeconds * 1000);

    try {
      const result = await db.transaction(async (tx) => {
        const inventoryRows = await tx
          .select()
          .from(seatInventory)
          .where(and(
            eq(seatInventory.tripId, tripId),
            eq(seatInventory.seatNo, seatNo),
            inArray(seatInventory.legIndex, legIndexes)
          ))
          .for('update');

        if (inventoryRows.length !== legIndexes.length) {
          return {
            success: false,
            reason: 'INCOMPLETE_INVENTORY',
            conflictSeats: [seatNo]
          };
        }

        const conflicts: string[] = [];
        for (const row of inventoryRows) {
          if (row.booked || row.holdRef) {
            conflicts.push(seatNo);
          }
        }

        if (conflicts.length > 0) {
          return {
            success: false,
            reason: 'SEAT_CONFLICT',
            conflictSeats: conflicts
          };
        }

        await tx
          .update(seatInventory)
          .set({ holdRef })
          .where(and(
            eq(seatInventory.tripId, tripId),
            eq(seatInventory.seatNo, seatNo),
            inArray(seatInventory.legIndex, legIndexes)
          ));

        await tx.insert(seatHolds).values({
          holdRef,
          tripId,
          seatNo,
          legIndexes,
          ttlClass,
          operatorId,
          expiresAt
        });

        return {
          success: true,
          holdRef,
          expiresAt
        };
      });

      if (result.success) {
        webSocketService.emitInventoryUpdated(tripId, seatNo, legIndexes);
      }

      return result;
    } catch (error) {
      console.error(`[ATOMIC_HOLD] Hold creation failed:`, error);
      return {
        success: false,
        reason: 'TRANSACTION_ERROR',
        conflictSeats: [seatNo]
      };
    }
  }

  async releaseHoldByRef(holdRef: string): Promise<{ success: boolean }> {
    try {
      const releasedHold = await db.transaction(async (tx) => {
        const [hold] = await tx
          .select()
          .from(seatHolds)
          .where(eq(seatHolds.holdRef, holdRef));

        if (!hold) {
          return null;
        }

        await tx
          .update(seatInventory)
          .set({ holdRef: null })
          .where(eq(seatInventory.holdRef, holdRef));

        await tx
          .delete(seatHolds)
          .where(eq(seatHolds.holdRef, holdRef));

        return hold;
      });

      if (releasedHold) {
        webSocketService.emitInventoryUpdated(
          releasedHold.tripId,
          releasedHold.seatNo,
          releasedHold.legIndexes as number[]
        );
        webSocketService.emitHoldsReleased(releasedHold.tripId, [releasedHold.seatNo]);
        return { success: true };
      }

      return { success: false };
    } catch (error) {
      console.error(`[ATOMIC_HOLD] Hold release failed:`, error);
      return { success: false };
    }
  }
}
