use reqwest::{Body, Response};
use tempfile::SpooledTempFile;
use tokio_util::io::ReaderStream;

use crate::config;

use super::downloader::error::DownloadError;
use super::downloader::types::spooled_temp_file_into_async_read;
use super::http_client::CLIENT;

/// Sends the FB2 `file` to the converter, asking for `target_type` (epub/mobi).
pub async fn convert_file(
    file: SpooledTempFile,
    target_type: String,
) -> Result<Response, DownloadError> {
    let body = Body::wrap_stream(ReaderStream::new(spooled_temp_file_into_async_read(file)));

    let url = config::join_url(&config::CONFIG.converter_url, &target_type);

    let response = CLIENT
        .post(url)
        .body(body)
        .header("Authorization", &config::CONFIG.converter_api_key)
        .timeout(config::CONFIG.converter_timeout)
        .send()
        .await
        .map_err(|_| DownloadError::ConverterUnavailable)?;

    response.error_for_status().map_err(|err| {
        let status = err.status().map(|s| s.as_u16()).unwrap_or(0);
        DownloadError::ConverterFailed(status)
    })
}
