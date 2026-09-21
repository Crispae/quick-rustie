//! Error type for the indexer.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, IndexerError>;

#[derive(Debug, thiserror::Error)]
pub enum IndexerError {
    /// Bad caller-supplied configuration (URIs, sizes, ids).
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),

    /// The index already exists with a doc mapping / URI different from the one this
    /// crate would create. Indexing into it would silently corrupt search semantics.
    #[error("index `{index_id}` already exists with a different configuration: {detail}")]
    IndexConfigMismatch { index_id: String, detail: String },

    /// Some documents do not satisfy the index's doc mapping. Nothing from the offending
    /// batch was submitted, so the run can be resumed after fixing the input or mapping.
    #[error("{count} document(s) rejected by the doc mapping, e.g. {}", sample.join("; "))]
    InvalidDocuments { count: u64, sample: Vec<String> },

    #[error("i/o error on `{}`: {source}", path.display())]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The Quickwit indexing pipeline exited abnormally; nothing from the failed
    /// batch was published (splits and checkpoints are committed atomically).
    #[error("indexing pipeline failed: {0}")]
    Pipeline(String),

    /// Storage / metastore / actor-system failure.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

impl IndexerError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
