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

/// Max bytes buffered from a single mirror response into a temp file.
/// Configurable via `MAX_DOWNLOAD_BYTES` (default 200 MiB).
///
/// Max declared uncompressed bytes / compression ratio allowed for a single
/// ZIP entry before it is treated as a ZIP bomb and rejected.
/// Configurable via `MAX_DECOMPRESSED_BYTES` (default 200 MiB) and
/// `MAX_COMPRESSION_RATIO` (default 100).
#[derive(Clone, Copy)]
pub struct DownloadLimits {
    pub max_download_bytes: usize,
    pub max_decompressed_bytes: u64,
    pub max_compression_ratio: u64,
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn parses_valid_sources_without_proxy() {
        let sources =
            parse_fl_sources(r#"[{"url": "http://example.com"}]"#, Duration::from_secs(5));
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
                    *received_clone.lock().unwrap() =
                        String::from_utf8_lossy(&buf[..n]).to_string();
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
