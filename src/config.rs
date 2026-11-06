use once_cell::sync::Lazy;
use reqwest::Url;
use serde::Deserialize;
use std::collections::HashSet;
use std::net::IpAddr;
use std::time::Duration;

fn get_env(env: &'static str) -> String {
    std::env::var(env).unwrap_or_else(|_| panic!("Cannot get the {} env variable", env))
}

fn require_non_empty(value: String, env: &'static str) -> String {
    if value.trim().is_empty() {
        panic!("{env} must not be empty");
    }
    value
}

fn get_secret_env(env: &'static str) -> String {
    require_non_empty(get_env(env), env)
}

fn validate_zip_compression_level(level: i64) -> i64 {
    if !(0..=9).contains(&level) {
        panic!("ZIP_COMPRESSION_LEVEL must be in 0..=9, got {level}");
    }
    level
}

fn validate_compression_ratio(ratio: u64) -> u64 {
    if ratio == 0 {
        panic!("MAX_COMPRESSION_RATIO must be greater than 0");
    }
    ratio
}

fn validate_positive_usize(value: usize, env: &'static str) -> usize {
    if value == 0 {
        panic!("{env} must be greater than 0");
    }
    value
}

/// Parses a base URL from an env value and makes sure its path ends with `/`,
/// so that `Url::join` appends relative paths instead of replacing the last
/// path segment.
fn parse_base_url(raw: &str, env: &'static str) -> Url {
    let mut url =
        Url::parse(raw.trim()).unwrap_or_else(|err| panic!("{env} must be a valid URL: {err}"));
    if !url.path().ends_with('/') {
        let path = format!("{}/", url.path());
        url.set_path(&path);
    }
    url
}

/// Joins a relative path (built from numeric ids / allowlisted file types only)
/// onto a base URL produced by `parse_base_url`.
pub fn join_url(base: &Url, relative: &str) -> Url {
    base.join(relative.trim_start_matches('/'))
        .expect("relative path built from ids and allowlisted types must join onto a base URL")
}

/// Host (and port) of a mirror URL, suitable for logs and metric labels.
/// Never includes userinfo, path or query.
pub fn source_label(raw: &str) -> String {
    match Url::parse(raw) {
        Ok(url) => match (url.host_str(), url.port()) {
            (Some(host), Some(port)) => format!("{host}:{port}"),
            (Some(host), None) => host.to_string(),
            _ => "invalid".to_string(),
        },
        Err(_) => "invalid".to_string(),
    }
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

fn parse_i64_or(raw: Option<&str>, env: &'static str, default: i64) -> i64 {
    match raw {
        Some(v) => v
            .parse()
            .unwrap_or_else(|err| panic!("{env} must be a valid integer: {err}")),
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
    #[serde(default)]
    api_key: Option<String>,
}

#[derive(Clone)]
pub struct SourceConfig {
    pub url: std::sync::Arc<str>,
    pub proxy: Option<String>,
    pub client: reqwest::Client,
}

/// Exact hosts only (no URLs, ports, wildcards or suffix matching). IPv6
/// literals may be written with or without brackets.
fn parse_redirect_allowed_hosts(raw: &str) -> HashSet<String> {
    raw.split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .map(|host| {
            let host = host
                .parse::<IpAddr>()
                .map(|ip| match ip {
                    IpAddr::V4(ip) => ip.to_string(),
                    IpAddr::V6(ip) => format!("[{ip}]"),
                })
                .unwrap_or_else(|_| host.to_string());
            let url = Url::parse(&format!("http://{host}/"))
                .expect("MIRROR_REDIRECT_ALLOWED_HOSTS must contain valid hosts");
            assert!(
                url.host_str().is_some()
                    && url.port().is_none()
                    && url.username().is_empty()
                    && url.password().is_none()
                    && url.path() == "/"
                    && url.query().is_none()
                    && url.fragment().is_none()
                    && (!host.contains(':') || (host.starts_with('[') && host.ends_with(']')))
                    && !host.contains(['/', '@', '?', '#']),
                "MIRROR_REDIRECT_ALLOWED_HOSTS must contain hosts only, without ports"
            );
            url.host_str().unwrap().to_string()
        })
        .collect()
}

fn restricted_redirect_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            ip.is_loopback()
                || ip.is_private()
                || ip.is_link_local()
                || ip.is_unspecified()
                || ip.is_multicast()
                || ip.is_broadcast()
        }
        IpAddr::V6(ip) => {
            if let Some(ip) = ip.to_ipv4_mapped() {
                return restricted_redirect_ip(IpAddr::V4(ip));
            }
            ip.is_loopback()
                || ip.is_unspecified()
                || ip.is_multicast()
                || (ip.segments()[0] & 0xfe00 == 0xfc00) // unique local
                || (ip.segments()[0] & 0xffc0 == 0xfe80) // link local
        }
    }
}

