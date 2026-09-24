//! Connect Quickwit storage to a local MinIO (S3-compatible) instance.
//!
//! Defaults match the local `rustie-minio` container
//! (`docker-compose.minio.yml` in this repo / RustIE):
//! - endpoint `http://127.0.0.1:9010`
//! - bucket `rustie-dev`
//! - credentials `minioadmin` / `minioadmin`
//!
//! Override via env: `MINIO_ENDPOINT`, `MINIO_BUCKET`, `MINIO_ACCESS_KEY`,
//! `MINIO_SECRET_KEY`, or `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY`.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use quickwit_common::uri::Uri;
use quickwit_config::{
    ChecksumAlgorithm, S3StorageConfig, StorageBackendFlavor, StorageConfig, StorageConfigs,
};
use quickwit_storage::{Storage, StorageResolver};

/// Connection settings for a MinIO / S3-compatible endpoint.
#[derive(Clone)]
pub struct MinioConfig {
    pub endpoint: String,
    pub bucket: String,
    pub access_key: String,
    pub secret_key: String,
    /// Object-key prefix inside the bucket (no leading slash), e.g. `indexes/demo`.
    pub prefix: String,
    /// Signing region. `None`: the MinIO flavor's default (`minio`). Set it for real
    /// S3-compatible providers (`MINIO_REGION` in [`MinioConfig::from_env`]).
    pub region: Option<String>,
}

// Manual impl so the secret key never lands in logs or panic messages.
impl std::fmt::Debug for MinioConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MinioConfig")
            .field("endpoint", &self.endpoint)
            .field("bucket", &self.bucket)
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .field("prefix", &self.prefix)
            .field("region", &self.region)
            .finish()
    }
}

impl Default for MinioConfig {
    fn default() -> Self {
        Self::from_env()
    }
}

impl MinioConfig {
    /// Build a config from `MINIO_*` / `AWS_*` env vars, falling back to the
    /// local `rustie-minio` defaults.
    pub fn from_env() -> Self {
        Self {
            endpoint: std::env::var("MINIO_ENDPOINT")
                .unwrap_or_else(|_| "http://127.0.0.1:9010".to_string()),
            bucket: std::env::var("MINIO_BUCKET").unwrap_or_else(|_| "rustie-dev".to_string()),
            access_key: std::env::var("MINIO_ACCESS_KEY")
                .or_else(|_| std::env::var("AWS_ACCESS_KEY_ID"))
                .unwrap_or_else(|_| "minioadmin".to_string()),
            secret_key: std::env::var("MINIO_SECRET_KEY")
                .or_else(|_| std::env::var("AWS_SECRET_ACCESS_KEY"))
                .unwrap_or_else(|_| "minioadmin".to_string()),
            prefix: std::env::var("MINIO_PREFIX").unwrap_or_else(|_| "quick-rustie".to_string()),
            region: std::env::var("MINIO_REGION").ok().filter(|r| !r.is_empty()),
        }
    }

    /// `s3://{bucket}/{prefix}/` URI used by Quickwit storage.
    pub fn index_uri(&self) -> Result<Uri> {
        let raw = if self.prefix.is_empty() {
            format!("s3://{}/", self.bucket)
        } else {
            format!("s3://{}/{}/", self.bucket, self.prefix.trim_matches('/'))
        };
        Uri::from_str(&raw).with_context(|| format!("invalid MinIO URI `{raw}`"))
    }

    /// Build Quickwit [`S3StorageConfig`] with the MinIO flavor applied.
    pub fn s3_storage_config(&self) -> S3StorageConfig {
        let mut configs = StorageConfigs::new(vec![StorageConfig::S3(S3StorageConfig {
            flavor: Some(StorageBackendFlavor::MinIO),
            endpoint: Some(self.endpoint.clone()),
            access_key_id: Some(self.access_key.clone()),
            secret_access_key: Some(self.secret_key.clone()),
            // MD5 works reliably with MinIO; CRC32C can fail on some S3-compat paths.
            checksum_algorithm: ChecksumAlgorithm::Md5,
            ..Default::default()
        })]);
        configs.apply_flavors();
        let mut s3 = configs.find_s3().cloned().expect("S3 config just inserted");
        // The MinIO flavor forces region `minio`; a real provider needs its own region.
        if let Some(region) = &self.region {
            s3.region = Some(region.clone());
        }
        s3
    }
}

/// Quickwit [`StorageResolver`] pointed at this MinIO config.
pub fn minio_storage_resolver(config: &MinioConfig) -> StorageResolver {
    let storage_configs = StorageConfigs::new(vec![StorageConfig::S3(config.s3_storage_config())]);
    StorageResolver::configured(&storage_configs)
}

/// Resolve an [`Storage`] handle for `s3://{bucket}/{prefix}/`.
pub async fn connect_minio(config: &MinioConfig) -> Result<Arc<dyn Storage>> {
    let uri = config.index_uri()?;
    let resolver = minio_storage_resolver(config);
    resolver.resolve(&uri).await.with_context(|| {
        format!(
            "failed to resolve MinIO storage at {} (bucket={}, endpoint={})",
            uri, config.bucket, config.endpoint
        )
    })
}

/// Write a small object under the config prefix, then read it back. Useful as a connectivity check.
pub async fn ping_minio(config: &MinioConfig) -> Result<Vec<u8>> {
    let storage = connect_minio(config).await?;
    let path = Path::new("connectivity-ping.txt");
    let payload = format!("quick-rustie ping @ {}\n", chrono_like_timestamp()).into_bytes();

    storage
        .put(path, Box::new(payload.clone()))
        .await
        .context("MinIO put failed")?;

    let bytes = storage
        .get_all(path)
        .await
        .context("MinIO get_all failed")?;

    Ok(bytes.to_vec())
}

fn chrono_like_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_uri_uses_local_rustie_minio() {
        // Clear overrides so the test is hermetic when runners inject env.
        // (We only assert shape of the constructed URI from an explicit config.)
        let cfg = MinioConfig {
            endpoint: "http://127.0.0.1:9010".into(),
            bucket: "rustie-dev".into(),
            access_key: "minioadmin".into(),
            secret_key: "s3cr3t-value".into(),
            prefix: "quick-rustie".into(),
            region: None,
        };
        assert!(
            !format!("{cfg:?}").contains("s3cr3t-value"),
            "secret must be redacted"
        );
        let uri = cfg.index_uri().unwrap();
        assert_eq!(uri.as_str(), "s3://rustie-dev/quick-rustie/");
        let s3 = cfg.s3_storage_config();
        assert_eq!(s3.endpoint.as_deref(), Some("http://127.0.0.1:9010"));
        assert_eq!(s3.region.as_deref(), Some("minio"));
        assert!(s3.force_path_style_access);
    }
}
