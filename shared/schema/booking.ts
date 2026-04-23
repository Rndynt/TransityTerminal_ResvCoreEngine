import { sql } from "drizzle-orm";
import { pgTable, text, uuid, timestamp, integer, numeric, jsonb } from "drizzle-orm/pg-core";
import { createInsertSchema } from "drizzle-zod";
import { z } from "zod";
import { bookingStatusEnum, channelEnum, paymentMethodEnum, paymentStatusEnum, printStatusEnum, ticketStatusEnum, bookingHistoryActionEnum } from "./enums";
import { trips } from "./scheduling";
import { stops, outlets } from "./network";
import { appUsers } from "./app-users";
import { promotions, vouchers } from "./promo";

export const bookingGroups = pgTable("booking_groups", {
  id:          uuid("id").primaryKey().default(sql`gen_random_uuid()`),
  groupCode:   text("group_code").notNull().unique(),
  type:        text("type").notNull().default("round_trip"),
  channel:     channelEnum("channel").notNull().default("CSO"),
  totalAmount: integer("total_amount").notNull(),
  outletId:    uuid("outlet_id").references(() => outlets.id),
  createdBy:   text("created_by"),
  createdAt:   timestamp("created_at", { withTimezone: true }).defaultNow(),
}, (table) => ({
  idxBookingGroupsCreatedAt: sql`CREATE INDEX IF NOT EXISTS idx_booking_groups_created_at ON ${table} (created_at)`
}));

export const insertBookingGroupSchema = createInsertSchema(bookingGroups).omit({
  id: true, createdAt: true,
});
export type BookingGroup = typeof bookingGroups.$inferSelect;
export type InsertBookingGroup = z.infer<typeof insertBookingGroupSchema>;

export const bookings = pgTable("bookings", {
  id:                 uuid("id").primaryKey().default(sql`gen_random_uuid()`),
  bookingCode:        text("booking_code").unique(),
  status:             bookingStatusEnum("status").default('pending'),
  groupId:            uuid("group_id").references(() => bookingGroups.id),
  legType:            text("leg_type").notNull().default("single"),
  tripId:             uuid("trip_id").notNull().references(() => trips.id),
  originStopId:       uuid("origin_stop_id").notNull().references(() => stops.id),
  destinationStopId:  uuid("destination_stop_id").notNull().references(() => stops.id),
  originSeq:          integer("origin_seq").notNull(),
  destinationSeq:     integer("destination_seq").notNull(),
  channel:            channelEnum("channel").default('CSO'),
  outletId:           uuid("outlet_id").references(() => outlets.id),
  snapOriginStopName:      text("snap_origin_stop_name"),
  snapDestinationStopName: text("snap_destination_stop_name"),
  snapDepartureHHMM:       text("snap_departure_hhmm"),
  snapOutletName:          text("snap_outlet_name"),
  totalAmount:        numeric("total_amount", { precision: 12, scale: 2 }).notNull(),
  discountAmount:     numeric("discount_amount", { precision: 12, scale: 2 }).default('0'),
  promoId:            uuid("promo_id"),
  voucherCode:        text("voucher_code"),
  currency:           text("currency").default('IDR'),
  createdBy:          text("created_by"),
  salesChannelCode:   text("sales_channel_code"),
  salesChannelName:   text("sales_channel_name"),
  appUserId:          uuid("app_user_id").references(() => appUsers.id),
  pendingExpiresAt:   timestamp("pending_expires_at", { withTimezone: true }),
  idempotencyKey:     text("idempotency_key"),
  createdAt:          timestamp("created_at", { withTimezone: true }).defaultNow()
}, (table) => ({
  idxBookingsTripId: sql`CREATE INDEX IF NOT EXISTS idx_bookings_trip_id ON ${table} (trip_id)`,
  idxBookingsStatus: sql`CREATE INDEX IF NOT EXISTS idx_bookings_status ON ${table} (status)`,
  idxBookingsTripStatus: sql`CREATE INDEX IF NOT EXISTS idx_bookings_trip_status ON ${table} (trip_id, status)`,
  idxBookingsOutletId: sql`CREATE INDEX IF NOT EXISTS idx_bookings_outlet_id ON ${table} (outlet_id)`,
  idxBookingsCreatedAt: sql`CREATE INDEX IF NOT EXISTS idx_bookings_created_at ON ${table} (created_at)`,
  idxBookingsPendingExpiry: sql`CREATE INDEX IF NOT EXISTS idx_bookings_pending_expiry ON ${table} (pending_expires_at) WHERE status = 'pending'`,
  idxBookingsAppUserId: sql`CREATE INDEX IF NOT EXISTS idx_bookings_app_user_id ON ${table} (app_user_id) WHERE app_user_id IS NOT NULL`,
  idxBookingsGroupId: sql`CREATE INDEX IF NOT EXISTS idx_bookings_group_id ON ${table} (group_id) WHERE group_id IS NOT NULL`,
  idxBookingsOriginStop: sql`CREATE INDEX IF NOT EXISTS idx_bookings_origin_stop ON ${table} (origin_stop_id)`,
  idxBookingsDestStop: sql`CREATE INDEX IF NOT EXISTS idx_bookings_destination_stop ON ${table} (destination_stop_id)`,
  idxBookingsOutletCreated: sql`CREATE INDEX IF NOT EXISTS idx_bookings_outlet_created ON ${table} (outlet_id, created_at DESC)`,
  uniqBookingsIdempotency: sql`CREATE UNIQUE INDEX IF NOT EXISTS uniq_bookings_idempotency_key ON ${table} (idempotency_key) WHERE idempotency_key IS NOT NULL`
}));

