//! RustIE pattern matching inside Quickwit leaf search.
//!
//! Registers three extensions with the Quickwit fork (see `docs/rustie-fork.md`):
//!
//! - [`GraphSidecar`]: every split carries a GPH2 file (`rustie.gph2`) with each sentence's
//!   dependency graph and colocated tag / entity / chunk ids, row `i` = document `i`, built while
//!   indexing and rebuilt on merges.
//! - [`RustieQueryExtension`]: `{"type": "extension", "kind": "rustie", "payload":
//!   {"pattern": "..."}}` runs a RustIE pattern inside each split. Candidates come from the
//!   postings; each candidate is matched exactly (span VM for token patterns, the bound graph
//!   evaluator for traversals) on token positions from the postings and the graph from GPH2
//!   blocks. Non-matching documents never leave the split, so hit counts and paging are exact.
//!
//! Call [`register`] once at process start, before any indexing or search actor runs.

use std::sync::Arc;

mod blocks;
mod candidates;
mod expand;
mod query;
mod sidecar;

pub use query::{PatternPayload, RustieQueryExtension};
pub use sidecar::GraphSidecar;

/// File name of the graph component inside a split.
pub const GRAPH_FILE: &str = "rustie.gph2";

/// `kind` of RustIE extension queries.
pub const QUERY_KIND: &str = "rustie";

/// Token fields stored as dictionary ids inside the graph file, so graph patterns test them
/// without touching postings.
pub const COLOCATED_FIELDS: &[&str] = &["tag", "entity", "chunk"];

/// Install the sidecar and the query extension. Idempotent.
pub fn register() {
    quickwit_extensions::register_split_sidecar(Arc::new(GraphSidecar));
    quickwit_extensions::register_query_extension(Arc::new(RustieQueryExtension::default()));
}
