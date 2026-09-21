//! Run RustIE / Odinson patterns against the IE postings index on Quickwit.
//!
//! A query is compiled by `rustie-compiler` into (1) a Quickwit **prefilter** — a superset of
//! the matching sentences, evaluated by the index — and (2) an exact in-memory matcher that
//! runs on the stored tokens and dependency graph of each candidate. Only sentences that
//! survive step 2 are returned, with the matched spans and named captures.
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
mod prefilter;
pub mod searcher;
pub mod server;

pub use error::{Result, SearchError};
pub use model::{
    CaptureOut, MatchOut, QueryKind, SearchQuery, SearchResults, SentenceHit, SpanOut,
};
pub use rustie_indexer::{IndexSummary, MinioConfig};
pub use searcher::{Searcher, SearcherOptions};
