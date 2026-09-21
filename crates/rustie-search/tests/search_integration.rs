//! End-to-end: index a tiny corpus into MinIO, then query it through the library and the
//! HTTP router. Skipped unless `RUSTIE_MINIO_TEST=1`.
//!
//! ```bash
//! RUSTIE_MINIO_TEST=1 cargo test -p rustie-search --test search_integration -- --ignored --nocapture
//! ```

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use rustie_indexer::{Indexer, IndexerOptions, MinioConfig};
use rustie_search::{QueryKind, SearchError, SearchQuery, Searcher, SearcherOptions, server};
use tower::ServiceExt;

/// One-sentence Odinson doc: `words`/`pos`/`norm` aligned; token 1 is the head of token 0
/// (`nsubj`) and, when there is a third token, of token 2 (`advmod`).
fn odinson_doc(id: &str, words: &[&str], pos: &[&str]) -> String {
    let field = |name: &str, toks: &[&str]| {
        format!(
            r#"{{"name": "{name}", "$type": "ai.lum.odinson.TokensField", "tokens": {}}}"#,
            serde_json::to_string(toks).unwrap()
        )
    };
    let mut edges = vec![r#"[1, 0, "nsubj"]"#.to_string()];
    if words.len() > 2 {
        edges.push(r#"[1, 2, "advmod"]"#.to_string());
    }
    format!(
        r#"{{"id": "{id}", "sentences": [{{"numTokens": {n}, "fields": [{w}, {p}, {norm},
            {{"name": "dependencies", "$type": "ai.lum.odinson.GraphField",
              "edges": [{edges}], "roots": [1]}}]}}]}}"#,
        n = words.len(),
        w = field("word", words),
        p = field("pos", pos),
        norm = field("norm", words),
        edges = edges.join(",")
    )
}

