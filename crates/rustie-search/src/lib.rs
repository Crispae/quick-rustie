//! Run RustIE / Odinson patterns against the IE postings index on Quickwit.
//!
//! A query runs as one Quickwit search in which every split matches the pattern exactly (token
//! positions from its postings, dependency graph from its GPH2 component; see `rustie-leaf`), so
//! only matching sentences are counted, ranked and fetched. The page's matched spans and named
//! captures are then rendered from the stored sentences.
//!
//! By default the search stack is **embedded** in-process. Pass
//! [`SearcherOptions::searcher_endpoint`] to dial a remote `rustie-node` over gRPC
//! `root_search` (gateway mode).
//!
//! ```no_run
//! # async fn demo() -> rustie_search::Result<()> {
//! use rustie_search::{MinioConfig, SearchQuery, Searcher, SearcherOptions};
//!
//! let minio = MinioConfig::from_env();
//! let searcher = Searcher::connect(minio.clone(), SearcherOptions::for_bucket(&minio.bucket)).await?;
//! let results = searcher
//!     .search(SearchQuery::new("[word=John] >nsubj [pos=VBZ]").limit(10))
//!     .await?;
//! for hit in results.hits {
//!     println!("{} {:?}", hit.sentence_id, hit.matches[0].spans[0].text);
//! }
//! # Ok(()) }
//! ```
//!
//! The HTTP API is in [`server`]; the `rustie-serve` binary runs it.

mod cursor;
mod error;
mod eval;
mod model;
pub mod searcher;
pub mod server;

pub use error::{Result, SearchError};
pub use model::{
    CaptureOut, MatchOut, QueryKind, SearchQuery, SearchResults, SentenceHit, SpanOut,
};
pub use rustie_indexer::{IndexSummary, MinioConfig};
pub use searcher::{Searcher, SearcherOptions};
