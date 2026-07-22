pub mod types;
pub mod utils;
pub mod zip;

use reqwest::{Response, Url};
use std::time::Instant;
use tracing::{error, warn};

use crate::config;

use self::types::{Data, DownloadResult};
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

    let mut base = match Url::parse(basic_url) {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!(
                "downloader_source_requests_total",
                "source" => source_config.url.clone(),
                "outcome" => "invalid_base_url"
            )
            .increment(1);
            warn!(
                source = %source_config.url,
                book_id,
                file_type = book_file_type,
                stage = "mirror_fetch",
                error = %err,
                "configured mirror base URL failed to parse"
            );
            return None;
        }
    };

    if !base.path().ends_with('/') {
        let path_with_slash = format!("{}/", base.path());
        base.set_path(&path_with_slash);
    }

    let book_file_type_lower = book_file_type.to_lowercase();
    let relative = if book_file_type_lower == "fb2"
        || book_file_type_lower == "epub"
        || book_file_type_lower == "mobi"
    {
        format!("b/{book_id}/{book_file_type}")
    } else {
        format!("b/{book_id}/download")
    };

    let url = match base.join(&relative) {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!(
                "downloader_source_requests_total",
                "source" => source_config.url.clone(),
                "outcome" => "invalid_base_url"
            )
            .increment(1);
            warn!(
                source = %source_config.url,
                book_id,
                file_type = book_file_type,
                stage = "mirror_fetch",
                error = %err,
                "failed to join mirror URL"
            );
            return None;
        }
    };

    let fetch_start = Instant::now();
    let response = source_config.client.get(url).send().await;

    let response = match response {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!(
                "downloader_source_requests_total",
                "source" => source_config.url.clone(),
                "outcome" => "request_error"
            )
            .increment(1);
            warn!(
                source = %source_config.url,
                book_id,
                file_type = book_file_type,
                stage = "mirror_fetch",
                error = %err,
                "mirror request failed"
            );
            return None;
        }
    };

    let response = match response.error_for_status() {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!(
                "downloader_source_requests_total",
                "source" => source_config.url.clone(),
                "outcome" => "http_error"
            )
            .increment(1);
            warn!(
                source = %source_config.url,
                book_id,
                file_type = book_file_type,
                stage = "mirror_fetch",
                error = %err,
                "mirror returned an error status"
            );
            return None;
        }
    };

    let headers = response.headers();
    let content_type = match headers.get("Content-Type") {
        Some(v) => v.to_str().unwrap_or(""),
        None => "",
    };

    if book_file_type_lower == "html" && content_type.contains("text/html") {
        metrics::counter!(
            "downloader_source_requests_total",
            "source" => source_config.url.clone(),
            "outcome" => "success"
        )
        .increment(1);
        metrics::histogram!("download_stage_duration_seconds", "stage" => "mirror_fetch")
            .record(fetch_start.elapsed().as_secs_f64());
        return Some((response, false));
    }

    if content_type.contains("text/html") {
        metrics::counter!(
            "downloader_source_requests_total",
            "source" => source_config.url.clone(),
            "outcome" => "unexpected_html"
        )
        .increment(1);
        warn!(
            source = %source_config.url,
            book_id,
            file_type = book_file_type,
            stage = "mirror_fetch",
            "mirror served an HTML page instead of the requested file"
        );
        return None;
    }

    metrics::counter!(
        "downloader_source_requests_total",
        "source" => source_config.url.clone(),
        "outcome" => "success"
    )
    .increment(1);
    metrics::histogram!("download_stage_duration_seconds", "stage" => "mirror_fetch")
        .record(fetch_start.elapsed().as_secs_f64());

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
                None => {
                    warn!(
                        source = %source_config.url,
                        book_id = book.remote_id,
                        file_type = %file_type,
                        stage = "buffer_response",
                        "failed to read HTML archive response body"
                    );
                    return None;
                }
            };

        return Some(DownloadResult::new(
            data,
            filename,
            filename_ascii,
            data_size,
        ));
    }

    if !is_zip && !final_need_zip && !converting {
        let filename = get_filename_by_book(&book, &file_type, false, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, false, true, normalized);
        let (data, data_size) =
            match response_to_download_data(response, limits.max_download_bytes).await {
                Some(v) => v,
                None => {
                    warn!(
                        source = %source_config.url,
                        book_id = book.remote_id,
                        file_type = %file_type,
                        stage = "buffer_response",
                        "failed to read direct download response body"
                    );
                    return None;
                }
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
            None => {
                warn!(
                    source = %source_config.url,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "buffer_response",
                    "failed to buffer zip response body to a temp file"
                );
                return None;
            }
        };

        let unzip_start = Instant::now();
        let unzip_result = match tokio::task::spawn_blocking(move || {
            unzip(
                temp_file_to_unzip,
                "fb2",
                limits.max_decompressed_bytes,
                limits.max_compression_ratio,
            )
        })
        .await
        {
            Ok(v) => v,
            Err(err) => {
                metrics::histogram!("download_stage_duration_seconds", "stage" => "unzip")
                    .record(unzip_start.elapsed().as_secs_f64());
                error!(
                    source = %source_config.url,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "unzip",
                    error = %err,
                    "unzip task panicked"
                );
                return None;
            }
        };
        metrics::histogram!("download_stage_duration_seconds", "stage" => "unzip")
            .record(unzip_start.elapsed().as_secs_f64());

        match unzip_result {
            Some(v) => v,
            None => {
                warn!(
                    source = %source_config.url,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "unzip",
                    "no matching entry found in zip archive, or the entry exceeded size/ratio limits"
                );
                return None;
            }
        }
    };

    let (clean_file, data_size) = if converting {
        match convert_file(unzipped_temp_file, file_type.to_string()).await {
            Some(mut response) => {
                match response_to_tempfile(&mut response, limits.max_download_bytes).await {
                    Some(v) => v,
                    None => {
                        warn!(
                            source = %source_config.url,
                            book_id = book.remote_id,
                            file_type = %file_type,
                            stage = "buffer_response",
                            "failed to buffer converted response body to a temp file"
                        );
                        return None;
                    }
                }
            }
            None => return None,
        }
    } else {
        (unzipped_temp_file, data_size)
    };

    if !final_need_zip {
        let filename = get_filename_by_book(&book, &file_type, false, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, false, true, normalized);

        return Some(DownloadResult::new(
            Data::SpooledTempFile(clean_file),
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

    let zip_start = Instant::now();
    let zip_result = match tokio::task::spawn_blocking(move || zip(clean_file, &filename)).await {
        Ok(v) => v,
        Err(err) => {
            metrics::histogram!("download_stage_duration_seconds", "stage" => "zip")
                .record(zip_start.elapsed().as_secs_f64());
            error!(
                source = %source_config.url,
                book_id = book.remote_id,
                file_type = %file_type,
                stage = "zip",
                error = %err,
                "zip task panicked"
            );
            return None;
        }
    };
    metrics::histogram!("download_stage_duration_seconds", "stage" => "zip")
        .record(zip_start.elapsed().as_secs_f64());

    match zip_result {
        Some((t_file, data_size)) => {
            let filename = get_filename_by_book(&book, &file_type, true, false, normalized);
            let filename_ascii = get_filename_by_book(&book, &file_type, true, true, normalized);

            Some(DownloadResult::new(
                Data::SpooledTempFile(t_file),
                filename,
                filename_ascii,
                data_size,
            ))
        }
        None => {
            warn!(
                source = %source_config.url,
                book_id = book.remote_id,
                file_type = %file_type,
                stage = "zip",
                "failed to create result zip archive"
            );
            None
        }
    }
}

pub async fn start_download_futures(
    book: &BookWithRemote,
    file_type: &str,
    normalized: bool,
    sources: &[config::SourceConfig],
    limits: config::DownloadLimits,
    overall_deadline: std::time::Duration,
) -> Option<DownloadResult> {
    let attempt = async {
        for source_config in sources {
            if let Some(result) = download_chain(
                book.clone(),
                file_type.to_string(),
                source_config.clone(),
                false,
                normalized,
                limits,
            )
            .await
            {
                return Some(result);
            }

            if file_type == "epub" || file_type == "fb2" {
                if let Some(result) = download_chain(
                    book.clone(),
                    file_type.to_string(),
                    source_config.clone(),
                    true,
                    normalized,
                    limits,
                )
                .await
                {
                    return Some(result);
                }
            }
        }

        None
    };

    tokio::time::timeout(overall_deadline, attempt)
        .await
        .unwrap_or(None)
}

pub async fn book_download(
    source_id: u32,
    remote_id: u32,
    file_type: &str,
    normalized: bool,
) -> Result<Option<DownloadResult>, Box<dyn std::error::Error + Send + Sync>> {
    let book = match get_remote_book(source_id, remote_id).await {
        Ok(v) => v,
        Err(err) => return Err(Box::new(err)),
    };

    match start_download_futures(
        &book,
        file_type,
        normalized,
        &config::CONFIG.fl_sources,
        config::CONFIG.download_limits,
        config::CONFIG.overall_download_timeout,
    )
    .await
    {
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

    fn make_source_config_with_client(
        url: String,
        client: reqwest::Client,
    ) -> config::SourceConfig {
        config::SourceConfig {
            url,
            proxy: None,
            client,
        }
    }

    async fn spawn_counting_server(
        response: Vec<u8>,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count_clone = count.clone();

        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                count_clone.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let response = response.clone();
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = socket.read(&mut buf).await;
                    let _ = socket.write_all(&response).await;
                    let _ = socket.shutdown().await;
                });
            }
        });

        (format!("http://{addr}"), count)
    }

    async fn spawn_stalling_server() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                std::future::pending::<()>().await;
            }
        });

        format!("http://{addr}")
    }

    #[derive(Clone, Default)]
    struct CapturingWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl CapturingWriter {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl std::io::Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn find_counter(
        snapshot: &[(
            metrics_util::CompositeKey,
            Option<metrics::Unit>,
            Option<metrics::SharedString>,
            metrics_util::debugging::DebugValue,
        )],
        metric_name: &str,
        label_key: &str,
        label_value: &str,
    ) -> Option<u64> {
        snapshot.iter().find_map(|(key, _, _, value)| {
            let k = key.key();
            if k.name() != metric_name {
                return None;
            }
            if !k
                .labels()
                .any(|l| l.key() == label_key && l.value() == label_value)
            {
                return None;
            }
            match value {
                metrics_util::debugging::DebugValue::Counter(n) => Some(*n),
                _ => None,
            }
        })
    }

    #[tokio::test]
    async fn direct_success_skips_conversion_attempt() {
        let body = b"fake epub content";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/epub+zip\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let (base_url, hit_count) = spawn_counting_server(response.into_bytes()).await;
        let source_config = make_source_config(base_url);
        let book = make_book("epub");

        let result = start_download_futures(
            &book,
            "epub",
            true,
            &[source_config],
            generous_limits(),
            std::time::Duration::from_secs(5),
        )
        .await;

        assert!(result.is_some());
        assert_eq!(
            hit_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "conversion fallback must not be attempted once the direct download succeeds"
        );
    }

    #[tokio::test]
    async fn stalled_mirror_fails_over_to_next_source() {
        let stalling_url = spawn_stalling_server().await;
        let stalling_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(200))
            .build()
            .unwrap();
        let stalling_source = make_source_config_with_client(stalling_url, stalling_client);

        let body = b"fake fb2 content";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/fb2\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let (working_url, _) = spawn_counting_server(response.into_bytes()).await;
        let working_source = make_source_config(working_url);

        let book = make_book("fb2");
        let start = tokio::time::Instant::now();

        let result = start_download_futures(
            &book,
            "fb2",
            true,
            &[stalling_source, working_source],
            generous_limits(),
            std::time::Duration::from_secs(10),
        )
        .await;

        let elapsed = start.elapsed();

        assert!(result.is_some(), "should fail over to the working mirror");
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "failover should happen once the stalled mirror's own timeout fires, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn overall_deadline_bounds_total_latency_even_if_all_mirrors_stall() {
        let stalling_url = spawn_stalling_server().await;
        let stalling_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap();
        let stalling_source = make_source_config_with_client(stalling_url, stalling_client);

        let book = make_book("fb2");
        let start = tokio::time::Instant::now();

        let result = start_download_futures(
            &book,
            "fb2",
            true,
            &[stalling_source],
            generous_limits(),
            std::time::Duration::from_millis(300),
        )
        .await;

        let elapsed = start.elapsed();

        assert!(result.is_none());
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "overall deadline should cut the attempt short even though the mirror's own timeout is much longer, took {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn direct_download_filename_and_ascii_share_the_requested_extension() {
        let body = b"fake epub content";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/epub+zip\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let base_url = spawn_raw_server(response.into_bytes()).await;
        let source_config = make_source_config(base_url);
        // The library records this book as "fb2", but the client is requesting "epub"
        // directly (no conversion) — the served bytes are epub, so both filename headers
        // must say epub, not fb2.
        let book = make_book("fb2");

        let result = download_chain(
            book,
            "epub".to_string(),
            source_config,
            false,
            true,
            generous_limits(),
        )
        .await;

        let data = result.expect("direct download should succeed");
        assert!(
            data.filename.ends_with(".epub"),
            "filename should use the requested/served type, got {:?}",
            data.filename
        );
        assert!(
            data.filename_ascii.ends_with(".epub"),
            "filename_ascii should use the requested/served type, got {:?}",
            data.filename_ascii
        );
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
        assert!(matches!(data.data, Data::SpooledTempFile(_)));
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

        let mut reader = data.get_async_read();
        let mut buf = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf)
            .await
            .unwrap();
        assert_eq!(buf, body);
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

    #[test]
    fn mirror_http_error_is_logged_and_counted() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let capture = CapturingWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();

        let (result, base_url) = metrics::with_local_recorder(&recorder, || {
            let _guard = tracing::subscriber::set_default(subscriber);
            rt.block_on(async {
                let response = "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\n\r\n";
                let base_url = spawn_raw_server(response.as_bytes().to_vec()).await;
                let source_config = make_source_config(base_url.clone());
                (download(&1, "fb2", &source_config).await, base_url)
            })
        });

        assert!(result.is_none());

        let logs = capture.contents();
        assert!(
            logs.contains("mirror returned an error status"),
            "expected the http-error log line, got: {logs}"
        );
        assert!(
            logs.contains(&base_url),
            "expected the log line to name the source URL, got: {logs}"
        );

        let snapshot = snapshotter.snapshot().into_vec();
        assert_eq!(
            find_counter(
                &snapshot,
                "downloader_source_requests_total",
                "outcome",
                "http_error"
            ),
            Some(1),
            "expected downloader_source_requests_total{{outcome=\"http_error\"}} == 1, got {snapshot:?}"
        );
    }

    #[test]
    fn mirror_connect_failure_is_logged_and_counted() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let capture = CapturingWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();

        // Bind then immediately drop the listener so the port is guaranteed to refuse
        // connections — a stable way to trigger a connect-level request_error deterministically.
        let dead_url = rt.block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);
            format!("http://{addr}")
        });

        let result = metrics::with_local_recorder(&recorder, || {
            let _guard = tracing::subscriber::set_default(subscriber);
            rt.block_on(async {
                let source_config = make_source_config(dead_url.clone());
                download(&1, "fb2", &source_config).await
            })
        });

        assert!(result.is_none());

        let logs = capture.contents();
        assert!(
            logs.contains("mirror request failed"),
            "expected the request-error log line, got: {logs}"
        );

        let snapshot = snapshotter.snapshot().into_vec();
        assert_eq!(
            find_counter(
                &snapshot,
                "downloader_source_requests_total",
                "outcome",
                "request_error"
            ),
            Some(1),
            "expected downloader_source_requests_total{{outcome=\"request_error\"}} == 1, got {snapshot:?}"
        );
    }

    #[test]
    fn unzip_failure_is_logged_with_stage() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let capture = CapturingWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();

        let result = {
            let _guard = tracing::subscriber::set_default(subscriber);
            rt.block_on(async {
                let body = b"this is not a zip file";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    std::str::from_utf8(body).unwrap()
                );
                let base_url = spawn_raw_server(response.into_bytes()).await;
                let source_config = make_source_config(base_url);
                let book = make_book("fb2");

                download_chain(
                    book,
                    "fb2zip".to_string(),
                    source_config,
                    false,
                    true,
                    generous_limits(),
                )
                .await
            })
        };

        assert!(result.is_none());

        let logs = capture.contents();
        assert!(
            logs.contains("stage=\"unzip\""),
            "expected an unzip-stage log line, got: {logs}"
        );
    }

    #[tokio::test]
    async fn uppercase_file_type_uses_type_specific_url_not_generic_download() {
        let received: std::sync::Arc<std::sync::Mutex<String>> =
            std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let received_clone = received.clone();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                if let Ok(n) = socket.read(&mut buf).await {
                    *received_clone.lock().unwrap() =
                        String::from_utf8_lossy(&buf[..n]).to_string();
                }
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });

        let source_config = make_source_config(format!("http://{addr}"));
        let _ = download(&42, "FB2", &source_config).await;

        let request = received.lock().unwrap().clone();
        assert!(
            request.starts_with("GET /b/42/FB2 "),
            "an uppercase FB2 book_file_type must still take the type-specific URL branch, got: {request:?}"
        );
    }

    #[tokio::test]
    async fn mirror_request_path_matches_expected_url() {
        let received: std::sync::Arc<std::sync::Mutex<String>> =
            std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let received_clone = received.clone();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                if let Ok(n) = socket.read(&mut buf).await {
                    *received_clone.lock().unwrap() =
                        String::from_utf8_lossy(&buf[..n]).to_string();
                }
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });

        let source_config = make_source_config(format!("http://{addr}"));
        let _ = download(&42, "fb2", &source_config).await;

        let request = received.lock().unwrap().clone();
        assert!(
            request.starts_with("GET /b/42/fb2 "),
            "expected request path /b/42/fb2, got: {request:?}"
        );
    }

    #[tokio::test]
    async fn mirror_url_rejects_invalid_base_url_instead_of_panicking() {
        let source_config = make_source_config("not a valid url".to_string());

        let result = download(&42, "fb2", &source_config).await;

        assert!(result.is_none());
    }
}
