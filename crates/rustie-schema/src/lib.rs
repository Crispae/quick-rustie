//! RustIE schema for Quickwit indexing (token postings + dependency graphs).
//!
//! One Quickwit document = one sentence. Token fields are pipe-joined so the `rustie_tokens`
//! tokenizer assigns Tantivy position = linguistic token index. Edge label fields
//! (`incoming_edges` / `outgoing_edges`) are pipe-joined the same way, with comma-joined labels
//! at one slot; the `rustie_edges` tokenizer places every label of a slot at that slot's
//! position, so several labels on one token are all found there. Both tokenizers are registered
//! by `rustie_leaf::register()` (see [`crate::encoding::slot_terms`] for the format). Graphs are
//! also stored as JSON for in-memory traversal after retrieval.

pub mod encoding;
pub mod error;
pub mod fields;
pub mod flatten;
pub mod graph;
pub mod mapping;
pub mod odinson;
pub mod sentence;

pub use encoding::{
    decode_edges, decode_tokens, decode_tokens_from_quickwit, encode_edges, encode_tokens,
    escape_join_field, labels_by_direction, slot_terms, unescape_join_field, EMPTY_TOKEN_SENTINEL,
};
pub use error::{Result, SchemaError};
pub use fields::{
    DEFAULT_INDEXED_TOKEN_FIELDS, FIELD_INCOMING_EDGES, FIELD_OUTGOING_EDGES,
    RUSTIE_EDGE_TOKENIZER_NAME, RUSTIE_TOKEN_TOKENIZER_NAME, STRUCTURAL_FIELDS, TOKEN_FIELDS,
};
pub use flatten::{flatten_document, flatten_odinson_json};
pub use graph::{DependencyEdge, SentenceGraph, DEFAULT_BASIC_GRAPH_FIELD, DEFAULT_GRAPH_FIELD};
pub use mapping::{
    postings_doc_mapping_yaml, postings_index_config_yaml, IndexConfigOptions,
    DEFAULT_SPLIT_NUM_DOCS_TARGET,
    PostingsMappingOptions,
};
pub use odinson::{
    Document as OdinsonDocument, Field as OdinsonField, Sentence as OdinsonSentence,
};
pub use sentence::SentenceDoc;
