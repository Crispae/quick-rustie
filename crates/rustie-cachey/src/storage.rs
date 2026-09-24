//! [`CacheyStorage`]: routes `.split` `get_slice` through Cachey.

use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use async_trait::async_trait;
use quickwit_common::uri::Uri;
use quickwit_storage::{
    BulkDeleteError, OwnedBytes, PutPayload, SendableAsync, Storage, StorageErrorKind,
    StorageResult,
};
use reqwest::Client;
use tokio::io::AsyncRead;
use tracing::error;
use url::Url;

use crate::client::{FetchOutcome, build_c0_config, fetch_range, object_key};
use crate::{Breaker, BreakerKind, CacheyConfig, CacheyStats, is_split};

pub struct CacheyStorage {
    inner: Arc<dyn Storage>,
    bucket: String,
    prefix: PathBuf,
    base_url: Url,
    c0_config: Option<String>,
    fallback: bool,
    client: Client,
    stats: &'static CacheyStats,
    breaker: Arc<Breaker>,
}

impl fmt::Debug for CacheyStorage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CacheyStorage")
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("base_url", &self.base_url)
            .field("inner", &self.inner)
            .finish()
    }
}

impl CacheyStorage {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        inner: Arc<dyn Storage>,
        bucket: String,
        prefix: PathBuf,
        cfg: CacheyConfig,
        force_path_style: bool,
        client: Client,
        stats: &'static CacheyStats,
        breaker: Arc<Breaker>,
    ) -> Self {
        let c0_config = build_c0_config(force_path_style, cfg.c0_config.as_deref());
        Self {
            inner,
            bucket,
            prefix,
            base_url: cfg.url,
            c0_config,
            fallback: cfg.fallback,
            client,
            stats,
            breaker,
        }
    }

    pub fn inner(&self) -> &Arc<dyn Storage> {
        &self.inner
    }

    pub fn bucket(&self) -> &str {
        &self.bucket
    }

    pub fn object_key_for(&self, path: &Path) -> String {
        object_key(&self.prefix, path)
    }

    async fn get_slice_via_cachey(
        &self,
        path: &Path,
        range: Range<usize>,
    ) -> StorageResult<OwnedBytes> {
        if range.start >= range.end {
            return Ok(OwnedBytes::empty());
        }
        if self.breaker.is_open() {
            return self.fallback_or_err(path, range, "breaker open").await;
        }

        let key = self.object_key_for(path);
        let outcome = fetch_range(
            &self.client,
            &self.base_url,
            &self.bucket,
            &key,
            range.clone(),
            self.c0_config.as_deref(),
            self.stats,
        )
        .await;

        match outcome {
            FetchOutcome::Ok { body, .. } => {
                self.breaker.record_success();
                Ok(OwnedBytes::new(body.to_vec()))
            }
            FetchOutcome::NotFound => {
                let exists = match self.inner.exists(path).await {
                    Ok(exists) => exists,
                    // S3 itself is failing, so we can't tell a real 404 from a misconfigured
                    // Cachey; don't report a permanent NotFound. Let the direct read surface
                    // S3's own error (or succeed).
                    Err(err) => {
                        return self
                            .fallback_or_err(
                                path,
                                range,
                                &format!("cachey 404, s3 exists check failed: {err}"),
                            )
                            .await;
                    }
                };
                if exists {
                    self.stats.misconfig.fetch_add(1, Ordering::Relaxed);
                    error!(
                        target: "rustie_cachey",
                        kind = %self.bucket,
                        key = %key,
                        base = %self.base_url,
                        "cachey returned 404 but object exists on S3 (misconfig); falling back"
                    );
                    self.breaker.trip(BreakerKind::Misconfig);
                    return self
                        .fallback_or_err(path, range, "cachey 404 misconfig")
                        .await;
                }
                // Cachey answered correctly; if this was the half-open probe, close the breaker.
                self.breaker.record_success();
                self.stats.not_found.fetch_add(1, Ordering::Relaxed);
                Err(StorageErrorKind::NotFound
                    .with_error(anyhow::anyhow!("object not found via cachey: {key}")))
            }
            FetchOutcome::RangeNotSatisfiable => {
                self.breaker.record_success();
                self.stats.errors.fetch_add(1, Ordering::Relaxed);
                Err(StorageErrorKind::Io.with_error(anyhow::anyhow!(
                    "cachey 416 range not satisfiable for {key} bytes={}..{}",
                    range.start,
                    range.end
                )))
            }
            FetchOutcome::Shed => {
                self.stats.shed_fallbacks.fetch_add(1, Ordering::Relaxed);
                self.breaker.trip(BreakerKind::Shed);
                self.fallback_or_err(path, range, "cachey 503").await
            }
            FetchOutcome::Retryable(err) => {
                self.stats
                    .transport_fallbacks
                    .fetch_add(1, Ordering::Relaxed);
                self.breaker.trip(BreakerKind::Transport);
                self.fallback_or_err(path, range, &format!("cachey error: {err}"))
                    .await
            }
        }
    }

    async fn fallback_or_err(
        &self,
        path: &Path,
        range: Range<usize>,
        reason: &str,
    ) -> StorageResult<OwnedBytes> {
        if !self.fallback {
            return Err(StorageErrorKind::Service.with_error(anyhow::anyhow!(
                "cachey failed ({reason}); fallback disabled"
            )));
        }
        self.inner.get_slice(path, range).await
    }
}

#[async_trait]
impl Storage for CacheyStorage {
    async fn check_connectivity(&self) -> anyhow::Result<()> {
        self.inner.check_connectivity().await
    }

    async fn put(&self, path: &Path, payload: Box<dyn PutPayload>) -> StorageResult<()> {
        self.inner.put(path, payload).await
    }

    fn copy_to<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        path: &'life1 Path,
        output: &'life2 mut dyn SendableAsync,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = StorageResult<()>> + Send + 'async_trait>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        self.inner.copy_to(path, output)
    }

    async fn copy_to_file(&self, path: &Path, output_path: &Path) -> StorageResult<u64> {
        self.inner.copy_to_file(path, output_path).await
    }

    async fn get_slice(&self, path: &Path, range: Range<usize>) -> StorageResult<OwnedBytes> {
        if is_split(path) {
            self.get_slice_via_cachey(path, range).await
        } else {
            self.inner.get_slice(path, range).await
        }
    }

    async fn get_slice_stream(
        &self,
        path: &Path,
        range: Range<usize>,
    ) -> StorageResult<Box<dyn AsyncRead + Send + Unpin>> {
        if is_split(path) {
            // Buffered get_slice — not real streaming.
            let bytes = self.get_slice_via_cachey(path, range).await?;
            Ok(Box::new(std::io::Cursor::new(bytes)))
        } else {
            self.inner.get_slice_stream(path, range).await
        }
    }

    async fn get_all(&self, path: &Path) -> StorageResult<OwnedBytes> {
        self.inner.get_all(path).await
    }

    async fn delete(&self, path: &Path) -> StorageResult<()> {
        self.inner.delete(path).await
    }

    async fn bulk_delete<'a>(&self, paths: &[&'a Path]) -> Result<(), BulkDeleteError> {
        self.inner.bulk_delete(paths).await
    }

    async fn file_num_bytes(&self, path: &Path) -> StorageResult<u64> {
        self.inner.file_num_bytes(path).await
    }

    async fn exists(&self, path: &Path) -> StorageResult<bool> {
        self.inner.exists(path).await
    }

    fn uri(&self) -> &Uri {
        self.inner.uri()
    }
}
