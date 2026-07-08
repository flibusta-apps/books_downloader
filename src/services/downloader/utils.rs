use bytes::Buf;
use reqwest::Response;
use tempfile::SpooledTempFile;

use std::io::{Seek, SeekFrom, Write};

pub fn parse_content_length(headers: &reqwest::header::HeaderMap) -> Option<usize> {
    headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

pub async fn response_to_tempfile(res: &mut Response) -> Option<(SpooledTempFile, usize)> {
    let mut tmp_file = tempfile::spooled_tempfile(5 * 1024 * 1024);

    let mut data_size: usize = 0;

    {
        loop {
            let chunk = res.chunk().await;

            let result = match chunk {
                Ok(v) => v,
                Err(_) => return None,
            };

            let data = match result {
                Some(v) => v,
                None => break,
            };

            data_size += data.len();

            match tmp_file.write_all(data.chunk()) {
                Ok(_) => (),
                Err(_) => return None,
            }
        }

        tmp_file.seek(SeekFrom::Start(0)).unwrap();
    }

    Some((tmp_file, data_size))
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
        headers.insert(CONTENT_LENGTH, HeaderValue::from_bytes(&[0xFF, 0xFE]).unwrap());
        assert_eq!(parse_content_length(&headers), None);
    }
}