export const insertBookingSchema = createInsertSchema(bookings).omit({ id: true, createdAt: true });
export type Booking = typeof bookings.$inferSelect;
export type InsertBooking = z.infer<typeof insertBookingSchema>;

// Aplikasi promo per booking — 1 baris per promo (mendukung stacking & audit).
// bookingCode/ticketNumber tidak di-snapshot di sini krn bisa di-join dari bookings/passengers via bookingId.
export const bookingPromoApplications = pgTable("booking_promo_applications", {
  id:              uuid("id").primaryKey().default(sql`gen_random_uuid()`),
  bookingId:       uuid("booking_id").notNull().references(() => bookings.id, { onDelete: 'cascade' }),
  promoId:         uuid("promo_id").notNull().references(() => promotions.id),
  promoCode:       text("promo_code").notNull(),         // snapshot kode promo
  voucherId:       uuid("voucher_id").references(() => vouchers.id),
  voucherCode:     text("voucher_code"),                  // snapshot kode voucher (jika ada)
  source:          text("source").notNull(),              // 'auto' | 'manual'
  discountAmount:  numeric("discount_amount", { precision: 12, scale: 2 }).notNull(),
  createdAt:       timestamp("created_at", { withTimezone: true }).defaultNow(),
}, (table) => ({
  idxBpaBookingId: sql`CREATE INDEX IF NOT EXISTS idx_bpa_booking_id ON ${table} (booking_id)`,
  idxBpaPromoId:   sql`CREATE INDEX IF NOT EXISTS idx_bpa_promo_id ON ${table} (promo_id)`,
  uniqBpaBookingPromo: sql`CREATE UNIQUE INDEX IF NOT EXISTS uniq_bpa_booking_promo ON ${table} (booking_id, promo_id)`,
}));

export const insertBookingPromoApplicationSchema = createInsertSchema(bookingPromoApplications).omit({ id: true, createdAt: true });
export type BookingPromoApplication = typeof bookingPromoApplications.$inferSelect;
export type InsertBookingPromoApplication = z.infer<typeof insertBookingPromoApplicationSchema>;