/// Checks literal addresses only; DNS resolution/rebinding is not covered.
fn mirror_redirect_allowed(original: &Url, target: &Url, allowed: &HashSet<String>) -> bool {
    if !matches!(target.scheme(), "http" | "https") {
        return false;
    }
    let Some(host) = target.host_str() else {
        return false;
    };
    if allowed.contains(host) {
        return true;
    }
    if host
        .trim_matches(['[', ']'])
        .parse::<IpAddr>()
        .is_ok_and(restricted_redirect_ip)
    {
        return false;
    }
    let Some(origin) = original.host_str() else {
        return false;
    };
    // Same host, or a subdomain of it (e.g. flibusta.is -> static.flibusta.is).
    // Never a parent domain or an unrelated host sharing a suffix.
    host == origin
        || (origin.parse::<IpAddr>().is_err()
            && host
                .strip_suffix(origin)
                .is_some_and(|prefix| prefix.ends_with('.') && prefix.len() > 1))
}

fn mirror_redirect_policy(allowed: HashSet<String>) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        // previous includes the initial request: permit at most five hops.
        if attempt.previous().len() > 5 {
            return attempt.error("mirror redirect limit exceeded");
        }
        if attempt
            .previous()
            .first()
            .is_some_and(|original| mirror_redirect_allowed(original, attempt.url(), &allowed))
        {
            attempt.follow()
        } else {
            // Error rather than stop: never serve the rejected 3xx body as a book.
            attempt.error("mirror redirect target is not allowed")
        }
    })
}

fn build_client(
    proxy: Option<&str>,
    api_key: Option<&str>,
    mirror_request_timeout: Duration,
) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .redirect(mirror_redirect_policy(parse_redirect_allowed_hosts(
            &std::env::var("MIRROR_REDIRECT_ALLOWED_HOSTS").unwrap_or_default(),
        )))
        .timeout(mirror_request_timeout);

    if let Some(v) = proxy {
        let proxy = reqwest::Proxy::all(v)
            .unwrap_or_else(|err| panic!("FL_SOURCES: invalid proxy URL {v:?}: {err}"));
        builder = builder.proxy(proxy);
    }

    if let Some(key) = api_key {
        // Never include the key in the panic message.
        let mut value = reqwest::header::HeaderValue::from_str(key)
            .unwrap_or_else(|_| panic!("FL_SOURCES: invalid api_key (not a valid header value)"));
        value.set_sensitive(true);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::AUTHORIZATION, value);
        builder = builder.default_headers(headers);
    }

    builder
        .build()
        .unwrap_or_else(|err| panic!("FL_SOURCES: failed to build HTTP client: {err}"))
}

pub(crate) fn parse_fl_sources(
    raw_json: &str,
    mirror_request_timeout: Duration,
) -> Vec<SourceConfig> {
    let raw: Vec<RawSourceConfig> = serde_json::from_str(raw_json)
        .expect("FL_SOURCES must be a JSON array of {url, proxy?, api_key?}");

    raw.into_iter()
        .map(|r| {
            let api_key = r.api_key.as_deref().filter(|k| !k.is_empty());
            let client = build_client(r.proxy.as_deref(), api_key, mirror_request_timeout);
            SourceConfig {
                url: r.url.into(),
                proxy: r.proxy,
                client,
            }
        })
        .collect()
}

