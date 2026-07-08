# Panic on Untrusted Input Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate every `unwrap()` in the download path that can be triggered by untrusted external data (mirror HTTP responses, ZIP contents, config env vars), so a malformed mirror response degrades to an HTTP error instead of aborting the whole process.

**Architecture:** Replace direct `unwrap()`s with `Option`/`Result`-propagating code (`?`, `.ok()?`, `unwrap_or`). Extract one shared helper for "turn a `Response` into `(Data, usize)`" that transparently falls back to buffering when `Content-Length` is missing/invalid. Validate and build per-source HTTP clients once at config-load time instead of per-request. Make `SENTRY_DSN` optional. Remove `panic = 'abort'` last, once nothing left can panic on network/ZIP input.

**Tech Stack:** Rust, tokio, reqwest, axum, zip, tempfile. No new dependencies — tests that need a controllable HTTP server use a hand-rolled `tokio::net::TcpListener` responder instead of adding a mocking crate.

## Global Constraints

- Do not add new crates/dependencies — `tokio` (already has the `full` feature) is enough to hand-roll a raw HTTP test server.
- Only fix the `unwrap()`s enumerated in `docs/specs/01-panic-on-untrusted-input.md` (01.1–01.6). Leave local-only unwraps (e.g. seeking a freshly created temp file) alone — they don't depend on untrusted input and are out of spec scope.
- Preserve current streaming behavior: when a mirror sends a valid `Content-Length`, the response must still stream via `Data::Response` (no unnecessary buffering).
- `cargo test` and `cargo build --release` must pass after every task.

---

### Task 1: Safe `Content-Length` parsing helper

**Files:**
- Modify: `src/services/downloader/utils.rs`

**Interfaces:**
- Produces: `pub fn parse_content_length(headers: &reqwest::header::HeaderMap) -> Option<usize>` — used by Task 2's `response_to_download_data`.

- [ ] **Step 1: Write the failing tests**

