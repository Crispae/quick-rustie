//! Storage factories that wrap or delegate to a base [`StorageResolver`].

use std::sync::Arc;

use async_trait::async_trait;
use quickwit_common::uri::Uri;
use quickwit_config::{S3StorageConfig, StorageBackend};
use quickwit_storage::{Storage, StorageFactory, StorageResolver, StorageResolverError};
use reqwest::Client;

use crate::client::split_s3_uri;
use crate::storage::CacheyStorage;
use crate::{Breaker, CacheyConfig, CacheyStats};

pub struct DelegatingFactory {
    backend: StorageBackend,
    base: StorageResolver,
}

impl DelegatingFactory {
    pub fn new(backend: StorageBackend, base: StorageResolver) -> Self {
        Self { backend, base }
    }
}

#[async_trait]
impl StorageFactory for DelegatingFactory {
    fn backend(&self) -> StorageBackend {
        self.backend
    }

    async fn resolve(&self, uri: &Uri) -> Result<Arc<dyn Storage>, StorageResolverError> {
        self.base.resolve(uri).await
    }
}

pub struct CacheyS3StorageFactory {
    base: StorageResolver,
    s3_config: S3StorageConfig,
    cfg: CacheyConfig,
    client: Client,
    stats: &'static CacheyStats,
    breaker: Arc<Breaker>,
}

impl CacheyS3StorageFactory {
    pub fn new(
        base: StorageResolver,
        s3_config: S3StorageConfig,
        cfg: CacheyConfig,
        client: Client,
        stats: &'static CacheyStats,
        breaker: Arc<Breaker>,
    ) -> Self {
        Self {
            base,
            s3_config,
            cfg,
            client,
            stats,
            breaker,
        }
    }
}

#[async_trait]
impl StorageFactory for CacheyS3StorageFactory {
    fn backend(&self) -> StorageBackend {
        StorageBackend::S3
    }

    async fn resolve(&self, uri: &Uri) -> Result<Arc<dyn Storage>, StorageResolverError> {
        let inner = self.base.resolve(uri).await?;
        let (bucket, prefix) = split_s3_uri(uri)
            .ok_or_else(|| StorageResolverError::InvalidUri(format!("not an s3 uri: {uri}")))?;
        Ok(Arc::new(CacheyStorage::new(
            inner,
            bucket,
            prefix,
            self.cfg.clone(),
            self.s3_config.force_path_style_access,
            self.client.clone(),
            self.stats,
            Arc::clone(&self.breaker),
        )))
    }
}
