//! JSON error envelope.
//!
//! Every failure maps to `{ "error": ..., "error_description": ... }` with the correct
//! status code; 401s additionally carry `WWW-Authenticate: Bearer`.

use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    /// Malformed request body / query.
    #[error("invalid_request: {0}")]
    InvalidRequest(String),

    /// Missing/invalid ingest bearer token.
    #[error("unauthorized: {0}")]
    Unauthorized(String),

    /// Unexpected internal failure (store I/O, rendering).
    #[error("server_error: {0}")]
    Internal(String),
}

impl AppError {
    fn parts(&self) -> (StatusCode, &'static str, String, bool) {
        match self {
            AppError::InvalidRequest(d) => {
                (StatusCode::BAD_REQUEST, "invalid_request", d.clone(), false)
            }
            AppError::Unauthorized(d) => {
                (StatusCode::UNAUTHORIZED, "unauthorized", d.clone(), true)
            }
            AppError::Internal(d) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                d.clone(),
                false,
            ),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error, description, www_authenticate) = self.parts();
        let body = Json(serde_json::json!({
            "error": error,
            "error_description": description,
        }));
        let mut response = (status, body).into_response();
        if www_authenticate {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
        }
        response
    }
}
