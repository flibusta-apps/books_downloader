use reqwest::{Body, Response};
use tempfile::SpooledTempFile;
use tokio_util::io::ReaderStream;

use crate::config;

use super::downloader::types::SpooledTempAsyncRead;

pub async fn convert_file(file: SpooledTempFile, file_type: String) -> Option<Response> {
    let body = Body::wrap_stream(ReaderStream::new(SpooledTempAsyncRead::new(file)));

    let response = reqwest::Client::new()
        .post(format!("{}{}", config::CONFIG.converter_url, file_type))
        .body(body)
        .header("Authorization", &config::CONFIG.converter_api_key)
        .send()
        .await;

    let response = match response {
        Ok(v) => v,
        Err(_) => return None,
    };

    let response = match response.error_for_status() {
        Ok(v) => v,
        Err(_) => return None,
    };

    Some(response)
}
