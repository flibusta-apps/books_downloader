use axum::{
    body::StreamBody,
    extract::Path,
    http::{header, StatusCode},
    response::{AppendHeaders, IntoResponse},
};
use axum::{
    http::{self, Request},
    middleware::{self, Next},
    response::Response,
    routing::get,
    Router,
};
use axum_prometheus::PrometheusMetricLayer;
use base64::{engine::general_purpose, Engine};
use tokio_util::io::ReaderStream;
use tower_http::trace::{self, TraceLayer};
use tracing::Level;

use crate::{
    config::CONFIG,
    services::{
        book_library::get_book, downloader::book_download, filename_getter::get_filename_by_book,
    },
};


pub async fn download(
    Path((source_id, remote_id, file_type)): Path<(u32, u32, String)>,
) -> impl IntoResponse {
    let download_result = match book_download(source_id, remote_id, file_type.as_str()).await {
        Ok(v) => v,
        Err(_) => return Err((StatusCode::NO_CONTENT, "Can't download!".to_string())),
    };

    let data = match download_result {
        Some(v) => v,
        None => return Err((StatusCode::NO_CONTENT, "Can't download!".to_string())),
    };

    let filename = data.filename.clone();
    let filename_ascii = data.filename_ascii.clone();
    let file_size = data.data_size;

    let reader = data.get_async_read();
    let stream = ReaderStream::new(reader);
    let body = StreamBody::new(stream);

    let encoder = general_purpose::STANDARD;

    let headers = AppendHeaders([
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename={filename_ascii}"),
        ),
        (
            header::CONTENT_LENGTH,
            format!("{file_size}")
        ),
        (
            header::HeaderName::from_static("x-filename"),
            encoder.encode(filename_ascii),
        ),
        (
            header::HeaderName::from_static("x-filename-b64"),
            encoder.encode(filename),
        ),
    ]);

    Ok((headers, body))
}

pub async fn get_filename(Path((book_id, file_type)): Path<(u32, String)>) -> (StatusCode, String) {
    let filename = match get_book(book_id).await {
        Ok(book) => get_filename_by_book(&book, file_type.as_str(), false, false),
        Err(_) => return (StatusCode::BAD_REQUEST, "Book not found!".to_string()),
    };

    (StatusCode::OK, filename)
}

async fn auth<B>(req: Request<B>, next: Next<B>) -> Result<Response, StatusCode> {
    let auth_header = req
        .headers()
        .get(http::header::AUTHORIZATION)
        .and_then(|header| header.to_str().ok());

    let auth_header = if let Some(auth_header) = auth_header {
        auth_header
    } else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    if auth_header != CONFIG.api_key {
        return Err(StatusCode::UNAUTHORIZED);
    }

    Ok(next.run(req).await)
}

pub async fn get_router() -> Router {
    let (prometheus_layer, metric_handle) = PrometheusMetricLayer::pair();

    let app_router = Router::new()
        .route("/download/:source_id/:remote_id/:file_type", get(download))
        .route("/filename/:book_id/:file_type", get(get_filename))
        .layer(middleware::from_fn(auth))
        .layer(prometheus_layer);

    let metric_router =
        Router::new().route("/metrics", get(|| async move { metric_handle.render() }));

    Router::new()
        .nest("/", app_router)
        .nest("/", metric_router)
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(trace::DefaultMakeSpan::new().level(Level::INFO))
                .on_response(trace::DefaultOnResponse::new().level(Level::INFO)),
        )
}
