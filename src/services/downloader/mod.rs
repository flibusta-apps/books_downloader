pub mod error;
pub mod types;
pub mod utils;
pub mod zip;

use once_cell::sync::Lazy;
use reqwest::{Response, Url};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{error, warn};

use crate::config::{self, source_label};

use self::error::DownloadError;
use self::types::{Data, DownloadResult};
use self::utils::{response_to_download_data, response_to_tempfile};
use self::zip::{is_precompressed_file_type, unzip, zip};

use super::book_library::types::BookWithRemote;
use super::convert::convert_file;
use super::{book_library::get_remote_book, filename_getter::get_filename_by_book};

/// How long a request waits for a free download slot before being rejected with 503.
const SEMAPHORE_WAIT: Duration = Duration::from_secs(2);

static DOWNLOAD_SEMAPHORE: Lazy<std::sync::Arc<Semaphore>> =
    Lazy::new(|| std::sync::Arc::new(Semaphore::new(config::CONFIG.max_concurrent_downloads)));

/// Acquires a global download slot. Returns `None` when saturated after a short wait.
pub async fn acquire_download_permit() -> Option<OwnedSemaphorePermit> {
    match tokio::time::timeout(
        SEMAPHORE_WAIT,
        std::sync::Arc::clone(&DOWNLOAD_SEMAPHORE).acquire_owned(),
    )
    .await
    {
        Ok(Ok(permit)) => Some(permit),
        _ => None,
    }
}

/// The file type that is fetched from the mirror for a requested type.
fn mirror_file_type(requested: &str) -> &str {
    if requested == "fb2zip" {
        "fb2"
    } else {
        requested
    }
}

/// A zip wrapper must be unpacked only for fb2/fb2zip requests; every other
/// format (e.g. epub, which is itself a zip) is served exactly as received.
fn needs_unzip(requested: &str, is_zip: bool) -> bool {
    is_zip && matches!(requested, "fb2" | "fb2zip")
}

/// Conversion is only possible from an fb2 source into epub/mobi.
pub fn conversion_fallback_allowed(source_type: &str, requested: &str) -> bool {
    source_type.eq_ignore_ascii_case("fb2") && matches!(requested, "epub" | "mobi")
}

fn filename_pair(
    book: &BookWithRemote,
    file_type: &str,
    force_zip: bool,
    normalized: bool,
) -> (String, String) {
    (
        get_filename_by_book(book, file_type, force_zip, false, normalized),
        get_filename_by_book(book, file_type, force_zip, true, normalized),
    )
}

