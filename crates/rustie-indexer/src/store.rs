//! Storage / metastore access shared by the indexer and read-only consumers (search).

use std::str::FromStr;

use anyhow::Context;
use quickwit_common::uri::Uri;
use quickwit_config::MetastoreConfigs;
use quickwit_metastore::{
    IndexMetadataResponseExt, ListSplitsRequestExt, MetastoreResolver,
    MetastoreServiceStreamSplitsExt, SplitState,
};
use quickwit_proto::metastore::{
    IndexMetadataRequest, ListSplitsRequest, MetastoreService, MetastoreServiceClient,
};
use quickwit_storage::StorageResolver;

use crate::error::{IndexerError, Result};
use crate::minio::{MinioConfig, minio_storage_resolver};

/// Published split / document counts as recorded in the metastore.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexSummary {
    pub index_uid: String,
    pub num_published_splits: usize,
    pub num_docs: u64,
    pub uncompressed_bytes: u64,
}

/// Resolve storage for `metastore_uri` on MinIO, verify connectivity, and open the
/// file-backed metastore there.
///
/// The file-backed metastore has no polling in this Quickwit version: a client only sees
/// what was published before it (re)loaded an index, so long-lived readers should call this
/// again to pick up newly indexed splits.
pub async fn open_metastore(
    minio: &MinioConfig,
    metastore_uri: &str,
) -> Result<(StorageResolver, MetastoreServiceClient)> {
    let uri = Uri::from_str(metastore_uri).map_err(|err| {
        IndexerError::InvalidConfig(format!("metastore_uri `{metastore_uri}`: {err}"))
    })?;
    let storage_resolver = minio_storage_resolver(minio);
    storage_resolver
        .resolve(&uri)
        .await
        .with_context(|| format!("cannot resolve metastore storage {uri}"))?
        .check_connectivity()
        .await
        .with_context(|| {
            format!(
                "cannot reach MinIO at {} (bucket `{}`); is it running and does the bucket exist?",
                minio.endpoint, minio.bucket
            )
        })?;
    let metastore =
        MetastoreResolver::configured(storage_resolver.clone(), &MetastoreConfigs::default())
            .resolve(&uri)
            .await
            .with_context(|| format!("cannot open metastore at {uri}"))?;
    Ok((storage_resolver, metastore))
}

/// Published split / document counts for `index_id`.
pub async fn index_summary(
    metastore: &MetastoreServiceClient,
    index_id: &str,
) -> Result<IndexSummary> {
    let metastore = metastore.clone();
    let index_uid = metastore
        .index_metadata(IndexMetadataRequest::for_index_id(index_id.to_string()))
        .await
        .map_err(anyhow::Error::from)?
        .deserialize_index_metadata()
        .map_err(anyhow::Error::from)?
        .index_uid;
    let request =
        ListSplitsRequest::try_from_index_uid(index_uid.clone()).map_err(anyhow::Error::from)?;
    let splits = metastore
        .list_splits(request)
        .await
        .map_err(anyhow::Error::from)?
        .collect_splits()
        .await
        .map_err(anyhow::Error::from)?;
    let mut summary = IndexSummary {
        index_uid: index_uid.to_string(),
        num_published_splits: 0,
        num_docs: 0,
        uncompressed_bytes: 0,
    };
    for split in splits
        .iter()
        .filter(|split| split.split_state == SplitState::Published)
    {
        summary.num_published_splits += 1;
        summary.num_docs += split.split_metadata.num_docs as u64;
        summary.uncompressed_bytes += split.split_metadata.uncompressed_docs_size_in_bytes;
    }
    Ok(summary)
}
