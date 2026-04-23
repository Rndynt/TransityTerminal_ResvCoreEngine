# Reservation Engine Contract Specification

Dokumen ini berisi kontrak **lengkap dan otoritatif** untuk core engine reservasi
TransityTerminal. Semua detail di sini harus diikuti persis oleh implementasi
bahasa lain (Rust, Go, dll) supaya kompatibel 1:1 dengan sistem yang sudah berjalan.

Versi: 1.0 — Snapshot dari kode produksi per April 2026.

---

## 1. Domain Model (PostgreSQL Schema)

### 1.1 `seat_inventory`
Sumber kebenaran status setiap kursi per leg per trip.

| Kolom | Tipe | Constraint | Catatan |
|---|---|---|---|
| `id` | uuid | PK, default `gen_random_uuid()` | |
| `trip_id` | uuid | NOT NULL, FK → `trips.id` | |
| `seat_no` | text | NOT NULL | mis. "1A", "2B" |
| `leg_index` | integer | NOT NULL | 0-based, urut sesuai `trip_stop_times` |
| `booked` | boolean | default `false` | `true` jika sudah dipesan permanen |
| `hold_ref` | text | nullable | UUID hold aktif (jika sedang ditahan) |

**Index wajib:**
- `uniq_seat_inv_trip_seat_leg` UNIQUE on `(trip_id, seat_no, leg_index)`
- `idx_seat_inv_trip_seat` on `(trip_id, seat_no)`
- `idx_seat_inv_trip_id` on `(trip_id)`
- `idx_seat_inv_trip_leg` on `(trip_id, leg_index)`

**Aturan invariant:**
- Satu baris per kombinasi `(trip_id, seat_no, leg_index)`.
- Jika `booked = true`, `hold_ref` HARUS null.
- Jika `hold_ref` not null, `booked` HARUS false.
- Saat trip di-materialize, semua baris inventory dibuat dengan `booked=false, hold_ref=null`.

---

### 1.2 `seat_holds`
Catatan penahanan sementara (TTL-based).

| Kolom | Tipe | Constraint | Catatan |
|---|---|---|---|
| `id` | uuid | PK | |
| `hold_ref` | text | NOT NULL, UNIQUE | UUID v4, dijadikan referensi balik di `seat_inventory.hold_ref` |
| `trip_id` | uuid | NOT NULL, FK → `trips.id` | |
| `seat_no` | text | NOT NULL | |
| `leg_indexes` | integer[] | NOT NULL | Daftar leg yang dipegang oleh hold ini |
| `ttl_class` | text | NOT NULL | `'short'` atau `'long'` |
| `operator_id` | text | NOT NULL | ID kasir/user yang membuat hold |
| `booking_id` | text | nullable | Diisi saat hold dipromosikan ke booking |
| `expires_at` | timestamptz | NOT NULL | Waktu kadaluarsa absolut |
| `created_at` | timestamptz | default `now()` | |

**Index wajib:**
- `idx_seat_holds_trip_id` on `(trip_id)`
- `idx_seat_holds_expires_at` on `(expires_at)`
- `idx_seat_holds_active` on `(trip_id, expires_at) WHERE booking_id IS NULL` (partial)
- `idx_seat_holds_booking_id` on `(booking_id) WHERE booking_id IS NOT NULL`
- `idx_seat_holds_trip_seat` on `(trip_id, seat_no)`

**TTL durations (HARUS persis sama):**
- `ttl_class = 'short'` → **300 detik** (5 menit) — dipakai saat user pilih kursi di seat-map
- `ttl_class = 'long'` → **1800 detik** (30 menit) — dipakai untuk pending booking

---

### 1.3 `bookings` (header)
Header transaksi booking. Engine reservasi **HANYA peduli** kolom-kolom berikut:

| Kolom | Tipe | Catatan |
|---|---|---|
| `id` | uuid | PK |
| `booking_code` | text | UNIQUE, format mis. `BK-XXXXXX` |
| `status` | enum `booking_status` | lihat enum di §2 |
| `trip_id` | uuid | FK |
| `origin_seq` | integer | sequence stop asal |
| `destination_seq` | integer | sequence stop tujuan |
| `pending_expires_at` | timestamptz nullable | wajib diisi jika `status='pending'` |
| `idempotency_key` | text | UNIQUE partial — mencegah double-submit |

