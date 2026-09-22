//! Predicate-cache hit behavior: an identical RustIE pattern issued twice against the same
//! long-lived `SearcherContext` must, on the second call, (a) already have a `predicate_cache`
//! entry and return an identical result, and (b) not touch storage again — proof that
//! `RustieQueryExtension::build()` (hence GPH2 I/O and graph scoring) never ran the second time.
//!
//! This needs a *shared* `SearcherContext`, so it cannot use `single_node_search` (rebuilds a
//! fresh, empty `SearcherContext` — and so a fresh, empty predicate cache — on every call; see
//! `leaf_search.rs`). It calls `quickwit_search::leaf::single_doc_mapping_leaf_search` directly
//! instead, the same lower-level entry point the fork's own predicate-cache tests use.

use std::sync::Arc;

use quickwit_config::{CacheConfig, SearcherConfig};
use quickwit_indexing::TestSandbox;
use quickwit_metastore::SplitMetadata;
use quickwit_proto::search::{CountHits, SearchRequest, SplitIdAndFooterOffsets};
use quickwit_query::query_ast::{ExtensionQuery, PredicateCache as _, QueryAst};
use quickwit_search::leaf::single_doc_mapping_leaf_search;
use quickwit_search::{SearcherContext, list_all_splits};
use quickwit_storage::CountingStorage;
use rustie_schema::{IndexConfigOptions, postings_index_config_yaml};
use serde_json::{Value as JsonValue, json};

/// The `doc_mapping` section of the IE index config (same as `leaf_search.rs`).
fn doc_mapping_yaml() -> String {
    let config: serde_yaml::Value =
        serde_yaml::from_str(&postings_index_config_yaml(&IndexConfigOptions::default())).unwrap();
    serde_yaml::to_string(&config["doc_mapping"]).unwrap()
}

/// One Odinson document: `[the, cat, sat]` with `cat` as the `nsubj` dependent of `sat`, so
/// `[tag=VBD] >nsubj [word=cat]` — a graph pattern, so a hit is a meaningful claim about GPH2 —
/// matches it.
fn odinson_doc(id: &str) -> String {
    let field = |name: &str, values: &[&str]| json!({"name": name, "$type": "ai.lum.odinson.TokensField", "tokens": values});
    json!({
        "id": id,
        "sentences": [{
            "numTokens": 3,
            "fields": [
                field("word", &["the", "cat", "sat"]),
                field("lemma", &["the", "cat", "sit"]),
                field("tag", &["DT", "NN", "VBD"]),
                {"name": "dependencies", "$type": "ai.lum.odinson.GraphField",
                 "edges": [[2, 1, "nsubj"], [2, 0, "det"]], "roots": [2]}
            ]
        }]
    })
    .to_string()
}

/// The fork's own `extract_split_and_footer_offsets` (`quickwit-search/src/lib.rs`) is private,
/// so this is a local, trivial reimplementation of the same field mapping.
fn to_split_offsets(meta: &SplitMetadata) -> SplitIdAndFooterOffsets {
    SplitIdAndFooterOffsets {
        split_id: meta.split_id.to_string(),
        split_footer_start: meta.footer_offsets.start,
        split_footer_end: meta.footer_offsets.end,
        timestamp_start: meta.time_range.as_ref().map(|r| *r.start()),
        timestamp_end: meta.time_range.as_ref().map(|r| *r.end()),
        num_docs: meta.num_docs as u64,
    }
}

