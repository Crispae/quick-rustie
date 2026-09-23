//! Quickwit `IndexingService` wired against an arbitrary (MinIO-backed) storage +
//! metastore, without the `testsuite` feature.
//!
//! This follows `quickwit index ingest --local` (`quickwit-cli/src/tool.rs`): a private
//! single-node cluster (in-memory gossip transport, no sockets), one indexing service
//! actor, and one indexing + merge pipeline pair per source run.
//!
//! With `--cluster-config`, the indexer joins the real rustie-cluster as a gossip-only
//! member and subscribes [`SearchJobPlacer`] to the uploader's [`EventBroker`], so
//! `ReportSplitsRequest`s reach searchers' on-disk split caches.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use bytesize::ByteSize;
use futures::StreamExt;
use quickwit_actors::{ActorHandle, Mailbox, Universe};
use quickwit_cluster::{
    Cluster, ClusterChange, ClusterMember, FailureDetectorConfig, GenerationId,
    start_cluster_service,
};
use quickwit_common::pubsub::{EventBroker, EventSubscriptionHandle};
use quickwit_common::runtimes::{RuntimesConfig, initialize_runtimes};
use quickwit_common::tower::Change;
use quickwit_common::uri::Uri;
use quickwit_config::{ConfigFormat, IndexerConfig, NodeConfig, SourceConfig};
use quickwit_indexing::actors::{IndexingService, MergeSchedulerService};
use quickwit_indexing::models::{
    DetachIndexingPipeline, DetachMergePipeline, IndexingStatistics, SpawnPipeline,
};
use quickwit_indexing::{FinishPendingMergesAndShutdownPipeline, IndexingSplitCache};
use quickwit_ingest::IngesterPool;
use quickwit_proto::indexing::CpuCapacity;
use quickwit_proto::ingest::ingester::IngesterStatus;
use quickwit_proto::metastore::MetastoreServiceClient;
use quickwit_proto::search::ReportSplitsRequest;
use quickwit_proto::types::{NodeId, PipelineUid};
use quickwit_search::{SearchJobPlacer, SearcherPool, create_search_client_from_grpc_addr};
use quickwit_storage::{StorageResolver, load_file};
use tracing::{info, warn};

use crate::error::{IndexerError, Result};

/// Outcome of one [`PipelineRuntime::run_source`].
pub(crate) struct SourceRun {
    pub statistics: IndexingStatistics,
    /// Time to spawn the pipeline and detach its handles, before indexing starts.
    pub spawn: Duration,
    /// Time until the source was exhausted and its splits published.
    pub indexing: Duration,
    /// Time spent afterwards letting merges finish.
    pub merge_drain: Duration,
}

/// Owns the actor system for the lifetime of an [`Indexer`](crate::Indexer).
pub(crate) struct PipelineRuntime {
    universe: Universe,
    service: Mailbox<IndexingService>,
    service_handle: ActorHandle<IndexingService>,
    /// Kept alive so the process stays in the gossip mesh and the placer subscription
    /// remains registered (dropping the handle unsubscribes).
    _cluster: Cluster,
    _event_broker: EventBroker,
    _report_splits_subscription_handle: Option<EventSubscriptionHandle>,
    _search_job_placer: Option<SearchJobPlacer>,
}

