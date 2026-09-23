//! [`Searcher`]: a RustIE pattern as one Quickwit search.
//!
//! The pattern travels as a `rustie` extension query and is matched exactly inside each split
//! (see `rustie-leaf`): only matching sentences are counted, ranked and fetched. This process
//! then renders the matched spans of the returned page from the stored sentences.
//!
//! Two backends:
//! - **Embedded** (default): in-process `SearchServiceImpl` + one-node pool (laptop / MinIO smoke).
//! - **Gateway**: gRPC `SearchServiceClient::root_search` to a remote `rustie-node` (same RPC the
//!   embedded path uses locally). Keeps a direct `open_metastore` client for [`Searcher::summary`]
//!   only. The dialed searcher endpoint is a known SPOF for root entry (leaf fan-out behind it
//!   is resilient).

use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytesize::ByteSize;
use quickwit_config::{CacheConfig, SearcherConfig, SplitCacheLimits};
use quickwit_metastore::{
    IndexMetadataResponseExt, ListSplitsRequestExt, MetastoreServiceStreamSplitsExt, SplitState,
};
use quickwit_proto::metastore::{
    IndexMetadataRequest, ListSplitsRequest, MetastoreService, MetastoreServiceClient,
};
use quickwit_proto::search::{CountHits, PartialHit, ReportSplit, SearchRequest, SearchResponse};
use quickwit_search::{
    ClusterClient, SearchJobPlacer, SearchServiceClient, SearchServiceImpl, SearcherContext,
    SearcherPool, create_search_client_from_grpc_addr, root_search,
};
use quickwit_storage::{SearchSplitCache, StorageResolver};
use rustie_compiler::{CompiledQuery, QueryCompiler};
use rustie_indexer::{IndexSummary, IndexerOptions, MinioConfig, index_summary, open_metastore};
use tokio::sync::RwLock;
use tracing::warn;

use crate::cursor;
use crate::error::{Result, SearchError};
use crate::eval::{Evaluator, StoredSentence};
use crate::model::{QueryKind, SearchQuery, SearchResults, Timing};

/// Where the index lives and the resource limits applied to every query.
#[derive(Debug, Clone)]
pub struct SearcherOptions {
    pub index_id: String,
    pub metastore_uri: String,
    /// Hard cap on `limit` (default 1 000).
    pub max_limit: usize,
    /// Wall-clock budget per query.
    pub timeout: Duration,

    /// When set, search goes to this remote Quickwit searcher gRPC addr via
    /// [`SearchServiceClient::root_search`] (gateway mode). When `None`, use the embedded
    /// in-process stack. Phase 1: a single endpoint (root-entry SPOF; leaf fan-out is separate).
    pub searcher_endpoint: Option<SocketAddr>,
    /// Max gRPC message size for the gateway client. Default 20 MiB (Quickwit `GrpcConfig`).
    pub grpc_max_message_size: ByteSize,

    /// Byte budget (MB) of Quickwit's in-memory fast-field cache. Default 1024 (matches
    /// `SearcherConfig::default()`). Ignored in gateway mode.
    pub fast_field_cache_mb: u64,
    /// Byte budget (MB) of the split-footer (file-bundle metadata + hotcache) cache. Default 500.
    /// Ignored in gateway mode.
    pub split_footer_cache_mb: u64,
    /// Byte budget (MB) of the per-split partial-result cache (memoizes whole leaf responses,
    /// e.g. across identical pages of the same query). Default 64. Ignored in gateway mode.
    pub partial_request_cache_mb: u64,
    /// Byte budget (MB) of the predicate cache (see `rustie_leaf::cache_wrapped_query_ast`).
    /// Default 256. Ignored in gateway mode (lives on the remote node).
    pub predicate_cache_mb: u64,

    /// Root directory for the on-disk split cache (whole downloaded split files kept between
    /// queries and across restarts; not cleaned up automatically). Required (must be `Some`)
    /// when `split_cache_limits` is `Some`; ignored otherwise. Embedded mode only.
    pub split_cache_dir: Option<PathBuf>,
    /// Limits for the on-disk split cache. `None` (default) disables it entirely: every query
    /// re-fetches split data from object storage, subject only to the in-memory caches above.
    /// Embedded mode only.
    pub split_cache_limits: Option<SplitCacheLimits>,
}

impl SearcherOptions {
    pub fn for_bucket(bucket: &str) -> Self {
        let indexer = IndexerOptions::for_bucket(bucket);
        Self {
            index_id: indexer.index_id,
            metastore_uri: indexer.metastore_uri,
            max_limit: 1_000,
            timeout: Duration::from_secs(30),
            searcher_endpoint: None,
            grpc_max_message_size: ByteSize::mib(20),
            fast_field_cache_mb: 1024,
            split_footer_cache_mb: 500,
            partial_request_cache_mb: 64,
            predicate_cache_mb: 256,
            split_cache_dir: None,
            split_cache_limits: None,
        }
    }

