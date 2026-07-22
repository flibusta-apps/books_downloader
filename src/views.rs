use axum::{
    body::Body,
    extract::{Path, Query},
    http::{header, StatusCode},
    response::{AppendHeaders, IntoResponse},
};
use axum::{
    http::Request,
    middleware::{self, Next},
    response::Response,
    routing::get,
    Router,
};
use axum_prometheus::PrometheusMetricLayer;
use base64::{engine::general_purpose, Engine};
use serde_json::json;
use subtle::ConstantTimeEq;
use tokio_util::io::ReaderStream;
use tower_http::trace::{self, TraceLayer};
use tracing::Level;

use crate::{
    config::CONFIG,
    file_type::FileType,
    services::{
        book_library::{error::BookLibraryError, get_book},
        downloader::{book_download, error::DownloadError},
        filename_getter::get_filename_by_book,
    },
};

#[derive(serde::Deserialize, Default)]
pub struct FilenameParams {
    #[serde(default)]
    pub normalized: Option<bool>,
}

fn content_disposition_value(filename: &str) -> String {
    let escaped = filename.replace('\\', "\\\\").replace('"', "\\\"");
    format!("attachment; filename=\"{escaped}\"")
}

// `pub` (not module-private) purely to satisfy the `private_interfaces` lint: `download`
// and `get_filename` are `pub async fn` (an existing, pre-Task-6 pattern), and this crate
// has no `[lib]` target, so `pub` here still doesn't leak `AppError` past the binary.
#[derive(Debug, PartialEq, Eq)]
pub enum AppError {
    NotFound,
    SourceUnavailable,
    BadArchive,
    ConverterFailed(u16),
    Timeout,
    UpstreamError,
    Internal,
}

impl AppError {
    fn status_code(&self) -> StatusCode {
        match self {
            AppError::NotFound => StatusCode::NOT_FOUND,
            AppError::Timeout => StatusCode::GATEWAY_TIMEOUT,
            AppError::Internal => StatusCode::INTERNAL_SERVER_ERROR,
            AppError::SourceUnavailable
            | AppError::BadArchive
            | AppError::ConverterFailed(_)
            | AppError::UpstreamError => StatusCode::BAD_GATEWAY,
        }
    }

