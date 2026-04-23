//! Reservation Engine load test.
//!
//! Seeds N seats × 1 leg into the engine's Postgres for a freshly-generated
//! trip_id, then drives concurrent hold/release (or hold/confirm) flows over
//! HTTP and reports throughput + latency percentiles.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use futures::future::join_all;
use hdrhistogram::Histogram;
use hmac::{Hmac, Mac};
use reqwest::Client;
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use tokio::sync::Mutex;
use uuid::Uuid;

type HmacSha256 = Hmac<Sha256>;

#[derive(Parser, Debug, Clone)]
struct Args {
    #[arg(long, env = "ENGINE_URL", default_value = "http://localhost:8000")]
    engine_url: String,
    #[arg(long, env = "RESERVATION_ENGINE_HMAC_SECRET")]
    hmac_secret: String,
    #[arg(long, env = "ENGINE_DATABASE_URL")]
    database_url: String,
    #[arg(long, env = "SEATS", default_value_t = 200)]
    seats: usize,
    #[arg(long, env = "CONCURRENCY", default_value_t = 32)]
    concurrency: usize,
    #[arg(long, env = "OPS", default_value_t = 2_000)]
    ops: usize,
    /// Either "hold-release" or "hold-confirm".
    #[arg(long, env = "SCENARIO", default_value = "hold-release")]
    scenario: String,
    /// Reuse this trip_id instead of auto-discovering one. The load test will
    /// seed unique seats (prefix `LT-<run>-`) into this trip and clean them up
    /// afterwards, leaving real seat rows untouched.
    #[arg(long, env = "TRIP_ID")]
    trip_id: Option<Uuid>,
}

struct Stats {
    hold_hist: Histogram<u64>,
    second_hist: Histogram<u64>, // release or confirm latency
    ok: u64,
    conflict: u64,
    second_ok: u64,
    errors: u64,
}