    /// Enables the on-disk split cache: whole `.split` files are kept under `dir` between
    /// queries and across restarts, up to `max_mb` megabytes and `max_splits` splits.
    /// Only applies to embedded mode.
    pub fn with_split_cache(mut self, dir: PathBuf, max_mb: u64, max_splits: u32) -> Result<Self> {
        self.split_cache_dir = Some(dir);
        self.split_cache_limits = Some(SplitCacheLimits {
            max_num_bytes: ByteSize::mb(max_mb),
            max_num_splits: NonZeroU32::new(max_splits)
                .ok_or_else(|| SearchError::InvalidQuery("split_cache_max_splits must be > 0".into()))?,
            // Fixed at the fork's own defaults (its per-field default functions are private):
            // one download at a time, up to 100 file descriptors held open.
            num_concurrent_downloads: NonZeroU32::new(1).unwrap(),
            max_file_descriptors: NonZeroU32::new(100).unwrap(),
        });
        Ok(self)
    }
}

const DEFAULT_LIMIT: usize = 20;
const MAX_QUERY_CHARS: usize = 10_000;

/// Read-only handle on the index; cheap to share behind an `Arc`.
pub struct Searcher {
    options: SearcherOptions,
    minio: MinioConfig,
    compiler: QueryCompiler,
    backend: SearchBackend,
}

enum SearchBackend {
    /// In-process leaf/root (laptop default).
    Embedded {
        context: Arc<SearcherContext>,
        stack: RwLock<EmbeddedStack>,
    },
    /// Remote `rustie-node` via gRPC `root_search`; local metastore for `summary` only.
    Gateway {
        client: Box<SearchServiceClient>,
        metastore: RwLock<MetastoreServiceClient>,
    },
}

/// Everything derived from one metastore view (embedded mode).
struct EmbeddedStack {
    metastore: MetastoreServiceClient,
    cluster_client: ClusterClient,
}

impl EmbeddedStack {
    fn new(
        context: &Arc<SearcherContext>,
        metastore: MetastoreServiceClient,
        storage_resolver: StorageResolver,
    ) -> Self {
        // A one-node "cluster" whose only searcher is this process.
        let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 7280);
        let pool = SearcherPool::default();
        let cluster_client = ClusterClient::new(SearchJobPlacer::new(pool.clone()));
        let service = Arc::new(SearchServiceImpl::new(
            metastore.clone(),
            storage_resolver,
            cluster_client.clone(),
            Arc::clone(context),
        ));
        pool.insert(addr, SearchServiceClient::from_service(service, addr));
        Self {
            metastore,
            cluster_client,
        }
    }
}

/// Reports every published split of `index_id` to `split_cache`, so its background downloader
/// can start fetching them.
///
/// Quickwit's own on-disk split cache expects indexers to gossip newly published splits to
/// searchers over gRPC (`SearchJobPlacer`/`report_splits`, see the fork's
/// `quickwit-search/src/search_job_placer.rs`); `rustie-search`'s single-process, MinIO-only
/// searcher has no such channel, so nothing ever calls `SearchSplitCache::report_splits` and the
/// cache silently never downloads anything unless something does this explicitly. This is that
/// something: it lists the index's published splits from the metastore and reports each one.
///
/// Best-effort: an index that doesn't exist yet, or a transient metastore error, only means the
/// split cache stays empty until the next successful call — this must never fail `connect` or
/// `refresh`, matching how the rest of this searcher tolerates the index not existing yet.
///
/// Kept for **embedded** mode. Clustered `rustie-node` searchers get live `report_splits` from
/// an indexer-role node (phase 2); this stand-in is not used in gateway mode.
async fn report_splits_to_cache(
    metastore: &MetastoreServiceClient,
    index_id: &str,
    split_cache: &SearchSplitCache,
) {
    let result: anyhow::Result<()> = async {
        let index_metadata = metastore
            .clone()
            .index_metadata(IndexMetadataRequest::for_index_id(index_id.to_string()))
            .await?
            .deserialize_index_metadata()?;
        let storage_uri = index_metadata.index_uri().to_string();
        let request = ListSplitsRequest::try_from_index_uid(index_metadata.index_uid)?;
        let splits = metastore
            .clone()
            .list_splits(request)
            .await?
            .collect_splits()
            .await?;
        let report_splits = splits
            .into_iter()
            .filter(|split| split.split_state == SplitState::Published)
            .map(|split| ReportSplit {
                split_id: split.split_metadata.split_id.to_string(),
                storage_uri: storage_uri.clone(),
            })
            .collect();
        split_cache.report_splits(report_splits);
        Ok(())
    }
    .await;
    if let Err(err) = result {
        warn!(%err, index_id, "could not report splits to the on-disk split cache");
    }
}

