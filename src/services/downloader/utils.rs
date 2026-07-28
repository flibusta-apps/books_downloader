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

pub async fn response_to_tempfile(
    res: &mut Response,
    max_bytes: usize,
) -> Result<(SpooledTempFile, usize), DownloadError> {
    if let Some(declared) = content_length(res) {
        if declared > max_bytes {
            return Err(DownloadError::SourceUnavailable);
        }
    }

    let mut tmp_file = tempfile::spooled_tempfile(5 * 1024 * 1024);

    let mut data_size: usize = 0;

    loop {
        let chunk = res.chunk().await;

        let result = match chunk {
            Ok(v) => v,
            Err(_) => return Err(DownloadError::SourceUnavailable),
        };

        let data = match result {
            Some(v) => v,
            None => break,
        };

        data_size += data.len();

        if data_size > max_bytes {
            return Err(DownloadError::SourceUnavailable);
        }

        match tmp_file.write_all(data.chunk()) {
            Ok(_) => (),
            Err(_) => return Err(DownloadError::SourceUnavailable),
        }
    }

    match tmp_file.seek(SeekFrom::Start(0)) {
        Ok(_) => (),
        Err(_) => return Err(DownloadError::SourceUnavailable),
    }

    Ok((tmp_file, data_size))
}

pub async fn response_to_download_data(
    mut response: Response,
    max_bytes: usize,
) -> Result<(Data, usize), DownloadError> {
    if let Some(size) = content_length(&response) {
        return Ok((Data::Response(response), size));
    }

    let (tmp_file, size) = response_to_tempfile(&mut response, max_bytes).await?;
    Ok((Data::SpooledTempFile(tmp_file), size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue, CONTENT_LENGTH};

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
