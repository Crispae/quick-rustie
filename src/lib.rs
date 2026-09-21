//! Library surface for Quickwit indexing against S3 / S3-compatible storage,
//! plus RustIE query language and postings schema.
//!
//! Core crates (from [quickwit-oss/quickwit](https://github.com/quickwit-oss/quickwit)):
//! - [`quickwit_indexing`] — actor-based indexing pipeline
//! - [`quickwit_storage`] — `s3://` (and Azure/GCS/file) storage backends
//! - [`quickwit_aws`] — AWS credential / S3 client setup
//! - [`quickwit_config`] — node / index / storage configuration (`s3://...` URIs)
//! - [`quickwit_metastore`] — index metadata (file-backed metastore can live on S3)
//! - [`quickwit_doc_mapper`] — document schema / mapping for indexing
//!
//! RustIE workspace crates:
//! - [`rustie_query`] — Odinson-style patterns (`[word=John] >nsubj …`)
//! - [`rustie_schema`] — sentence docs + Quickwit postings mapping (`configs/ie_postings.yaml`)
//! - [`rustie_compiler`] — compile patterns to Quickwit filters + in-memory plans
//!
//! - [`rustie_search`] — run patterns against the index (`Searcher`, HTTP API)
//! - [`rustie_indexer`] — create the postings index on MinIO and ingest Odinson docs
//!
//! Local MinIO: see [`minio`] — defaults to `rustie-minio` on `http://127.0.0.1:9010`.

pub use quickwit_aws as aws;
pub use quickwit_common as common;
pub use quickwit_config as config;
pub use quickwit_doc_mapper as doc_mapper;
pub use quickwit_indexing as indexing;
pub use quickwit_metastore as metastore;
pub use quickwit_storage as storage;

pub use rustie_query;
pub use rustie_query::{Pattern, QueryError, QueryParser};

pub use rustie_schema;
pub use rustie_schema::{
    flatten_odinson_json, postings_index_config_yaml, IndexConfigOptions, SentenceDoc,
};

pub use rustie_compiler;
pub use rustie_compiler::{
    evaluate_on_sentence, CandidateFilter, CompiledQuery, QueryCompiler,
};

// MinIO helpers live in `rustie-indexer`; re-exported here so `quick_rustie::minio` keeps working.
pub use rustie_indexer;
pub use rustie_search;
pub use rustie_indexer::minio;
pub use rustie_indexer::{
    connect_minio, minio_storage_resolver, ping_minio, IndexStatus, IndexSummary, Indexer,
    IndexerError, IndexerOptions, IndexingStats, InvalidDocPolicy, MinioConfig, StopHandle,
};

/// Package name helper for smoke tests / scaffolding.
pub fn library_name() -> &'static str {
    env!("CARGO_PKG_NAME")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matches_package() {
        assert_eq!(library_name(), "quick_rustie");
    }

    #[test]
    fn rustie_query_parses_traversal() {
        let pattern = QueryParser::new()
            .parse_query("[word=John] >nsubj [pos=VBZ]")
            .expect("parse");
        assert!(matches!(pattern, Pattern::GraphTraversal { .. }));
    }

    #[test]
    fn rustie_schema_flattens_sentence() {
        let json = r#"{
          "id": "d1",
          "sentences": [{
            "numTokens": 2,
            "fields": [{
              "name": "word",
              "$type": "ai.lum.odinson.TokensField",
              "tokens": ["Hello", "world"]
            }]
          }]
        }"#;
        let docs = flatten_odinson_json(json).expect("flatten");
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].to_quickwit_json()["word"], "Hello|world");
    }

    #[test]
    fn rustie_compiler_compiles_graph() {
        let compiled = QueryCompiler::new()
            .compile("[word=John] >nsubj [pos=VBZ]")
            .expect("compile");
        assert!(matches!(compiled, CompiledQuery::Graph(_)));
        assert_ne!(compiled.candidate().to_quickwit_query(), "*");
    }
}
