use bytes::Buf;
use reqwest::Response;
use tempfile::SpooledTempFile;

use std::io::{Seek, SeekFrom, Write};

use super::error::DownloadError;
use super::types::Data;

pub fn parse_content_length(headers: &reqwest::header::HeaderMap) -> Option<usize> {
    headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

pub fn content_length(response: &Response) -> Option<usize> {
    parse_content_length(response.headers())
}

/// Flush buffered chunks to the spooled temp file once this many bytes are pending.
const WRITE_BATCH_BYTES: usize = 256 * 1024;

async fn write_blocking(
    mut file: SpooledTempFile,
    buf: Vec<u8>,
) -> Result<SpooledTempFile, DownloadError> {
    tokio::task::spawn_blocking(move || file.write_all(&buf).map(|_| file))
        .await
        .map_err(|err| DownloadError::Internal(format!("temp file write task failed: {err}")))?
        .map_err(|err| DownloadError::LocalIo(err.to_string()))
}

pub async fn response_to_tempfile(
    res: &mut Response,
    max_bytes: usize,
    spool_threshold: usize,
) -> Result<(SpooledTempFile, usize), DownloadError> {
    if let Some(declared) = content_length(res) {
        if declared > max_bytes {
            return Err(DownloadError::TooLarge);
        }
    }

    let mut tmp_file = tempfile::spooled_tempfile(spool_threshold);
    let mut pending: Vec<u8> = Vec::new();

    let mut data_size: usize = 0;

    loop {
        let data = match res.chunk().await {
            Ok(Some(v)) => v,
            Ok(None) => break,
            Err(_) => return Err(DownloadError::SourceUnavailable),
        };

        data_size += data.len();

        if data_size > max_bytes {
            return Err(DownloadError::TooLarge);
        }

        pending.extend_from_slice(data.chunk());

        if pending.len() >= WRITE_BATCH_BYTES {
            tmp_file = write_blocking(tmp_file, std::mem::take(&mut pending)).await?;
        }
    }

    if !pending.is_empty() {
        tmp_file = write_blocking(tmp_file, pending).await?;
    }

    let tmp_file = tokio::task::spawn_blocking(move || {
        let mut f = tmp_file;
        f.seek(SeekFrom::Start(0)).map(|_| f)
    })
    .await
    .map_err(|err| DownloadError::Internal(format!("temp file seek task failed: {err}")))?
    .map_err(|err| DownloadError::LocalIo(err.to_string()))?;

    Ok((tmp_file, data_size))
}

pub async fn response_to_download_data(
    mut response: Response,
    max_bytes: usize,
) -> Result<(Data, usize), DownloadError> {
    if let Some(size) = content_length(&response) {
        if size > max_bytes {
            return Err(DownloadError::TooLarge);
        }
        // The body is streamed to the client and truncated to `size` bytes
        // (see `DownloadResult::get_async_read`), so at most `size <= max_bytes`
        // bytes are ever forwarded.
        return Ok((Data::Response(response), size));
    }

    let (tmp_file, size) = response_to_tempfile(&mut response, max_bytes, 5 * 1024 * 1024).await?;
    Ok((Data::SpooledTempFile(tmp_file), size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, CONTENT_LENGTH};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn response_from_raw(raw: Vec<u8>) -> Response {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(&raw).await;
                let _ = socket.shutdown().await;
            }
        });
        reqwest::get(format!("http://{addr}")).await.unwrap()
    }

    #[tokio::test]
    async fn declared_content_length_over_limit_is_rejected() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc".to_vec();
        let response = response_from_raw(raw).await;
        let result = response_to_download_data(response, 10).await;
        assert!(matches!(result, Err(DownloadError::TooLarge)));
    }

    #[tokio::test]
    async fn declared_content_length_within_limit_is_streamed() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc".to_vec();
        let response = response_from_raw(raw).await;
        let (data, size) = response_to_download_data(response, 10).await.unwrap();
        assert_eq!(size, 3);
        assert!(matches!(data, Data::Response(_)));
    }

    #[tokio::test]
    async fn tempfile_buffering_rejects_declared_oversize() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nabc".to_vec();
        let mut response = response_from_raw(raw).await;
        let result = response_to_tempfile(&mut response, 10, 1024).await;
        assert!(matches!(result, Err(DownloadError::TooLarge)));
    }

    #[tokio::test]
    async fn tempfile_buffering_caps_streamed_bytes() {
        let raw =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n14\r\naaaaaaaaaaaaaaaaaaaa\r\n0\r\n\r\n"
                .to_vec();
        let mut response = response_from_raw(raw).await;
        let result = response_to_tempfile(&mut response, 10, 1024).await;
        assert!(matches!(result, Err(DownloadError::TooLarge)));
    }

    #[test]
    fn parses_valid_content_length() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("1234"));
        assert_eq!(parse_content_length(&headers), Some(1234));
    }

    #[test]
    fn missing_content_length_returns_none() {
        let headers = HeaderMap::new();
        assert_eq!(parse_content_length(&headers), None);
    }

    #[test]
    fn non_numeric_content_length_returns_none() {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("chunked"));
        assert_eq!(parse_content_length(&headers), None);
    }

    #[test]
    fn non_ascii_content_length_returns_none() {
        let mut headers = HeaderMap::new();
        headers.insert(
            CONTENT_LENGTH,
            HeaderValue::from_bytes(&[0xFF, 0xFE]).unwrap(),
        );
        assert_eq!(parse_content_length(&headers), None);
    }
}
