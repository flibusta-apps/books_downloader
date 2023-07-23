use once_cell::sync::Lazy;
use serde::Deserialize;

fn get_env(env: &'static str) -> String {
    std::env::var(env).unwrap_or_else(|_| panic!("Cannot get the {} env variable", env))
}

#[derive(Deserialize, Clone)]
pub struct SourceConfig {
    pub url: String,
    pub proxy: Option<String>
}

pub struct Config {
    pub api_key: String,

    pub fl_sources: Vec<SourceConfig>,

    pub book_library_api_key: String,
    pub book_library_url: String,

    pub converter_url: String,

    pub sentry_dsn: String
}

impl Config {
    pub fn load() -> Config {
        Config {
            api_key: get_env("API_KEY"),

            fl_sources: serde_json::from_str(&get_env("FL_SOURCES")).unwrap(),

            book_library_api_key: get_env("BOOK_LIBRARY_API_KEY"),
            book_library_url: get_env("BOOK_LIBRARY_URL"),

            converter_url: get_env("CONVERTER_URL"),

            sentry_dsn: get_env("SENTRY_DSN")
        }
    }
}

pub static CONFIG: Lazy<Config> = Lazy::new(|| {
    Config::load()
});
