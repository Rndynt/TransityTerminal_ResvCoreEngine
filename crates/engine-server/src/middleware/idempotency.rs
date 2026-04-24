//! Idempotency layer for write endpoints (contract §6).
//!
//! Same key + identical body → cached response replayed.
//! Same key + different body → 409 Conflict.
//! Entries TTL = 24h, capacity bounded.
//!
//! ## Volatility note
//!
//! The store is **in-process Moka cache** — it does NOT survive engine
//! restart. If the engine crashes / is redeployed within the 24h replay
//! window, an `Idempotency-Key` previously seen will look fresh and the
//! request will be re-executed (e.g. a re-tried hold could create a
//! second hold). For sidecar deployments with long uptime this is
//! acceptable; for stricter guarantees swap this for a Redis-backed
//! store using `SET NX` with TTL=24h.

use std::time::Duration;

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use moka::future::Cache;
use sha2::{Digest, Sha256};

use crate::error::ApiError;
use crate::state::AppState;

const MAX_BODY_BYTES: usize = 1 * 1024 * 1024;

#[derive(Clone, Debug)]
pub struct CachedResponse {
    pub status: u16,
    pub body: Vec<u8>,
    pub body_hash: String, // hash of REQUEST body that produced this response
}

#[derive(Clone)]
pub struct IdempotencyStore {
    cache: Cache<String, CachedResponse>,
}

impl IdempotencyStore {
    pub fn new(max_capacity: u64) -> Self {
        Self {
            cache: Cache::builder()
                .max_capacity(max_capacity)
                .time_to_live(Duration::from_secs(24 * 3600))
                .build(),
        }
    }

    pub async fn get(&self, key: &str) -> Option<CachedResponse> {
        self.cache.get(key).await
    }

    pub async fn put(&self, key: String, value: CachedResponse) {
        self.cache.insert(key, value).await;
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

    let key = match get_idempotency_key(request.headers()) {
        Some(k) => k,
        None => return Ok(next.run(request).await),
    };

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

    if let Some(cached) = state.idempotency.get(&key).await {
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

    if parts.status.is_success() || parts.status == StatusCode::CONFLICT {
        state
            .idempotency
            .put(
                key,
                CachedResponse {
                    status: parts.status.as_u16(),
                    body: resp_bytes.to_vec(),
                    body_hash,
                },
            )
            .await;
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