> Kolom finance (total_amount, discount, dll), promo, channel, outlet, snapshot, dan
> passengers tetap di-handle oleh layer Node. Engine reservasi tidak menyentuh.

---

## 2. Enums (PostgreSQL)

```sql
CREATE TYPE booking_status AS ENUM
  ('pending', 'confirmed', 'checked_in', 'paid', 'cancelled', 'refunded', 'unseated');

CREATE TYPE ticket_status AS ENUM
  ('active', 'cancelled', 'refunded', 'checked_in', 'no_show', 'unseated');

CREATE TYPE trip_status AS ENUM
  ('scheduled', 'cancelled', 'closed');
```

Engine reservasi **hanya boleh membuat transisi**:
- `bookings.status`: `pending → confirmed`, `pending → cancelled`, `confirmed → cancelled`
- `seat_inventory`: `(free) → (held) → (booked) → (free)`

---

## 3. Operasi Engine (kontrak fungsional)

### 3.1 `atomic_hold(request) → AtomicHoldResult`

**Input:**
```rust
struct SeatHoldRequest {
    trip_id: Uuid,
    seat_no: String,
    leg_indexes: Vec<i32>,  // urut & semua leg yang dilewati origin→destination
    operator_id: String,
    ttl_class: TtlClass,    // Short | Long
}
```

**Output:**
```rust
enum AtomicHoldResult {
    Success { hold_ref: Uuid, expires_at: DateTime<Utc> },
    Failure { reason: HoldFailureReason, conflict_seats: Vec<String> },
}

enum HoldFailureReason {
    IncompleteInventory,  // jumlah row inventory tidak match jumlah leg_indexes
    SeatConflict,         // sudah booked atau ada hold_ref
    TransactionError,     // exception/DB error
}
```

**Algoritma WAJIB (transaksional):**

```sql
BEGIN;

-- 1. Lock baris inventory (row-level, blocking)
SELECT * FROM seat_inventory
 WHERE trip_id = $1 AND seat_no = $2 AND leg_index = ANY($3)
   FOR UPDATE;

-- 2. Validasi:
--    a. count(rows) == length(leg_indexes) → else INCOMPLETE_INVENTORY
--    b. semua row: booked=false AND hold_ref IS NULL → else SEAT_CONFLICT

-- 3. Generate hold_ref = UUID v4
-- 4. expires_at = now() + (300s short | 1800s long)

-- 5. Tandai inventory:
UPDATE seat_inventory SET hold_ref = $hold_ref
 WHERE trip_id = $1 AND seat_no = $2 AND leg_index = ANY($3);

-- 6. Insert seat_holds:
INSERT INTO seat_holds
  (hold_ref, trip_id, seat_no, leg_indexes, ttl_class, operator_id, expires_at)
VALUES (...);

COMMIT;
```

**Side-effect setelah commit sukses:**
- Publish event `inventory.updated` ke Redis pub/sub channel:
  ```json
  { "trip_id": "...", "seat_no": "1A", "leg_indexes": [0,1,2] }
  ```

---

### 3.2 `release_hold_by_ref(hold_ref) → { success: bool }`

**Algoritma:**
```sql
BEGIN;
  SELECT * FROM seat_holds WHERE hold_ref = $1;  -- jika tidak ada, return {success:false}

  UPDATE seat_inventory SET hold_ref = NULL WHERE hold_ref = $1;
  DELETE FROM seat_holds WHERE hold_ref = $1;
COMMIT;
```

**Events setelah commit:**
- `inventory.updated` (trip_id, seat_no, leg_indexes)
- `holds.released` (trip_id, [seat_no])

---

### 3.3 `confirm_booking(hold_ref, booking_id) → { success, conflict? }`

Mempromosikan hold menjadi booking permanen.

**Algoritma:**
```sql
BEGIN;
  -- 1. Validasi hold masih ada dan belum expired
  SELECT * FROM seat_holds WHERE hold_ref = $1 AND expires_at > now() FOR UPDATE;
  -- jika tidak ada → return Failure(HoldExpiredOrMissing)

  -- 2. Lock inventory rows yang dipegang
  SELECT * FROM seat_inventory WHERE hold_ref = $1 FOR UPDATE;

  -- 3. Set booked=true, hold_ref=NULL
  UPDATE seat_inventory SET booked = true, hold_ref = NULL WHERE hold_ref = $1;

  -- 4. Tandai hold sebagai consumed
  UPDATE seat_holds SET booking_id = $2 WHERE hold_ref = $1;
COMMIT;
```