pub async fn download(
    book_id: u32,
    book_file_type: &str,
    source_config: &config::SourceConfig,
) -> Result<(Response, bool), DownloadError> {
    let source = source_label(&source_config.url);
    let book_file_type = book_file_type.to_lowercase();
    let book_file_type = book_file_type.as_str();

    let mut base = match Url::parse(&source_config.url) {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!(
                "downloader_source_requests_total",
                "source" => source.clone(),
                "outcome" => "invalid_base_url"
            )
            .increment(1);
            warn!(
                source = %source,
                book_id,
                file_type = book_file_type,
                stage = "mirror_fetch",
                error = %err,
                "configured mirror base URL failed to parse"
            );
            return Err(DownloadError::SourceUnavailable);
        }
    };

    if !base.path().ends_with('/') {
        let path_with_slash = format!("{}/", base.path());
        base.set_path(&path_with_slash);
    }

    let relative = if matches!(book_file_type, "fb2" | "epub" | "mobi") {
        format!("b/{book_id}/{book_file_type}")
    } else {
        format!("b/{book_id}/download")
    };

    let url = match base.join(&relative) {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!(
                "downloader_source_requests_total",
                "source" => source.clone(),
                "outcome" => "invalid_base_url"
            )
            .increment(1);
            warn!(
                source = %source,
                book_id,
                file_type = book_file_type,
                stage = "mirror_fetch",
                error = %err,
                "failed to join mirror URL"
            );
            return Err(DownloadError::SourceUnavailable);
        }
    };

    let fetch_start = Instant::now();
    let response = source_config.client.get(url).send().await;

    let response = match response {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!(
                "downloader_source_requests_total",
                "source" => source.clone(),
                "outcome" => "request_error"
            )
            .increment(1);
            // `reqwest::Error` Display may embed the request URL (with userinfo);
            // drop it and log only the error kind.
            let err = err.without_url();
            warn!(
                source = %source,
                book_id,
                file_type = book_file_type,
                stage = "mirror_fetch",
                error = %err,
                "mirror request failed"
            );
            return Err(DownloadError::SourceUnavailable);
        }
    };

    let response = match response.error_for_status() {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!(
                "downloader_source_requests_total",
                "source" => source.clone(),
                "outcome" => "http_error"
            )
            .increment(1);
            let err = err.without_url();
            warn!(
                source = %source,
                book_id,
                file_type = book_file_type,
                stage = "mirror_fetch",
                error = %err,
                "mirror returned an error status"
            );
            return Err(DownloadError::SourceUnavailable);
        }
    };

    let headers = response.headers();
    let content_type = match headers.get("Content-Type") {
        Some(v) => v.to_str().unwrap_or(""),
        None => "",
    };

    if book_file_type == "html" && content_type.contains("text/html") {
        metrics::counter!(
            "downloader_source_requests_total",
            "source" => source.clone(),
            "outcome" => "success"
        )
        .increment(1);
        metrics::histogram!("download_stage_duration_seconds", "stage" => "mirror_fetch")
            .record(fetch_start.elapsed().as_secs_f64());
        return Ok((response, false));
    }

    if content_type.contains("text/html") {
        metrics::counter!(
            "downloader_source_requests_total",
            "source" => source.clone(),
            "outcome" => "unexpected_html"
        )
        .increment(1);
        warn!(
            source = %source,
            book_id,
            file_type = book_file_type,
            stage = "mirror_fetch",
            "mirror served an HTML page instead of the requested file"
        );
        return Err(DownloadError::SourceUnavailable);
    }

    metrics::counter!(
        "downloader_source_requests_total",
        "source" => source.clone(),
        "outcome" => "success"
    )
    .increment(1);
    metrics::histogram!("download_stage_duration_seconds", "stage" => "mirror_fetch")
        .record(fetch_start.elapsed().as_secs_f64());

    let is_zip = content_type.contains("application/zip");

    Ok((response, is_zip))
}

