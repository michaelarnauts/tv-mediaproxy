use actix_web::{HttpResponse, ResponseError};
use serde_json::json;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum AppError {
    #[error("Authentication error: {0}")]
    Auth(String),

    #[error("Proxy error: {0}")]
    Proxy(String),

    #[error("Internal server error: {0}")]
    Internal(String),

    #[error("Upstream service error: {0}")]
    Upstream(String),

    #[error("DASH/MPD processing error: {0}")]
    Mpd(String),

    #[error("DRM decryption error: {0}")]
    Drm(String),

    #[error("Bad request: {0}")]
    BadRequest(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Serde JSON error: {0}")]
    SerdeJsonError(#[from] serde_json::Error),
}

impl ResponseError for AppError {
    fn error_response(&self) -> HttpResponse {
        let response = match self {
            AppError::Auth(msg) => HttpResponse::Unauthorized().json(json!({ "error": msg })),
            AppError::Proxy(msg) => HttpResponse::BadGateway().json(json!({ "error": msg })),
            AppError::Internal(msg) => {
                HttpResponse::InternalServerError().json(json!({ "error": msg }))
            }
            AppError::Upstream(msg) => HttpResponse::BadGateway().json(json!({ "error": msg })),
            AppError::Mpd(msg) => HttpResponse::BadGateway().json(json!({ "error": msg })),
            AppError::Drm(msg) => HttpResponse::InternalServerError().json(json!({ "error": msg })),
            AppError::BadRequest(msg) => HttpResponse::BadRequest().json(json!({ "error": msg })),
            AppError::NotFound(msg) => HttpResponse::NotFound().json(json!({ "error": msg })),
            AppError::SerdeJsonError(err) => {
                HttpResponse::InternalServerError().json(json!({ "error": err.to_string() }))
            }
        };
        // Client errors are in the access log; failures need their reason.
        if response.status().is_server_error() {
            tracing::warn!("{self}");
        }
        response
    }
}

pub type AppResult<T> = Result<T, AppError>;
