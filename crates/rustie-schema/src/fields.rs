//! Canonical field names for IE indexing (tokens + graph).

/// Token fields indexed with positions (position-aware tokenizer).
pub const TOKEN_FIELDS: &[&str] = &[
    "word", "lemma", "pos", "tag", "entity", "chunk", "norm", "raw",
];

/// Structural fields on every sentence document.
pub const STRUCTURAL_FIELDS: &[&str] = &["doc_id", "sentence_id", "sentence_length"];

/// Position-aware edge posting fields (RustIE-compatible).
pub const FIELD_INCOMING_EDGES: &str = "incoming_edges";
pub const FIELD_OUTGOING_EDGES: &str = "outgoing_edges";

/// Tokenizer name for token fields (`word`, `lemma`, …): one term per pipe slot, slot index =
/// position. Registered by `rustie_leaf::register()` through the Quickwit fork's
/// tokenizer-registration hook (`quickwit_extensions::register_tokenizer`); see
/// [`crate::encoding::slot_terms`] for the format it reads (`multi = false`).
pub const RUSTIE_TOKEN_TOKENIZER_NAME: &str = "rustie_tokens";

/// Tokenizer name for edge label fields (`incoming_edges`, `outgoing_edges`): every comma-joined
/// label within a pipe slot becomes its own term, all at that slot's position, so a token's
/// several incoming/outgoing labels are all found at the same position. See
/// [`crate::encoding::slot_terms`] (`multi = true`).
pub const RUSTIE_EDGE_TOKENIZER_NAME: &str = "rustie_edges";

/// Default token fields written into the Quickwit mapping.
///
/// Every field [`flatten_document`](crate::flatten_document) can emit must be declared
/// here, otherwise the strict mapping rejects (or the indexer has to drop) the field.
pub const DEFAULT_INDEXED_TOKEN_FIELDS: &[&str] = TOKEN_FIELDS;
