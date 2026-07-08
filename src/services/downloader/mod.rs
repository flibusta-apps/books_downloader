pub mod types;
pub mod utils;
pub mod zip;

use reqwest::Response;
use tokio::task::JoinSet;

use crate::config;

use self::types::{Data, DownloadResult, SpooledTempAsyncRead};
use self::utils::{response_to_download_data, response_to_tempfile};
use self::zip::{unzip, zip};

use super::book_library::types::BookWithRemote;
use super::covert::convert_file;
use super::{book_library::get_remote_book, filename_getter::get_filename_by_book};

pub async fn download<'a>(
    book_id: &'a u32,
    book_file_type: &'a str,
    source_config: &'a config::SourceConfig,
) -> Option<(Response, bool)> {
    let basic_url = &source_config.url;

    let url = if book_file_type == "fb2" || book_file_type == "epub" || book_file_type == "mobi" {
        format!("{basic_url}/b/{book_id}/{book_file_type}")
    } else {
        format!("{basic_url}/b/{book_id}/download")
    };

    let response = source_config.client.get(url).send().await;

    let response = match response {
        Ok(v) => v,
        Err(_) => return None,
    };

    let response = match response.error_for_status() {
        Ok(v) => v,
        Err(_) => return None,
    };

    let headers = response.headers();
    let content_type = match headers.get("Content-Type") {
        Some(v) => v.to_str().unwrap_or(""),
        None => "",
    };

    if book_file_type.to_lowercase() == "html" && content_type.contains("text/html") {
        return Some((response, false));
    }

    if content_type.contains("text/html") {
        return None;
    }

    let is_zip = content_type.contains("application/zip");

    Some((response, is_zip))
}

pub async fn download_chain(
    book: BookWithRemote,
    file_type: String,
    source_config: config::SourceConfig,
    converting: bool,
    normalized: bool,
    limits: config::DownloadLimits,
) -> Option<DownloadResult> {
    let final_need_zip = file_type == "fb2zip";

    let file_type_ = if converting {
        book.file_type.clone()
    } else {
        file_type.clone()
    };

    let (mut response, is_zip) = match download(&book.remote_id, &file_type_, &source_config).await
    {
        Some(v) => v,
        None => return None,
    };

    if is_zip && book.file_type.to_lowercase() == "html" {
        let filename = get_filename_by_book(&book, &file_type, true, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, true, true, normalized);
        let (data, data_size) =
            match response_to_download_data(response, limits.max_download_bytes).await {
                Some(v) => v,
                None => return None,
            };

        return Some(DownloadResult::new(
            data,
            filename,
            filename_ascii,
            data_size,
        ));
    }

    if !is_zip && !final_need_zip && !converting {
        let filename = get_filename_by_book(&book, &book.file_type, false, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, false, true, normalized);
        let (data, data_size) =
            match response_to_download_data(response, limits.max_download_bytes).await {
                Some(v) => v,
                None => return None,
            };

        return Some(DownloadResult::new(
            data,
            filename,
            filename_ascii,
            data_size,
        ));
    };

    let (unzipped_temp_file, data_size) = {
        let temp_file_to_unzip_result =
            response_to_tempfile(&mut response, limits.max_download_bytes).await;
        let temp_file_to_unzip = match temp_file_to_unzip_result {
            Some(v) => v.0,
            None => return None,
        };

        match unzip(
            temp_file_to_unzip,
            "fb2",
            limits.max_decompressed_bytes,
            limits.max_compression_ratio,
        ) {
            Some(v) => v,
            None => return None,
        }
    };

    let (mut clean_file, data_size) = if converting {
        match convert_file(unzipped_temp_file, file_type.to_string()).await {
            Some(mut response) => {
                match response_to_tempfile(&mut response, limits.max_download_bytes).await {
                    Some(v) => v,
                    None => return None,
                }
            }
            None => return None,
        }
    } else {
        (unzipped_temp_file, data_size)
    };

    if !final_need_zip {
        let t = SpooledTempAsyncRead::new(clean_file);
        let filename = get_filename_by_book(&book, &file_type, false, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, false, true, normalized);

        return Some(DownloadResult::new(
            Data::SpooledTempAsyncRead(t),
            filename,
            filename_ascii,
            data_size,
        ));
    };

    let t_file_type = if file_type == "fb2zip" {
        "fb2"
    } else {
        &file_type
    };
    let filename = get_filename_by_book(&book, t_file_type, false, false, normalized);
    match zip(&mut clean_file, filename.as_str()) {
        Some((t_file, data_size)) => {
            let t = SpooledTempAsyncRead::new(t_file);
            let filename = get_filename_by_book(&book, &file_type, true, false, normalized);
            let filename_ascii = get_filename_by_book(&book, &file_type, true, true, normalized);

            Some(DownloadResult::new(
                Data::SpooledTempAsyncRead(t),
                filename,
                filename_ascii,
                data_size,
            ))
        }
        None => None,
    }
}

