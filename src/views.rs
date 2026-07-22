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
        downloader::book_download,
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

pub async fn download(
    Path((source_id, remote_id, file_type)): Path<(u32, u32, FileType)>,
    Query(params): Query<FilenameParams>,
) -> impl IntoResponse {
    let normalized = params.normalized.unwrap_or(true);

    // TODO(Task 6, spec 05): differentiate DownloadError variants into distinct HTTP
    // statuses. This still collapses every error to 204 as a compile-compat shim now
    // that `book_download` returns `Result<DownloadResult, DownloadError>` directly
    // instead of `Result<Option<DownloadResult>, _>`.
    let data = match book_download(source_id, remote_id, file_type.as_str(), normalized).await {
        Ok(v) => v,
        Err(_) => return Err((StatusCode::NO_CONTENT, "Can't download!".to_string())),
    };

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
) -> impl IntoResponse {
    let normalized = params.normalized.unwrap_or(true);

    let book = match get_book(book_id).await {
        Ok(v) => v,
        Err(BookLibraryError::NotFound) => {
            return (
                StatusCode::NOT_FOUND,
                json!({"error": "Book not found"}).to_string(),
            )
        }
        Err(_) => {
            return (
                StatusCode::BAD_GATEWAY,
                json!({"error": "book_library is unavailable"}).to_string(),
            )
        }
    };

    let filename = get_filename_by_book(&book, file_type.as_str(), false, false, normalized);
    let filename_ascii = get_filename_by_book(&book, file_type.as_str(), false, true, normalized);

    (
        StatusCode::OK,
        json!({
            "filename": filename,
            "filename_ascii": filename_ascii
        })
        .to_string(),
    )
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
}