impl Searcher {
    pub async fn connect(minio: MinioConfig, options: SearcherOptions) -> Result<Self> {
        if options.max_limit == 0 {
            return Err(SearchError::InvalidQuery("max_limit must be > 0".into()));
        }
        if options.split_cache_limits.is_some() && options.split_cache_dir.is_none() {
            return Err(SearchError::InvalidQuery(
                "split_cache_limits requires split_cache_dir".into(),
            ));
        }

        // register() before metastore use: summary() deserializes mappings that name
        // rustie_tokens / rustie_edges. Leaf matching still runs on rustie-node in gateway mode.
        rustie_leaf::register();

        // Always open metastore: embedded needs it for search; gateway needs it for summary().
        let (storage_resolver, metastore) =
            open_metastore(&minio, &options.metastore_uri).await?;

        if let Some(endpoint) = options.searcher_endpoint {
            // Gateway: no local SearchServiceImpl / SearcherContext / ClusterClient.
            let client =
                create_search_client_from_grpc_addr(endpoint, options.grpc_max_message_size);
            return Ok(Self {
                options,
                minio,
                compiler: QueryCompiler::new(),
                backend: SearchBackend::Gateway {
                    client: Box::new(client),
                    metastore: RwLock::new(metastore),
                },
            });
        }

        let split_cache_opt = match (&options.split_cache_dir, &options.split_cache_limits) {
            (Some(dir), Some(limits)) => Some(
                SearchSplitCache::with_root_path(dir.clone(), storage_resolver.clone(), *limits)
                    .map_err(|err| SearchError::Backend(format!("split cache: {err}")))?,
            ),
            _ => None,
        };
        // `SearcherConfig` has private fields, so struct-update syntax isn't usable across the
        // crate boundary; start from its default and mutate the fields we override.
        let mut searcher_config = SearcherConfig::default();
        searcher_config.fast_field_cache =
            CacheConfig::default_with_capacity(ByteSize::mb(options.fast_field_cache_mb));
        searcher_config.split_footer_cache =
            CacheConfig::default_with_capacity(ByteSize::mb(options.split_footer_cache_mb));
        searcher_config.partial_request_cache =
            CacheConfig::default_with_capacity(ByteSize::mb(options.partial_request_cache_mb));
        searcher_config.predicate_cache =
            CacheConfig::default_with_capacity(ByteSize::mb(options.predicate_cache_mb));
        searcher_config.split_cache = options.split_cache_limits;
        if let Some(split_cache) = &split_cache_opt {
            report_splits_to_cache(&metastore, &options.index_id, split_cache).await;
        }
        let context = Arc::new(SearcherContext::new_without_invoker(
            searcher_config,
            split_cache_opt,
        ));
        Ok(Self {
            options,
            minio,
            compiler: QueryCompiler::new(),
            backend: SearchBackend::Embedded {
                stack: RwLock::new(EmbeddedStack::new(&context, metastore, storage_resolver)),
                context,
            },
        })
    }

    pub fn options(&self) -> &SearcherOptions {
        &self.options
    }

    /// Whether this searcher dials a remote `rustie-node` for search.
    pub fn is_gateway(&self) -> bool {
        matches!(self.backend, SearchBackend::Gateway { .. })
    }

    /// Re-open the metastore so splits published since the last (re)load become visible.
    /// The file-backed metastore does not poll on its own. Embedded caches are kept.
    /// In gateway mode, only the local summary metastore is refreshed (search hits the remote).
    pub async fn refresh(&self) -> Result<()> {
        match &self.backend {
            SearchBackend::Embedded { context, stack } => {
                let (storage_resolver, metastore) =
                    open_metastore(&self.minio, &self.options.metastore_uri).await?;
                if let Some(split_cache) = &context.split_cache_opt {
                    report_splits_to_cache(&metastore, &self.options.index_id, split_cache).await;
                }
                *stack.write().await = EmbeddedStack::new(context, metastore, storage_resolver);
            }
            SearchBackend::Gateway { metastore, .. } => {
                let (_storage_resolver, fresh) =
                    open_metastore(&self.minio, &self.options.metastore_uri).await?;
                *metastore.write().await = fresh;
            }
        }
        Ok(())
    }

    pub async fn summary(&self) -> Result<IndexSummary> {
        let metastore = match &self.backend {
            SearchBackend::Embedded { stack, .. } => stack.read().await.metastore.clone(),
            SearchBackend::Gateway { metastore, .. } => metastore.read().await.clone(),
        };
        Ok(index_summary(&metastore, &self.options.index_id).await?)
    }

