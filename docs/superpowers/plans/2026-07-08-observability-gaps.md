# Observability Gaps Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make download/conversion failures visible in logs and metrics, and make log lines correlatable to a specific request — closing the three gaps in `docs/specs/06-observability-gaps.md` (06.1 error logging, 06.2 per-source/converter metrics, 06.3 span identity). 06.4 (optional `/ready` endpoint) is explicitly out of scope for this plan.

**Architecture:** Add `tracing::warn!`/`error!` calls at every point in `src/services/downloader/mod.rs`, `src/services/covert.rs`, and `src/services/downloader/utils.rs` where an error or `None` is currently returned silently — the underlying error value is already in scope at each of those points, so no `Result`-based refactor (that's Spec 05's job) is needed. Converter failures specifically log at `error!` so they reach Sentry (per `main.rs`'s existing `EventFilter::Event` on `ERROR`); other per-mirror failures log at `warn!` since sequential failover is expected to recover from a single dead mirror. Add `metrics = "0.24"` as a direct dependency (already pinned transitively via `axum-prometheus` → `metrics-exporter-prometheus`, so this adds no new crate to the dependency tree) and record `downloader_source_requests_total{source,outcome}`, `converter_requests_total{outcome}`, and a `download_stage_duration_seconds{stage}` histogram at the same call sites. Add `#[tracing::instrument]` to `book_download` so every log line emitted underneath it during a request inherits `source_id`/`remote_id`/`file_type` as span fields automatically, per tracing's span-context propagation — no need to thread IDs through every function signature.

**Tech Stack:** Rust, `tracing` (already a dependency), `metrics` (new direct dependency, already resolved transitively at 0.24.2), `metrics-util` (new dev-dependency only, for `DebuggingRecorder` in tests, already resolved transitively at 0.20.0). No new crates enter the dependency tree.

## Global Constraints

- Do not add any crate that isn't already in `Cargo.lock` today. `metrics` and `metrics-util` both already appear there as transitive dependencies of `axum-prometheus`/`metrics-exporter-prometheus` — adding them as explicit `[dependencies]`/`[dev-dependencies]` entries must resolve to the exact versions already locked (`metrics = "0.24"`, `metrics-util = "0.20"` as a dev-dependency).
- `cargo build` and `cargo test` must pass after every task.
- Follow the existing test convention: hand-roll a raw HTTP server with `tokio::net::TcpListener` for HTTP-behavior tests (no mocking crate). Never force `config::CONFIG`'s `Lazy` init from a test — it panics because required env vars aren't set in the test process. This means `covert.rs`, `views.rs`, and `book_download` (which all read `config::CONFIG` internally) cannot get new automated tests as part of this plan without a larger `Result`/dependency-injection refactor that's out of scope here (that's Spec 05's concern) — those tasks are verified by full-suite regression + manual run instead, consistent with how the prior timeouts/failover plan handled `covert.rs` and the SIGTERM handler.
- Log severity policy: use `warn!` for a single mirror/stage failure that failover or the caller can route around; use `error!` only for failures that are either a bug (a blocking task panicking) or have no fallback (every converter failure — there's exactly one converter, no failover, and Sentry visibility for these is an explicit acceptance criterion).
- Every new log line includes a `stage` field naming which part of the pipeline failed (e.g. `"mirror_fetch"`, `"buffer_response"`, `"unzip"`, `"convert"`, `"zip"`), plus whatever of `source`/`url`, `book_id`/`remote_id`, and `file_type` is available at that call site, so operators can tell which mirror failed, why conversion failed, and why a request returned "Can't download!".
- Metric label values: `source` is the mirror's base URL (bounded set from `FL_SOURCES`, matches the spec's own `source` label naming); `outcome` is a short snake_case string (`success`, `request_error`, `http_error`, `unexpected_html`, `client_build_error`).

---

### Task 1: Metrics + structured logging in `downloader/mod.rs`

**Files:**
- Modify: `Cargo.toml`
- Modify: `src/services/downloader/mod.rs:1-244` (imports, `download`, `download_chain`)
- Test: `src/services/downloader/mod.rs` (`mod tests` block)

**Interfaces:**
- Consumes: nothing new from other tasks.
- Produces: no signature changes — `download`, `download_chain`, `start_download_futures`, `book_download` keep their existing signatures. New Prometheus metrics: `downloader_source_requests_total{source, outcome}` (counter), `download_stage_duration_seconds{stage}` (histogram, stages `"mirror_fetch"`, `"unzip"`, `"zip"`). Later tasks (2, 3) add more outcomes to the same metric family names but don't depend on anything defined here.

- [ ] **Step 1: Add the `metrics` dependency and a `metrics-util` dev-dependency**

In `Cargo.toml`, add `metrics = "0.24"` to `[dependencies]` (anywhere in the list, e.g. right after `axum-prometheus = "0.9.0"`), and add a new `[dev-dependencies]` section at the end of the file:

```toml
axum-prometheus = "0.9.0"
metrics = "0.24"
```

```toml
[dev-dependencies]
metrics-util = "0.20"
```

Run: `cargo tree -p metrics -p metrics-util`
Expected: both resolve to the versions already in `Cargo.lock` before this change (`metrics v0.24.2`, `metrics-util v0.20.0`) — confirms no new crate versions were pulled in.

- [ ] **Step 2: Write the failing tests**

Add these helpers and tests to the `mod tests` block at the bottom of `src/services/downloader/mod.rs` (after the existing `spawn_stalling_server` helper, before the first `#[tokio::test]`):

```rust
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
```

Then add these three tests:

```rust
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
```

- [ ] **Step 3: Run tests to verify they fail to compile**

Run: `cargo test mirror_http_error_is_logged_and_counted mirror_connect_failure_is_logged_and_counted unzip_failure_is_logged_with_stage`
Expected: FAIL — `metrics`/`metrics_util` not found, and no log/metric output exists yet at these call sites.

- [ ] **Step 4: Add imports and instrument `download`**

At the top of `src/services/downloader/mod.rs`, replace:

```rust
use reqwest::Response;

use crate::config;
```

with:

```rust
use reqwest::Response;
use std::time::Instant;
use tracing::{error, warn};

use crate::config;
```

Then replace the whole `download` function with:

```rust
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

    if book_file_type.to_lowercase() == "html" && content_type.contains("text/html") {
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
```

- [ ] **Step 5: Instrument `download_chain`**

Replace the whole `download_chain` function with:

```rust
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
        let filename = get_filename_by_book(&book, &book.file_type, false, false, normalized);
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
```

- [ ] **Step 6: Run the new tests to verify they pass**

Run: `cargo test mirror_http_error_is_logged_and_counted mirror_connect_failure_is_logged_and_counted unzip_failure_is_logged_with_stage`
Expected: PASS (3 passed).

- [ ] **Step 7: Run the full test suite to check for regressions**

Run: `cargo test`
Expected: PASS — all pre-existing `downloader` tests (`direct_success_skips_conversion_attempt`, `stalled_mirror_fails_over_to_next_source`, `overall_deadline_bounds_total_latency_even_if_all_mirrors_stall`, `missing_content_length_falls_back_to_buffering`, `html_zip_missing_content_length_falls_back_to_buffering`, `valid_content_length_streams_without_buffering`, `binary_content_type_does_not_panic`, `corrupt_zip_body_returns_none_instead_of_panicking`, `oversized_body_without_content_length_is_rejected`) plus the 3 new tests, plus every other module's tests, all pass unchanged — this task only adds side effects at existing return points, no control-flow changes.

- [ ] **Step 8: Commit**

```bash
git add Cargo.toml Cargo.lock src/services/downloader/mod.rs
git commit -m "feat: add per-source metrics and structured error logging to the download pipeline"
```

---

### Task 2: Metrics + structured logging in `covert.rs` (Sentry-visible)

**Files:**
- Modify: `src/services/covert.rs`

**Interfaces:**
- Consumes: nothing from Task 1 (independent file).
- Produces: no signature change to `convert_file`. New metric: `converter_requests_total{outcome}` with outcomes `success`, `request_error`, `http_error`, `client_build_error`. Adds a `"convert"` stage value to the `download_stage_duration_seconds` histogram Task 1 created.

`convert_file` reads `config::CONFIG.converter_url`/`converter_api_key` directly (global `Lazy` static), so per the Global Constraints this task cannot get an automated test without a larger dependency-injection refactor — this matches the prior timeouts/failover plan's treatment of the same function (it also declined to add a dedicated test to `covert.rs` for the same reason). Verification here is full-suite regression (nothing else calls `convert_file`, so this is a compile-and-inspect check) plus a manual run.

- [ ] **Step 1: Add logging and metrics to `convert_file`**

Replace the whole of `src/services/covert.rs` with:

```rust
use reqwest::{Body, Response};
use std::time::{Duration, Instant};
use tempfile::SpooledTempFile;
use tokio_util::io::ReaderStream;
use tracing::error;

use crate::config;

use super::downloader::types::spooled_temp_file_into_async_read;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

pub async fn convert_file(file: SpooledTempFile, file_type: String) -> Option<Response> {
    let body = Body::wrap_stream(ReaderStream::new(spooled_temp_file_into_async_read(file)));

    let client = match reqwest::Client::builder()
        .connect_timeout(config::CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
    {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!("converter_requests_total", "outcome" => "client_build_error")
                .increment(1);
            error!(
                file_type = %file_type,
                stage = "convert",
                error = %err,
                "failed to build converter HTTP client"
            );
            return None;
        }
    };

    let convert_start = Instant::now();
    let response = client
        .post(format!("{}{}", config::CONFIG.converter_url, file_type))
        .body(body)
        .header("Authorization", &config::CONFIG.converter_api_key)
        .send()
        .await;

    let response = match response {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!("converter_requests_total", "outcome" => "request_error")
                .increment(1);
            error!(
                file_type = %file_type,
                stage = "convert",
                error = %err,
                "converter request failed"
            );
            return None;
        }
    };

    let response = match response.error_for_status() {
        Ok(v) => v,
        Err(err) => {
            metrics::counter!("converter_requests_total", "outcome" => "http_error").increment(1);
            error!(
                file_type = %file_type,
                stage = "convert",
                error = %err,
                "converter returned an error status"
            );
            return None;
        }
    };

    metrics::counter!("converter_requests_total", "outcome" => "success").increment(1);
    metrics::histogram!("download_stage_duration_seconds", "stage" => "convert")
        .record(convert_start.elapsed().as_secs_f64());

    Some(response)
}
```

- [ ] **Step 2: Run the full test suite**

Run: `cargo test`
Expected: PASS — `covert.rs` has no tests of its own; this confirms the crate still compiles and every other module's tests still pass unchanged (nothing calls `convert_file` in a test today).

- [ ] **Step 3: Manually verify converter failures are logged at `error!` (Sentry-visible) and counted**

`main.rs`'s `sentry_layer` forwards exactly `EventFilter::Event` for `&tracing::Level::ERROR`, unchanged by this plan — this step proves the new `error!` call sites in `convert_file` actually fire and are counted, which is what feeds that pre-existing forwarding path. Stand up a fake `book_library` (returns a valid book) and a fake mirror (serves a zip for the `fb2` path, 404s for `epub` so the direct attempt fails over into the convert path), then point `CONVERTER_URL` at an unreachable address so `convert_file`'s `request_error` branch fires:

```bash
mkdir -p /tmp/mirror_files
python3 -c "
import zipfile
with zipfile.ZipFile('/tmp/mirror_files/book.zip', 'w') as zf:
    zf.writestr('book.fb2', '<FictionBook>fake fb2 content</FictionBook>')
"

cat > /tmp/fake_book_library.py << 'EOF'
import http.server, json

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        body = {"id": 1, "title": "Test Book", "lang": "ru", "file_type": "fb2", "uploaded": "2024-01-01", "authors": []}
        self.wfile.write(json.dumps(body).encode())
    def log_message(self, format, *args):
        pass

http.server.HTTPServer(("127.0.0.1", 9091), Handler).serve_forever()
EOF

cat > /tmp/fake_mirror.py << 'EOF'
import http.server

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/b/2/fb2":
            with open("/tmp/mirror_files/book.zip", "rb") as f:
                data = f.read()
            self.send_response(200)
            self.send_header("Content-Type", "application/zip")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        else:
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
    def log_message(self, format, *args):
        pass

http.server.HTTPServer(("127.0.0.1", 9092), Handler).serve_forever()
EOF

python3 /tmp/fake_book_library.py &
python3 /tmp/fake_mirror.py &
sleep 1
curl -s -m 2 http://127.0.0.1:9091/api/v1/books/remote/1/2 > /dev/null && echo "book_library ready"

cargo build --release
API_KEY=test \
FL_SOURCES='[{"url":"http://127.0.0.1:9092"}]' \
BOOK_LIBRARY_API_KEY=test \
BOOK_LIBRARY_URL=http://127.0.0.1:9091 \
CONVERTER_URL=http://127.0.0.1:1/ \
CONVERTER_API_KEY=test \
./target/release/books_downloader &
sleep 2
curl -s -H "Authorization: test" http://127.0.0.1:8080/download/1/2/epub -o /dev/null
sleep 1
curl -s -H "Authorization: test" http://127.0.0.1:8080/metrics | grep converter_requests_total
pkill -f target/release/books_downloader
pkill -f fake_book_library.py
pkill -f fake_mirror.py
```

Expected: the process's stdout contains an `ERROR`-level line — `converter request failed file_type=epub stage="convert" error=...` — nested inside the `book_download{source_id=1 remote_id=2 file_type="epub"}` span (confirming Task 5's span identity threads through this call site too), and `/metrics` includes `converter_requests_total{outcome="request_error"} 1`. (This exact scenario was run against the finished Task 1–5 code during plan authoring and produced this output.)

