//! GPH2 — the per-split dependency-graph component.
//!
//! One file per split holds every sentence's graph as compact binary columns, grouped in
//! blocks of [`BLOCK_DOCS`] documents with a block offset index in the trailer, so a reader can
//! range-read only the blocks its candidate documents live in (object storage friendly) and a
//! merge can stream them. Document `d` is row `d`: rows are aligned with the search index's
//! document ids.
//!
//! Pure bytes-in / structs-out; no dependency on the search engine or on storage.

pub mod backbone;
pub mod format;
pub mod merge;
pub mod reader;
pub mod record;
pub mod spool;
pub mod view;
pub mod writer;

pub use backbone::{BackboneSplit, ROOT, split_backbone};
pub use format::{BLOCK_DOCS, END_RECORD_LEN, GPH2_MAGIC, GPH2_VERSION, Gph2Trailer};
pub use merge::{MergeSource, merge_sidecars, merge_sources, sidecar_doc_count};
pub use record::SentenceRecord;
pub use spool::SpoolWriter;
pub use view::{SentenceScratch, SentenceView};
pub use writer::Gph2Writer;

/// GPH2 files name their segment by a canonical lowercase, dash-free id.
pub(crate) fn normalize_uuid(id: &str) -> String {
    id.replace('-', "").to_ascii_lowercase()
}
