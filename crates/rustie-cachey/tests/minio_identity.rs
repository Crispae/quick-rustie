//! Gated identity test: Cachey vs direct S3 on a real split.
//!
//! ```bash
//! docker compose -f docker-compose.minio.yml up -d cachey
//! RUSTIE_CACHEY_TEST=1 RUSTIE_CACHEY_TEST_SPLIT=path/to/file.split \
//!   cargo test -p rustie-cachey --test minio_identity -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use quickwit_common::uri::Uri;
use quickwit_config::{
    ChecksumAlgorithm, S3StorageConfig, StorageBackendFlavor, StorageConfig, StorageConfigs,
};
use quickwit_storage::StorageResolver;
use rustie_cachey::{CacheyConfig, global_stats, storage_resolver};
use url::Url;

fn env_enabled() -> bool {
    matches!(
        std::env::var("RUSTIE_CACHEY_TEST").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

fn minio_s3_config() -> S3StorageConfig {
    let endpoint =
        std::env::var("MINIO_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:9010".into());
    let access = std::env::var("MINIO_ACCESS_KEY")
        .or_else(|_| std::env::var("AWS_ACCESS_KEY_ID"))
        .unwrap_or_else(|_| "minioadmin".into());
    let secret = std::env::var("MINIO_SECRET_KEY")
        .or_else(|_| std::env::var("AWS_SECRET_ACCESS_KEY"))
        .unwrap_or_else(|_| "minioadmin".into());
    let mut configs = StorageConfigs::new(vec![StorageConfig::S3(S3StorageConfig {
        flavor: Some(StorageBackendFlavor::MinIO),
        endpoint: Some(endpoint),
        access_key_id: Some(access),
        secret_access_key: Some(secret),
        checksum_algorithm: ChecksumAlgorithm::Md5,
        ..Default::default()
    })]);
    configs.apply_flavors();
    configs.find_s3().cloned().expect("s3 config")
}

fn index_uri() -> Uri {
    let bucket = std::env::var("MINIO_BUCKET").unwrap_or_else(|_| "rustie-dev".into());
    let prefix = std::env::var("MINIO_PREFIX").unwrap_or_else(|_| "quick-rustie".into());
    let raw = if prefix.is_empty() {
        format!("s3://{bucket}/")
    } else {
        format!("s3://{bucket}/{}/", prefix.trim_matches('/'))
    };
    Uri::from_str(&raw).expect("index uri")
}

#[tokio::test]
#[ignore = "requires MinIO + Cachey; set RUSTIE_CACHEY_TEST=1"]
async fn cachey_ranges_match_direct_s3() {
    if !env_enabled() {
        eprintln!("skip: RUSTIE_CACHEY_TEST not set");
        return;
    }

    let s3 = minio_s3_config();
    let base =
        StorageResolver::configured(&StorageConfigs::new(vec![StorageConfig::S3(s3.clone())]));
    let index_uri = index_uri();
    let direct = base.resolve(&index_uri).await.expect("resolve s3");

    let split_path = PathBuf::from(std::env::var("RUSTIE_CACHEY_TEST_SPLIT").unwrap_or_else(
        |_| {
            "indexes/pubmed-slots".into() // placeholder; override with a real relative key
        },
    ));

    let len = match direct.file_num_bytes(&split_path).await {
        Ok(n) if n > 0 => n as usize,
        Ok(_) => {
            eprintln!("skip: empty split {}", split_path.display());
            return;
        }
        Err(err) => {
            eprintln!(
                "skip: cannot stat {} ({err:#}). Set RUSTIE_CACHEY_TEST_SPLIT to a path relative to {index_uri}",
                split_path.display()
            );
            return;
        }
    };

    let cachey_url =
        std::env::var("RUSTIE_CACHEY_URL").unwrap_or_else(|_| "http://127.0.0.1:9020".into());
    let cfg = CacheyConfig {
        url: Url::parse(&cachey_url).unwrap(),
        c0_config: None,
        fallback: false,
    };

    // Wait briefly for Cachey /stats.
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    let stats_url = format!("{}/stats", cachey_url.trim_end_matches('/'));
    if client.get(&stats_url).send().await.is_err() {
        eprintln!("skip: Cachey not reachable at {stats_url}");
        return;
    }

    let wrapped = storage_resolver(base, s3, cfg);
    let cachey = wrapped.resolve(&index_uri).await.expect("resolve cachey");

    let ranges: Vec<std::ops::Range<usize>> = {
        let mut v = vec![0..1024.min(len), 0..1.min(len), (len - 1)..len];
        let page = 16 * 1024 * 1024;
        if len > page + 512 {
            v.push((page - 512)..(page + 512));
        }
        if len > 8192 {
            v.push((len / 2)..(len / 2 + 4096).min(len));
        }
        v
    };

    for range in ranges {
        let a = direct
            .get_slice(&split_path, range.clone())
            .await
            .unwrap_or_else(|e| panic!("direct {range:?}: {e}"));
        let before = global_stats().snapshot();
        let b = cachey
            .get_slice(&split_path, range.clone())
            .await
            .unwrap_or_else(|e| panic!("cachey {range:?}: {e}"));
        assert_eq!(
            a.as_ref(),
            b.as_ref(),
            "mismatch on {} {range:?}",
            split_path.display()
        );
        let _ = cachey.get_slice(&split_path, range.clone()).await.unwrap();
        let delta = global_stats().snapshot().since(&before);
        assert!(
            delta.requests >= 1,
            "expected cachey requests for {range:?}"
        );
    }
}
