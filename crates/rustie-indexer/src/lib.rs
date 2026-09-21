//! Index flattened Odinson sentences into a Quickwit index stored on MinIO / S3.
//!
//! - [`minio`] — MinIO connection settings and Quickwit storage helpers.
//! - [`Indexer`] — creates the IE postings index (mapping from `rustie-schema`) and runs
//!   Quickwit's indexing pipeline against it.
//! - [`flatten_source`] — Odinson JSON directory → Quickwit sentence documents.
//!
//! ```no_run
//! # async fn demo() -> rustie_indexer::Result<()> {
//! use rustie_indexer::{Indexer, IndexerOptions, MinioConfig};
//!
//! let minio = MinioConfig::from_env();
//! let indexer = Indexer::connect(minio.clone(), IndexerOptions::for_bucket(&minio.bucket)).await?;
//! let stats = indexer.index_odinson_dir("data".as_ref(), Some(10), 200).await?;
//! println!("{} docs", stats.docs_processed);
//! indexer.shutdown().await;
//! # Ok(()) }
//! ```

mod error;
pub mod flatten_source;
mod index;
mod lanes;
pub mod minio;
mod pipeline;
mod store;

pub use error::{IndexerError, Result};
pub use flatten_source::{FileFailure, FlattenedBatch};
pub use index::{
    DEFAULT_GC_DELETION_GRACE, GcReport, IndexStatus, Indexer, IndexerOptions, IndexingStats,
    InvalidDocPolicy, StopHandle,
};
pub use minio::{MinioConfig, connect_minio, minio_storage_resolver, ping_minio};
pub use store::{IndexSummary, index_summary, open_metastore};
