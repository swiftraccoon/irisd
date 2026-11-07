//! Error type mapped to HTTP responses.

use std::sync::atomic::{AtomicU64, Ordering};

use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use thiserror::Error;

/// Process-wide identifiers pairing a generic HTTP 500 body with its logged detail.
static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// A handler failure, rendered as a JSON `{ "error": … }` body.
///
/// HTTP 500 bodies never carry the failure's text: they read
/// `{"error":"internal error","request_id":<n>}`, and the detail is logged at warn level with
/// the same process-wide `request_id`. HTTP 400, 404, 502, and 503 bodies keep their message.
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
    /// An admission budget is full: one media response class, the WebSocket socket caps, or
    /// a camera's single in-flight snapshot. Rendered as HTTP 503 with `Retry-After: 1`.
    #[error("busy: {0}")]
    Busy(String),
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    request_id: Option<u64>,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self {
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Catalog(iris_catalog::CatalogError::Overloaded) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::Catalog(_) | Self::Io(_) => StatusCode::INTERNAL_SERVER_ERROR,
            Self::Upstream(_) => StatusCode::BAD_GATEWAY,
            Self::Unavailable(_) | Self::Busy(_) => StatusCode::SERVICE_UNAVAILABLE,
        };
        if status == StatusCode::INTERNAL_SERVER_ERROR {
            let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(request_id, error = %self, "request failed");
            let body = ErrorBody {
                error: "internal error".to_owned(),
                request_id: Some(request_id),
            };
            return (status, Json(body)).into_response();
        }
        let busy = matches!(self, Self::Busy(_));
        let body = ErrorBody {
            error: self.to_string(),
            request_id: None,
        };
        let mut response = (status, Json(body)).into_response();
        if busy {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn body_text(response: Response) -> String {
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("error body");
        String::from_utf8(bytes.to_vec()).expect("UTF-8 error body")
    }

    #[test]
    fn catalog_overload_is_service_unavailable_without_retry_after() {
        let response = ApiError::Catalog(iris_catalog::CatalogError::Overloaded).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(response.headers().get(header::RETRY_AFTER).is_none());
    }

    #[tokio::test]
    async fn busy_admission_is_service_unavailable_with_retry_after() {
        let response =
            ApiError::Busy("too many concurrent streaming media responses".into()).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::RETRY_AFTER], "1");
        assert_eq!(
            body_text(response).await,
            r#"{"error":"busy: too many concurrent streaming media responses"}"#
        );
    }

    #[tokio::test]
    async fn internal_errors_return_a_generic_body_with_a_request_id() {
        let first =
            ApiError::Io(std::io::Error::other("detail-marker /synthetic/path")).into_response();
        assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let first: serde_json::Value =
            serde_json::from_str(&body_text(first).await).expect("JSON error body");
        assert_eq!(first["error"], "internal error");
        let first_id = first["request_id"].as_u64().expect("numeric request id");
        let rendered = first.to_string();
        assert!(!rendered.contains("detail-marker"));
        assert!(!rendered.contains("storage"));
        assert!(!rendered.contains("/synthetic/path"));

        let second = ApiError::Catalog(iris_catalog::CatalogError::Closed).into_response();
        let second: serde_json::Value =
            serde_json::from_str(&body_text(second).await).expect("JSON error body");
        assert_eq!(second["error"], "internal error");
        assert!(second["request_id"].as_u64().expect("numeric request id") > first_id);
    }

    #[tokio::test]
    async fn client_errors_keep_their_message() {
        assert_eq!(
            body_text(ApiError::NotFound.into_response()).await,
            r#"{"error":"not found"}"#
        );
        assert_eq!(
            body_text(ApiError::BadRequest("bad 't'".into()).into_response()).await,
            r#"{"error":"bad 't'"}"#
        );
    }
}
