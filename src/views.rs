use axum::{
    body::StreamBody,
    extract::Path,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, AppendHeaders},
};
use tokio_util::io::ReaderStream;

use crate::{config, services::{book_library::get_book, filename_getter::get_filename_by_book, downloader::book_download}};

pub async fn download(
    Path((source_id, remote_id, file_type)): Path<(u32, u32, String)>,
    headers: HeaderMap
) -> impl IntoResponse {
    let config_api_key = config::CONFIG.api_key.clone();

    let api_key = match headers.get("Authorization") {
        Some(v) => v,
        None => return Err((StatusCode::FORBIDDEN, "No api-key!".to_string())),
    };

    if config_api_key != api_key.to_str().unwrap() {
        return Err((StatusCode::FORBIDDEN, "Wrong api-key!".to_string()))
    }

    let download_result = match book_download(source_id, remote_id, file_type.as_str()).await {
        Ok(v) => v,
        Err(err) => {
            return Err((StatusCode::NO_CONTENT, "Can't download!".to_string()))
        },
    };

    let (data, filename) = match download_result {
        Some(v) => v,
        None => return Err((StatusCode::NO_CONTENT, "Can't download!".to_string())),
    };

    let reader = data.get_async_read();
    let stream = ReaderStream::new(reader);
    let body = StreamBody::new(stream);

    let headers = AppendHeaders([
        (header::CONTENT_DISPOSITION, format!("attachment; filename={filename}"))
    ]);

    Ok((headers, body))
}

pub async fn get_filename(
    Path((book_id, file_type)): Path<(u32, String)>,
    headers: HeaderMap
) -> (StatusCode, String){
    let config_api_key = config::CONFIG.api_key.clone();

    let api_key = match headers.get("Authorization") {
        Some(v) => v,
        None => return (StatusCode::FORBIDDEN, "No api-key!".to_string()),
    };

    if config_api_key != api_key.to_str().unwrap() {
        return (StatusCode::FORBIDDEN, "Wrong api-key!".to_string())
    }

    let filename = match get_book(book_id).await {
        Ok(book) => get_filename_by_book(&book, file_type.as_str(), false),
        Err(_) => return (StatusCode::BAD_REQUEST, "Book not found!".to_string()),
    };

    (StatusCode::OK, filename)
}