**Event:** `inventory.updated` per kursi yang dikonfirmasi.

---

### 3.4 `cancel_booking_seats(trip_id, seat_no, leg_indexes) → { success }`

Dipanggil saat passenger di-cancel (lihat `bookings.routes.ts` line 134-180).

```sql
BEGIN;
  UPDATE seat_inventory
     SET booked = false, hold_ref = NULL
   WHERE trip_id = $1 AND seat_no = $2 AND leg_index = ANY($3);
COMMIT;
```

**Event:** `inventory.updated` per leg.

---

### 3.5 `expire_holds() → { released_count }`  (background reaper)

Jalankan setiap **60 detik** (cocok dengan scheduler Node yang ada sekarang).

```sql
BEGIN;
  -- Pakai advisory lock supaya tidak double-run di multi-instance
  SELECT pg_try_advisory_lock(hashtext('reservation_reaper'));

  WITH expired AS (
    SELECT hold_ref, trip_id, seat_no, leg_indexes
      FROM seat_holds
     WHERE expires_at <= now() AND booking_id IS NULL
     FOR UPDATE SKIP LOCKED
     LIMIT 500
  ),
  cleared AS (
    UPDATE seat_inventory SET hold_ref = NULL
     WHERE hold_ref IN (SELECT hold_ref FROM expired)
     RETURNING trip_id, seat_no, leg_index
  )
  DELETE FROM seat_holds WHERE hold_ref IN (SELECT hold_ref FROM expired);

  SELECT pg_advisory_unlock(hashtext('reservation_reaper'));
COMMIT;
```

**Event:** untuk setiap kursi yang dilepas, publish `holds.released`.

---

### 3.6 `get_inventory_snapshot(trip_id) → InventorySnapshot`

Read-only. Untuk seat-map UI.

**Output:**
```rust
struct InventorySnapshot {
    trip_id: Uuid,
    seats: Vec<SeatState>,
}
struct SeatState {
    seat_no: String,
    leg_states: Vec<LegState>,  // index = leg_index
}
struct LegState {
    leg_index: i32,
    status: SeatStatusKind,  // Free | Held | Booked
    hold_expires_at: Option<DateTime<Utc>>,
}
```

---

## 4. API Surface Saat Ini (Node, untuk kompatibilitas)

Endpoint Node yang HARUS terus bekerja sama persis (engine baru harus dipanggil oleh
endpoint ini, bukan menggantikannya):

| Method | Path | Fungsi internal yang dipakai |
|---|---|---|
| POST | `/api/holds` | `atomic_hold()` |
| DELETE | `/api/holds/:holdRef` | `release_hold_by_ref()` |
| POST | `/api/bookings` | `atomic_hold()` + `confirm_booking()` (1 transaksi outer) |
| POST | `/api/bookings/pending` | `atomic_hold(ttl=long)` + insert booking dengan status='pending' |
| DELETE | `/api/bookings/pending/:id` | `release_hold_by_ref()` + update booking ke 'cancelled' |
| PATCH | `/api/passengers/:id/cancel` | `cancel_booking_seats()` |

---

## 5. Real-time Event Schema (Redis Pub/Sub)

Channel: `reservation.events` (single channel, JSON message).

```json
// inventory.updated
{
  "type": "inventory.updated",
  "trip_id": "uuid",
  "seat_no": "1A",
  "leg_indexes": [0, 1, 2],
  "ts": "2026-04-23T15:00:00Z"
}

// holds.released
{
  "type": "holds.released",
  "trip_id": "uuid",
  "seat_nos": ["1A", "2B"],
  "ts": "..."
}
```

Konsumer (Node WS server) akan re-broadcast ke client lewat Socket.io room `trip:{trip_id}`.

---

## 6. Idempotency

- Header request: `Idempotency-Key: <opaque-string>`
- Engine simpan mapping `(idempotency_key) → (cached_response, created_at)` selama
  **24 jam** (in-memory LRU + Redis backup).
- Jika request datang dengan key yang sama dan body identik → kembalikan response cache.
- Jika body berbeda → return HTTP 409.

Mapping ini **wajib** untuk endpoint write: `atomic_hold`, `confirm_booking`.

---

## 7. Service-to-Service Auth

