//! Service-to-service auth (contract §7).
//!
//! Headers:
//!   X-Service-Id: terminal
//!   X-Timestamp:  <unix_seconds>
//!   X-Signature:  <hex>  (HMAC-SHA256 over "{ts}.{method}.{path}.{body_sha256}")
//!
//! Reject if |now - timestamp| > 30s (configurable).

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::ApiError;
use crate::state::AppState;

const MAX_BODY_BYTES: usize = 1 * 1024 * 1024; // 1 MiB cap on signed body

type HmacSha256 = Hmac<Sha256>;

pub async fn verify(
    State(state): State<AppState>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, Response> {
    // Allow GET /healthz to bypass auth for liveness probes.
    let path = request.uri().path().to_string();
    if path == "/api/v1/healthz" {
        return Ok(next.run(request).await);
    }

    let method = request.method().clone();
    let headers = request.headers().clone();

    let svc_id = headers
        .get("x-service-id")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::unauthorized("missing X-Service-Id").into_response())?;

    // P3 §10.13: defense-in-depth allowlist. If a partial secret leak
    // ever occurs, an attacker can't trivially impersonate another
    // service without also knowing a valid svc_id from the configured
    // operator list. Empty allowlist = check disabled (back-compat).
    if !state.allowed_service_ids.is_empty()
        && !state.allowed_service_ids.iter().any(|allowed| allowed == svc_id)
    {
        return Err(ApiError::unauthorized("X-Service-Id not in allowlist").into_response());
    }

    let signature_hex = headers
        .get("x-signature")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::unauthorized("missing X-Signature").into_response())?;

    let timestamp_str = headers
        .get("x-timestamp")
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| ApiError::unauthorized("missing X-Timestamp").into_response())?;

    let timestamp: i64 = timestamp_str
        .parse()
        .map_err(|_| ApiError::unauthorized("invalid X-Timestamp").into_response())?;

    let now = chrono::Utc::now().timestamp();
    if (now - timestamp).abs() > state.hmac_skew_secs {
        return Err(ApiError::unauthorized("timestamp skew exceeded").into_response());
    }

    // Buffer body for signing + downstream re-injection.
    let (parts, body) = request.into_parts();
    let bytes = to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "BODY_TOO_LARGE", "body exceeds 1MB").into_response())?;

    let body_sha = {
        let mut h = Sha256::new();
        h.update(&bytes);
        hex::encode(h.finalize())
    };

    // Method is uppercased explicitly for stable signing across HTTP libs.
    let signing_string = format!(
        "{ts}.{method}.{path}.{body}",
        ts = timestamp,
        method = method.as_str().to_uppercase(),
        path = path,
        body = body_sha
    );

    let mut mac = HmacSha256::new_from_slice(state.hmac_secret.as_bytes())
        .map_err(|_| ApiError::internal("invalid HMAC key").into_response())?;
    mac.update(signing_string.as_bytes());
    let expected = mac.finalize().into_bytes();

    let got = hex::decode(signature_hex)
        .map_err(|_| ApiError::unauthorized("invalid signature encoding").into_response())?;

    if got.len() != expected.len() || got.ct_eq(&expected).unwrap_u8() == 0 {
        return Err(ApiError::unauthorized("signature mismatch").into_response());
    }

    tracing::debug!(svc_id, "hmac verified");

    let mut new_request = Request::from_parts(parts, Body::from(bytes));
    // Stash decoded svc id for downstream handlers if needed.
    new_request.extensions_mut().insert(ServiceIdentity(svc_id.to_string()));

    Ok(next.run(new_request).await)
}

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct ServiceIdentity(pub String);
