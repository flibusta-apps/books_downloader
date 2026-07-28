use reqwest::{Body, Response};
use tempfile::SpooledTempFile;
use tokio_util::io::ReaderStream;

use crate::config;

use super::downloader::error::DownloadError;
use super::downloader::types::spooled_temp_file_into_async_read;
use super::http_client::CLIENT;

pub async fn convert_file(
    file: SpooledTempFile,
    file_type: String,
) -> Result<Response, DownloadError> {
    let body = Body::wrap_stream(ReaderStream::new(spooled_temp_file_into_async_read(file)));

    let response = CLIENT
        .post(format!("{}{}", config::CONFIG.converter_url, file_type))
        .body(body)
        .header("Authorization", &config::CONFIG.converter_api_key)
        .send()
        .await
        .map_err(|_| DownloadError::SourceUnavailable)?;

    response.error_for_status().map_err(|err| {
        let status = err.status().map(|s| s.as_u16()).unwrap_or(0);
        DownloadError::ConverterFailed(status)
    })
}
