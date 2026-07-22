use thiserror::Error;

#[derive(Debug, Error)]
pub enum BookLibraryError {
    #[error("book not found")]
    NotFound,
    #[error("book_library request failed: {0}")]
    RequestFailed(#[source] reqwest::Error),
    #[error("book_library returned an error status: {0}")]
    UpstreamError(#[source] reqwest::Error),
}