- Engine HANYA boleh diakses oleh internal service (Node Terminal).
- Auth: HMAC-SHA256 signature di header.
  - Header: `X-Service-Id: terminal`, `X-Signature: <hex>`, `X-Timestamp: <unix>`
  - Signature = HMAC(secret, `{timestamp}.{method}.{path}.{body_sha256}`)
  - Reject jika `|now - timestamp| > 30s`.
- Secret di-rotate via env var `RESERVATION_ENGINE_HMAC_SECRET`.

---

## 8. Database Connection

- Connect ke PostgreSQL yang **sama** dengan TransityTerminal (shared DB).
- Engine HANYA boleh write ke tabel:
  - `seat_inventory`
  - `seat_holds`
  - Update kolom `bookings.status` & `bookings.pending_expires_at` (saat confirm/cancel)
- Engine boleh read dari: `trips`, `trip_stop_times` (untuk validasi leg_index).
- Recommended: buat PG role `reservation_engine` dengan grant terbatas.
- Connection pool: minimal 10, max 50 koneksi.

---

## 9. Konsistensi Behavior dengan Kode Saat Ini

Detail-detail kecil yang HARUS sama (kalau berbeda, akan ada bug halus):

1. **Hold conflict reporting**: `conflict_seats` mengembalikan `[seat_no]` (single-element array),
   bukan list semua leg. Lihat `atomicHold.service.ts:49,64`.
2. **Hold ref format**: UUID v4 lowercase string.
3. **TTL exact**: 300s & 1800s — tidak boleh +/- 1 detik.
4. **Leg validation**: `inventoryRows.length !== legIndexes.length` → `INCOMPLETE_INVENTORY`.
5. **Inventory clearing on cancel**: SET `booked=false, hold_ref=NULL` (kedua kolom).
6. **Cascade cancel**: jika SEMUA passenger di booking jadi `cancelled` atau `unseated`,
   header `bookings.status` ikut jadi `cancelled`. Lihat `bookings.routes.ts:162-168`.
7. **WS event order**: untuk release, kirim `inventory.updated` DULU, baru `holds.released`.
   Lihat `atomicHold.service.ts:134-139`.

---

## 10. Testing Checklist (untuk validasi parity)

Skenario integration test minimum yang harus pass di engine baru sebelum cutover:

- [ ] Hold sukses: 1 seat, 1 leg
- [ ] Hold sukses: 1 seat, multi-leg
- [ ] Hold gagal: seat sudah booked
- [ ] Hold gagal: seat sudah di-hold orang lain
- [ ] Hold gagal: leg_index tidak ada di inventory
- [ ] Race: 2 hold concurrent untuk seat sama → tepat 1 menang, 1 dapat SEAT_CONFLICT
- [ ] Release hold valid → inventory clear, event terbit
- [ ] Release hold yang sudah tidak ada → `{success:false}`, no event
- [ ] Confirm hold valid → booked=true, hold record marked dengan booking_id
- [ ] Confirm hold yang sudah expired → fail, no inventory change
- [ ] Reaper: hold expired dilepas dalam ≤1 menit
- [ ] Reaper: hold dengan booking_id NOT NULL tidak dilepas
- [ ] Cancel passenger: inventory free, jika last passenger → booking jadi cancelled
- [ ] Idempotency: 2 hold request dengan key sama → response identik, hanya 1 hold di DB

---

## 11. File Referensi di Codebase Terminal

- `shared/schema/inventory.ts` — schema seat_inventory & seat_holds
- `shared/schema/booking.ts` — schema bookings header & passengers
- `shared/schema/enums.ts` — semua PG enum
- `server/modules/bookings/atomicHold.service.ts` — implementasi referensi hold/release
- `server/modules/bookings/bookings.service.ts` — booking creation flow
- `server/modules/bookings/bookings.routes.ts` — endpoint API
- `server/scheduler.ts` — reaper job
- `server/realtime/ws.ts` — WebSocket fanout
- `server/realtime/redis.ts` — Redis adapter

---

## 12. Migrasi Bertahap (Strangler Fig)

1. **Shadow mode**: engine baru menerima trafik mirror dari endpoint Node, hasil
   dibandingkan tapi tidak dipakai. (2-3 minggu)
2. **Canary 1 outlet**: 1 outlet pakai engine baru sebagai source-of-truth.
3. **Cutover hold + release**: semua hold via engine.
4. **Cutover confirm + cancel**: semua transisi state via engine.
5. **Cleanup**: hapus `atomicHold.service.ts` di Node, biarkan jadi pure proxy.