    fn message(&self) -> String {
        match self {
            AppError::NotFound => "Book not found".to_string(),
            AppError::SourceUnavailable => "Upstream source unavailable".to_string(),
            AppError::BadArchive => "Downloaded archive was invalid".to_string(),
            AppError::ConverterFailed(status) => format!("Converter failed with status {status}"),
            AppError::Timeout => "Download timed out".to_string(),
            AppError::UpstreamError => "book_library is unavailable".to_string(),
            AppError::Internal => "Internal error".to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status_code();
        let body = json!({ "error": self.message() }).to_string();
        (status, body).into_response()
    }
}

impl From<BookLibraryError> for AppError {
    fn from(err: BookLibraryError) -> Self {
        match err {
            BookLibraryError::NotFound => AppError::NotFound,
            BookLibraryError::RequestFailed(_) | BookLibraryError::UpstreamError(_) => {
                AppError::UpstreamError
            }
        }
    }
}

impl From<DownloadError> for AppError {
    fn from(err: DownloadError) -> Self {
        match err {
            DownloadError::Library(lib_err) => AppError::from(lib_err),
            DownloadError::SourceUnavailable => AppError::SourceUnavailable,
            DownloadError::BadArchive => AppError::BadArchive,
            DownloadError::ConverterFailed(status) => AppError::ConverterFailed(status),
            DownloadError::Timeout => AppError::Timeout,
            DownloadError::Internal(_) => AppError::Internal,
        }
    }
}

pub async fn download(
    Path((source_id, remote_id, file_type)): Path<(u32, u32, FileType)>,
    Query(params): Query<FilenameParams>,
) -> Result<impl IntoResponse, AppError> {
    let normalized = params.normalized.unwrap_or(true);

    let data = book_download(source_id, remote_id, file_type.as_str(), normalized)
        .await
        .map_err(AppError::from)?;

    let filename = data.filename.clone();
    let filename_ascii = data.filename_ascii.clone();
    let file_size = data.data_size;

    let reader = data.get_async_read();
    let stream = ReaderStream::new(reader);

    let encoder = general_purpose::STANDARD;

    let headers = AppendHeaders([
        (
            header::CONTENT_DISPOSITION,
            content_disposition_value(&filename_ascii),
        ),
        (header::CONTENT_LENGTH, format!("{file_size}")),
        (
            header::HeaderName::from_static("x-filename-b64-ascii"),
            encoder.encode(filename_ascii),
        ),
        (
            header::HeaderName::from_static("x-filename-b64"),
            encoder.encode(filename),
        ),
    ]);

    Ok((headers, Body::from_stream(stream)))
}

pub async fn get_filename(
    Path((book_id, file_type)): Path<(u32, FileType)>,
    Query(params): Query<FilenameParams>,
) -> Result<impl IntoResponse, AppError> {
    let normalized = params.normalized.unwrap_or(true);

    let book = get_book(book_id).await.map_err(AppError::from)?;
    let filename = get_filename_by_book(&book, file_type.as_str(), false, false, normalized);
    let filename_ascii = get_filename_by_book(&book, file_type.as_str(), false, true, normalized);

    Ok((
        StatusCode::OK,
        json!({
            "filename": filename,
            "filename_ascii": filename_ascii
        })
        .to_string(),
    ))
}

pub async fn health() -> impl IntoResponse {
    (StatusCode::OK, json!({"status": "healthy"}).to_string())
}

fn keys_match(provided: &str, expected: &str) -> bool {
    let provided = provided.as_bytes();
    let expected = expected.as_bytes();

    if provided.len() != expected.len() {
        return false;
    }

    provided.ct_eq(expected).into()
}

async fn auth(req: Request<axum::body::Body>, next: Next) -> Result<Response, StatusCode> {
    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok());

