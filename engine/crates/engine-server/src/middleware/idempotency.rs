//! Idempotency layer for write endpoints (contract §6).
//!
//! Same key + identical body → cached response replayed.
//! Same key + different body → 409 Conflict.
//! Entries TTL = 24h.
//!
//! ## Durable backend (P1 §10.3)
//!
//! The store is **Postgres-backed** so cached responses survive engine
//! restart (rolling deploy, crash, OOM kill, sleep). Each cache hit is a
//! single indexed PK lookup and each miss is one INSERT … ON CONFLICT DO
//! NOTHING — well within the latency budget for write endpoints. Expired
//! entries are filtered at read time; the engine's reaper task sweeps
//! them periodically (`reaper_task::run` calls `sweep_expired`).
//!
//! Migration: `engine/migrations/0002_idempotency_cache.sql`.

use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::error::ApiError;
use crate::middleware::hmac::ServiceIdentity;
use crate::state::AppState;

const MAX_BODY_BYTES: usize = 1 * 1024 * 1024;

/// Default TTL for cached responses (contract §6).
pub const IDEMPOTENCY_TTL: Duration = Duration::from_secs(24 * 3600);

#[derive(Clone, Debug)]
pub struct CachedResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub body_hash: String, // hash of REQUEST body that produced this response
}

#[derive(Clone)]
pub struct IdempotencyStore {
    pool: sqlx::PgPool,
    ttl: Duration,
}

impl IdempotencyStore {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            pool,
            ttl: IDEMPOTENCY_TTL,
        }
    }

    /// Read a cached response. Returns `None` for missing AND expired entries
    /// (the `expires_at > now()` filter rejects stale rows even if the
    /// reaper hasn't swept them yet).
    pub async fn get(&self, key: &str) -> Result<Option<CachedResponse>, sqlx::Error> {
        let row = sqlx::query(
            r#"
            SELECT body_hash, status_code, response_body
              FROM engine_idempotency_cache
             WHERE key = $1
               AND expires_at > now()
            "#,
        )
        .bind(key)
        .fetch_optional(&self.pool)
        .await?;

        let Some(row) = row else {
            return Ok(None);
        };

        let body_hash: String = row.try_get("body_hash")?;
        let status_code: i32 = row.try_get("status_code")?;
        let body: Vec<u8> = row.try_get("response_body")?;

        Ok(Some(CachedResponse {
            status: status_code as u16,
            body,
            body_hash,
        }))
    }

    /// Insert-or-overwrite-expired cached response.
    ///
    /// Returns `Ok(true)` if this writer either inserted a new row OR
    /// successfully overwrote an expired-but-unswept row, `Ok(false)`
    /// if another active (non-expired) row already exists for the key
    /// (concurrent winner — its body is already cached).
    ///
    /// The `WHERE … expires_at <= now()` clause on the conflict path
    /// is critical: without it, an expired row would block re-insertion
    /// for the entire window between TTL expiry and the next reaper
    /// sweep (`get` filters expired rows so the handler would re-execute
    /// on every retry, and then `put` would silently no-op).
    pub async fn put(&self, key: &str, value: CachedResponse) -> Result<bool, sqlx::Error> {
        let expires_at: DateTime<Utc> = Utc::now() + chrono::Duration::from_std(self.ttl).unwrap();
        let result = sqlx::query(
            r#"
            INSERT INTO engine_idempotency_cache
                   (key, body_hash, status_code, response_body, expires_at)
            VALUES ($1, $2, $3, $4, $5)
            ON CONFLICT (key) DO UPDATE
               SET body_hash     = EXCLUDED.body_hash,
                   status_code   = EXCLUDED.status_code,
                   response_body = EXCLUDED.response_body,
                   expires_at    = EXCLUDED.expires_at,
                   created_at    = now()
             WHERE engine_idempotency_cache.expires_at <= now()
            "#,
        )
        .bind(key)
        .bind(&value.body_hash)
        .bind(value.status as i32)
        .bind(&value.body)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() > 0)
    }

    /// Periodic cleanup — DELETE entries where `expires_at <= now()`.
    /// Returns the number of rows removed. Safe to call concurrently with
    /// reads; expired rows are already filtered by `get`.
    pub async fn sweep_expired(&self) -> Result<u64, sqlx::Error> {
        let result = sqlx::query("DELETE FROM engine_idempotency_cache WHERE expires_at <= now()")
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }
}