impl PipelineRuntime {
    pub(crate) async fn start(
        node_id: NodeId,
        data_dir: &Path,
        metastore: MetastoreServiceClient,
        storage_resolver: StorageResolver,
        cluster_config: Option<&Path>,
    ) -> anyhow::Result<Self> {
        let runtimes_config = RuntimesConfig::default();
        initialize_runtimes(runtimes_config).context("failed to start actor runtimes")?;

        let (cluster, event_broker, report_handle, placer, service_node_id) =
            if let Some(config_path) = cluster_config {
                let (cluster, event_broker, handle, placer, node_id) =
                    start_cluster_mode(config_path).await?;
                (cluster, event_broker, Some(handle), Some(placer), node_id)
            } else {
                let cluster = create_local_cluster(node_id.clone()).await?;
                (cluster, EventBroker::default(), None, None, node_id)
            };

        let indexer_config = IndexerConfig::default();
        let split_cache =
            Arc::new(IndexingSplitCache::from_config(&indexer_config, data_dir).await?);

        let universe = Universe::new();
        let merge_scheduler: Mailbox<MergeSchedulerService> = universe.get_or_spawn_one();
        let service = IndexingService::new(
            service_node_id,
            data_dir.to_path_buf(),
            indexer_config,
            runtimes_config.num_threads_blocking,
            cluster.clone(),
            metastore,
            None, // no ingest API queue: documents come from the Vec source only
            Some(merge_scheduler),
            IngesterPool::default(),
            storage_resolver,
            event_broker.clone(),
            split_cache,
        )
        .await?;
        let (service, service_handle) = universe.spawn_builder().spawn(service);
        Ok(Self {
            universe,
            service,
            service_handle,
            _cluster: cluster,
            _event_broker: event_broker,
            _report_splits_subscription_handle: report_handle,
            _search_job_placer: placer,
        })
    }

    /// Spawn an indexing pipeline for `source_config`, wait for the source to be
    /// exhausted and its splits published, then let in-flight merges finish.
    pub(crate) async fn run_source(
        &self,
        index_id: &str,
        source_config: SourceConfig,
    ) -> Result<SourceRun> {
        let spawn_started = Instant::now();
        let pipeline_id = self
            .service
            .ask_for_res(SpawnPipeline {
                index_id: index_id.to_string(),
                source_config,
                pipeline_uid: PipelineUid::random(),
            })
            .await
            .map_err(|err| IndexerError::Pipeline(format!("failed to spawn pipeline: {err}")))?;
        let merge_handle = self
            .service
            .ask_for_res(DetachMergePipeline {
                pipeline_id: pipeline_id.merge_pipeline_id(),
            })
            .await
            .map_err(|err| IndexerError::Pipeline(format!("failed to detach merge: {err}")))?;
        let indexing_handle = self
            .service
            .ask_for_res(DetachIndexingPipeline { pipeline_id })
            .await
            .map_err(|err| IndexerError::Pipeline(format!("failed to detach pipeline: {err}")))?;

        let spawn = spawn_started.elapsed();
        let started = Instant::now();
        let (exit_status, statistics) = indexing_handle.join().await;
        let indexing = started.elapsed();

        // Drain merges on the failure path too: a half-finished merge would otherwise be
        // left staged in the metastore until the next janitor pass.
        if let Err(err) = merge_handle
            .mailbox()
            .ask(FinishPendingMergesAndShutdownPipeline)
            .await
        {
            warn!(%err, "merge pipeline did not acknowledge shutdown request");
        }
        merge_handle.join().await;
        let merge_drain = started.elapsed() - indexing;

        if !exit_status.is_success() {
            return Err(IndexerError::Pipeline(format!(
                "pipeline exited with {exit_status:?}"
            )));
        }
        Ok(SourceRun {
            statistics,
            spawn,
            indexing,
            merge_drain,
        })
    }

    pub(crate) async fn shutdown(self) {
        if let Err(err) = self.universe.send_exit_with_success(&self.service).await {
            warn!(%err, "failed to ask indexing service to exit");
        }
        self.service_handle.join().await;
        self.universe.quit().await;
        info!("indexing runtime stopped");
    }
}

