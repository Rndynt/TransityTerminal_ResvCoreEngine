use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{middleware as axum_mw, Json, Router};
use serde::Deserialize;
use serde_json::json;
use uuid::Uuid;

use crate::error::ApiError;
use crate::middleware::{hmac, idempotency};
use crate::state::AppState;
use engine_core::{
    atomic_hold, cancel_booking_seats, confirm_booking, get_inventory_snapshot,
    release_hold_by_ref, AtomicHoldResult, SeatHoldRequest,
};

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/api/v1/healthz", get(healthz))
        .route("/api/v1/holds", post(post_hold))
        .route("/api/v1/holds/:hold_ref", delete(delete_hold))
        .route("/api/v1/holds/:hold_ref/confirm", post(post_confirm))
        .route("/api/v1/cancel-seats", post(post_cancel_seats))
        .route("/api/v1/inventory/:trip_id", get(get_snapshot))
        .layer(axum_mw::from_fn_with_state(state.clone(), idempotency::layer))
        .layer(axum_mw::from_fn_with_state(state.clone(), hmac::verify))
        .with_state(state);

    api
}

async fn healthz() -> impl IntoResponse {
    Json(json!({ "status": "ok", "service": "reservation-engine" }))
}

async fn post_hold(
    State(state): State<AppState>,
    Json(req): Json<SeatHoldRequest>,
) -> Result<(StatusCode, Json<AtomicHoldResult>), ApiError> {
    if req.leg_indexes.is_empty() {
        return Err(ApiError::bad_request("leg_indexes must not be empty"));
    }
    if req.seat_no.trim().is_empty() {
        return Err(ApiError::bad_request("seat_no required"));
    }
    let result = atomic_hold(&state.pool, &*state.publisher, req).await?;
    let status = if result.is_success() {
        StatusCode::CREATED
    } else {
        StatusCode::CONFLICT
    };
    Ok((status, Json(result)))
}

async fn delete_hold(
    State(state): State<AppState>,
    Path(hold_ref): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let r = release_hold_by_ref(&state.pool, &*state.publisher, &hold_ref).await?;
    Ok(Json(json!({ "success": r.success })))
}

#[derive(Debug, Deserialize)]
pub struct ConfirmBody {
    pub booking_id: String,
}

async fn post_confirm(
    State(state): State<AppState>,
    Path(hold_ref): Path<String>,
    Json(body): Json<ConfirmBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let r = confirm_booking(&state.pool, &*state.publisher, &hold_ref, &body.booking_id).await?;
    Ok(Json(json!({
        "success": r.success,
        "conflict": r.conflict,
    })))
}

#[derive(Debug, Deserialize)]
pub struct CancelBody {
    pub trip_id: Uuid,
    pub seat_no: String,
    pub leg_indexes: Vec<i32>,
}

async fn post_cancel_seats(
    State(state): State<AppState>,
    Json(body): Json<CancelBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let r = cancel_booking_seats(
        &state.pool,
        &*state.publisher,
        body.trip_id,
        &body.seat_no,
        &body.leg_indexes,
    )
    .await?;
    Ok(Json(json!({ "success": r.success })))
}

async fn get_snapshot(
    State(state): State<AppState>,
    Path(trip_id): Path<Uuid>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let snap = get_inventory_snapshot(&state.pool, trip_id).await?;
    Ok(Json(serde_json::to_value(snap).unwrap()))
}
