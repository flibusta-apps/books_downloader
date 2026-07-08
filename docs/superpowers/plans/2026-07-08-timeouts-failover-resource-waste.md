# Timeouts, Failover & Resource-Waste Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give every outbound HTTP client an explicit timeout, bound the worst-case latency of a `/download` request, stop racing full downloads/conversions from every mirror simultaneously, fix the HTTPS proxy bypass, and let in-flight requests finish on SIGTERM.

**Architecture:** Add fixed connect-timeout and configurable per-attempt/overall-deadline constants to `config.rs`, apply them when building every `reqwest::Client` (fl_sources, book_library, converter). Replace `start_download_futures`'s all-mirrors-at-once `JoinSet` with a sequential loop over `sources` (now an explicit parameter, not read from global `CONFIG`, so it stays unit-testable) that tries the direct download first and only falls back to the convert path on failure, per mirror, before moving to the next mirror. Wrap the whole loop in `tokio::time::timeout`. Swap `reqwest::Proxy::http` for `reqwest::Proxy::all` so HTTPS mirrors actually go through the configured proxy. Add `axum::serve(...).with_graceful_shutdown(...)` listening for SIGTERM/SIGINT.

**Tech Stack:** Rust, tokio (`time`, `signal` — already enabled via the `full` feature), reqwest, axum. No new dependencies.

## Global Constraints

- Do not add new crates/dependencies.
- `cargo test` and `cargo build --release` must pass after every task.
- Follow the existing test convention in this codebase: hand-roll a raw HTTP server with `tokio::net::TcpListener` for HTTP-behavior tests (no mocking crate). Never force `config::CONFIG`'s `Lazy` init from a test — it panics because required env vars (`API_KEY`, `FL_SOURCES`, etc.) aren't set in the test process. Any function under test must take its dependencies (client, url, timeouts) as explicit parameters instead of reading `config::CONFIG` internally.
- New timeouts:
  - Connect timeout is **fixed** at 10s for every client (`config::CONNECT_TIMEOUT`) — not configurable, per the spec's own "~10s" wording.
  - `MIRROR_TIMEOUT_SECS` — per-mirror-attempt total request timeout, default `300` (5 minutes, generous for large files).
  - `DOWNLOAD_TIMEOUT_SECS` — overall deadline for the whole `/download` failover loop, default `300`.
  - book_library and converter clients get fixed (non-configurable) total-request timeouts: 30s and 120s respectively — they aren't part of the mirror-failover tuning surface the spec calls out.

---

### Task 1: HTTP timeout config + fix HTTPS proxy bypass

**Files:**
- Modify: `src/config.rs`

