//! End-to-end test against a live MinIO. Skipped unless `RUSTIE_MINIO_TEST=1`.
//!
//! ```bash
//! docker compose -f docker-compose.minio.yml up -d
//! RUSTIE_MINIO_TEST=1 cargo test -p rustie-indexer --test minio_integration -- --ignored --nocapture
//! ```

use rustie_indexer::{IndexStatus, Indexer, IndexerOptions, MinioConfig, ping_minio};

fn odinson_doc(id: &str, words: &[&str]) -> String {
    let tokens = serde_json::to_string(words).unwrap();
    let edges: Vec<String> = (1..words.len())
        .map(|i| format!("[{}, {}, \"nsubj\"]", i, i - 1))
        .collect();
    format!(
        r#"{{"id": "{id}", "sentences": [{{
            "numTokens": {n},
            "fields": [
              {{"name": "word", "$type": "ai.lum.odinson.TokensField", "tokens": {tokens}}},
              {{"name": "norm", "$type": "ai.lum.odinson.TokensField", "tokens": {tokens}}},
              {{"name": "dependencies", "$type": "ai.lum.odinson.GraphField",
                "edges": [{edges}], "roots": [0]}}
            ]}}]}}"#,
        n = words.len(),
        edges = edges.join(",")
    )
}

#[tokio::test]
#[ignore = "needs MinIO; set RUSTIE_MINIO_TEST=1"]
async fn index_create_ingest_reingest() {
    if std::env::var("RUSTIE_MINIO_TEST").as_deref() != Ok("1") {
        eprintln!("RUSTIE_MINIO_TEST!=1, skipping");
        return;
    }
    let minio = MinioConfig::from_env();
    ping_minio(&minio).await.expect("MinIO reachable");

    // Isolated namespace so parallel/aborted runs never touch the real index.
    let unique = format!(
        "it-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let mut options = IndexerOptions::for_bucket(&minio.bucket);
    options.index_id = format!("ie-postings-{unique}");
    options.metastore_uri = format!("s3://{}/{unique}/metastore", minio.bucket);
    options.index_root_uri = format!("s3://{}/{unique}/indexes", minio.bucket);

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("a.json"),
        odinson_doc("a", &["Dogs", "bark", "loudly"]),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("b.json"),
        odinson_doc("b", &["Cats", "sleep"]),
    )
    .unwrap();
    std::fs::write(dir.path().join("bad.json"), "{ broken").unwrap();

    let indexer = Indexer::connect(minio.clone(), options.clone())
        .await
        .unwrap();
    let outcome = run(&indexer, dir.path()).await;
    // Always clean up, even if assertions failed.
    let _ = indexer.delete_index().await;
    indexer.shutdown().await;
    outcome
}

async fn run(indexer: &Indexer, data: &std::path::Path) {
    assert_eq!(indexer.ensure_index().await.unwrap(), IndexStatus::Created);
    assert_eq!(
        indexer.ensure_index().await.unwrap(),
        IndexStatus::AlreadyExisted
    );

    let stats = indexer.index_odinson_dir(data, None, 10).await.unwrap();
    assert_eq!(stats.files_seen, 3);
    assert_eq!(stats.files_failed, 1, "bad.json is skipped, not fatal");
    assert_eq!(stats.docs_submitted, 2);
    assert_eq!(stats.docs_invalid, 0);
    assert!(
        stats.unmapped_fields.is_empty(),
        "norm/raw are part of the mapping now"
    );
    assert!(stats.splits_published >= 1);
    assert_eq!(stats.batches_already_indexed, 0);

    let summary = indexer.summary().await.unwrap();
    assert!(summary.num_published_splits >= 1);
    assert_eq!(summary.num_docs, 2);

    // Same content again: the content-derived checkpoint makes it a no-op.
    let again = indexer.index_odinson_dir(data, None, 10).await.unwrap();
    assert_eq!(again.batches_already_indexed, 1);
    assert_eq!(
        indexer.summary().await.unwrap().num_docs,
        2,
        "no duplicates"
    );
}
