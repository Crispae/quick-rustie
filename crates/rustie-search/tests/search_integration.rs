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

    // Every term is in the sentence, but not that relation: rejected inside the split, so it is
    // not even counted.
    let r = searcher
        .search(SearchQuery::new("[pos=VBZ] >advmod [pos=NNP]").count(true))
        .await
        .unwrap();
    assert!(r.hits.is_empty());
    assert_eq!(r.total_hits, 0);

    // Regex over the term dictionary.
    let r = searcher
        .search(SearchQuery::new("[word=/J.*/]"))
        .await
        .unwrap();
    assert_eq!(r.hits.len(), 1);

    // Adjacency from token positions: `e` has both words, not adjacent, and is not counted.
    let r = searcher
        .search(SearchQuery::new("[word=the] [word=cat]").count(true))
        .await
        .unwrap();
    assert_eq!(r.hits.len(), 1);
    assert_eq!(r.hits[0].doc_id, "d");
    assert_eq!(r.hits[0].matches[0].spans[0].text, "the cat");
    assert_eq!((r.total_hits, r.total_is_exact), (1, true));

    // Fuzzy matching is case-insensitive.
    let r = searcher
        .search(SearchQuery::new("[word=john~]"))
        .await
        .unwrap();
    assert_eq!(r.hits.len(), 1, "fuzzy prefilter must admit `John`");

    // Anchors are redundant for whole-token matching; `\b` is outside the term dictionary's
    // regex dialect and is resolved by scanning the dictionary instead.
    let r = searcher
        .search(SearchQuery::new("[word=/^J.*$/]"))
        .await
        .unwrap();
    assert_eq!(r.hits.len(), 1);
    let r = searcher
        .search(SearchQuery::new(r"[word=/\bJohn/]"))
        .await
        .unwrap();
    assert_eq!(r.hits.len(), 1);

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

/// Smoke check for the on-disk split cache: enabling it must create and populate its directory.
/// Not a hit/miss assertion — `SearchSplitCache`'s internals aren't visible from outside
/// `quickwit-storage` — just proof the wiring in `Searcher::connect` actually takes effect.
#[tokio::test]
#[ignore = "needs MinIO; set RUSTIE_MINIO_TEST=1"]
async fn split_cache_directory_is_populated() {
    if std::env::var("RUSTIE_MINIO_TEST").as_deref() != Ok("1") {
        eprintln!("RUSTIE_MINIO_TEST!=1, skipping");
        return;
    }
    let minio = MinioConfig::from_env();
    let unique = format!(
        "it-split-cache-{}-{}",
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

    let docs_dir = tempfile::tempdir().unwrap();
    std::fs::write(
        docs_dir.path().join("a.json"),
        odinson_doc("a", &["John", "runs", "fast"], &["NNP", "VBZ", "RB"]),
    )
    .unwrap();

    let indexer = Indexer::connect(minio.clone(), indexer_options.clone())
        .await
        .unwrap();
    let indexed = indexer.index_odinson_dir(docs_dir.path(), None, 10).await;
    let outcome = match indexed {
        Ok(stats) => {
            assert_eq!(stats.docs_processed, 1);
            let split_cache_dir = tempfile::tempdir().unwrap();
            let mut options = SearcherOptions::for_bucket(&minio.bucket);
            options.index_id = indexer_options.index_id.clone();
            options.metastore_uri = indexer_options.metastore_uri.clone();
            options = options
                .with_split_cache(split_cache_dir.path().to_path_buf(), 100, 100)
                .unwrap();
            let searcher = Searcher::connect(minio.clone(), options).await.unwrap();
            let result = searcher.search(SearchQuery::new("[word=John]")).await;
            // The split cache's downloader is a background task (see `download_task.rs`) that
            // polls for candidates once a second when idle, then copies the split from MinIO:
            // poll for it to land rather than checking once immediately after the search.
            let mut populated = false;
            for _ in 0..30 {
                populated = std::fs::read_dir(split_cache_dir.path())
                    .map(|entries| entries.count() > 0)
                    .unwrap_or(false);
                if populated {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            result.map(|r| (r, populated))
        }
        Err(err) => panic!("indexing failed: {err}"),
    };
    let _ = indexer.delete_index().await;
    indexer.shutdown().await;
    let (response, populated) = outcome.unwrap();
    assert_eq!(response.hits.len(), 1);
    assert!(
        populated,
        "split cache directory must hold at least one file after a query"
    );
}

/// Gateway mode: same MinIO index as embedded, but `root_search` goes to a remote `rustie-node`
/// gRPC (`RUSTIE_SEARCHER_ENDPOINT`, default `127.0.0.1:7281`).
///
/// Prerequisites:
/// 1. MinIO up; `RUSTIE_MINIO_TEST=1`
/// 2. A `rustie-node` already running against the same bucket (see `configs/rustie-node.yaml`),
///    with `rustie_leaf::register()` installed — start it before this test.
///
/// Compares embedded vs gateway hit sets for one pattern (node×1 + gateway). Multi-node
/// (node×2) is the same gateway client pointed at either peer; leaf fan-out is inside Quickwit.
///
/// ```bash
/// # terminal 1
/// cargo run --release -p rustie-node -- --config configs/rustie-node.yaml
/// # terminal 2
/// RUSTIE_MINIO_TEST=1 cargo test -p rustie-search --test search_integration \
///   gateway_matches_embedded -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "needs MinIO + running rustie-node; set RUSTIE_MINIO_TEST=1"]
async fn gateway_matches_embedded() {
    if std::env::var("RUSTIE_MINIO_TEST").as_deref() != Ok("1") {
        eprintln!("RUSTIE_MINIO_TEST!=1, skipping");
        return;
    }
    let endpoint: std::net::SocketAddr = std::env::var("RUSTIE_SEARCHER_ENDPOINT")
        .unwrap_or_else(|_| "127.0.0.1:7281".into())
        .parse()
        .expect("RUSTIE_SEARCHER_ENDPOINT must be host:port");

    let minio = MinioConfig::from_env();
    // Use the shared default index the local rustie-node is expected to see (already indexed
    // via rustie-index). If the index is empty, the test still checks that both backends agree.
    let mut embedded_opts = SearcherOptions::for_bucket(&minio.bucket);
    let mut gateway_opts = embedded_opts.clone();
    gateway_opts.searcher_endpoint = Some(endpoint);

    let embedded = Searcher::connect(minio.clone(), embedded_opts)
        .await
        .expect("embedded connect");
    assert!(!embedded.is_gateway());

    let gateway = Searcher::connect(minio, gateway_opts)
        .await
        .expect("gateway connect");
    assert!(gateway.is_gateway());

    // summary() uses the local metastore in both modes.
    let _ = embedded.summary().await;
    let _ = gateway.summary().await;

    let pattern = "[word=John] >nsubj [pos=VBZ]";
    let emb = embedded
        .search(SearchQuery::new(pattern).count(true).limit(20))
        .await
        .expect("embedded search");
    let gw = gateway
        .search(SearchQuery::new(pattern).count(true).limit(20))
        .await
        .expect("gateway search");

    let emb_ids: Vec<_> = emb.hits.iter().map(|h| h.sentence_id.clone()).collect();
    let gw_ids: Vec<_> = gw.hits.iter().map(|h| h.sentence_id.clone()).collect();
    assert_eq!(emb_ids, gw_ids, "hit order/ids must match");
    assert_eq!(emb.total_hits, gw.total_hits);
    assert_eq!(emb.kind, gw.kind);
}
