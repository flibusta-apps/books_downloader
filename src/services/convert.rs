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

    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    Err(converter_status_error(status.as_u16()))
}

/// Maps a non-success converter status to an error. None of these are caused by
/// a mirror, so failover must stop on all of them.
/// - 422: the conversion itself failed (bad input) -> `ConverterFailed`
/// - 408 (body read timeout) / 503 (queue full) -> `ConverterUnavailable`
/// - anything else -> `ConverterFailed(status)`
pub(crate) fn converter_status_error(status: u16) -> DownloadError {
    match status {
        408 | 503 => DownloadError::ConverterUnavailable,
        other => DownloadError::ConverterFailed(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converter_status_mapping() {
        assert!(matches!(
            converter_status_error(422),
            DownloadError::ConverterFailed(422)
        ));
        assert!(matches!(
            converter_status_error(408),
            DownloadError::ConverterUnavailable
        ));
        assert!(matches!(
            converter_status_error(503),
            DownloadError::ConverterUnavailable
        ));
        assert!(!converter_status_error(422).is_mirror_failure());
        assert!(!converter_status_error(503).is_mirror_failure());
    }
}