pub async fn download_chain(
    book: std::sync::Arc<BookWithRemote>,
    file_type: String,
    source_config: config::SourceConfig,
    converting: bool,
    normalized: bool,
    limits: config::DownloadLimits,
) -> Result<DownloadResult, DownloadError> {
    let file_type = file_type.to_lowercase();
    let source = source_label(&source_config.url);

    if converting && !conversion_fallback_allowed(&book.file_type, &file_type) {
        return Err(DownloadError::Internal(
            "unsupported conversion requested".to_string(),
        ));
    }

    let final_need_zip = file_type == "fb2zip";

    // Converting always starts from the fb2 source; otherwise fetch what was requested.
    let fetch_type = if converting {
        "fb2"
    } else {
        mirror_file_type(&file_type)
    };

    let (mut response, is_zip) = download(book.remote_id, fetch_type, &source_config).await?;

    if is_zip && !converting && file_type == "html" {
        let (filename, filename_ascii) = filename_pair(&book, &file_type, true, normalized);
        let (data, data_size) = response_to_download_data(response, limits.max_download_bytes)
            .await
            .inspect_err(|_| {
                warn!(
                    source = %source,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "buffer_response",
                    "failed to read HTML archive response body"
                );
            })?;

        return Ok(DownloadResult::new(
            data,
            filename,
            filename_ascii,
            data_size,
        ));
    }

    let unzip_needed = needs_unzip(fetch_type, is_zip);

    if !converting && !final_need_zip && !unzip_needed {
        let (filename, filename_ascii) = filename_pair(&book, &file_type, false, normalized);
        let (data, data_size) = response_to_download_data(response, limits.max_download_bytes)
            .await
            .inspect_err(|_| {
                warn!(
                    source = %source,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "buffer_response",
                    "failed to read direct download response body"
                );
            })?;

        return Ok(DownloadResult::new(
            data,
            filename,
            filename_ascii,
            data_size,
        ));
    };

    let (buffered_file, buffered_size) =
        response_to_tempfile(&mut response, limits.max_download_bytes, 5 * 1024 * 1024)
            .await
            .inspect_err(|_| {
                warn!(
                    source = %source,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "buffer_response",
                    "failed to buffer response body to a temp file"
                );
            })?;

    let (unzipped_temp_file, data_size) = if unzip_needed {
        let expected_inner = fetch_type.to_string();
        let unzip_start = Instant::now();
        let unzip_result = match tokio::task::spawn_blocking(move || {
            unzip(
                buffered_file,
                &expected_inner,
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
                    source = %source,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "unzip",
                    error = %err,
                    "unzip task panicked"
                );
                return Err(DownloadError::Internal("unzip task panicked".to_string()));
            }
        };
        metrics::histogram!("download_stage_duration_seconds", "stage" => "unzip")
            .record(unzip_start.elapsed().as_secs_f64());

        match unzip_result {
            Ok(v) => v,
            Err(err) => {
                warn!(
                    source = %source,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "unzip",
                    error = %err,
                    "no matching entry found in zip archive, or the entry exceeded size/ratio limits"
                );
                return Err(err);
            }
        }
    } else {
        // Raw (non-zipped) fb2 served for an fb2zip request or for conversion.
        (buffered_file, buffered_size)
    };

    let (clean_file, data_size) = if converting {
        let mut converted = convert_file(unzipped_temp_file, file_type.to_string())
            .await
            .map_err(|err| {
                warn!(
                    source = %source,
                    book_id = book.remote_id,
                    file_type = %file_type,
                    stage = "convert",
                    error = %err,
                    "converter request failed"
                );
                err
            })?;

        response_to_tempfile(
            &mut converted,
            limits.max_download_bytes,
            config::CONFIG.converted_response_spool_bytes,
        )
        .await
        .inspect_err(|_| {
            warn!(
                source = %source,
                book_id = book.remote_id,
                file_type = %file_type,
                stage = "buffer_response",
                "failed to buffer converted response body to a temp file"
            );
        })?
    } else {
        (unzipped_temp_file, data_size)
    };

    if !final_need_zip {
        let (filename, filename_ascii) = filename_pair(&book, &file_type, false, normalized);

        return Ok(DownloadResult::new(
            Data::SpooledTempFile(clean_file),
            filename,
            filename_ascii,
            data_size,
        ));
    };

    let t_file_type = "fb2";
    let filename = get_filename_by_book(&book, t_file_type, false, false, normalized);
    let stored = is_precompressed_file_type(t_file_type);
    let compression_level = limits.zip_compression_level;

    let zip_start = Instant::now();
    let zip_result = match tokio::task::spawn_blocking(move || {
        zip(clean_file, &filename, compression_level, stored)
    })
    .await
    {
        Ok(v) => v,
        Err(err) => {
            metrics::histogram!("download_stage_duration_seconds", "stage" => "zip")
                .record(zip_start.elapsed().as_secs_f64());
            error!(
                source = %source,
                book_id = book.remote_id,
                file_type = %file_type,
                stage = "zip",
                error = %err,
                "zip task panicked"
            );
            return Err(DownloadError::Internal("zip task panicked".to_string()));
        }
    };
    metrics::histogram!("download_stage_duration_seconds", "stage" => "zip")
        .record(zip_start.elapsed().as_secs_f64());

    match zip_result {
        Ok((t_file, data_size)) => {
            let (filename, filename_ascii) = filename_pair(&book, &file_type, true, normalized);

            Ok(DownloadResult::new(
                Data::SpooledTempFile(t_file),
                filename,
                filename_ascii,
                data_size,
            ))
        }
        Err(err) => {
            warn!(
                source = %source,
                book_id = book.remote_id,
                file_type = %file_type,
                stage = "zip",
                error = %err,
                "failed to create result zip archive"
            );
            Err(err)
        }
    }
}

/// Runs one mirror attempt under `attempt_timeout`. A timeout counts as a
/// mirror failure so that the next mirror is tried.
async fn attempt_chain(
    book: &std::sync::Arc<BookWithRemote>,
    file_type: &str,
    source_config: &config::SourceConfig,
    converting: bool,
    normalized: bool,
    limits: config::DownloadLimits,
    attempt_timeout: Duration,
) -> Result<DownloadResult, DownloadError> {
    let fut = download_chain(
        std::sync::Arc::clone(book),
        file_type.to_string(),
        source_config.clone(),
        converting,
        normalized,
        limits,
    );
    match tokio::time::timeout(attempt_timeout, fut).await {
        Ok(result) => result,
        Err(_) => {
            warn!(
                source = %source_label(&source_config.url),
                book_id = book.remote_id,
                file_type,
                converting,
                "mirror attempt timed out"
            );
            Err(DownloadError::SourceUnavailable)
        }
    }
}