/// Max bytes buffered from a single mirror response into a temp file.
/// Configurable via `MAX_DOWNLOAD_BYTES` (default 512 MiB).
///
/// Max declared uncompressed bytes / compression ratio allowed for a single
/// ZIP entry before it is treated as a ZIP bomb and rejected.
/// Configurable via `MAX_DECOMPRESSED_BYTES` (default 512 MiB) and
/// `MAX_COMPRESSION_RATIO` (default 100).
#[derive(Clone, Copy)]
pub struct DownloadLimits {
    pub max_download_bytes: usize,
    pub max_decompressed_bytes: u64,
    pub max_compression_ratio: u64,
    /// DEFLATE level (0-9) for result ZIP archives (`ZIP_COMPRESSION_LEVEL`).
    pub zip_compression_level: i64,
}

pub struct Config {
    pub api_key: String,

    pub fl_sources: Vec<SourceConfig>,

    pub book_library_api_key: String,
    pub book_library_url: Url,

    pub converter_url: Url,
    pub converter_api_key: String,

    pub sentry_dsn: Option<String>,

    pub download_limits: DownloadLimits,

    /// Wall-clock deadline for the whole `/download` failover loop across all
    /// mirrors, including the book_library metadata fetch. Configurable via
    /// `DOWNLOAD_TIMEOUT_SECS` (default 115s; callers wait 120s).
    pub overall_download_timeout: Duration,

    /// Wall-clock limit of fetching/preparing source bytes from one mirror.
    /// Conversion has its own budget and is not a mirror failure.
    /// Configurable via `MIRROR_ATTEMPT_TIMEOUT_SECS` (default 45s).
    pub mirror_attempt_timeout: Duration,

    /// Timeout of a single converter request. Configurable via
    /// `CONVERTER_TIMEOUT_SECS` (default 100s).
    pub converter_timeout: Duration,

    /// Max number of downloads processed at once. Configurable via
    /// `MAX_CONCURRENT_DOWNLOADS` (default 8).
    pub max_concurrent_downloads: usize,

    /// Maximum idle time while sending a response body, including backpressure.
    /// `BODY_IDLE_TIMEOUT_SECS` defaults to 60 seconds.
    pub body_idle_timeout: Duration,

    /// In-memory spool threshold used when buffering a converter response to a
    /// temp file ahead of a subsequent ZIP step. Smaller than the general 5 MiB
    /// default used elsewhere, since this buffer is a short-lived intermediate
    /// hop rather than the final downloaded artifact. Configurable via
    /// `CONVERTED_RESPONSE_SPOOL_BYTES` (default 512 KiB).
    pub converted_response_spool_bytes: usize,
}

