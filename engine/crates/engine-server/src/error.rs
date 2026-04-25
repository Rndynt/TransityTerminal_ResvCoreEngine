use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    /// Optional structured payload merged into the response body under
    /// `details`. Use for shape-specific extras (e.g. `conflict_seats`
    /// for SEAT_CONFLICT). Stays `None` for most errors.
    pub details: Option<Value>,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: None,
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "BAD_REQUEST", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "CONFLICT", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "INTERNAL", message)
    }

    /// Attach structured details to this error.
    #[allow(dead_code)]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }
}

impl IntoResponse for ApiError {
    /// Engine error body shape (P2 §10.5) — *flat* and uniform across
    /// every failure path. Mirrors the success/failure shape used by
    /// the hold endpoint so a single TT-side parser handles both.
    ///
    /// ```json
    /// {
    ///   "success": false,
    ///   "reason":  "BAD_REQUEST" | "UNAUTHORIZED" | "CONFLICT" | ...,
    ///   "message": "human readable",
    ///   "details": { ... }   // optional, omitted when null
    /// }
    /// ```
    fn into_response(self) -> Response {
        let mut body = json!({
            "success": false,
            "reason":  self.code,
            "message": self.message,
        });
        if let Some(d) = self.details {
            // Safe unwrap: body was just constructed as an Object.
            body.as_object_mut().unwrap().insert("details".to_string(), d);
        }
        (self.status, Json(body)).into_response()
    }
}

impl From<engine_core::EngineError> for ApiError {
    fn from(e: engine_core::EngineError) -> Self {
        match e {
            engine_core::EngineError::HoldExpiredOrMissing => {
                ApiError::new(StatusCode::CONFLICT, "HOLD_EXPIRED_OR_MISSING", e.to_string())
            }
            other => ApiError::internal(other.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;

    async fn body_json(resp: Response) -> Value {
        let bytes = to_bytes(resp.into_body(), 4096).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn flat_shape_bad_request() {
        let resp = ApiError::bad_request("missing field x").into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert_eq!(v["success"], false);
        assert_eq!(v["reason"], "BAD_REQUEST");
        assert_eq!(v["message"], "missing field x");
        assert!(v.get("details").is_none(), "details omitted when None");
        assert!(v.get("error").is_none(), "no nested error wrapper");
    }

    #[tokio::test]
    async fn flat_shape_with_details() {
        let resp = ApiError::conflict("seat busy")
            .with_details(json!({ "conflict_seats": ["12A"] }))
            .into_response();
        let v = body_json(resp).await;
        assert_eq!(v["success"], false);
        assert_eq!(v["reason"], "CONFLICT");
        assert_eq!(v["details"]["conflict_seats"][0], "12A");
    }
}