#[tokio::test]
async fn repeated_pattern_hits_predicate_cache() -> anyhow::Result<()> {
    rustie_leaf::register();
    let index_id = "rustie-leaf-predicate-cache";
    let sandbox = TestSandbox::create(index_id, &doc_mapping_yaml(), "{}", &["word"]).await?;

    let flattened = rustie_schema::flatten_odinson_json(&odinson_doc("d0"))?;
    let docs: Vec<JsonValue> = flattened
        .iter()
        .map(rustie_schema::SentenceDoc::to_quickwit_json)
        .collect();
    // One add_documents call => exactly one single-segment split (the packager hard-asserts
    // one segment per split), so `CacheEntry`'s one-(SegmentId, HitSet)-per-split design is
    // never stressed here.
    sandbox.add_documents(docs).await?;

    // Disable the whole-response partial-request cache: `single_doc_mapping_leaf_search` checks
    // it BEFORE the query ever reaches `doc_mapper.query()`/`CacheNode`, so left at its default
    // a second identical request would be served from there instead, never touching the
    // predicate cache and making this test pass for the wrong reason.
    // `SearcherConfig` has private fields, so struct-update syntax (`..SearcherConfig::default()`)
    // isn't usable here; mutate the one field this test needs instead.
    let mut searcher_config = SearcherConfig::default();
    searcher_config.partial_request_cache = CacheConfig::no_cache();
    let searcher_context = Arc::new(SearcherContext::new_without_invoker(searcher_config, None));

    let splits_meta = list_all_splits(vec![sandbox.index_uid()], &sandbox.metastore()).await?;
    assert_eq!(splits_meta.len(), 1, "one add_documents call => one split");
    let splits = vec![to_split_offsets(&splits_meta[0])];
    let split_id = splits[0].split_id.clone();

    let pattern = "[tag=VBD] >nsubj [word=cat]";
    let query_ast = rustie_leaf::cache_wrapped_query_ast(pattern);
    let request = Arc::new(SearchRequest {
        index_id_patterns: vec![index_id.to_string()],
        query_ast: serde_json::to_string(&query_ast)?,
        max_hits: 10_000,
        count_hits: CountHits::CountAll as i32,
        ..Default::default()
    });

    // `CacheNode::fill_cache_state` keys the predicate cache on `json(self.inner)` — the plain
    // extension query the `CacheNode` wraps, not the `QueryAst::Cache(..)` envelope itself.
    let inner_ast = QueryAst::Extension(ExtensionQuery {
        kind: rustie_leaf::QUERY_KIND.to_string(),
        payload: rustie_leaf::PatternPayload {
            pattern: pattern.to_string(),
        }
        .to_json(),
    });
    let cache_key = serde_json::to_string(&inner_ast)?;

    let (storage, counters) = CountingStorage::instrument_storage(sandbox.storage());

    assert!(
        searcher_context
            .predicate_cache
            .get(split_id.clone(), cache_key.clone())
            .is_none(),
        "predicate cache must be empty before the first call"
    );

    let first = single_doc_mapping_leaf_search(
        searcher_context.clone(),
        request.clone(),
        storage.clone(),
        splits.clone(),
        sandbox.doc_mapper(),
    )
    .await?;
    assert!(
        first.failed_splits.is_empty(),
        "first call failed: {:?}",
        first.failed_splits
    );
    assert_eq!(first.num_hits, 1, "the sentence must match the pattern");

    let (bytes_after_first, requests_after_first) = counters.snapshot();
    assert!(
        requests_after_first > 0,
        "the first (miss) call must actually read the split"
    );

    assert!(
        searcher_context
            .predicate_cache
            .get(split_id.clone(), cache_key.clone())
            .is_some(),
        "predicate cache must hold an entry after the first (miss) call"
    );

    let second = single_doc_mapping_leaf_search(
        searcher_context.clone(),
        request.clone(),
        storage.clone(),
        splits.clone(),
        sandbox.doc_mapper(),
    )
    .await?;
    assert!(
        second.failed_splits.is_empty(),
        "second call failed: {:?}",
        second.failed_splits
    );
    assert_eq!(
        second.num_hits, first.num_hits,
        "a hit must return the same count"
    );

    let (bytes_after_second, requests_after_second) = counters.snapshot();
    assert_eq!(
        requests_after_second, requests_after_first,
        "second identical call must not touch storage again (GPH2 must not be re-read)"
    );
    assert_eq!(bytes_after_second, bytes_after_first);

    sandbox.assert_quit().await;
    Ok(())
}