**Interfaces:**
- Produces: `pub const CONNECT_TIMEOUT: Duration` (10s, consumed by Task 2's book_library/converter clients), `fn build_client(proxy: Option<&str>, mirror_request_timeout: Duration) -> reqwest::Client` (signature change — was `build_client(proxy: Option<&str>)`), `fn parse_fl_sources(raw_json: &str, mirror_request_timeout: Duration) -> Vec<SourceConfig>` (signature change — was `parse_fl_sources(raw_json: &str)`), `pub overall_download_timeout: Duration` field on `Config` — consumed by Task 3's `book_download`.

- [ ] **Step 1: Write the failing tests**

Update the four existing calls to `parse_fl_sources` in the `mod tests` block at the bottom of `src/config.rs` to pass a timeout, and add new tests. Replace the whole `mod tests` block with:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn parses_valid_sources_without_proxy() {
        let sources = parse_fl_sources(r#"[{"url": "http://example.com"}]"#, Duration::from_secs(5));
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].url, "http://example.com");
        assert!(sources[0].proxy.is_none());
    }

    #[test]
    fn parses_valid_sources_with_proxy() {
        let sources = parse_fl_sources(
            r#"[{"url": "http://example.com", "proxy": "http://proxy.local:8080"}]"#,
            Duration::from_secs(5),
        );
        assert_eq!(sources[0].proxy.as_deref(), Some("http://proxy.local:8080"));
    }

    #[test]
    #[should_panic(expected = "FL_SOURCES must be a JSON array")]
    fn invalid_json_panics_with_actionable_message() {
        parse_fl_sources("not json", Duration::from_secs(5));
    }

    #[test]
    #[should_panic(expected = "invalid proxy URL")]
    fn invalid_proxy_panics_at_load_time() {
        parse_fl_sources(
            r#"[{"url": "http://example.com", "proxy": "not a valid proxy url"}]"#,
            Duration::from_secs(5),
        );
    }

    #[test]
    fn download_limit_missing_value_uses_default() {
        assert_eq!(parse_usize_or(None, "MAX_DOWNLOAD_BYTES", 42), 42);
    }

    #[test]
    fn download_limit_valid_value_overrides_default() {
        assert_eq!(parse_usize_or(Some("1234"), "MAX_DOWNLOAD_BYTES", 42), 1234);
    }

    #[test]
    #[should_panic(expected = "MAX_DOWNLOAD_BYTES must be a valid non-negative integer")]
    fn download_limit_invalid_value_panics() {
        parse_usize_or(Some("not-a-number"), "MAX_DOWNLOAD_BYTES", 42);
    }

    #[test]
    fn compression_ratio_missing_value_uses_default() {
        assert_eq!(parse_u64_or(None, "MAX_COMPRESSION_RATIO", 100), 100);
    }

    #[test]
    fn compression_ratio_valid_value_overrides_default() {
        assert_eq!(parse_u64_or(Some("7"), "MAX_COMPRESSION_RATIO", 100), 7);
    }

    #[test]
    #[should_panic(expected = "MAX_COMPRESSION_RATIO must be a valid non-negative integer")]
    fn compression_ratio_invalid_value_panics() {
        parse_u64_or(Some("nope"), "MAX_COMPRESSION_RATIO", 100);
    }

    #[test]
    fn duration_secs_missing_value_uses_default() {
        assert_eq!(
            parse_duration_secs_or(None, "MIRROR_TIMEOUT_SECS", 42),
            Duration::from_secs(42)
        );
    }

    #[test]
    fn duration_secs_valid_value_overrides_default() {
        assert_eq!(
            parse_duration_secs_or(Some("7"), "MIRROR_TIMEOUT_SECS", 42),
            Duration::from_secs(7)
        );
    }

    #[tokio::test]
    async fn https_request_uses_proxy_when_configured() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let received: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let received_clone = received.clone();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0u8; 1024];
                if let Ok(n) = socket.read(&mut buf).await {
                    *received_clone.lock().unwrap() = String::from_utf8_lossy(&buf[..n]).to_string();
                }
                let _ = socket
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = socket.shutdown().await;
            }
        });

        let proxy_url = format!("http://{addr}");
        let client = build_client(Some(&proxy_url), Duration::from_secs(5));

        let _ = client.get("https://example.invalid/path").send().await;

        let request = received.lock().unwrap().clone();
        assert!(
            request.starts_with("CONNECT "),
            "expected an HTTPS request to tunnel through the proxy via CONNECT, got: {request:?}"
        );
    }

    #[tokio::test]
    async fn mirror_client_request_timeout_fires_on_stalled_response() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                std::future::pending::<()>().await;
            }
        });

        let client = build_client(None, Duration::from_millis(200));

        let start = tokio::time::Instant::now();
        let result = client.get(format!("http://{addr}")).send().await;
        let elapsed = start.elapsed();

        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_secs(2),
            "expected the configured request timeout to fire quickly, took {elapsed:?}"
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cargo test --lib config`
Expected: FAIL — `parse_fl_sources`/`build_client` called with wrong number of arguments, `parse_duration_secs_or` and `CONNECT_TIMEOUT` not found.

- [ ] **Step 3: Implement the timeout config and the proxy fix**

Replace the top of `src/config.rs` (from the imports down through `parse_fl_sources`) with:

```rust
use once_cell::sync::Lazy;
use serde::Deserialize;
use std::time::Duration;

fn get_env(env: &'static str) -> String {
    std::env::var(env).unwrap_or_else(|_| panic!("Cannot get the {} env variable", env))
}

fn parse_usize_or(raw: Option<&str>, env: &'static str, default: usize) -> usize {
    match raw {
        Some(v) => v
            .parse()
            .unwrap_or_else(|err| panic!("{env} must be a valid non-negative integer: {err}")),
        None => default,
    }
}

fn parse_u64_or(raw: Option<&str>, env: &'static str, default: u64) -> u64 {
    match raw {
        Some(v) => v
            .parse()
            .unwrap_or_else(|err| panic!("{env} must be a valid non-negative integer: {err}")),
        None => default,
    }
}

fn parse_duration_secs_or(raw: Option<&str>, env: &'static str, default_secs: u64) -> Duration {
    Duration::from_secs(parse_u64_or(raw, env, default_secs))
}