pub async fn start_download_futures(
    book: &BookWithRemote,
    file_type: &str,
    normalized: bool,
    sources: &[config::SourceConfig],
    limits: config::DownloadLimits,
    attempt_timeout: Duration,
    overall_deadline: Duration,
) -> Result<DownloadResult, DownloadError> {
    let book = std::sync::Arc::new(book.clone());
    let file_type = file_type.to_lowercase();
    let can_convert = conversion_fallback_allowed(&book.file_type, &file_type);

    let attempt = async {
        let mut last_err = DownloadError::SourceUnavailable;

        for source_config in sources {
            match attempt_chain(
                &book,
                &file_type,
                source_config,
                false,
                normalized,
                limits,
                attempt_timeout,
            )
            .await
            {
                Ok(result) => return Ok(result),
                Err(err) if !err.is_mirror_failure() => return Err(err),
                Err(err) => last_err = err,
            }

            if can_convert {
                match attempt_chain(
                    &book,
                    &file_type,
                    source_config,
                    true,
                    normalized,
                    limits,
                    attempt_timeout,
                )
                .await
                {
                    Ok(result) => return Ok(result),
                    Err(err) if !err.is_mirror_failure() => return Err(err),
                    Err(err) => last_err = err,
                }
            }
        }

        Err(last_err)
    };

    match tokio::time::timeout(overall_deadline, attempt).await {
        Ok(result) => result,
        Err(_) => Err(DownloadError::Timeout),
    }
}