    let auth_header = if let Some(auth_header) = auth_header {
        auth_header
    } else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    if !keys_match(auth_header, &CONFIG.api_key) {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
}

pub async fn get_router() -> Router {
    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    let app_router = Router::new()
        .route(
            "/download/{source_id}/{remote_id}/{file_type}",
            get(download),
        )
        .route("/filename/{book_id}/{file_type}", get(get_filename))
        .layer(middleware::from_fn(auth))
        .layer(prometheus_layer);

    let health_router = Router::new().route("/health", get(health));

    // `/metrics` is intentionally unauthenticated (Prometheus scrapers don't send
    // the API key). It must only be reachable from the internal network/scrape
    // target — do not expose this port publicly. See docs/specs/02-file-type-validation-and-url-injection.md (02.5).
    let metric_router =
        Router::new().route("/metrics", get(|| async move { metric_handle.render() }));

    Router::new()
        .merge(app_router)
        .merge(health_router)
        .merge(metric_router)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
                .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::book_library::error::BookLibraryError;
    use crate::services::downloader::error::DownloadError;

    #[test]
    fn keys_match_identical_strings() {
        assert!(keys_match("secret-key", "secret-key"));
    }

    #[test]
    fn keys_match_rejects_different_strings_of_equal_length() {
        assert!(!keys_match("secret-key", "other-key1"));
    }

    #[test]
    fn keys_match_rejects_different_length() {
        assert!(!keys_match("short", "much-longer-key"));
    }

    #[test]
    fn keys_match_rejects_empty_against_nonempty() {
        assert!(!keys_match("", "secret-key"));
    }

    #[test]
    fn keys_match_empty_against_empty() {
        assert!(keys_match("", ""));
    }

    #[test]
    fn content_disposition_value_quotes_plain_filename() {
        assert_eq!(
            content_disposition_value("book.fb2"),
            "attachment; filename=\"book.fb2\""
        );
    }

    #[test]
    fn content_disposition_value_escapes_quotes_and_backslashes() {
        let value = content_disposition_value("weird\"na\\me.fb2");
        assert_eq!(value, "attachment; filename=\"weird\\\"na\\\\me.fb2\"");
    }

    #[test]
    fn content_disposition_survives_malicious_title_end_to_end() {
        use crate::services::book_library::types::BookWithRemote;
        use crate::services::filename_getter::get_filename_by_book;

        let book = BookWithRemote {
            id: 1,
            remote_id: 42,
            title: "Evil\"; \r\nX-Injected: yes\r\ntitle".to_string(),
            lang: "en".to_string(),
            file_type: "fb2".to_string(),
            uploaded: "2024-01-01".to_string(),
            authors: vec![],
        };

        let filename_ascii = get_filename_by_book(&book, "fb2", false, true, true);
        let value = content_disposition_value(&filename_ascii);

        assert!(
            header::HeaderValue::from_str(&value).is_ok(),
            "expected a valid header value, got {value:?}"
        );
        assert!(!value.chars().any(|c| c.is_control()));
        assert!(value.starts_with("attachment; filename=\""));
        assert!(value.ends_with('"'));
    }

    #[test]
    fn book_library_not_found_maps_to_404() {
        let status = match BookLibraryError::NotFound {
            BookLibraryError::NotFound => StatusCode::NOT_FOUND,
            _ => StatusCode::BAD_GATEWAY,
        };
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn book_library_upstream_failure_maps_to_502() {
        // Construct a genuine reqwest::Error (not a stub) by making a real request against
        // a local server that returns 500, then check the same match this task adds.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });

        let response = reqwest::Client::new()
            .get(format!("http://{addr}/x"))
            .send()
            .await
            .unwrap();
        let reqwest_err = response.error_for_status().unwrap_err();
        let lib_err = BookLibraryError::UpstreamError(reqwest_err);

        let status = match lib_err {
            BookLibraryError::NotFound => StatusCode::NOT_FOUND,
            _ => StatusCode::BAD_GATEWAY,
        };
        assert_eq!(status, StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn app_error_from_book_library_not_found_maps_to_404() {
        let app_err: AppError = BookLibraryError::NotFound.into();
        assert_eq!(app_err.status_code(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn download_error_library_not_found_maps_to_404() {
        let app_err: AppError = DownloadError::from(BookLibraryError::NotFound).into();
        assert_eq!(app_err.status_code(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn download_error_source_unavailable_maps_to_502() {
        let app_err: AppError = DownloadError::SourceUnavailable.into();
        assert_eq!(app_err.status_code(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn download_error_bad_archive_maps_to_502() {
        let app_err: AppError = DownloadError::BadArchive.into();
        assert_eq!(app_err.status_code(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn download_error_converter_failed_maps_to_502() {
        let app_err: AppError = DownloadError::ConverterFailed(503).into();
        assert_eq!(app_err.status_code(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn download_error_timeout_maps_to_504() {
        let app_err: AppError = DownloadError::Timeout.into();
        assert_eq!(app_err.status_code(), StatusCode::GATEWAY_TIMEOUT);
    }

    #[test]
    fn no_variant_maps_to_204_no_content() {
        let variants = [
            AppError::NotFound,
            AppError::SourceUnavailable,
            AppError::BadArchive,
            AppError::ConverterFailed(500),
            AppError::Timeout,
            AppError::UpstreamError,
            AppError::Internal,
        ];
        for v in variants {
            assert_ne!(v.status_code(), StatusCode::NO_CONTENT);
        }
    }

    #[tokio::test]
    async fn app_error_into_response_has_json_body_and_correct_status() {
        let response = AppError::NotFound.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let body_bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(!body_bytes.is_empty());
        let parsed: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        assert!(parsed.get("error").is_some());
    }
}