/// Connect timeout applied to every outbound HTTP client (mirrors, book_library,
/// converter). Fixed rather than configurable: a slow TCP/TLS handshake isn't a
/// scenario operators need to tune per deployment.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

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

fn build_client(proxy: Option<&str>, mirror_request_timeout: Duration) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(mirror_request_timeout);

    if let Some(v) = proxy {
        let proxy = reqwest::Proxy::all(v)
            .unwrap_or_else(|err| panic!("FL_SOURCES: invalid proxy URL {v:?}: {err}"));
        builder = builder.proxy(proxy);
    }

    builder
        .build()
        .unwrap_or_else(|err| panic!("FL_SOURCES: failed to build HTTP client: {err}"))
}

fn parse_fl_sources(raw_json: &str, mirror_request_timeout: Duration) -> Vec<SourceConfig> {
    let raw: Vec<RawSourceConfig> =
        serde_json::from_str(raw_json).expect("FL_SOURCES must be a JSON array of {url, proxy?}");

    raw.into_iter()
        .map(|r| {
            let client = build_client(r.proxy.as_deref(), mirror_request_timeout);
            SourceConfig {
                url: r.url,
                proxy: r.proxy,
                client,
            }
        })
        .collect()
}
```

Then update the `Config` struct and `Config::load` (the `DownloadLimits` struct and its doc comment above it are unchanged — leave them in place between `parse_fl_sources` and `Config`):

```rust
pub struct Config {
    pub api_key: String,

    pub fl_sources: Vec<SourceConfig>,

    pub book_library_api_key: String,
    pub book_library_url: String,

    pub converter_url: String,
    pub converter_api_key: String,

    pub sentry_dsn: Option<String>,

    pub download_limits: DownloadLimits,

    /// Wall-clock deadline for the whole `/download` failover loop across all
    /// mirrors. Configurable via `DOWNLOAD_TIMEOUT_SECS` (default 300s).
    pub overall_download_timeout: Duration,
}

impl Config {
    pub fn load() -> Config {
        let mirror_request_timeout = parse_duration_secs_or(
            std::env::var("MIRROR_TIMEOUT_SECS").ok().as_deref(),
            "MIRROR_TIMEOUT_SECS",
            300,
        );

        Config {
            api_key: get_env("API_KEY"),

            fl_sources: parse_fl_sources(&get_env("FL_SOURCES"), mirror_request_timeout),

            book_library_api_key: get_env("BOOK_LIBRARY_API_KEY"),
            book_library_url: get_env("BOOK_LIBRARY_URL"),

            converter_url: get_env("CONVERTER_URL"),
            converter_api_key: get_env("CONVERTER_API_KEY"),

            sentry_dsn: std::env::var("SENTRY_DSN").ok(),

            download_limits: DownloadLimits {
                max_download_bytes: parse_usize_or(
                    std::env::var("MAX_DOWNLOAD_BYTES").ok().as_deref(),
                    "MAX_DOWNLOAD_BYTES",
                    200 * 1024 * 1024,
                ),
                max_decompressed_bytes: parse_u64_or(
                    std::env::var("MAX_DECOMPRESSED_BYTES").ok().as_deref(),
                    "MAX_DECOMPRESSED_BYTES",
                    200 * 1024 * 1024,
                ),
                max_compression_ratio: parse_u64_or(
                    std::env::var("MAX_COMPRESSION_RATIO").ok().as_deref(),
                    "MAX_COMPRESSION_RATIO",
                    100,
                ),
            },

            overall_download_timeout: parse_duration_secs_or(
                std::env::var("DOWNLOAD_TIMEOUT_SECS").ok().as_deref(),
                "DOWNLOAD_TIMEOUT_SECS",
                300,
            ),
        }
    }
}

pub static CONFIG: Lazy<Config> = Lazy::new(Config::load);
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib config`
Expected: PASS (all tests including the two new ones and the updated `parse_fl_sources` calls).

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "fix: add HTTP timeouts to mirror clients and route HTTPS through configured proxy"
```

---

### Task 2: Apply timeouts to book_library and converter clients

**Files:**
- Modify: `src/services/book_library/mod.rs`
- Modify: `src/services/covert.rs`

**Interfaces:**
- Consumes: `config::CONNECT_TIMEOUT` (Task 1).
- Produces: `async fn _make_request<T>(client: &reqwest::Client, url: &str, api_key: &str, params: Vec<(&str, String)>) -> Result<T, ...>` (signature change — was `_make_request<T>(url: &str, params: Vec<(&str, String)>)`, decoupled from `config::CONFIG` so it's unit-testable without forcing the global config's `Lazy` init).

