//! RustIE pattern matching inside Quickwit leaf search.
//!
//! Registers four extensions with the Quickwit fork (see `docs/rustie-fork.md`):
//!
//! - [`GraphSidecar`]: every split carries a GPH2 file (`rustie.gph2`) with each sentence's
//!   dependency graph and colocated tag / entity / chunk ids, row `i` = document `i`, built while
//!   indexing and rebuilt on merges.
//! - [`RustieQueryExtension`]: `{"type": "extension", "kind": "rustie", "payload":
//!   {"pattern": "..."}}` runs a RustIE pattern inside each split. Candidates come from the
//!   postings; each candidate is matched exactly (span VM for token patterns, the bound graph
//!   evaluator for traversals) on token positions from the postings and the graph from GPH2
//!   blocks. Non-matching documents never leave the split, so hit counts and paging are exact.
//! - `rustie_tokens` / `rustie_edges`: tokenizers for the doc mapping's token and edge-label
//!   fields (see [`tokenizer`]), so a token's position in postings equals its linguistic index,
//!   and an edge-label field's postings put every label of one token at that token's position.
//!
//! Call [`register`] once at process start, before any indexing or search actor runs.

use std::sync::Arc;

mod blocks;
mod candidates;
mod expand;
mod query;
mod sidecar;
mod tokenizer;

pub use query::{
    PatternPayload, RustieQueryExtension, cache_wrapped_query_ast, set_same_token_refine,
};
pub use sidecar::GraphSidecar;

/// File name of the graph component inside a split.
pub const GRAPH_FILE: &str = "rustie.gph2";

/// `kind` of RustIE extension queries.
pub const QUERY_KIND: &str = "rustie";

/// Token fields stored as dictionary ids inside the graph file, so graph patterns test them
/// without touching postings.
pub const COLOCATED_FIELDS: &[&str] = &["tag", "entity", "chunk"];

/// Install the sidecar, the query extension and the tokenizers. Idempotent.
pub fn register() {
    quickwit_extensions::register_split_sidecar(Arc::new(GraphSidecar));
    quickwit_extensions::register_query_extension(Arc::new(RustieQueryExtension::default()));
    quickwit_extensions::register_tokenizer(
        rustie_schema::RUSTIE_TOKEN_TOKENIZER_NAME,
        tokenizer::slot_text_analyzer(false),
    );
    quickwit_extensions::register_tokenizer(
        rustie_schema::RUSTIE_EDGE_TOKENIZER_NAME,
        tokenizer::slot_text_analyzer(true),
    );
}