#[tokio::test]
#[ignore = "needs MinIO; set RUSTIE_MINIO_TEST=1"]
async fn index_then_search() {
    if std::env::var("RUSTIE_MINIO_TEST").as_deref() != Ok("1") {
        eprintln!("RUSTIE_MINIO_TEST!=1, skipping");
        return;
    }
    let minio = MinioConfig::from_env();
    let unique = format!(
        "it-search-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let mut indexer_options = IndexerOptions::for_bucket(&minio.bucket);
    indexer_options.index_id = format!("ie-postings-{unique}");
    indexer_options.metastore_uri = format!("s3://{}/{unique}/metastore", minio.bucket);
    indexer_options.index_root_uri = format!("s3://{}/{unique}/indexes", minio.bucket);

    let dir = tempfile::tempdir().unwrap();
    let corpus = [
        ("a", vec!["John", "runs", "fast"], vec!["NNP", "VBZ", "RB"]),
        ("b", vec!["Mary", "sleeps"], vec!["NNP", "VBZ"]),
        ("c", vec!["Dogs", "bark"], vec!["NNS", "VBP"]),
        ("d", vec!["the", "cat", "sat"], vec!["DT", "NN", "VBD"]),
        // Same words as `d`, not adjacent in that order: an AND-of-terms prefilter would
        // admit it, a phrase prefilter must not.
        ("e", vec!["cat", "the", "end"], vec!["NN", "DT", "NN"]),
    ];
    for (id, words, pos) in &corpus {
        std::fs::write(
            dir.path().join(format!("{id}.json")),
            odinson_doc(id, words, pos),
        )
        .unwrap();
    }

    let indexer = Indexer::connect(minio.clone(), indexer_options.clone())
        .await
        .unwrap();
    let indexed = indexer.index_odinson_dir(dir.path(), None, 10).await;
    let outcome = match indexed {
        Ok(stats) => {
            assert_eq!(stats.docs_processed, 5);
            let mut options = SearcherOptions::for_bucket(&minio.bucket);
            options.index_id = indexer_options.index_id.clone();
            options.metastore_uri = indexer_options.metastore_uri.clone();
            let searcher = Arc::new(Searcher::connect(minio.clone(), options).await.unwrap());
            // Run in a task so an assertion failure still lets us delete the index.
            tokio::spawn(queries(searcher)).await
        }
        Err(err) => panic!("indexing failed: {err}"),
    };
    let _ = indexer.delete_index().await;
    indexer.shutdown().await;
    outcome.unwrap();
}

async fn queries(searcher: Arc<Searcher>) {
    // Surface pattern, exact term.
    let r = searcher
        .search(SearchQuery::new("[word=John]"))
        .await
        .unwrap();
    assert_eq!(r.kind, QueryKind::Surface);
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].doc_id, "a");
    assert_eq!(r.hits[0].matches[0].spans[0].text, "John");
    assert!(r.exhausted);

    // Case-sensitive: the prefilter must not lowercase.
    assert!(
        searcher
            .search(SearchQuery::new("[word=john]"))
            .await
            .unwrap()
            .hits
            .is_empty()
    );

    // `norm` is part of the mapping.
    let r = searcher
        .search(SearchQuery::new("[norm=Mary]"))
        .await
        .unwrap();
    assert_eq!(r.hits.len(), 1);

    // Graph traversal: exact evaluation after the prefilter.
    let r = searcher
        .search(SearchQuery::new("[pos=VBZ] >nsubj [pos=NNP]"))
        .await
        .unwrap();
    assert_eq!(r.kind, QueryKind::Graph);
    let mut ids: Vec<_> = r.hits.iter().map(|h| h.doc_id.as_str()).collect();
    ids.sort();
    assert_eq!(ids, ["a", "b"]);
    let spans = &r.hits[0].matches[0].spans;
    assert_eq!(spans.len(), 2, "one span per traversal endpoint");
    assert_eq!(spans[0].text.as_str(), r.hits[0].words[1]);

    // Prefilter false positive: every term is in the sentence, but not that relation.
    let r = searcher
        .search(SearchQuery::new("[pos=VBZ] >advmod [pos=NNP]"))
        .await
        .unwrap();
    assert!(
        r.hits.is_empty(),
        "{} candidates must be filtered exactly",
        r.candidates_scanned
    );

    // Regex prefilter.
    let r = searcher
        .search(SearchQuery::new("[word=/J.*/]"))
        .await
        .unwrap();
    assert_eq!(r.hits.len(), 1);

    // Token-aware prefilter: adjacency is decided by the index (positions), so the
    // non-adjacent sentence is never even fetched.
    let r = searcher
        .search(SearchQuery::new("[word=the] [word=cat]").count(true))
        .await
        .unwrap();
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].doc_id, "d");
    assert_eq!(r.hits[0].matches[0].spans[0].text, "the cat");
    assert_eq!(
        r.candidates_total,
        Some(1),
        "phrase prefilter admits only adjacent"
    );
    assert_eq!(r.candidates_scanned, 1);
    assert_eq!(r.candidate_query["type"], "full_text");

    // Fuzzy matching is case-insensitive, and so must its prefilter be.
    let r = searcher
        .search(SearchQuery::new("[word=john~]"))
        .await
        .unwrap();
    assert_eq!(r.hits.len(), 1, "fuzzy prefilter must admit `John`");

    // `^…$` is stripped; `\b` is beyond the index's regex engine, so it is dropped at plan
    // time while the exact matcher still enforces it.
    let r = searcher
        .search(SearchQuery::new("[word=/^J.*$/]"))
        .await
        .unwrap();
    assert_eq!((r.hits.len(), r.prefilter_relaxed_clauses), (1, 0));
    let r = searcher
        .search(SearchQuery::new(r"[word=/\bJohn/]"))
        .await
        .unwrap();
    assert_eq!((r.hits.len(), r.prefilter_relaxed_clauses), (1, 1));

    // Cursor paging resumes after the last examined candidate: no overlap, no gaps.
    let first = searcher
        .search(SearchQuery::new("[pos=/VB.?/]").limit(1))
        .await
        .unwrap();
    assert_eq!(first.hits.len(), 1);
    assert!(!first.exhausted);
    let cursor = first.next_cursor.clone().expect("more pages");
    let rest = searcher
        .search(
            SearchQuery::new("[pos=/VB.?/]")
                .limit(10)
                .cursor(cursor.clone()),
        )
        .await
        .unwrap();
    assert_eq!(rest.hits.len(), 3);
    assert!(rest.exhausted && rest.next_cursor.is_none());
    let mut all: Vec<_> = std::iter::once(&first.hits[0])
        .chain(rest.hits.iter())
        .map(|h| h.sentence_id.clone())
        .collect();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 4, "every match exactly once across pages");
    // A cursor is bound to its query.
    assert!(matches!(
        searcher
            .search(SearchQuery::new("[pos=NN]").cursor(cursor))
            .await,
        Err(SearchError::InvalidQuery(_))
    ));

    // Bad input is the caller's fault.
    for bad in ["[word=", "", "   "] {
        assert!(
            matches!(
                searcher.search(SearchQuery::new(bad)).await,
                Err(SearchError::InvalidQuery(_))
            ),
            "{bad:?}"
        );
    }

    // HTTP layer.
    let app = server::router(Arc::clone(&searcher), 4);
    let response = app
        .clone()
        .oneshot(
            Request::get("/v1/search?q=%5Bword%3DJohn%5D&limit=5")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["hits"][0]["sentence_id"], "a_0");

    let response = app
        .clone()
        .oneshot(
            Request::post("/v1/search")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query": "[word="}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let response = app
        .oneshot(Request::get("/v1/index").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}
