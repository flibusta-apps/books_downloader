use thiserror::Error;

use crate::services::book_library::error::BookLibraryError;

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("upstream source unavailable")]
    SourceUnavailable,
    #[error("downloaded archive was invalid or contained no matching entry")]
    BadArchive,
    #[error("source file exceeds the configured size limit")]
    TooLarge,
    #[error("converter failed with status {0}")]
    ConverterFailed(u16),
    #[error("converter is unreachable")]
    ConverterUnavailable,
    #[error("local I/O error: {0}")]
    LocalIo(String),
    #[error("overall download deadline exceeded")]
    Timeout,
    #[error("internal error: {0}")]
    Internal(String),
    #[error(transparent)]
    Library(#[from] BookLibraryError),
}

impl DownloadError {
    /// True when the failure may be specific to one mirror, so trying the next
    /// mirror can help. Errors caused by the converter, local resources, size
    /// limits or the global deadline would repeat identically on every mirror.
    pub fn is_mirror_failure(&self) -> bool {
        matches!(
            self,
            DownloadError::SourceUnavailable | DownloadError::BadArchive
        )
    }
}