- [ ] **Step 4: Commit**

```bash
git add src/services/covert.rs
git commit -m "feat: log converter failures at error level and count outcomes"
```

---

### Task 3: Structured logging in `downloader/utils.rs`

**Files:**
- Modify: `src/services/downloader/utils.rs`
- Test: `src/services/downloader/utils.rs` (`mod tests` block)

**Interfaces:**
- Consumes: nothing from Tasks 1–2.
- Produces: no signature changes to `response_to_tempfile`/`response_to_download_data`/`parse_content_length`.

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block at the bottom of `src/services/downloader/utils.rs` (after the existing `non_ascii_content_length_returns_none` test):

```rust
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

    #[tokio::test]
    async fn declared_content_length_over_limit_is_rejected_and_logged() {
        let capture = CapturingWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(capture.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999999999\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });

        let mut response = reqwest::Client::new()
            .get(format!("http://{addr}"))
            .send()
            .await
            .unwrap();

        let result = response_to_tempfile(&mut response, 10).await;

        assert!(result.is_none());
        let logs = capture.contents();
        assert!(
            logs.contains("declared Content-Length above the configured download limit"),
            "expected the oversized-declared-length log line, got: {logs}"
        );
        assert!(
            logs.contains(&format!("http://{addr}")),
            "expected the log line to name the response URL, got: {logs}"
        );
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test declared_content_length_over_limit_is_rejected_and_logged`
Expected: FAIL — `response_to_tempfile` still returns `None` (test's `assert!(result.is_none())` passes) but the log-content assertion fails because no log line is emitted yet.

- [ ] **Step 3: Add logging to `response_to_tempfile`**

Replace the whole of `src/services/downloader/utils.rs` above the `#[cfg(test)]` block with:

```rust
use bytes::Buf;
use reqwest::Response;
use tempfile::SpooledTempFile;
use tracing::{error, warn};

use std::io::{Seek, SeekFrom, Write};

use super::types::Data;

pub fn parse_content_length(headers: &reqwest::header::HeaderMap) -> Option<usize> {
    headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}

pub async fn response_to_tempfile(
    res: &mut Response,
    max_bytes: usize,
) -> Option<(SpooledTempFile, usize)> {
    let url = res.url().clone();

    if let Some(declared) = res.content_length() {
        if declared > max_bytes as u64 {
            warn!(
                url = %url,
                declared_bytes = declared,
                max_bytes,
                stage = "buffer_response",
                "response declared Content-Length above the configured download limit"
            );
            return None;
        }
    }

    let mut tmp_file = tempfile::spooled_tempfile(5 * 1024 * 1024);

    let mut data_size: usize = 0;

    {
        loop {
            let chunk = res.chunk().await;

            let result = match chunk {
                Ok(v) => v,
                Err(err) => {
                    warn!(
                        url = %url,
                        stage = "buffer_response",
                        error = %err,
                        "error reading response body chunk"
                    );
                    return None;
                }
            };

            let data = match result {
                Some(v) => v,
                None => break,
            };

            data_size += data.len();

            if data_size > max_bytes {
                warn!(
                    url = %url,
                    max_bytes,
                    stage = "buffer_response",
                    "response body exceeded the configured download limit"
                );
                return None;
            }

            match tmp_file.write_all(data.chunk()) {
                Ok(_) => (),
                Err(err) => {
                    error!(
                        url = %url,
                        stage = "buffer_response",
                        error = %err,
                        "failed to write response body to temp file"
                    );
                    return None;
                }
            }
        }

        match tmp_file.seek(SeekFrom::Start(0)) {
            Ok(_) => (),
            Err(err) => {
                error!(
                    url = %url,
                    stage = "buffer_response",
                    error = %err,
                    "failed to rewind buffered temp file"
                );
                return None;
            }
        }
    }

    Some((tmp_file, data_size))
}

pub async fn response_to_download_data(
    mut response: Response,
    max_bytes: usize,
) -> Option<(Data, usize)> {
    if let Some(size) = parse_content_length(response.headers()) {
        return Some((Data::Response(response), size));
    }

    let (tmp_file, size) = response_to_tempfile(&mut response, max_bytes).await?;
    Some((Data::SpooledTempFile(tmp_file), size))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test`
Expected: PASS — the new test plus all pre-existing `utils::tests` (`parses_valid_content_length`, `missing_content_length_returns_none`, `non_numeric_content_length_returns_none`, `non_ascii_content_length_returns_none`) and every other module's tests.

- [ ] **Step 5: Commit**

```bash
git add src/services/downloader/utils.rs
git commit -m "feat: log response-buffering failures in downloader/utils"
```

---

### Task 4: Structured logging in `views.rs`

**Files:**
- Modify: `src/views.rs:34-101`

**Interfaces:**
- Consumes: nothing from Tasks 1–3.
- Produces: no signature changes to `download`/`get_filename`.

`download` and `get_filename` call `book_download`/`get_book`/`get_book`, which read `config::CONFIG` internally, so — per the Global Constraints — this task cannot get an automated test without forcing `CONFIG`'s `Lazy` init (which panics without the required env vars) or a larger DI refactor. `views.rs` has no existing test module today for the same reason. Verified by full-suite regression + manual run.

- [ ] **Step 1: Add logging to the `download` and `get_filename` handlers**

In `src/views.rs`, add `tracing::warn` to the imports — replace:

```rust
use tracing::Level;
```

with:

```rust
use tracing::{warn, Level};
```

Then replace the `download` function with:

```rust
pub async fn download(
    Path((source_id, remote_id, file_type)): Path<(u32, u32, String)>,
    Query(params): Query<FilenameParams>,
) -> impl IntoResponse {
    let normalized = params.normalized.unwrap_or(true);

    let download_result =
        match book_download(source_id, remote_id, file_type.as_str(), normalized).await {
            Ok(v) => v,
            Err(err) => {
                warn!(
                    source_id,
                    remote_id,
                    file_type = %file_type,
                    stage = "resolve_book",
                    error = %err,
                    "failed to resolve the requested book from book_library"
                );
                return Err((StatusCode::NO_CONTENT, "Can't download!".to_string()));
            }
        };

    let data = match download_result {
        Some(v) => v,
        None => {
            warn!(
                source_id,
                remote_id,
                file_type = %file_type,
                stage = "download",
                "no source could serve the requested book"
            );
            return Err((StatusCode::NO_CONTENT, "Can't download!".to_string()));
        }
    };

    let filename = data.filename.clone();
    let filename_ascii = data.filename_ascii.clone();
    let file_size = data.data_size;

    let reader = data.get_async_read();
    let stream = ReaderStream::new(reader);

    let encoder = general_purpose::STANDARD;

    let headers = AppendHeaders([
        (
            header::CONTENT_DISPOSITION,
            format!("attachment; filename={filename_ascii}"),
        ),
        (header::CONTENT_LENGTH, format!("{file_size}")),
        (
            header::HeaderName::from_static("x-filename-b64-ascii"),
            encoder.encode(filename_ascii),
        ),
        (
            header::HeaderName::from_static("x-filename-b64"),
            encoder.encode(filename),
        ),
    ]);

    Ok((headers, Body::from_stream(stream)))
}
```

Then replace the `get_filename` function with:

```rust
pub async fn get_filename(
    Path((book_id, file_type)): Path<(u32, String)>,
    Query(params): Query<FilenameParams>,
) -> impl IntoResponse {
    let normalized = params.normalized.unwrap_or(true);

    let (filename, filename_ascii) = match get_book(book_id).await {
        Ok(book) => (
            get_filename_by_book(&book, file_type.as_str(), false, false, normalized),
            get_filename_by_book(&book, file_type.as_str(), false, true, normalized),
        ),
        Err(err) => {
            warn!(
                book_id,
                file_type = %file_type,
                stage = "resolve_book",
                error = %err,
                "book not found in book_library"
            );
            return (StatusCode::BAD_REQUEST, "Book not found!".to_string());
        }
    };

    (
        StatusCode::OK,
        json!({
            "filename": filename,
            "filename_ascii": filename_ascii
        })
        .to_string(),
    )
}
```

- [ ] **Step 2: Run the full test suite**

Run: `cargo test`
Expected: PASS — `views.rs` has no tests of its own; this confirms the crate still compiles and every other module's tests still pass unchanged.

- [ ] **Step 3: Commit**

```bash
git add src/views.rs
git commit -m "feat: log request-handler-level download and filename-lookup failures"
```

---

### Task 5: Span identity via `#[tracing::instrument]` on `book_download`

**Files:**
- Modify: `src/services/downloader/mod.rs:246-270`

**Interfaces:**
- Consumes: Task 1's edits to the same file (this task only touches `book_download`, which Task 1 didn't modify).
- Produces: no signature change. Every `warn!`/`error!` emitted by `book_download`'s call tree (`get_remote_book`, `start_download_futures`, `download_chain`, `download`, `convert_file`, `response_to_tempfile`) during a single request now carries `source_id`, `remote_id`, `file_type` as inherited span fields, satisfying the acceptance criterion that log lines for a given request can be correlated.

