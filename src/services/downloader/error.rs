use thiserror::Error;

use crate::services::book_library::error::BookLibraryError;

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("upstream source unavailable")]
    SourceUnavailable,
    #[error("downloaded archive was invalid or contained no matching entry")]
    BadArchive,
    #[error("converter failed with status {0}")]
    ConverterFailed(u16),
    #[error("overall download deadline exceeded")]
    Timeout,
    #[error("internal error: {0}")]
    Internal(String),
    #[error(transparent)]
    Library(#[from] BookLibraryError),
}