impl Stats {
    fn new() -> Self {
        Self {
            hold_hist: Histogram::new(3).unwrap(),
            second_hist: Histogram::new(3).unwrap(),
            ok: 0,
            conflict: 0,
            second_ok: 0,
            errors: 0,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    println!("== Reservation Engine Load Test ==");
    println!(
        "seats={}  concurrency={}  ops={}  scenario={}",
        args.seats, args.concurrency, args.ops, args.scenario
    );

    // 1. Seed inventory directly via SQL (engine doesn't expose a seed endpoint).
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&args.database_url)
        .await
        .context("connect db")?;

    // Find a trip_id to attach our load-test seats to. The schema FKs
    // seat_inventory.trip_id → trips.id, so we cannot just generate one.
    let trip_id = match args.trip_id {
        Some(t) => t,
        None => {
            let row: Option<(Uuid,)> =
                sqlx::query_as("SELECT id FROM trips ORDER BY created_at DESC LIMIT 1")
                    .fetch_optional(&pool)
                    .await?;
            match row {
                Some((id,)) => id,
                None => anyhow::bail!(
                    "no trips found in DB; pass --trip-id <uuid> to use a specific trip"
                ),
            }
        }
    };

    // Use a unique seat_no prefix per run so we never collide with real seat
    // rows and our cleanup is safe.
    let run_tag: String = Uuid::new_v4().simple().to_string()[..8].to_string();
    let seat_prefix = format!("LT-{}-", run_tag);

    let mut tx = pool.begin().await?;
    for i in 0..args.seats {
        let seat_no = format!("{}{:04}", seat_prefix, i);
        sqlx::query(
            "INSERT INTO seat_inventory (trip_id, seat_no, leg_index, booked, hold_ref)
             VALUES ($1, $2, 1, false, NULL)",
        )
        .bind(trip_id)
        .bind(&seat_no)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    println!(
        "using trip {} — seeded {} synthetic seats (prefix {})",
        trip_id, args.seats, seat_prefix
    );

    // 2. Health check.
    let client = Client::builder()
        .pool_max_idle_per_host(args.concurrency)
        .timeout(Duration::from_secs(10))
        .build()?;
    let h = client
        .get(format!("{}/api/v1/healthz", args.engine_url))
        .send()
        .await?;
    if !h.status().is_success() {
        anyhow::bail!("engine healthz returned {}", h.status());
    }

    let stats = Arc::new(Mutex::new(Stats::new()));
    let sem = Arc::new(tokio::sync::Semaphore::new(args.concurrency));

    let scenario = args.scenario.clone();
    let start = Instant::now();
    let mut handles = Vec::with_capacity(args.ops);

    for i in 0..args.ops {
        let permit = sem.clone().acquire_owned().await.unwrap();
        let client = client.clone();
        let stats = stats.clone();
        let url = args.engine_url.clone();
        let secret = args.hmac_secret.clone();
        let seat_no = format!("{}{:04}", seat_prefix, i % args.seats);
        let trip = trip_id;
        let scenario = scenario.clone();

        handles.push(tokio::spawn(async move {
            let _permit = permit;
            let _ = run_one(&client, &url, &secret, trip, seat_no, &scenario, &stats).await;
        }));
    }
    join_all(handles).await;
    let elapsed = start.elapsed();

    let s = stats.lock().await;
    let total = s.ok + s.conflict + s.errors;
    let rps = total as f64 / elapsed.as_secs_f64();
    println!();
    println!(
        "[done] {} ops in {:.2}s = {:.0} req/s",
        total,
        elapsed.as_secs_f64(),
        rps
    );
    print_hist("hold", &s.hold_hist, s.ok, s.conflict);
    let label = match scenario.as_str() {
        "hold-confirm" => "confirm",
        _ => "release",
    };
    print_hist_simple(label, &s.second_hist, s.second_ok);
    println!("errors:  {}", s.errors);

    // 3. Cleanup — only the rows this run created.
    let like_pat = format!("{}%", seat_prefix);
    sqlx::query("DELETE FROM seat_holds WHERE trip_id = $1 AND seat_no LIKE $2")
        .bind(trip_id)
        .bind(&like_pat)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM seat_inventory WHERE trip_id = $1 AND seat_no LIKE $2")
        .bind(trip_id)
        .bind(&like_pat)
        .execute(&pool)
        .await?;
    println!("cleaned up {} synthetic seats", args.seats);

    Ok(())
}

async fn run_one(
    client: &Client,
    base: &str,
    secret: &str,
    trip_id: Uuid,
    seat_no: String,
    scenario: &str,
    stats: &Mutex<Stats>,
) -> Result<()> {
    // ── HOLD ──
    let body = serde_json::json!({
        "trip_id": trip_id,
        "seat_no": seat_no,
        "leg_indexes": [1],
        "operator_id": format!("loadtest-{}", &seat_no),
        "ttl_class": "short",
    });
    let body_str = serde_json::to_string(&body)?;
    let path = "/api/v1/holds";
    let headers = sign("POST", path, &body_str, secret);
    let idem = Uuid::new_v4().to_string();

    let t0 = Instant::now();
    let res = client
        .post(format!("{}{}", base, path))
        .header("Content-Type", "application/json")
        .header("X-Service-Id", headers.0)
        .header("X-Timestamp", headers.1)
        .header("X-Signature", headers.2)
        .header("Idempotency-Key", &idem)
        .body(body_str)
        .send()
        .await;
    let elapsed_us = t0.elapsed().as_micros() as u64;

    match res {
        Ok(r) if r.status().is_success() => {
            let v: serde_json::Value = r.json().await?;
            let hold_ref = v.get("hold_ref").and_then(|x| x.as_str()).unwrap_or("").to_string();

            let mut s = stats.lock().await;
            s.hold_hist.record(elapsed_us / 1000).ok();
            s.ok += 1;
            drop(s);

            // ── second op ──
            if scenario == "hold-confirm" {
                let cpath = format!("/api/v1/holds/{}/confirm", hold_ref);
                let cbody = serde_json::json!({
                    "booking_id": Uuid::new_v4(),
                    "operator_id": format!("loadtest-{}", &seat_no),
                });
                let cbody_str = serde_json::to_string(&cbody)?;
                let ch = sign("POST", &cpath, &cbody_str, secret);
                let cidem = Uuid::new_v4().to_string();
                let t1 = Instant::now();
                let cr = client
                    .post(format!("{}{}", base, cpath))
                    .header("Content-Type", "application/json")
                    .header("X-Service-Id", ch.0)
                    .header("X-Timestamp", ch.1)
                    .header("X-Signature", ch.2)
                    .header("Idempotency-Key", &cidem)
                    .body(cbody_str)
                    .send()
                    .await;
                let dt = t1.elapsed().as_micros() as u64;
                let mut s = stats.lock().await;
                s.second_hist.record(dt / 1000).ok();
                if cr.map(|r| r.status().is_success()).unwrap_or(false) {
                    s.second_ok += 1;
                }
            } else {
                // release
                let rpath = format!("/api/v1/holds/{}", hold_ref);
                let rh = sign("DELETE", &rpath, "", secret);
                let t1 = Instant::now();
                let rr = client
                    .delete(format!("{}{}", base, rpath))
                    .header("X-Service-Id", rh.0)
                    .header("X-Timestamp", rh.1)
                    .header("X-Signature", rh.2)
                    .send()
                    .await;
                let dt = t1.elapsed().as_micros() as u64;
                let mut s = stats.lock().await;
                s.second_hist.record(dt / 1000).ok();
                if rr.map(|r| r.status().is_success()).unwrap_or(false) {
                    s.second_ok += 1;
                }
            }
        }
        Ok(r) if r.status().as_u16() == 409 || r.status().as_u16() == 422 => {
            let mut s = stats.lock().await;
            s.hold_hist.record(elapsed_us / 1000).ok();
            s.conflict += 1;
        }
        Ok(_) | Err(_) => {
            let mut s = stats.lock().await;
            s.errors += 1;
        }
    }
    Ok(())
}

fn sign(method: &str, path: &str, body: &str, secret: &str) -> (String, String, String) {
    let ts = Utc::now().timestamp().to_string();
    let body_sha = {
        let mut h = Sha256::new();
        h.update(body.as_bytes());
        hex::encode(h.finalize())
    };
    let signing = format!("{}.{}.{}.{}", ts, method.to_uppercase(), path, body_sha);
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).expect("hmac key");
    mac.update(signing.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    ("loadtest".to_string(), ts, sig)
}

fn print_hist(label: &str, h: &Histogram<u64>, ok: u64, conflict: u64) {
    println!(
        "{:8} p50={}ms  p95={}ms  p99={}ms   ok={}  conflict={}",
        format!("{}:", label),
        h.value_at_quantile(0.50),
        h.value_at_quantile(0.95),
        h.value_at_quantile(0.99),
        ok,
        conflict
    );
}

fn print_hist_simple(label: &str, h: &Histogram<u64>, ok: u64) {
    println!(
        "{:8} p50={}ms  p95={}ms  p99={}ms   ok={}",
        format!("{}:", label),
        h.value_at_quantile(0.50),
        h.value_at_quantile(0.95),
        h.value_at_quantile(0.99),
        ok
    );
}
