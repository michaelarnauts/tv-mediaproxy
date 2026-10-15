use actix_web::http::StatusCode;
use actix_web::{HttpResponse, ResponseError};

#[derive(thiserror::Error, Debug)]
pub enum AppError {
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Upstream(String),
    #[error("{0}")]
    Internal(String),
}

impl ResponseError for AppError {
    fn status_code(&self) -> StatusCode {
        match self {
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Upstream(_) => StatusCode::BAD_GATEWAY,
            AppError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    fn error_response(&self) -> HttpResponse {
        // Client errors are in the access log; failures need their reason.
        if self.status_code().is_server_error() {
            tracing::warn!("{self}");
        }
        HttpResponse::build(self.status_code()).body(self.to_string())
    }
}

pub type AppResult<T> = Result<T, AppError>;