pub async fn book_download(
    source_id: u32,
    remote_id: u32,
    file_type: &str,
    normalized: bool,
) -> Result<DownloadResult, DownloadError> {
    let overall = config::CONFIG.overall_download_timeout;
    let started = Instant::now();

    let book = match tokio::time::timeout(overall, get_remote_book(source_id, remote_id)).await {
        Ok(result) => result?,
        Err(_) => return Err(DownloadError::Timeout),
    };

    let remaining = overall.saturating_sub(started.elapsed());

    start_download_futures(
        &book,
        file_type,
        normalized,
        &config::CONFIG.fl_sources,
        config::CONFIG.download_limits,
        config::CONFIG.mirror_attempt_timeout,
        remaining,
    )
    .await
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
            url: url.into(),
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
            zip_compression_level: 6,
        }
    }

    fn make_source_config_with_client(
        url: String,
        client: reqwest::Client,
    ) -> config::SourceConfig {
        config::SourceConfig {
            url: url.into(),
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

    // `tracing::subscriber::set_default` installs a thread-local override, but the
    // per-callsite `Interest` cache it relies on is process-global. When two tests
    // each install their own custom subscriber and run concurrently, one thread can
    // briefly observe the other's cached "not interested" decision for the same
    // `warn!`/`error!` callsite and silently drop the log line — a rare, real flake,
    // not a bug in `download_chain`. Serializing the handful of tests below removes
    // that race without affecting the (much larger) set of tests that don't touch
    // tracing's global subscriber state.
    static LOG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
            std::time::Duration::from_secs(5),
        )
        .await;

        assert!(result.is_ok());
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
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(10),
        )
        .await;

        let elapsed = start.elapsed();

        assert!(result.is_ok(), "should fail over to the working mirror");
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
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(300),
        )
        .await;

        let elapsed = start.elapsed();

        assert!(matches!(result, Err(DownloadError::Timeout)));
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
            std::sync::Arc::new(book),
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
            std::sync::Arc::new(book),
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
            std::sync::Arc::new(book),
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
            std::sync::Arc::new(book),
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

        let result = download(1, "fb2", &source_config).await;

        let (_, is_zip) = result.expect("download should not panic on binary Content-Type");
        assert!(!is_zip);
    }

    #[tokio::test]
    async fn corrupt_zip_body_returns_err_instead_of_panicking() {
        let body = b"this is not a zip file";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let base_url = spawn_raw_server(response.into_bytes()).await;
        let source_config = make_source_config(base_url);
        let book = make_book("fb2");

        let result = download_chain(
            std::sync::Arc::new(book),
            "fb2zip".to_string(),
            source_config,
            false,
            true,
            generous_limits(),
        )
        .await;

        assert!(result.is_err());
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
            zip_compression_level: 6,
        };

        let result = download_chain(
            std::sync::Arc::new(book),
            "fb2".to_string(),
            source_config,
            false,
            true,
            limits,
        )
        .await;

        assert!(result.is_err());
    }

    #[test]
    fn mirror_http_error_is_logged_and_counted() {
        let _log_lock = LOG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
                (download(1, "fb2", &source_config).await, base_url)
            })
        });

        assert!(result.is_err());

        let logs = capture.contents();
        assert!(
            logs.contains("mirror returned an error status"),
            "expected the http-error log line, got: {logs}"
        );
        assert!(
            logs.contains(&source_label(&base_url)),
            "expected the log line to name the source host, got: {logs}"
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
        let _log_lock = LOG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
                download(1, "fb2", &source_config).await
            })
        });

        assert!(result.is_err());

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
        let _log_lock = LOG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
                    "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    std::str::from_utf8(body).unwrap()
                );
                let base_url = spawn_raw_server(response.into_bytes()).await;
                let source_config = make_source_config(base_url);
                let book = make_book("fb2");

                download_chain(
                    std::sync::Arc::new(book),
                    "fb2zip".to_string(),
                    source_config,
                    false,
                    true,
                    generous_limits(),
                )
                .await
            })
        };

        assert!(result.is_err());

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
        let _ = download(42, "FB2", &source_config).await;

        let request = received.lock().unwrap().clone();
        assert!(
            request.starts_with("GET /b/42/fb2 "),
            "an uppercase FB2 book_file_type must be lowercased in the mirror URL, got: {request:?}"
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
        let _ = download(42, "fb2", &source_config).await;

        let request = received.lock().unwrap().clone();
        assert!(
            request.starts_with("GET /b/42/fb2 "),
            "expected request path /b/42/fb2, got: {request:?}"
        );
    }

    #[tokio::test]
    async fn mirror_url_rejects_invalid_base_url_instead_of_panicking() {
        let source_config = make_source_config("not a valid url".to_string());

        let result = download(42, "fb2", &source_config).await;

        assert!(result.is_err());
    }

    async fn capture_request_with_source(sources_json: &str) -> String {
        let received: std::sync::Arc<std::sync::Mutex<String>> =
            std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let received_clone = received.clone();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
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

        let json = sources_json.replace("ADDR", &format!("http://{addr}"));
        let sources = config::parse_fl_sources(&json, std::time::Duration::from_secs(5));
        let _ = download(42, "fb2", &sources[0]).await;

        let request = received.lock().unwrap().clone();
        request
    }

    #[tokio::test]
    async fn source_api_key_is_sent_as_raw_authorization_header() {
        let request =
            capture_request_with_source(r#"[{"url": "ADDR", "api_key": "s3cret"}]"#).await;
        assert!(
            request
                .to_lowercase()
                .contains("\r\nauthorization: s3cret\r\n"),
            "expected raw Authorization header, got: {request:?}"
        );
    }

    #[tokio::test]
    async fn source_without_api_key_sends_no_authorization_header() {
        for json in [
            r#"[{"url": "ADDR"}]"#,
            r#"[{"url": "ADDR", "api_key": ""}]"#,
        ] {
            let request = capture_request_with_source(json).await;
            assert!(request.starts_with("GET /b/42/fb2 "), "got: {request:?}");
            assert!(
                !request.to_lowercase().contains("authorization"),
                "unexpected Authorization header, got: {request:?}"
            );
        }
    }

    #[test]
    fn conversion_matrix() {
        assert!(conversion_fallback_allowed("fb2", "epub"));
        assert!(conversion_fallback_allowed("fb2", "mobi"));
        assert!(!conversion_fallback_allowed("fb2", "fb2"));
        assert!(!conversion_fallback_allowed("fb2", "fb2zip"));
        assert!(!conversion_fallback_allowed("fb2", "pdf"));
        assert!(!conversion_fallback_allowed("epub", "mobi"));
        assert!(!conversion_fallback_allowed("pdf", "epub"));
        assert!(!conversion_fallback_allowed("html", "epub"));
    }

    #[test]
    fn unzip_decision() {
        assert!(needs_unzip("fb2", true));
        assert!(needs_unzip("fb2zip", true) || needs_unzip(mirror_file_type("fb2zip"), true));
        assert!(!needs_unzip("fb2", false));
        assert!(!needs_unzip("epub", true));
        assert!(!needs_unzip("mobi", true));
        assert!(!needs_unzip("pdf", true));
    }

    #[test]
    fn mirror_failure_classification() {
        assert!(DownloadError::SourceUnavailable.is_mirror_failure());
        assert!(DownloadError::BadArchive.is_mirror_failure());
        assert!(!DownloadError::ConverterFailed(500).is_mirror_failure());
        assert!(!DownloadError::ConverterUnavailable.is_mirror_failure());
        assert!(!DownloadError::TooLarge.is_mirror_failure());
        assert!(!DownloadError::LocalIo("x".into()).is_mirror_failure());
        assert!(!DownloadError::Timeout.is_mirror_failure());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn too_large_stops_failover_to_next_mirror() {
        let _log_lock = LOG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/fb2\r\nContent-Length: 100\r\n\r\n"
                .to_vec();
        let (first_url, first_hits) = spawn_counting_server(response.clone()).await;
        let (second_url, second_hits) = spawn_counting_server(response).await;
        let limits = config::DownloadLimits {
            max_download_bytes: 10,
            max_decompressed_bytes: 5 * 1024 * 1024,
            max_compression_ratio: 1000,
            zip_compression_level: 6,
        };

        let result = start_download_futures(
            &make_book("pdf"),
            "pdf",
            true,
            &[
                make_source_config(first_url),
                make_source_config(second_url),
            ],
            limits,
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(5),
        )
        .await;

        assert!(matches!(result, Err(DownloadError::TooLarge)));
        assert_eq!(first_hits.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(second_hits.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn non_fb2_source_is_not_converted_on_mirror_failure() {
        let _log_lock = LOG_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let (url, hits) =
            spawn_counting_server(b"HTTP/1.1 500 Oops\r\nContent-Length: 0\r\n\r\n".to_vec()).await;

        let result = start_download_futures(
            &make_book("epub"),
            "epub",
            true,
            &[make_source_config(url)],
            generous_limits(),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_secs(5),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn pdf_is_served_as_is() {
        let body = b"%PDF-1.4 fake";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/pdf\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let base_url = spawn_raw_server(response.into_bytes()).await;

        let data = download_chain(
            std::sync::Arc::new(make_book("pdf")),
            "pdf".to_string(),
            make_source_config(base_url),
            false,
            true,
            generous_limits(),
        )
        .await
        .expect("pdf should be served as-is");

        assert!(data.filename.ends_with(".pdf"));
        assert_eq!(data.data_size, body.len());
    }

    #[tokio::test]
    async fn epub_served_as_zip_is_returned_as_is() {
        let body = b"PK fake epub bytes";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/zip\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let base_url = spawn_raw_server(response.into_bytes()).await;

        let data = download_chain(
            std::sync::Arc::new(make_book("fb2")),
            "epub".to_string(),
            make_source_config(base_url),
            false,
            true,
            generous_limits(),
        )
        .await
        .expect("epub zip should not be unzipped");

        assert_eq!(data.data_size, body.len());
        assert!(data.filename.ends_with(".epub"));
    }

    #[tokio::test]
    async fn raw_fb2_for_fb2zip_request_is_zipped() {
        let body = b"<FictionBook/>";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/x-fictionbook+xml\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            std::str::from_utf8(body).unwrap()
        );
        let base_url = spawn_raw_server(response.into_bytes()).await;

        let data = download_chain(
            std::sync::Arc::new(make_book("fb2")),
            "fb2zip".to_string(),
            make_source_config(base_url),
            false,
            true,
            generous_limits(),
        )
        .await
        .expect("raw fb2 should be zipped");

        assert!(data.filename.ends_with(".fb2.zip"));
        let Data::SpooledTempFile(file) = data.data else {
            panic!("expected a temp file");
        };
        let (mut inner, size) = unzip(file, "fb2", 1 << 20, 1000).expect("result must be a zip");
        let mut contents = Vec::new();
        std::io::Read::read_to_end(&mut inner, &mut contents).unwrap();
        assert_eq!(contents, body);
        assert_eq!(size, body.len());
    }
}