- [ ] **Step 1: Write the failing test**

Add a `mod tests` block at the bottom of `src/services/book_library/mod.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn stalled_server_times_out_quickly() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = [0u8; 1024];
                let _ = socket.read(&mut buf).await;
                std::future::pending::<()>().await;
            }
        });

        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_millis(200))
            .timeout(Duration::from_millis(200))
            .build()
            .unwrap();

        let start = tokio::time::Instant::now();
        let result: Result<serde_json::Value, _> =
            _make_request(&client, &format!("http://{addr}/x"), "key", vec![]).await;
        let elapsed = start.elapsed();

        assert!(result.is_err());
        assert!(
            elapsed < Duration::from_secs(2),
            "expected timeout to fire quickly, took {elapsed:?}"
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails to compile**

Run: `cargo test --lib book_library`
Expected: FAIL — `_make_request` called with the wrong argument count (current signature is `_make_request(url, params)`).

- [ ] **Step 3: Decouple `_make_request` from global `CONFIG` and add timeouts**

Replace the whole of `src/services/book_library/mod.rs` above the (new) `mod tests` block with:

```rust
pub mod types;

use once_cell::sync::Lazy;
use serde::de::DeserializeOwned;
use std::time::Duration;

use crate::config;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .connect_timeout(config::CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("failed to build book_library HTTP client")
});

async fn _make_request<T>(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    params: Vec<(&str, String)>,
) -> Result<T, Box<dyn std::error::Error + Send + Sync>>
where
    T: DeserializeOwned,
{
    let response = client
        .get(url)
        .query(&params)
        .header("Authorization", api_key)
        .send()
        .await;

    let response = match response {
        Ok(v) => v,
        Err(err) => return Err(Box::new(err)),
    };

    let response = match response.error_for_status() {
        Ok(v) => v,
        Err(err) => return Err(Box::new(err)),
    };

    match response.json::<T>().await {
        Ok(v) => Ok(v),
        Err(err) => Err(Box::new(err)),
    }
}

pub async fn get_sources() -> Result<types::Source, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{}/api/v1/sources", &config::CONFIG.book_library_url);
    _make_request(&CLIENT, &url, &config::CONFIG.book_library_api_key, vec![]).await
}

pub async fn get_book(
    book_id: u32,
) -> Result<types::BookWithRemote, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!("{}/api/v1/books/{book_id}", &config::CONFIG.book_library_url);
    _make_request(&CLIENT, &url, &config::CONFIG.book_library_api_key, vec![]).await
}

pub async fn get_remote_book(
    source_id: u32,
    remote_id: u32,
) -> Result<types::BookWithRemote, Box<dyn std::error::Error + Send + Sync>> {
    let url = format!(
        "{}/api/v1/books/remote/{source_id}/{remote_id}",
        &config::CONFIG.book_library_url
    );
    match _make_request::<types::Book>(&CLIENT, &url, &config::CONFIG.book_library_api_key, vec![])
        .await
    {
        Ok(v) => Ok(types::BookWithRemote::from_book(v, remote_id)),
        Err(err) => Err(err),
    }
}
```

Then, in `src/services/covert.rs`, add a connect+request timeout to the converter client. Replace the file with:

```rust
use reqwest::{Body, Response};
use std::time::Duration;
use tempfile::SpooledTempFile;
use tokio_util::io::ReaderStream;

use crate::config;

use super::downloader::types::spooled_temp_file_into_async_read;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

