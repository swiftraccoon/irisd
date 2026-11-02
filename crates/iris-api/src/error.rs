//! Error type mapped to HTTP responses.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;

/// A handler failure, rendered as a JSON `{ "error": … }` body.
#[derive(Debug, Error)]
pub enum ApiError {
    /// The request was malformed.
    #[error("{0}")]
    BadRequest(String),
    /// No such camera, stream, or segment.
    #[error("not found")]
    NotFound,
    /// The catalog failed.
    #[error("catalog: {0}")]
    Catalog(#[from] iris_catalog::CatalogError),
    /// A file could not be read.
    #[error("storage: {0}")]
    Io(#[from] std::io::Error),
    /// An upstream camera request failed.
    #[error("upstream: {0}")]
    Upstream(String),
    /// The resource exists but is not ready within the time a client waits.
    #[error("unavailable: {0}")]
    Unavailable(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Catalog(_) | Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Upstream(_) => StatusCode::BAD_GATEWAY,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            tracing::error!(error = %self, "request failed");
        }
        (
            status,
            Json(ErrorBody {
                error: self.to_string(),
            }),
        )
            .into_response()
    }
}