pub async fn start_download_futures(
    book: &BookWithRemote,
    file_type: &str,
    normalized: bool,
) -> Option<DownloadResult> {
    let mut tasks = JoinSet::new();

    for source_config in &config::CONFIG.fl_sources {
        tasks.spawn(download_chain(
            book.clone(),
            file_type.to_string(),
            source_config.clone(),
            false,
            normalized,
            config::CONFIG.download_limits,
        ));

        if file_type == "epub" || file_type == "fb2" {
            tasks.spawn(download_chain(
                book.clone(),
                file_type.to_string(),
                source_config.clone(),
                true,
                normalized,
                config::CONFIG.download_limits,
            ));
        }
    }

    while let Some(task_result) = tasks.join_next().await {
        if let Ok(Some(task_result)) = task_result {
            return Some(task_result);
        }
    }

    None
}

pub async fn book_download(
    source_id: u32,
    remote_id: u32,
    file_type: &str,
    normalized: bool,
) -> Result<Option<DownloadResult>, Box<dyn std::error::Error + Send + Sync>> {
    let book = match get_remote_book(source_id, remote_id).await {
        Ok(v) => v,
        Err(err) => return Err(err),
    };

    match start_download_futures(&book, file_type, normalized).await {
        Some(v) => Ok(Some(v)),
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::services::book_library::types::BookWithRemote;

    async fn spawn_raw_server(response: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket.write_all(&response).await;
                let _ = socket.shutdown().await;
            }
        });

        format!("http://{addr}")
    }

    fn make_source_config(url: String) -> config::SourceConfig {
        config::SourceConfig {
            url,
            proxy: None,
            client: reqwest::Client::new(),
        }
    }

    fn make_book(file_type: &str) -> BookWithRemote {
        BookWithRemote {
            id: 1,
            remote_id: 42,
            title: "Test Book".to_string(),
            lang: "ru".to_string(),
            file_type: file_type.to_string(),
            uploaded: "2024-01-01".to_string(),
            authors: vec![],
        }
    }

    fn generous_limits() -> config::DownloadLimits {
        config::DownloadLimits {
            max_download_bytes: 5 * 1024 * 1024,
            max_decompressed_bytes: 5 * 1024 * 1024,
            max_compression_ratio: 1000,
        }
    }

    #[tokio::test]
    async fn missing_content_length_falls_back_to_buffering() {
        let body = b"fake fb2 content";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/fb2\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let base_url = spawn_raw_server(response.into_bytes()).await;
        let source_config = make_source_config(base_url);
        let book = make_book("fb2");

        let result = download_chain(
            book,
            "fb2".to_string(),
            source_config,
            false,
            true,
            generous_limits(),
        )
        .await;

        let data = result.expect("download_chain should succeed despite missing Content-Length");
        assert_eq!(data.data_size, body.len());
        assert!(matches!(data.data, Data::SpooledTempAsyncRead(_)));
    }

    #[tokio::test]
    async fn html_zip_missing_content_length_falls_back_to_buffering() {
        let body = b"<html>fake</html>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let base_url = spawn_raw_server(response.into_bytes()).await;
        let source_config = make_source_config(base_url);
        let book = make_book("html");

        let result = download_chain(
            book,
            "html".to_string(),
            source_config,
            false,
            true,
            generous_limits(),
        )
        .await;

        let data = result.expect("download_chain should succeed despite missing Content-Length");
        assert_eq!(data.data_size, body.len());
    }

    #[tokio::test]
    async fn valid_content_length_streams_without_buffering() {
        let body = b"fake fb2 content";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/fb2\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let base_url = spawn_raw_server(response.into_bytes()).await;
        let source_config = make_source_config(base_url);
        let book = make_book("fb2");

        let result = download_chain(
            book,
            "fb2".to_string(),
            source_config,
            false,
            true,
            generous_limits(),
        )
        .await;

        let data = result.expect("download_chain should succeed with valid Content-Length");
        assert_eq!(data.data_size, body.len());
        assert!(matches!(data.data, Data::Response(_)));
    }

    #[tokio::test]
    async fn binary_content_type_does_not_panic() {
        let mut response = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: ".to_vec();
        response.extend_from_slice(&[0xFF, 0xFE]);
        response.extend_from_slice(b"\r\n\r\nhello");

        let base_url = spawn_raw_server(response).await;
        let source_config = make_source_config(base_url);

        let result = download(&1, "fb2", &source_config).await;

        let (_, is_zip) = result.expect("download should not panic on binary Content-Type");
        assert!(!is_zip);
    }

    #[tokio::test]
    async fn corrupt_zip_body_returns_none_instead_of_panicking() {
        let body = b"this is not a zip file";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let base_url = spawn_raw_server(response.into_bytes()).await;
        let source_config = make_source_config(base_url);
        let book = make_book("fb2");

        let result = download_chain(
            book,
            "fb2zip".to_string(),
            source_config,
            false,
            true,
            generous_limits(),
        )
        .await;

        assert!(result.is_none());
    }

    #[tokio::test]
    async fn oversized_body_without_content_length_is_rejected() {
        let body = vec![b'a'; 64];
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/fb2\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            body.len(),
            std::str::from_utf8(&body).unwrap()
        );
        let base_url = spawn_raw_server(response.into_bytes()).await;
        let source_config = make_source_config(base_url);
        let book = make_book("fb2");
        let limits = config::DownloadLimits {
            max_download_bytes: 10,
            max_decompressed_bytes: 5 * 1024 * 1024,
            max_compression_ratio: 1000,
        };

        let result =
            download_chain(book, "fb2".to_string(), source_config, false, true, limits).await;

        assert!(result.is_none());
    }
}