impl Config {
    pub fn load() -> Config {
        let mirror_request_timeout = parse_duration_secs_or(
            std::env::var("MIRROR_TIMEOUT_SECS").ok().as_deref(),
            "MIRROR_TIMEOUT_SECS",
            45,
        );

        Config {
            api_key: get_secret_env("API_KEY"),

            fl_sources: parse_fl_sources(&get_env("FL_SOURCES"), mirror_request_timeout),

            book_library_api_key: get_secret_env("BOOK_LIBRARY_API_KEY"),
            book_library_url: parse_base_url(&get_env("BOOK_LIBRARY_URL"), "BOOK_LIBRARY_URL"),

            converter_url: parse_base_url(&get_env("CONVERTER_URL"), "CONVERTER_URL"),
            converter_api_key: get_secret_env("CONVERTER_API_KEY"),

            sentry_dsn: std::env::var("SENTRY_DSN").ok(),

            download_limits: DownloadLimits {
                max_download_bytes: parse_usize_or(
                    std::env::var("MAX_DOWNLOAD_BYTES").ok().as_deref(),
                    "MAX_DOWNLOAD_BYTES",
                    512 * 1024 * 1024,
                ),
                max_decompressed_bytes: parse_u64_or(
                    std::env::var("MAX_DECOMPRESSED_BYTES").ok().as_deref(),
                    "MAX_DECOMPRESSED_BYTES",
                    512 * 1024 * 1024,
                ),
                max_compression_ratio: validate_compression_ratio(parse_u64_or(
                    std::env::var("MAX_COMPRESSION_RATIO").ok().as_deref(),
                    "MAX_COMPRESSION_RATIO",
                    100,
                )),
                zip_compression_level: validate_zip_compression_level(parse_i64_or(
                    std::env::var("ZIP_COMPRESSION_LEVEL").ok().as_deref(),
                    "ZIP_COMPRESSION_LEVEL",
                    6,
                )),
            },

            overall_download_timeout: parse_duration_secs_or(
                std::env::var("DOWNLOAD_TIMEOUT_SECS").ok().as_deref(),
                "DOWNLOAD_TIMEOUT_SECS",
                115,
            ),

            mirror_attempt_timeout: parse_duration_secs_or(
                std::env::var("MIRROR_ATTEMPT_TIMEOUT_SECS").ok().as_deref(),
                "MIRROR_ATTEMPT_TIMEOUT_SECS",
                45,
            ),

            converter_timeout: parse_duration_secs_or(
                std::env::var("CONVERTER_TIMEOUT_SECS").ok().as_deref(),
                "CONVERTER_TIMEOUT_SECS",
                100,
            ),

            max_concurrent_downloads: validate_positive_usize(
                parse_usize_or(
                    std::env::var("MAX_CONCURRENT_DOWNLOADS").ok().as_deref(),
                    "MAX_CONCURRENT_DOWNLOADS",
                    8,
                ),
                "MAX_CONCURRENT_DOWNLOADS",
            ),

            body_idle_timeout: parse_duration_secs_or(
                std::env::var("BODY_IDLE_TIMEOUT_SECS").ok().as_deref(),
                "BODY_IDLE_TIMEOUT_SECS",
                60,
            ),

            converted_response_spool_bytes: parse_usize_or(
                std::env::var("CONVERTED_RESPONSE_SPOOL_BYTES")
                    .ok()
                    .as_deref(),
                "CONVERTED_RESPONSE_SPOOL_BYTES",
                512 * 1024,
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
    fn redirect_allows_same_host_but_not_other_hosts_or_suffixes() {
        let original = Url::parse("https://mirror.example/book").unwrap();
        let allowed = HashSet::new();
        for target in [
            "https://mirror.example/new",
            "http://mirror.example:8080/new",
            "http://static.mirror.example:443/b.usr/book.pdf",
        ] {
            assert!(mirror_redirect_allowed(
                &original,
                &Url::parse(target).unwrap(),
                &allowed
            ));
        }
        for target in [
            "https://other.example/book",
            "https://mirror.example.evil/book",
            "https://evilmirror.example/book",
            "https://example/book",
            "http://books_archive/book",
            "file:///etc/passwd",
        ] {
            assert!(!mirror_redirect_allowed(
                &original,
                &Url::parse(target).unwrap(),
                &allowed
            ));
        }
    }

    #[test]
    fn redirect_allowlist_is_normalized_and_exact() {
        let original = Url::parse("https://mirror.example/book").unwrap();
        let allowed = parse_redirect_allowed_hosts(" CDN.Example, 169.254.169.254, ::1, ,");
        for target in [
            "https://cdn.example/book",
            "http://169.254.169.254/latest",
            "http://[::1]/book",
        ] {
            assert!(mirror_redirect_allowed(
                &original,
                &Url::parse(target).unwrap(),
                &allowed
            ));
        }
        assert!(!mirror_redirect_allowed(
            &original,
            &Url::parse("https://cdn.example.evil/book").unwrap(),
            &allowed,
        ));
    }

    #[test]
    fn restricted_ip_redirects_require_explicit_allowlisting_even_for_same_host() {
        for host in [
            "127.0.0.1",
            "10.0.0.1",
            "172.16.0.1",
            "192.168.1.1",
            "169.254.169.254",
            "0.0.0.0",
            "224.0.0.1",
            "255.255.255.255",
            "[::1]",
            "[::]",
            "[fc00::1]",
            "[fd12::1]",
            "[fe80::1]",
            "[febf::1]",
            "[ff02::1]",
            "[::ffff:127.0.0.1]",
            "[::ffff:169.254.169.254]",
            "2130706433",
            "0x7f000001",
        ] {
            let url = Url::parse(&format!("http://{host}/book")).unwrap();
            assert!(
                !mirror_redirect_allowed(&url, &url, &HashSet::new()),
                "{host}"
            );
            let allowed = parse_redirect_allowed_hosts(host);
            assert!(mirror_redirect_allowed(&url, &url, &allowed), "{host}");
        }
    }

    #[test]
    fn public_ip_and_internal_archive_same_host_redirects_still_work() {
        for host in ["8.8.8.8", "[2606:4700:4700::1111]", "books_archive"] {
            let original = Url::parse(&format!("http://{host}/book")).unwrap();
            let target = original.join("/download").unwrap();
            assert!(mirror_redirect_allowed(&original, &target, &HashSet::new()));
        }
    }

    #[test]
    fn parses_valid_sources_without_proxy() {
        let sources =
            parse_fl_sources(r#"[{"url": "http://example.com"}]"#, Duration::from_secs(5));
        assert_eq!(sources.len(), 1);
        assert_eq!(&*sources[0].url, "http://example.com");
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
    fn parses_sources_with_api_key_empty_or_absent() {
        let sources = parse_fl_sources(
            r#"[{"url": "http://a", "api_key": "secret"}, {"url": "http://b", "api_key": ""}, {"url": "http://c"}]"#,
            Duration::from_secs(5),
        );
        assert_eq!(sources.len(), 3);
    }

    #[test]
    #[should_panic(expected = "FL_SOURCES: invalid api_key")]
    fn invalid_api_key_panics_without_leaking_key() {
        parse_fl_sources(
            r#"[{"url": "http://example.com", "api_key": "bad\nkey"}]"#,
            Duration::from_secs(5),
        );
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
    #[should_panic(expected = "ZIP_COMPRESSION_LEVEL must be in 0..=9")]
    fn zip_level_out_of_range_panics() {
        validate_zip_compression_level(10);
    }

    #[test]
    fn zip_level_in_range_is_accepted() {
        assert_eq!(validate_zip_compression_level(0), 0);
        assert_eq!(validate_zip_compression_level(9), 9);
    }

    #[test]
    #[should_panic(expected = "MAX_COMPRESSION_RATIO must be greater than 0")]
    fn zero_compression_ratio_panics() {
        validate_compression_ratio(0);
    }

    #[test]
    #[should_panic(expected = "API_KEY must not be empty")]
    fn blank_secret_panics() {
        require_non_empty("  ".to_string(), "API_KEY");
    }

    #[test]
    fn base_url_join_handles_slashes() {
        for raw in ["http://conv:8080/api", "http://conv:8080/api/"] {
            let base = parse_base_url(raw, "X");
            assert_eq!(
                join_url(&base, "epub").as_str(),
                "http://conv:8080/api/epub"
            );
            assert_eq!(
                join_url(&base, "/epub").as_str(),
                "http://conv:8080/api/epub"
            );
        }
    }

    #[test]
    fn source_label_hides_userinfo_and_path() {
        assert_eq!(
            source_label("http://user:pass@mirror.example:8080/secret?x=1"),
            "mirror.example:8080"
        );
        assert_eq!(source_label("not a url"), "invalid");
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
        let client = build_client(Some(&proxy_url), None, Duration::from_secs(5));

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

        let client = build_client(None, None, Duration::from_millis(200));

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
