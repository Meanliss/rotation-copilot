pub mod admin;
pub mod proxy;

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Standard error response (Anthropic-shaped)
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "Unauthorized")
    }

    pub fn not_found(msg: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, msg)
    }

    pub fn bad_request(msg: &str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, msg)
    }

    pub fn rate_limited() -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, "Rate limit exceeded")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let error_type = match self.status.as_u16() {
            400 => "invalid_request_error",
            401 => "authentication_error",
            403 => "permission_error",
            404 => "not_found_error",
            413 => "request_too_large",
            429 => "rate_limit_error",
            529 => "overloaded_error",
            _ => "api_error",
        };

        let body = json!({
            "type": "error",
            "error": {
                "type": error_type,
                "message": self.message
            }
        });

        (self.status, axum::Json(body)).into_response()
    }
}