export const passengers = pgTable("passengers", {
  id:             uuid("id").primaryKey().default(sql`gen_random_uuid()`),
  ticketNumber:   text("ticket_number").unique(),
  ticketStatus:   ticketStatusEnum("ticket_status").default('active'),
  bookingId:      uuid("booking_id").notNull().references(() => bookings.id),
  seatNo:         text("seat_no").notNull(),
  fullName:       text("full_name").notNull(),
  phone:          text("phone"),
  idNumber:       text("id_number"),
  fareAmount:     numeric("fare_amount", { precision: 12, scale: 2 }).notNull(),
  fareBreakdown:  jsonb("fare_breakdown")
}, (table) => ({
  idxPassengersBookingId: sql`CREATE INDEX IF NOT EXISTS idx_passengers_booking_id ON ${table} (booking_id)`,
  idxPassengersBookingSeat: sql`CREATE INDEX IF NOT EXISTS idx_passengers_booking_seat ON ${table} (booking_id, seat_no)`
}));

export const insertPassengerSchema = createInsertSchema(passengers).omit({ id: true });
export type Passenger = typeof passengers.$inferSelect;
export type InsertPassenger = z.infer<typeof insertPassengerSchema>;

export const payments = pgTable("payments", {
  id:          uuid("id").primaryKey().default(sql`gen_random_uuid()`),
  bookingId:   uuid("booking_id").notNull().references(() => bookings.id),
  method:      paymentMethodEnum("method").notNull(),
  status:      paymentStatusEnum("status").default('pending'),
  amount:      numeric("amount", { precision: 12, scale: 2 }).notNull(),
  providerRef: text("provider_ref"),
  paidAt:      timestamp("paid_at", { withTimezone: true }).defaultNow()
}, (table) => ({
  idxPaymentsBookingId: sql`CREATE INDEX IF NOT EXISTS idx_payments_booking_id ON ${table} (booking_id)`,
  idxPaymentsProviderRef: sql`CREATE INDEX IF NOT EXISTS idx_payments_provider_ref ON ${table} (provider_ref) WHERE provider_ref IS NOT NULL`,
  idxPaymentsPaidAt: sql`CREATE INDEX IF NOT EXISTS idx_payments_paid_at ON ${table} (paid_at)`,
  // P3: functional index on (paid_at::date) so reports that group/filter by
  // date hit an index instead of a full scan when status='success'.
  idxPaymentsPaidDate: sql`CREATE INDEX IF NOT EXISTS idx_payments_paid_date ON ${table} ((paid_at::date)) WHERE status = 'success'`
}));

export const insertPaymentSchema = createInsertSchema(payments).omit({ id: true, paidAt: true });
export type Payment = typeof payments.$inferSelect;
export type InsertPayment = z.infer<typeof insertPaymentSchema>;

export const printJobs = pgTable("print_jobs", {
  id:        uuid("id").primaryKey().default(sql`gen_random_uuid()`),
  bookingId: uuid("booking_id").notNull().references(() => bookings.id),
  status:    printStatusEnum("status").default('queued'),
  attempts:  integer("attempts").default(0),
  lastError: text("last_error"),
  createdAt: timestamp("created_at", { withTimezone: true }).defaultNow()
}, (table) => ({
  idxPrintJobsBookingId: sql`CREATE INDEX IF NOT EXISTS idx_print_jobs_booking_id ON ${table} (booking_id)`
}));

export const insertPrintJobSchema = createInsertSchema(printJobs).omit({ id: true, createdAt: true });
export type PrintJob = typeof printJobs.$inferSelect;
export type InsertPrintJob = z.infer<typeof insertPrintJobSchema>;

export const bookingHistory = pgTable("booking_history", {
  id:            uuid("id").primaryKey().default(sql`gen_random_uuid()`),
  bookingId:     uuid("booking_id").notNull().references(() => bookings.id),
  passengerId:   uuid("passenger_id").references(() => passengers.id),
  action:        bookingHistoryActionEnum("action").notNull(),
  details:       jsonb("details"),
  performedBy:   text("performed_by"),
  createdAt:     timestamp("created_at", { withTimezone: true }).defaultNow()
}, (table) => ({
  idxBookingHistoryBookingId: sql`CREATE INDEX IF NOT EXISTS idx_booking_history_booking_id ON ${table} (booking_id)`
}));

export const insertBookingHistorySchema = createInsertSchema(bookingHistory).omit({ id: true, createdAt: true });
export type BookingHistory = typeof bookingHistory.$inferSelect;
export type InsertBookingHistory = z.infer<typeof insertBookingHistorySchema>;
