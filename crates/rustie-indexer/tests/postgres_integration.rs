//! End-to-end test against live MinIO + Postgres. Skipped unless `RUSTIE_PG_TEST=1`.
//!
//! ```bash
//! docker run -d --name rustie-postgres -p 5433:5432 \
//!   -e POSTGRES_USER=rustie -e POSTGRES_PASSWORD=rustie -e POSTGRES_DB=rustie postgres:16
//! docker compose -f docker-compose.minio.yml up -d
//! RUSTIE_PG_TEST=1 cargo test -p rustie-indexer --test postgres_integration -- --ignored --nocapture
//! ```

use rustie_indexer::{
    DEFAULT_POSTGRES_METASTORE_URI, IndexStatus, Indexer, IndexerOptions, MinioConfig, ping_minio,
};

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

fn postgres_uri() -> String {
    std::env::var("RUSTIE_METASTORE_URI")
        .unwrap_or_else(|_| DEFAULT_POSTGRES_METASTORE_URI.to_string())
}

#[tokio::test]
#[ignore = "needs MinIO + Postgres; set RUSTIE_PG_TEST=1"]
async fn postgres_index_summary_and_content_hash_idempotency() {
    if std::env::var("RUSTIE_PG_TEST").as_deref() != Ok("1") {
        eprintln!("RUSTIE_PG_TEST!=1, skipping");
        return;
    }
    let minio = MinioConfig::from_env();
    ping_minio(&minio).await.expect("MinIO reachable");

    let unique = format!(
        "pg-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    );
    let mut options = IndexerOptions::for_bucket(&minio.bucket);
    options.index_id = format!("ie-postings-{unique}");
    options.metastore_uri = postgres_uri();
    // Isolate split files under a unique prefix so parallel runs do not collide.
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

    let indexer = Indexer::connect(minio.clone(), options.clone())
        .await
        .expect("connect with Postgres metastore");
    let outcome = async {
        assert_eq!(indexer.ensure_index().await.unwrap(), IndexStatus::Created);

        let stats = indexer
            .index_odinson_dir(dir.path(), None, 10)
            .await
            .unwrap();
        assert_eq!(stats.files_seen, 2);
        assert_eq!(stats.docs_processed, 2);
        assert_eq!(stats.batches_already_indexed, 0);
        assert!(stats.splits_published >= 1);

        let summary = indexer.summary().await.unwrap();
        assert!(summary.num_published_splits >= 1);
        assert_eq!(summary.num_docs, 2);

        // Content-hash checkpoints on Postgres: every batch already indexed.
        let again = indexer.index_odinson_dir(dir.path(), None, 10).await.unwrap();
        assert_eq!(again.batches_already_indexed, 1);
        assert_eq!(again.docs_processed, 0);
        assert_eq!(indexer.summary().await.unwrap().num_docs, 2);
    }
    .await;
    let _ = indexer.delete_index().await;
    indexer.shutdown().await;
    outcome
}