pub async fn convert_file(file: SpooledTempFile, file_type: String) -> Option<Response> {
    let body = Body::wrap_stream(ReaderStream::new(spooled_temp_file_into_async_read(file)));

    let client = reqwest::Client::builder()
        .connect_timeout(config::CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .ok()?;

    let response = client
        .post(format!("{}{}", config::CONFIG.converter_url, file_type))
        .body(body)
        .header("Authorization", &config::CONFIG.converter_api_key)
        .send()
        .await;

    let response = match response {
        Ok(v) => v,
        Err(_) => return None,
    };

    let response = match response.error_for_status() {
        Ok(v) => v,
        Err(_) => return None,
    };

    Some(response)
}
```

`covert.rs` isn't getting a dedicated timeout test: it uses the exact same `.connect_timeout()`/`.timeout()` builder mechanism already proven by the `mirror_client_request_timeout_fires_on_stalled_response` (Task 1) and `stalled_server_times_out_quickly` (this task) tests, and exercising it here would require standing up a POST+streaming-body mock for no new coverage.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib book_library && cargo build`
Expected: PASS, and the workspace builds (confirms `covert.rs` still compiles against `convert_file`'s callers in `downloader/mod.rs`).

- [ ] **Step 5: Commit**

```bash
git add src/services/book_library/mod.rs src/services/covert.rs
git commit -m "fix: add connect/request timeouts to book_library and converter HTTP clients"
```

---

### Task 3: Sequential mirror failover, direct-before-convert, overall deadline

**Files:**
- Modify: `src/services/downloader/mod.rs`

**Interfaces:**
- Consumes: `config::SourceConfig`, `config::DownloadLimits` (existing), `config::CONFIG.overall_download_timeout: Duration` (Task 1).
- Produces: `pub async fn start_download_futures(book: &BookWithRemote, file_type: &str, normalized: bool, sources: &[config::SourceConfig], limits: config::DownloadLimits, overall_deadline: std::time::Duration) -> Option<DownloadResult>` (signature change — was `start_download_futures(book, file_type, normalized)`, reading `sources`/`limits` from `config::CONFIG` internally). `download_chain` and `download` are unchanged.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `src/services/downloader/mod.rs`, after the existing `make_book`/`generous_limits` helpers and before the first `#[tokio::test]`:

```rust
    fn make_source_config_with_client(url: String, client: reqwest::Client) -> config::SourceConfig {
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
```

Then add these three tests to the same `mod tests` block:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail to compile**

Run: `cargo test --lib downloader`
Expected: FAIL — `start_download_futures` called with 6 arguments but the current signature only takes 3.

- [ ] **Step 3: Rewrite `start_download_futures` and `book_download`**

In `src/services/downloader/mod.rs`, remove the `use tokio::task::JoinSet;` import line, then replace `start_download_futures` and `book_download` with:

```rust
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
        Err(err) => return Err(err),
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
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --lib downloader`
Expected: PASS — all existing `download`/`download_chain` tests plus the three new `start_download_futures` tests.

- [ ] **Step 5: Commit**

```bash
git add src/services/downloader/mod.rs
git commit -m "fix: fail over to mirrors sequentially, try direct download before converting, bound overall deadline"
```

---

### Task 4: Graceful shutdown on SIGTERM/SIGINT

**Files:**
- Modify: `src/main.rs`

**Interfaces:**
- Produces: `async fn shutdown_signal()` — private to `main.rs`, no other module depends on it.

This task has no automated test: it wires OS signal delivery into `axum::serve`, which isn't something `cargo test` can exercise without spawning the compiled binary as a subprocess and sending it a real signal — disproportionate for this fix. Verify manually per Step 3 below.

- [ ] **Step 1: Add the shutdown-signal future and wire it into `axum::serve`**

Replace `src/main.rs` with:

```rust
pub mod config;
pub mod services;
pub mod views;

use sentry::{integrations::debug_images::DebugImagesIntegration, types::Dsn, ClientOptions};
use sentry_tracing::EventFilter;
use std::{net::SocketAddr, str::FromStr};
use tracing::info;
use tracing_subscriber::{filter, layer::SubscriberExt, util::SubscriberInitExt};

use crate::views::get_router;

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }

    info!("Shutdown signal received, waiting for in-flight requests to finish...");
}

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

    let sentry_layer = sentry_tracing::layer().event_filter(|md| match md.level() {
        &tracing::Level::ERROR => EventFilter::Event,
        _ => EventFilter::Ignore,
    });

    tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(filter::LevelFilter::INFO)
        .with(sentry_layer)
        .init();

    let addr = SocketAddr::from(([0, 0, 0, 0], 8080));

    let app = get_router().await;

    info!("Start webserver...");
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
    info!("Webserver shutdown...")
}
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build`
Expected: builds with no errors or new warnings.

- [ ] **Step 3: Manually verify SIGTERM behavior**

Run the binary with the required env vars set (or `cargo run` in an environment that already has them), send it `SIGTERM` (e.g. `kill -TERM <pid>`), and confirm in the logs that "Shutdown signal received, waiting for in-flight requests to finish..." followed by "Webserver shutdown..." are printed, and that a concurrently in-flight `/download` request completes before the process exits.

- [ ] **Step 4: Commit**

```bash
git add src/main.rs
git commit -m "fix: shut down gracefully on SIGTERM/SIGINT instead of killing in-flight requests"
```
