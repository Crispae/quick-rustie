use rustie_compiler::CompileError;

pub type Result<T> = std::result::Result<T, SearchError>;

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    /// The query string failed to parse or compile, or a request parameter is out of range.
    /// Caused by the caller; maps to HTTP 400.
    #[error("invalid query: {0}")]
    InvalidQuery(String),

    #[error("search timed out after {0:?}")]
    Timeout(std::time::Duration),

    /// Storage, metastore or Quickwit search failure.
    #[error("search backend error: {0}")]
    Backend(String),
}

impl From<CompileError> for SearchError {
    fn from(err: CompileError) -> Self {
        Self::InvalidQuery(err.to_string())
    }
}

impl From<rustie_indexer::IndexerError> for SearchError {
    fn from(err: rustie_indexer::IndexerError) -> Self {
        Self::Backend(err.to_string())
    }
}