`book_download` reads `config::CONFIG` internally (via the `start_download_futures` call), so per the Global Constraints it cannot get a new automated test without forcing `CONFIG`'s `Lazy` init. This is also a single-attribute, non-branching change — `#[tracing::instrument]` only wraps span creation around the existing function body, it doesn't alter control flow. Verified by full-suite regression + a manual run.

- [ ] **Step 1: Add the `#[tracing::instrument]` attribute**

In `src/services/downloader/mod.rs`, replace:

```rust
pub async fn book_download(
    source_id: u32,
    remote_id: u32,
    file_type: &str,
    normalized: bool,
) -> Result<Option<DownloadResult>, Box<dyn std::error::Error + Send + Sync>> {
```

with:

```rust
#[tracing::instrument(skip(normalized))]
pub async fn book_download(
    source_id: u32,
    remote_id: u32,
    file_type: &str,
    normalized: bool,
) -> Result<Option<DownloadResult>, Box<dyn std::error::Error + Send + Sync>> {
```

(`normalized` is skipped because it's a boolean flag, not a request-identity field — including it would add noise without helping correlation.)

- [ ] **Step 2: Run the full test suite**

Run: `cargo test`
Expected: PASS — no test calls `book_download` directly (it requires live `config::CONFIG`), so this is a compile-and-regression check confirming the attribute doesn't break anything else in the file.

- [ ] **Step 3: Manually verify span fields appear in logs**

The `book_download` span only wraps `get_remote_book` + `start_download_futures`, and it's *exited* as soon as `book_download` returns — so a failure that happens in the caller (`views::download`, Task 4) after `book_download` returns `Err`/`None` will NOT show the span (that log line's fields come from the explicit `warn!(source_id, remote_id, ...)` call in `views.rs` instead). To see the span itself, the failure needs to happen *inside* `book_download`'s call tree — e.g. a mirror-fetch failure from Task 1's `download()`. That requires `book_library` to resolve successfully first, so stand up a throwaway fake `book_library` alongside an unreachable mirror:

