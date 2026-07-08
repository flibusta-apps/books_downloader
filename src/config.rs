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
}