/// Join the real cluster as a gossip-only member and wire
/// [`SearchJobPlacer`] → [`ReportSplitsRequest`] on a shared [`EventBroker`].
async fn start_cluster_mode(
    config_path: &Path,
) -> anyhow::Result<(
    Cluster,
    EventBroker,
    EventSubscriptionHandle,
    SearchJobPlacer,
    NodeId,
)> {
    let config_uri = Uri::from_str(&config_path.display().to_string())
        .with_context(|| format!("invalid cluster config path `{}`", config_path.display()))?;
    let config_content = load_file(&StorageResolver::unconfigured(), &config_uri)
        .await
        .with_context(|| format!("failed to load cluster config `{config_uri}`"))?;
    ensure_data_dir_from_config_bytes(&config_content)?;

    let config_format = ConfigFormat::sniff_from_uri(&config_uri)?;
    // Empty services: neither searcher nor indexer, so no control plane schedules this
    // process and no root dials it. Lane sources stay enabled: false in the YAML.
    let node_config = NodeConfig::load_with_enabled_services(
        config_format,
        config_content.as_slice(),
        Some(&HashSet::new()),
    )
    .await
    .with_context(|| format!("failed to parse cluster config `{config_uri}`"))?;

    let service_node_id = node_config.node_id.clone();
    info!(
        node_id = %service_node_id,
        cluster_id = %node_config.cluster_id,
        gossip = %node_config.gossip_advertise_addr,
        "joining rustie-cluster as gossip-only indexer"
    );
    let cluster = start_cluster_service(&node_config).await?;

    let searcher_pool = SearcherPool::default();
    let search_job_placer = SearchJobPlacer::new(searcher_pool.clone());
    let max_message_size = ByteSize::mib(20);
    let searcher_change_stream = cluster.change_stream().filter_map(move |cluster_change| {
        Box::pin(async move {
            match cluster_change {
                ClusterChange::Add(node) if node.is_searcher() => {
                    let chitchat_id = node.chitchat_id();
                    info!(
                        node_id = %chitchat_id.node_id,
                        "adding searcher to indexer report placer pool"
                    );
                    let grpc_addr = node.grpc_advertise_addr;
                    let client = create_search_client_from_grpc_addr(grpc_addr, max_message_size);
                    Some(Change::Insert(grpc_addr, client))
                }
                ClusterChange::Remove(node) if node.is_searcher() => {
                    let chitchat_id = node.chitchat_id();
                    info!(
                        node_id = %chitchat_id.node_id,
                        "removing searcher from indexer report placer pool"
                    );
                    Some(Change::Remove(node.grpc_advertise_addr))
                }
                _ => None,
            }
        })
    });
    searcher_pool.listen_for_changes(searcher_change_stream);

    let event_broker = EventBroker::default();
    let subscription_handle =
        event_broker.subscribe::<ReportSplitsRequest>(search_job_placer.clone());

    Ok((
        cluster,
        event_broker,
        subscription_handle,
        search_job_placer,
        service_node_id,
    ))
}

fn ensure_data_dir_from_config_bytes(config_content: &[u8]) -> anyhow::Result<()> {
    #[derive(serde::Deserialize)]
    struct DataDirHint {
        data_dir: Option<PathBuf>,
    }
    if let Ok(DataDirHint {
        data_dir: Some(path),
    }) = serde_yaml::from_slice::<DataDirHint>(config_content)
        && !path.exists()
    {
        std::fs::create_dir_all(&path)
            .with_context(|| format!("cannot create data_dir `{}`", path.display()))?;
        info!(path = %path.display(), "created indexer cluster data_dir");
    }
    Ok(())
}

/// A one-node cluster on the in-memory gossip transport. `IndexingService` publishes its
/// running plan into chitchat, so it needs a `Cluster`, but no peers or sockets.
async fn create_local_cluster(node_id: NodeId) -> anyhow::Result<Cluster> {
    let addr: SocketAddr = ([127, 0, 0, 1], 0).into();
    let self_node = ClusterMember {
        node_id,
        generation_id: GenerationId::now(),
        is_ready: false,
        enabled_services: HashSet::new(),
        gossip_advertise_addr: addr,
        grpc_advertise_addr: addr,
        indexing_tasks: Vec::new(),
        indexing_cpu_capacity: CpuCapacity::zero(),
        ingester_status: IngesterStatus::default(),
        availability_zone: None,
        enable_standalone_compactors: false,
    };
    let channel_factory =
        quickwit_transport::ChannelFactory::for_grpc(&quickwit_config::GrpcConfig::default())?;
    Cluster::join(
        "rustie-indexer".to_string(),
        self_node,
        addr,
        Vec::new(),
        Duration::from_secs(1),
        FailureDetectorConfig::default(),
        &quickwit_cluster::ChitchatTransport::default(),
        channel_factory,
    )
    .await
}