```bash
cat > /tmp/fake_book_library.py << 'EOF'
import http.server, json

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        body = {"id": 1, "title": "Test Book", "lang": "ru", "file_type": "fb2", "uploaded": "2024-01-01", "authors": []}
        self.wfile.write(json.dumps(body).encode())
    def log_message(self, format, *args):
        pass

http.server.HTTPServer(("127.0.0.1", 9091), Handler).serve_forever()
EOF
python3 /tmp/fake_book_library.py &

cargo build --release
API_KEY=test \
FL_SOURCES='[{"url":"http://127.0.0.1:1"}]' \
BOOK_LIBRARY_API_KEY=test \
BOOK_LIBRARY_URL=http://127.0.0.1:9091 \
CONVERTER_URL=http://127.0.0.1:1/ \
CONVERTER_API_KEY=test \
./target/release/books_downloader &

sleep 1
curl -s -H "Authorization: test" http://127.0.0.1:8080/download/1/2/fb2 -o /dev/null
sleep 1
pkill -f target/release/books_downloader
pkill -f fake_book_library.py
```

Expected output includes a line like:

```
WARN request{method=GET uri=/download/1/2/fb2 version=HTTP/1.1}:book_download{source_id=1 remote_id=2 file_type="fb2"}: mirror request failed source=http://127.0.0.1:1 book_id=2 file_type="fb2" stage="mirror_fetch" error=...
```

— confirming the `book_download{source_id=1 remote_id=2 file_type="fb2"}` span context prefixes the `warn!` line emitted by `download()` deep inside the call tree, without `download()` itself knowing about `source_id`. (This exact command was run against the finished Task 1–5 code during plan authoring and produced this output.)

- [ ] **Step 4: Commit**

```bash
git add src/services/downloader/mod.rs
git commit -m "feat: instrument book_download to correlate logs by request identity"
```
