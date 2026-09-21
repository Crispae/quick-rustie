//! Quickwit `IndexingService` wired against an arbitrary (MinIO-backed) storage +
//! metastore, without the `testsuite` feature.
//!
//! This follows `quickwit index ingest --local` (`quickwit-cli/src/tool.rs`): a private
//! single-node cluster (in-memory gossip transport, no sockets), one indexing service
//! actor, and one indexing + merge pipeline pair per source run.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use quickwit_actors::{ActorHandle, Mailbox, Universe};
use quickwit_cluster::{Cluster, ClusterMember, FailureDetectorConfig, GenerationId};
use quickwit_common::pubsub::EventBroker;
use quickwit_common::runtimes::{RuntimesConfig, initialize_runtimes};
use quickwit_config::{IndexerConfig, SourceConfig};
use quickwit_indexing::actors::{IndexingService, MergeSchedulerService};
use quickwit_indexing::models::{
    DetachIndexingPipeline, DetachMergePipeline, IndexingStatistics, SpawnPipeline,
};
use quickwit_indexing::{FinishPendingMergesAndShutdownPipeline, IndexingSplitCache};
use quickwit_ingest::IngesterPool;
use quickwit_proto::indexing::CpuCapacity;
use quickwit_proto::ingest::ingester::IngesterStatus;
use quickwit_proto::metastore::MetastoreServiceClient;
use quickwit_proto::types::{NodeId, PipelineUid};
use quickwit_storage::StorageResolver;
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
}

impl PipelineRuntime {
    pub(crate) async fn start(
        node_id: NodeId,
        data_dir: &Path,
        metastore: MetastoreServiceClient,
        storage_resolver: StorageResolver,
    ) -> anyhow::Result<Self> {
        let runtimes_config = RuntimesConfig::default();
        initialize_runtimes(runtimes_config).context("failed to start actor runtimes")?;

        let cluster = create_local_cluster(node_id.clone()).await?;
        let indexer_config = IndexerConfig::default();
        let split_cache =
            Arc::new(IndexingSplitCache::from_config(&indexer_config, data_dir).await?);

        let universe = Universe::new();
        let merge_scheduler: Mailbox<MergeSchedulerService> = universe.get_or_spawn_one();
        let service = IndexingService::new(
            node_id,
            data_dir.to_path_buf(),
            indexer_config,
            runtimes_config.num_threads_blocking,
            cluster,
            metastore,
            None, // no ingest API queue: documents come from the Vec source only
            Some(merge_scheduler),
            IngesterPool::default(),
            storage_resolver,
            EventBroker::default(),
            split_cache,
        )
        .await?;
        let (service, service_handle) = universe.spawn_builder().spawn(service);
        Ok(Self {
            universe,
            service,
            service_handle,
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
