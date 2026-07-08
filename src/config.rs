use once_cell::sync::Lazy;
use serde::Deserialize;

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
    let raw: Vec<RawSourceConfig> =
        serde_json::from_str(raw_json).expect("FL_SOURCES must be a JSON array of {url, proxy?}");

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
        }
    }
}

pub static CONFIG: Lazy<Config> = Lazy::new(Config::load);

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
        assert_eq!(sources[0].proxy.as_deref(), Some("http://proxy.local:8080"));
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
}
