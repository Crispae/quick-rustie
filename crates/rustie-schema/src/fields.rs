//! Canonical field names for IE indexing (tokens + graph).

/// Token fields indexed with positions (pipe tokenizer).
pub const TOKEN_FIELDS: &[&str] = &[
    "word", "lemma", "pos", "tag", "entity", "chunk", "norm", "raw",
];

/// Structural fields on every sentence document.
pub const STRUCTURAL_FIELDS: &[&str] = &["doc_id", "sentence_id", "sentence_length"];

/// Position-aware edge posting fields (RustIE-compatible).
pub const FIELD_INCOMING_EDGES: &str = "incoming_edges";
pub const FIELD_OUTGOING_EDGES: &str = "outgoing_edges";

/// Quickwit custom tokenizer name used in emitted index configs.
pub const PIPE_TOKENIZER_NAME: &str = "pipe_tokens";

/// Regex for Quickwit's `regex` tokenizer: one match per non-empty pipe segment.
///
/// Empty linguistic tokens are encoded as [`crate::EMPTY_TOKEN_SENTINEL`] before join
/// so positions stay aligned under this pattern.
pub const PIPE_TOKENIZER_PATTERN: &str = r"[^|]+";

/// Default token fields written into the Quickwit mapping.
///
/// Every field [`flatten_document`](crate::flatten_document) can emit must be declared
/// here, otherwise the strict mapping rejects (or the indexer has to drop) the field.
pub const DEFAULT_INDEXED_TOKEN_FIELDS: &[&str] = TOKEN_FIELDS;
