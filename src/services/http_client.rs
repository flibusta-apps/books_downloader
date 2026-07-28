use once_cell::sync::Lazy;
use std::time::Duration;

use crate::config;

/// Generous enough to cover the slowest of the plain (non-proxied) outbound
/// calls this client is shared across (book_library API, converter), while
/// still bounded so a fully stalled peer doesn't hang forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Shared `reqwest::Client` for plain (non-proxied) outbound HTTP calls —
/// book_library API requests and converter requests. Per-source mirror
/// clients (which may be proxied, see `config::SourceConfig`) are built and
/// owned separately since each can have its own proxy configuration.
pub static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .connect_timeout(config::CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()
        .expect("failed to build shared HTTP client")
});