Add to the bottom of `src/services/downloader/utils.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail to compile (function doesn't exist yet)**

Run: `cargo test --lib downloader::utils -- --nocapture`
Expected: FAIL with `cannot find function 'parse_content_length' in this scope`

- [ ] **Step 3: Implement `parse_content_length`**

Add above `pub async fn response_to_tempfile` in `src/services/downloader/utils.rs`:

```rust
pub fn parse_content_length(headers: &reqwest::header::HeaderMap) -> Option<usize> {
    headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib downloader::utils`
Expected: `test result: ok. 4 passed`

- [ ] **Step 5: Commit**

```bash
git add src/services/downloader/utils.rs
git commit -m "fix: parse Content-Length header without panicking on missing/invalid values"
```

---

### Task 2: Fallback-buffering helper, wired into both duplicated call sites

**Files:**
- Modify: `src/services/downloader/utils.rs`
- Modify: `src/services/downloader/mod.rs:19,35-44,101-108,121-128`

**Interfaces:**
- Consumes: `parse_content_length` (Task 1), `response_to_tempfile(&mut Response) -> Option<(SpooledTempFile, usize)>` (existing), `Data`, `SpooledTempAsyncRead` (existing, from `crate::services::downloader::types`).
- Produces: `pub async fn response_to_download_data(response: Response) -> Option<(Data, usize)>` — used by `download_chain` in `mod.rs`.

- [ ] **Step 1: Write the failing tests**

Add to the bottom of `src/services/downloader/mod.rs` (creating the module if absent):

```rust
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
        // NOTE: Task 5 adds a `client` field to `SourceConfig`. When that
        // task runs, update this helper to `config::SourceConfig { url, proxy: None, client: reqwest::Client::new() }`.
        config::SourceConfig { url, proxy: None }
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

        let result = download_chain(book, "fb2".to_string(), source_config, false, true).await;

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

        let result = download_chain(book, "html".to_string(), source_config, false, true).await;

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

        let result = download_chain(book, "fb2".to_string(), source_config, false, true).await;

        let data = result.expect("download_chain should succeed with valid Content-Length");
        assert_eq!(data.data_size, body.len());
        assert!(matches!(data.data, Data::Response(_)));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test --lib downloader::tests`
Expected: FAIL — compile error (`response_to_download_data` doesn't exist yet) or panics (old code still does `.unwrap()` on the missing `Content-Length` header).

- [ ] **Step 3: Implement `response_to_download_data` in `utils.rs`**

Add to `src/services/downloader/utils.rs`, above the `#[cfg(test)]` block:

```rust
use super::types::{Data, SpooledTempAsyncRead};

pub async fn response_to_download_data(mut response: Response) -> Option<(Data, usize)> {
    if let Some(size) = parse_content_length(response.headers()) {
        return Some((Data::Response(response), size));
    }

    let (tmp_file, size) = response_to_tempfile(&mut response).await?;
    Some((
        Data::SpooledTempAsyncRead(SpooledTempAsyncRead::new(tmp_file)),
        size,
    ))
}
```

(Place the `use super::types::{Data, SpooledTempAsyncRead};` line with the other `use` statements at the top of the file.)

- [ ] **Step 4: Wire the helper into `mod.rs`, replacing both duplicated unwrap blocks**

In `src/services/downloader/mod.rs`, change the imports:

```rust
use self::types::{Data, DownloadResult, SpooledTempAsyncRead};
use self::utils::{response_to_download_data, response_to_tempfile};
```

Replace the first duplicated block (the `is_zip && book.file_type.to_lowercase() == "html"` branch):

```rust
    if is_zip && book.file_type.to_lowercase() == "html" {
        let filename = get_filename_by_book(&book, &file_type, true, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, true, true, normalized);
        let (data, data_size) = match response_to_download_data(response).await {
            Some(v) => v,
            None => return None,
        };

        return Some(DownloadResult::new(data, filename, filename_ascii, data_size));
    }
```

Replace the second duplicated block (the `!is_zip && !final_need_zip && !converting` branch):

```rust
    if !is_zip && !final_need_zip && !converting {
        let filename = get_filename_by_book(&book, &book.file_type, false, false, normalized);
        let filename_ascii = get_filename_by_book(&book, &file_type, false, true, normalized);
        let (data, data_size) = match response_to_download_data(response).await {
            Some(v) => v,
            None => return None,
        };

        return Some(DownloadResult::new(data, filename, filename_ascii, data_size));
    };
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --lib downloader`
Expected: `test result: ok.` for all `downloader::tests::*` and `downloader::utils::tests::*` tests.

- [ ] **Step 6: Commit**

```bash
git add src/services/downloader/utils.rs src/services/downloader/mod.rs
git commit -m "fix: fall back to buffering when mirror omits or corrupts Content-Length"
```

---

### Task 3: Stop panicking on a non-UTF8 `Content-Type` header

**Files:**
- Modify: `src/services/downloader/mod.rs:60`

**Interfaces:**
- None (self-contained one-line fix inside `download()`, already covered by Task 2's `download` signature).

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `src/services/downloader/mod.rs` (from Task 2):

```rust
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
```

- [ ] **Step 2: Run test to verify it fails (panics)**

Run: `cargo test --lib downloader::tests::binary_content_type_does_not_panic`
Expected: FAIL — test thread panics inside `v.to_str().unwrap()`.

- [ ] **Step 3: Fix the unwrap**

In `src/services/downloader/mod.rs`, in `download()`:

```rust
    let headers = response.headers();
    let content_type = match headers.get("Content-Type") {
        Some(v) => v.to_str().unwrap_or(""),
        None => "",
    };
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test --lib downloader::tests::binary_content_type_does_not_panic`
Expected: `test result: ok. 1 passed`

- [ ] **Step 5: Commit**

```bash
git add src/services/downloader/mod.rs
git commit -m "fix: treat non-UTF8 Content-Type as empty instead of panicking"
```

---

### Task 4: Stop panicking on malformed ZIP archives

**Files:**
- Modify: `src/services/downloader/zip.rs`
- Modify: `src/services/downloader/mod.rs` (test module only, from Task 2)

**Interfaces:**
- Produces: `pub fn unzip(tmp_file: SpooledTempFile, file_type: &str) -> Option<(SpooledTempFile, usize)>` and `pub fn zip(tmp_file: &mut SpooledTempFile, filename: &str) -> Option<(SpooledTempFile, usize)>` — signatures unchanged, only their internals stop panicking.

- [ ] **Step 1: Write the failing tests**

Add to the bottom of `src/services/downloader/zip.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn corrupt_zip_bytes_return_none_instead_of_panicking() {
        let mut tmp_file = tempfile::spooled_tempfile(1024);
        tmp_file.write_all(b"this is not a zip file").unwrap();
        tmp_file.rewind().unwrap();

        let result = unzip(tmp_file, "fb2");

        assert!(result.is_none());
    }

    #[test]
    fn zip_then_unzip_round_trips_content() {
        let original = b"fb2 file contents";
        let mut input = tempfile::spooled_tempfile(1024);
        input.write_all(original).unwrap();
        input.rewind().unwrap();

        let (zipped, zipped_size) = zip(&mut input, "book.fb2").expect("zip should succeed");
        assert!(zipped_size > 0);

        let (mut unzipped, unzipped_size) =
            unzip(zipped, "fb2").expect("unzip should find the fb2 entry");
        assert_eq!(unzipped_size, original.len());

        let mut contents = Vec::new();
        std::io::Read::read_to_end(&mut unzipped, &mut contents).unwrap();
        assert_eq!(contents, original);
    }
}
```

- [ ] **Step 2: Run tests to verify `corrupt_zip_bytes_return_none_instead_of_panicking` fails**

Run: `cargo test --lib downloader::zip::tests`
Expected: FAIL — `corrupt_zip_bytes_return_none_instead_of_panicking` panics inside `zip::ZipArchive::new(tmp_file).unwrap()`; `zip_then_unzip_round_trips_content` should already pass since the happy path doesn't hit any unwrap issue yet.

- [ ] **Step 3: Remove the unwraps**

Replace the full contents of `src/services/downloader/zip.rs` (above the test module) with:

```rust
use std::io::Seek;

use tempfile::SpooledTempFile;
use zip::write::FileOptions;

pub fn unzip(tmp_file: SpooledTempFile, file_type: &str) -> Option<(SpooledTempFile, usize)> {
    let mut archive = zip::ZipArchive::new(tmp_file).ok()?;

    let file_type_lower = file_type.to_lowercase();

    for i in 0..archive.len() {
        let mut file = archive.by_index(i).ok()?;
        let filename = file.name();

        if filename.contains(&file_type_lower) || file.name().to_lowercase() == "elector" {
            let mut output_file = tempfile::spooled_tempfile(5 * 1024 * 1024);

            let size: usize = match std::io::copy(&mut file, &mut output_file) {
                Ok(v) => v.try_into().ok()?,
                Err(_) => return None,
            };

            output_file.rewind().ok()?;

            return Some((output_file, size));
        }
    }

    None
}

pub fn zip(tmp_file: &mut SpooledTempFile, filename: &str) -> Option<(SpooledTempFile, usize)> {
    let output_file = tempfile::spooled_tempfile(5 * 1024 * 1024);
    let mut archive = zip::ZipWriter::new(output_file);

    let options: FileOptions<_> = FileOptions::default()
        .compression_level(Some(9))
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o755);

    archive.start_file::<&str, ()>(filename, options).ok()?;

    std::io::copy(tmp_file, &mut archive).ok()?;

    let mut archive_result = archive.finish().ok()?;

    let data_size: usize = archive_result.stream_position().ok()?.try_into().ok()?;

    archive_result.rewind().ok()?;

    Some((archive_result, data_size))
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib downloader::zip::tests`
Expected: `test result: ok. 2 passed`

- [ ] **Step 5: Add the end-to-end regression test through `download_chain`**

Add to the `mod tests` block in `src/services/downloader/mod.rs`:

```rust
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

        let result = download_chain(book, "fb2zip".to_string(), source_config, false, true).await;

        assert!(result.is_none());
    }
```

- [ ] **Step 6: Run the full downloader test suite**

Run: `cargo test --lib downloader`
Expected: `test result: ok.` — all tests pass, including the new end-to-end one.

- [ ] **Step 7: Commit**

```bash
git add src/services/downloader/zip.rs src/services/downloader/mod.rs
git commit -m "fix: return None instead of panicking on malformed ZIP archives"
```

---

### Task 5: Validate proxies and build clients at startup; make `SENTRY_DSN` optional

**Files:**
- Modify: `src/config.rs`
- Modify: `src/services/downloader/mod.rs:19,35-44`
- Modify: `src/main.rs`

**Interfaces:**
- Produces: `pub struct SourceConfig { pub url: String, pub proxy: Option<String>, pub client: reqwest::Client }` (adds `client`), `pub sentry_dsn: Option<String>` on `Config` (was `String`).
- Consumes (Task 2/3's `download()`): now reads `source_config.client` directly instead of building a client per request.

- [ ] **Step 1: Write the failing tests**

Add to the bottom of `src/config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_sources_without_proxy() {
        let sources = parse_fl_sources(r#"[{"url": "http://example.com"}]"#);
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].url, "http://example.com");
        assert!(sources[0].proxy.is_none());
    }

    #[test]
    fn parses_valid_sources_with_proxy() {
        let sources = parse_fl_sources(
            r#"[{"url": "http://example.com", "proxy": "http://proxy.local:8080"}]"#,
        );
        assert_eq!(
            sources[0].proxy.as_deref(),
            Some("http://proxy.local:8080")
        );
    }

    #[test]
    #[should_panic(expected = "FL_SOURCES must be a JSON array")]
    fn invalid_json_panics_with_actionable_message() {
        parse_fl_sources("not json");
    }

    #[test]
    #[should_panic(expected = "invalid proxy URL")]
    fn invalid_proxy_panics_at_load_time() {
        parse_fl_sources(r#"[{"url": "http://example.com", "proxy": "not a valid proxy url"}]"#);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cargo test --lib config`
Expected: FAIL — `cannot find function 'parse_fl_sources' in this scope`, and `SourceConfig` has no field `client`.

- [ ] **Step 3: Rewrite `src/config.rs`**

Replace the full contents of `src/config.rs` above the `#[cfg(test)]` block with:

```rust
use once_cell::sync::Lazy;
use serde::Deserialize;

fn get_env(env: &'static str) -> String {
    std::env::var(env).unwrap_or_else(|_| panic!("Cannot get the {} env variable", env))
}

#[derive(Deserialize)]
struct RawSourceConfig {
    url: String,
    proxy: Option<String>,
}

#[derive(Clone)]
pub struct SourceConfig {
    pub url: String,
    pub proxy: Option<String>,
    pub client: reqwest::Client,
}

fn build_client(proxy: Option<&str>) -> reqwest::Client {
    match proxy {
        Some(v) => {
            let proxy = reqwest::Proxy::http(v)
                .unwrap_or_else(|err| panic!("FL_SOURCES: invalid proxy URL {v:?}: {err}"));
            reqwest::Client::builder()
                .proxy(proxy)
                .build()
                .unwrap_or_else(|err| {
                    panic!("FL_SOURCES: failed to build HTTP client for proxy {v:?}: {err}")
                })
        }
        None => reqwest::Client::new(),
    }
}

fn parse_fl_sources(raw_json: &str) -> Vec<SourceConfig> {
    let raw: Vec<RawSourceConfig> = serde_json::from_str(raw_json)
        .expect("FL_SOURCES must be a JSON array of {url, proxy?}");

    raw.into_iter()
        .map(|r| {
            let client = build_client(r.proxy.as_deref());
            SourceConfig {
                url: r.url,
                proxy: r.proxy,
                client,
            }
        })
        .collect()
}

pub struct Config {
    pub api_key: String,

    pub fl_sources: Vec<SourceConfig>,

    pub book_library_api_key: String,
    pub book_library_url: String,

    pub converter_url: String,
    pub converter_api_key: String,

    pub sentry_dsn: Option<String>,
}

impl Config {
    pub fn load() -> Config {
        Config {
            api_key: get_env("API_KEY"),

            fl_sources: parse_fl_sources(&get_env("FL_SOURCES")),

            book_library_api_key: get_env("BOOK_LIBRARY_API_KEY"),
            book_library_url: get_env("BOOK_LIBRARY_URL"),

            converter_url: get_env("CONVERTER_URL"),
            converter_api_key: get_env("CONVERTER_API_KEY"),

            sentry_dsn: std::env::var("SENTRY_DSN").ok(),
        }
    }
}

pub static CONFIG: Lazy<Config> = Lazy::new(Config::load);
```

- [ ] **Step 4: Update `download()` in `mod.rs` to use the pre-built client**

In `src/services/downloader/mod.rs`, remove the `CLIENT` static and the per-request client construction:

```rust
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
```

(Everything after `let response = ...send().await;` in `download()` stays as already fixed in Task 3.)

- [ ] **Step 5: Update the test helper `make_source_config` (Task 2/3's tests) for the new `client` field**

In `src/services/downloader/mod.rs`'s `mod tests`, `SourceConfig` now requires a `client`. Update the helper:

```rust
    fn make_source_config(url: String) -> config::SourceConfig {
        config::SourceConfig {
            url,
            proxy: None,
            client: reqwest::Client::new(),
        }
    }
```

- [ ] **Step 6: Make Sentry initialization conditional in `main.rs`**

Replace the top of `src/main.rs`'s `main()`:

```rust
#[tokio::main]
async fn main() {
    let _guard = config::CONFIG.sentry_dsn.as_deref().map(|dsn| {
        let options = ClientOptions {
            dsn: Some(Dsn::from_str(dsn).expect("SENTRY_DSN must be a valid Sentry DSN URL")),
            default_integrations: false,
            ..Default::default()
        }
        .add_integration(DebugImagesIntegration::new());

        sentry::init(options)
    });
```

(The rest of `main()` — tracing setup, router, listener — stays unchanged.)

- [ ] **Step 7: Run tests to verify they pass**

Run: `cargo test --lib`
Expected: `test result: ok.` for the whole crate (config, downloader, downloader::utils, downloader::zip, filename_getter).

- [ ] **Step 8: Manual verification of the SENTRY_DSN-optional acceptance criterion**

Run (with real values for the other required env vars, and `SENTRY_DSN` unset):

```bash
API_KEY=test FL_SOURCES='[{"url":"http://localhost:1"}]' BOOK_LIBRARY_API_KEY=test BOOK_LIBRARY_URL=http://localhost:1 CONVERTER_URL=http://localhost:1 CONVERTER_API_KEY=test cargo run
```

Expected: log line `Start webserver...` appears; process does not panic or exit due to a missing `SENTRY_DSN`. Stop it with Ctrl-C once confirmed.

- [ ] **Step 9: Commit**

```bash
git add src/config.rs src/services/downloader/mod.rs src/main.rs
git commit -m "fix: validate proxies and build HTTP clients at startup; make SENTRY_DSN optional"
```

---

### Task 6: Remove `panic = 'abort'`

**Files:**
- Modify: `Cargo.toml:13`

**Interfaces:**
- None — build-profile-only change, no code interface.

- [ ] **Step 1: Remove the `panic = 'abort'` line**

In `Cargo.toml`, delete this line from `[profile.release]`:

```toml
panic = 'abort'
```

So `[profile.release]` reads:

```toml
[profile.release]
opt-level = 3
debug = false
strip = true
lto = true
codegen-units = 1
```

- [ ] **Step 2: Verify the release profile still builds**

Run: `cargo build --release`
Expected: build succeeds with no errors.

- [ ] **Step 3: Run the full test suite one more time as a regression gate**

Run: `cargo test`
Expected: `test result: ok.` across every module — `config`, `downloader`, `downloader::utils`, `downloader::zip`, `filename_getter`.

- [ ] **Step 4: Confirm no reachable unwraps remain (acceptance criterion grep)**

Run: `grep -rn "unwrap()" src/`
Expected: remaining hits are only local/internal ones out of spec scope (e.g. `utils.rs`'s temp-file seek, `filename_getter.rs`'s `chars().next().unwrap()` guarded by an `is_empty()` check, `main.rs`'s `TcpListener::bind(...).unwrap()` / `axum::serve(...).unwrap()` which are startup-only and not attacker-controlled). None should be reachable from mirror response bodies/headers, ZIP contents, or `FL_SOURCES`/proxy parsing.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml
git commit -m "fix: remove panic=abort now that untrusted-input unwraps are eliminated"
```
