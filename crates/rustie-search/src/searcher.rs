//! [`Searcher`]: a RustIE pattern as one Quickwit search.
//!
//! The pattern travels as a `rustie` extension query and is matched exactly inside each split
//! (see `rustie-leaf`): only matching sentences are counted, ranked and fetched. This process
//! then renders the matched spans of the returned page from the stored sentences.

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use quickwit_config::SearcherConfig;
use quickwit_proto::metastore::MetastoreServiceClient;
use quickwit_proto::search::{CountHits, PartialHit, SearchRequest};
use quickwit_query::query_ast::{ExtensionQuery, QueryAst};
use quickwit_search::{
    ClusterClient, SearchJobPlacer, SearchServiceClient, SearchServiceImpl, SearcherContext,
    SearcherPool, root_search,
};
use quickwit_storage::StorageResolver;
use rustie_compiler::{CompiledQuery, QueryCompiler};
use rustie_indexer::{IndexSummary, IndexerOptions, MinioConfig, index_summary, open_metastore};
use rustie_leaf::PatternPayload;
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
}

impl SearcherOptions {
    pub fn for_bucket(bucket: &str) -> Self {
        let indexer = IndexerOptions::for_bucket(bucket);
        Self {
            index_id: indexer.index_id,
            metastore_uri: indexer.metastore_uri,
            max_limit: 1_000,
            timeout: Duration::from_secs(30),
        }
    }
}

const DEFAULT_LIMIT: usize = 20;
const MAX_QUERY_CHARS: usize = 10_000;

/// Read-only handle on the index; cheap to share behind an `Arc`.
pub struct Searcher {
    options: SearcherOptions,
    minio: MinioConfig,
    compiler: QueryCompiler,
    /// Quickwit's searcher caches (split footers, fast fields, partial results). Built once
    /// and kept for the life of the process, including across metastore refreshes: this is
    /// the counterpart of RustIE's hotcache. (`single_node_search` builds a fresh one per
    /// call, so every query would start cold.)
    context: Arc<SearcherContext>,
    stack: RwLock<Stack>,
}

/// Everything derived from one metastore view.
struct Stack {
    metastore: MetastoreServiceClient,
    cluster_client: ClusterClient,
}

impl Stack {
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

impl Searcher {
    pub async fn connect(minio: MinioConfig, options: SearcherOptions) -> Result<Self> {
        if options.max_limit == 0 {
            return Err(SearchError::InvalidQuery("max_limit must be > 0".into()));
        }
        // Splits are searched in this process: the leaf must know the `rustie` query.
        rustie_leaf::register();
        let context = Arc::new(SearcherContext::new_without_invoker(
            SearcherConfig::default(),
            None,
        ));
        let (storage_resolver, metastore) = open_metastore(&minio, &options.metastore_uri).await?;
        Ok(Self {
            options,
            minio,
            compiler: QueryCompiler::new(),
            stack: RwLock::new(Stack::new(&context, metastore, storage_resolver)),
            context,
        })
    }

    pub fn options(&self) -> &SearcherOptions {
        &self.options
    }

    /// Re-open the metastore so splits published since the last (re)load become visible.
    /// The file-backed metastore does not poll on its own. Caches are kept.
    pub async fn refresh(&self) -> Result<()> {
        let (storage_resolver, metastore) =
            open_metastore(&self.minio, &self.options.metastore_uri).await?;
        *self.stack.write().await = Stack::new(&self.context, metastore, storage_resolver);
        Ok(())
    }

    pub async fn summary(&self) -> Result<IndexSummary> {
        let metastore = self.stack.read().await.metastore.clone();
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
        let query_ast = QueryAst::Extension(ExtensionQuery {
            kind: rustie_leaf::QUERY_KIND.to_string(),
            payload: PatternPayload {
                pattern: query.query.clone(),
            }
            .to_json(),
        });

        let (metastore, cluster_client) = {
            let stack = self.stack.read().await;
            (stack.metastore.clone(), stack.cluster_client.clone())
        };
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
        let response = root_search(&self.context, request, &metastore, &cluster_client)
            .await
            .map_err(|err| SearchError::Backend(err.to_string()))?;
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
}