    /// Run `query`, bounded by the configured timeout.
    pub async fn search(&self, query: SearchQuery) -> Result<SearchResults> {
        let timeout = self.options.timeout;
        tokio::time::timeout(timeout, self.search_inner(query))
            .await
            .map_err(|_| SearchError::Timeout(timeout))?
    }

    async fn search_inner(&self, query: SearchQuery) -> Result<SearchResults> {
        let started = Instant::now();
        if query.query.trim().is_empty() || query.query.chars().count() > MAX_QUERY_CHARS {
            return Err(SearchError::InvalidQuery(format!(
                "query must be 1..={MAX_QUERY_CHARS} characters"
            )));
        }
        let limit = query
            .limit
            .unwrap_or(DEFAULT_LIMIT)
            .min(self.options.max_limit);
        if limit == 0 {
            return Err(SearchError::InvalidQuery("limit must be > 0".into()));
        }
        let after: Option<PartialHit> = query
            .cursor
            .as_deref()
            .map(|token| cursor::decode(&query.query, token))
            .transpose()?;

        // Compiled here too: a bad pattern is the caller's error (400), reported before any
        // split is touched, and the evaluator renders the spans of the returned hits.
        let compiled = self.compiler.compile(&query.query)?;
        let evaluator = Evaluator::new(&compiled)?;
        let kind = match compiled {
            CompiledQuery::Surface(_) => QueryKind::Surface,
            CompiledQuery::Graph(_) => QueryKind::Graph,
        };
        let query_ast = rustie_leaf::cache_wrapped_query_ast(&query.query);

        let request = SearchRequest {
            index_id_patterns: vec![self.options.index_id.clone()],
            query_ast: serde_json::to_string(&query_ast)
                .map_err(|err| SearchError::Backend(err.to_string()))?,
            max_hits: limit as u64,
            search_after: after,
            // Exact totals make every split evaluate every candidate; by default Quickwit may
            // stop once the page is filled.
            count_hits: if query.count {
                CountHits::CountAll
            } else {
                CountHits::Underestimate
            } as i32,
            ..Default::default()
        };
        let backend_started = Instant::now();
        let response = self.root_search(request).await?;
        let backend_us = backend_started.elapsed().as_micros() as u64;
        if !response.errors.is_empty() || !response.failed_splits.is_empty() {
            return Err(SearchError::Backend(format!(
                "{} split(s) failed: {}",
                response.failed_splits.len(),
                response.errors.join("; ")
            )));
        }

        let render_started = Instant::now();
        let mut hits = Vec::with_capacity(response.hits.len());
        for hit in &response.hits {
            let Some(sentence) = StoredSentence::from_hit_json(&hit.json) else {
                warn!("skipping hit with unexpected stored shape");
                continue;
            };
            match evaluator.evaluate(&sentence) {
                Some(sentence_hit) => hits.push(sentence_hit),
                // The split matched it, so this means the two evaluators disagree.
                None => warn!(sentence_id = %sentence.sentence_id, "hit without renderable match"),
            }
        }
        let render_us = render_started.elapsed().as_micros() as u64;

        let exhausted = response.hits.len() < limit;
        let next_cursor = match (response.hits.last(), exhausted) {
            (Some(last), false) => last
                .partial_hit
                .as_ref()
                .map(|partial| cursor::encode(&query.query, partial)),
            _ => None,
        };
        Ok(SearchResults {
            query: query.query,
            kind,
            hits,
            total_hits: response.num_hits,
            total_is_exact: query.count,
            exhausted,
            next_cursor,
            took_ms: started.elapsed().as_millis() as u64,
            timing: Timing {
                backend_us,
                render_us,
            },
        })
    }

    async fn root_search(&self, request: SearchRequest) -> Result<SearchResponse> {
        match &self.backend {
            SearchBackend::Embedded { context, stack } => {
                let (metastore, cluster_client) = {
                    let stack = stack.read().await;
                    (stack.metastore.clone(), stack.cluster_client.clone())
                };
                root_search(context, request, &metastore, &cluster_client)
                    .await
                    .map_err(|err| SearchError::Backend(err.to_string()))
            }
            SearchBackend::Gateway { client, .. } => {
                let mut client = client.as_ref().clone();
                client
                    .root_search(request)
                    .await
                    .map_err(|err| SearchError::Backend(err.to_string()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn for_bucket_defaults_to_embedded() {
        let opts = SearcherOptions::for_bucket("rustie-dev");
        assert!(opts.searcher_endpoint.is_none());
        assert_eq!(opts.grpc_max_message_size, ByteSize::mib(20));
    }

    #[test]
    fn gateway_endpoint_is_opt_in() {
        let mut opts = SearcherOptions::for_bucket("rustie-dev");
        opts.searcher_endpoint = Some("127.0.0.1:7281".parse().unwrap());
        assert!(opts.searcher_endpoint.is_some());
    }
}