pub async fn layer(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    // Only protect write methods.
    let m = request.method().clone();
    if !matches!(m, Method::POST | Method::PUT | Method::PATCH | Method::DELETE) {
        return Ok(next.run(request).await);
    }

    let raw_key = match get_idempotency_key(request.headers()) {
        Some(k) => k,
        None => return Ok(next.run(request).await),
    };

    // P2 §10.6: namespace the cache key by `(svc_id, method, path,
    // raw_key)` so two services or two endpoints can never collide on
    // the same `Idempotency-Key`. svc_id comes from the HMAC layer (it
    // populates `ServiceIdentity` in extensions before this middleware
    // runs); fall back to "anonymous" only if the request somehow
    // bypassed HMAC (e.g. /healthz — but that's GET-only and never
    // reaches here).
    let svc_id = request
        .extensions()
        .get::<ServiceIdentity>()
        .map(|s| s.0.as_str())
        .unwrap_or("anonymous");
    let path = request.uri().path().to_string();
    let method_str = m.as_str().to_string();
    let key = scoped_idempotency_key(svc_id, &method_str, &path, &raw_key);

    // Buffer the body so we can hash it AND replay to the inner handler.
    let (parts, body) = request.into_parts();
    let body_bytes = to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "BODY_TOO_LARGE", "body exceeds 1MB").into_response())?;

    let body_hash = {
        let mut h = Sha256::new();
        h.update(&body_bytes);
        hex::encode(h.finalize())
    };

    let cached = state
        .idempotency
        .get(&key)
        .await
        .map_err(|e| ApiError::internal(format!("idempotency lookup failed: {e}")).into_response())?;

    if let Some(cached) = cached {
        if cached.body_hash != body_hash {
            return Err(ApiError::conflict("idempotency key reused with different body").into_response());
        }
        let mut resp = Response::new(Body::from(cached.body.clone()));
        *resp.status_mut() = StatusCode::from_u16(cached.status).unwrap_or(StatusCode::OK);
        resp.headers_mut()
            .insert("content-type", "application/json".parse().unwrap());
        resp.headers_mut()
            .insert("x-idempotent-replayed", "true".parse().unwrap());
        return Ok(resp);
    }

    let new_request = Request::from_parts(parts, Body::from(body_bytes));
    let resp = next.run(new_request).await;

    // Capture response body to cache it, then re-emit.
    let (mut parts, body) = resp.into_parts();
    let resp_bytes = to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| ApiError::internal("response body too large to cache").into_response())?;

    // Cache any deterministic response: 2xx success, 409 Conflict (seat
    // conflict / hold expired), and 422 Unprocessable Entity (incomplete
    // inventory). Non-deterministic failures (5xx, 401/403 auth, 400 body
    // parse) are not cached because a retry might legitimately succeed.
    if parts.status.is_success()
        || parts.status == StatusCode::CONFLICT
        || parts.status == StatusCode::UNPROCESSABLE_ENTITY
    {
        // Best-effort write. If the DB write fails we still return the
        // response to the caller — losing a cache write is preferable to
        // failing the original request.
        if let Err(e) = state
            .idempotency
            .put(
                &key,
                CachedResponse {
                    status: parts.status.as_u16(),
                    body: resp_bytes.to_vec(),
                    body_hash,
                },
            )
            .await
        {
            tracing::warn!(error = %e, key = %key, "idempotency cache write failed");
        }
    }

    parts.headers.insert("x-idempotent-replayed", "false".parse().unwrap());
    Ok(Response::from_parts(parts, Body::from(resp_bytes)))
}

fn get_idempotency_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Build the cache key actually written to Postgres.
///
/// Mixes `svc_id`, HTTP method, request path, and the caller's
/// `Idempotency-Key` header into a single SHA-256 digest. This
/// prevents two distinct endpoints (or two services on the same
/// engine cluster) from colliding when callers happen to choose the
/// same raw idempotency key.
///
/// Components are separated by `|` (forbidden in URL paths and not
/// emitted by valid HTTP method tokens) and length-prefixed so a
/// pathological caller can't construct two distinct tuples that
/// hash to the same digest.
fn scoped_idempotency_key(svc_id: &str, method: &str, path: &str, raw_key: &str) -> String {
    let mut h = Sha256::new();
    for part in [svc_id, method, path, raw_key] {
        h.update((part.len() as u64).to_be_bytes());
        h.update(part.as_bytes());
        h.update(b"|");
    }
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::scoped_idempotency_key;

    #[test]
    fn distinct_routes_produce_distinct_keys() {
        let a = scoped_idempotency_key("terminal", "POST", "/api/v1/holds", "K1");
        let b = scoped_idempotency_key("terminal", "POST", "/api/v1/holds/abc/confirm", "K1");
        assert_ne!(a, b);
    }

    #[test]
    fn distinct_services_produce_distinct_keys() {
        let a = scoped_idempotency_key("terminal", "POST", "/api/v1/holds", "K1");
        let b = scoped_idempotency_key("console", "POST", "/api/v1/holds", "K1");
        assert_ne!(a, b);
    }

    #[test]
    fn same_request_produces_same_key() {
        let a = scoped_idempotency_key("terminal", "POST", "/api/v1/holds", "K1");
        let b = scoped_idempotency_key("terminal", "POST", "/api/v1/holds", "K1");
        assert_eq!(a, b);
    }

    #[test]
    fn length_prefix_prevents_concat_collision() {
        // Without length prefixing, ("ab", "c", ...) and ("a", "bc", ...) would collide.
        let a = scoped_idempotency_key("ab", "c", "/p", "k");
        let b = scoped_idempotency_key("a", "bc", "/p", "k");
        assert_ne!(a, b);
    }
}
