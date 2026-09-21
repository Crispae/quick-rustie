//! RustIE schema for Quickwit indexing (token postings + dependency graphs).
//!
//! One Quickwit document = one sentence. Token fields are pipe-joined so a
//! `pipe_tokens` tokenizer assigns Tantivy position = linguistic token index.
//! Graphs are stored as JSON for in-memory traversal; edge label postings
//! (`incoming_edges` / `outgoing_edges`) hold each sentence's *set* of edge labels for
//! document-level prefilters.

pub mod encoding;
pub mod error;
pub mod fields;
pub mod flatten;
pub mod graph;
pub mod mapping;
pub mod odinson;
pub mod sentence;

pub use encoding::{
    decode_edges, decode_edges_from_quickwit, decode_tokens, decode_tokens_from_quickwit,
    encode_edge_label_set, encode_edges, encode_edges_for_quickwit, encode_tokens,
    encode_tokens_for_quickwit, escape_join_field, labels_by_direction, unescape_join_field,
    EMPTY_TOKEN_SENTINEL,
};
pub use error::{Result, SchemaError};
pub use fields::{
    DEFAULT_INDEXED_TOKEN_FIELDS, FIELD_INCOMING_EDGES, FIELD_OUTGOING_EDGES, PIPE_TOKENIZER_NAME,
    PIPE_TOKENIZER_PATTERN, STRUCTURAL_FIELDS, TOKEN_FIELDS,
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
