use reqwest::{Body, Response};
use std::time::Duration;
use tempfile::SpooledTempFile;
use tokio_util::io::ReaderStream;

use crate::config;

use super::downloader::types::spooled_temp_file_into_async_read;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

pub async fn convert_file(file: SpooledTempFile, file_type: String) -> Option<Response> {
    let body = Body::wrap_stream(ReaderStream::new(spooled_temp_file_into_async_read(file)));

    let client = reqwest::Client::builder()
        .connect_timeout(config::CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .ok()?;

    let response = client
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
