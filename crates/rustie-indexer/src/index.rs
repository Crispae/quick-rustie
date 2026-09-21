//! [`Indexer`]: create the IE postings index on MinIO and feed it sentence documents.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use bytes::Bytes;
use quickwit_common::uri::Uri;
use quickwit_config::{
    CLI_SOURCE_ID, ConfigFormat, IndexConfig, SourceConfig, SourceInputFormat, SourceParams,
    VecSourceParams, build_doc_mapper, load_index_config_from_user_config,
};
use quickwit_doc_mapper::DocMapper;
use quickwit_index_management::{IndexService, IndexServiceError, run_garbage_collect};
use quickwit_metastore::{IndexMetadata, IndexMetadataResponseExt, MetastoreServiceExt};
use quickwit_proto::metastore::{
    IndexMetadataRequest, MetastoreError, MetastoreService, MetastoreServiceClient,
};
use quickwit_proto::types::NodeId;
use quickwit_storage::StorageResolver;
use rustie_schema::{IndexConfigOptions, postings_index_config_yaml};
use serde_json::Value as JsonValue;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::error::{IndexerError, Result};
use crate::flatten_source::{FileFailure, flatten_files, list_odinson_files};
use crate::minio::MinioConfig;
use crate::pipeline::PipelineRuntime;
use crate::store::{IndexSummary, index_summary, open_metastore};

/// Flattened batches buffered ahead of the indexing pipeline. One is enough to overlap
/// file parsing with indexing while keeping memory bounded.
const PREFETCH_BATCHES: usize = 1;

/// What to do with sentence documents the doc mapping rejects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum InvalidDocPolicy {
    /// Abort before submitting the batch containing them (default). Earlier batches stay
    /// published, and a re-run resumes at the failing batch.
    #[default]
    Fail,
    /// Drop them, count them in [`IndexingStats::docs_invalid`], index the rest.
    Skip,
}

/// Where the index lives and how documents are fed to Quickwit.
#[derive(Debug, Clone)]
pub struct IndexerOptions {
    pub index_id: String,
    /// File-backed metastore location, e.g. `s3://rustie-dev/metastore`.
    pub metastore_uri: String,
    /// Directory holding all indexes; the index lives at `{index_root_uri}/{index_id}`.
    pub index_root_uri: String,
    /// Scratch directory for split building and the split cache. `None` uses a temporary
    /// directory that is removed when the [`Indexer`] is dropped.
    pub data_dir: Option<PathBuf>,
    /// Documents per raw batch handed to the doc processor by the source.
    pub source_batch_num_docs: usize,
    pub on_invalid_doc: InvalidDocPolicy,
}

impl IndexerOptions {
    /// Defaults aligned with `configs/ie_postings.yaml` on the given bucket:
    /// metastore `s3://{bucket}/metastore`, index `s3://{bucket}/indexes/ie-postings`.
    pub fn for_bucket(bucket: &str) -> Self {
        Self {
            index_id: "ie-postings".to_string(),
            metastore_uri: format!("s3://{bucket}/metastore"),
            index_root_uri: format!("s3://{bucket}/indexes"),
            data_dir: None,
            source_batch_num_docs: 1_000,
            on_invalid_doc: InvalidDocPolicy::default(),
        }
    }

    pub fn index_uri(&self) -> String {
        format!(
            "{}/{}",
            self.index_root_uri.trim_end_matches('/'),
            self.index_id
        )
    }

    fn validate(&self) -> Result<()> {
        if self.index_id.is_empty() {
            return Err(IndexerError::InvalidConfig("index_id is empty".into()));
        }
        if self.source_batch_num_docs == 0 {
            return Err(IndexerError::InvalidConfig(
                "source_batch_num_docs must be > 0".into(),
            ));
        }
        for (name, uri) in [
            ("metastore_uri", &self.metastore_uri),
            ("index_root_uri", &self.index_root_uri),
        ] {
            Uri::from_str(uri).map_err(|err| {
                IndexerError::InvalidConfig(format!("{name} `{uri}` is not a valid URI: {err}"))
            })?;
        }
        Ok(())
    }
}

impl Default for IndexerOptions {
    fn default() -> Self {
        Self::for_bucket(&MinioConfig::from_env().bucket)
    }
}

/// Outcome of [`Indexer::ensure_index`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexStatus {
    Created,
    AlreadyExisted,
}

/// Aggregated counters for one or more indexed batches.
#[derive(Debug, Clone, Default)]
pub struct IndexingStats {
    /// Odinson files read (including ones that failed to flatten).
    pub files_seen: usize,
    pub files_failed: usize,
    /// First failures, capped at [`crate::flatten_source::MAX_RECORDED_FAILURES`].
    pub failures: Vec<FileFailure>,
    /// Sentence documents handed to Quickwit.
    pub docs_submitted: u64,
    /// Documents processed by the pipeline (valid or not).
    pub docs_processed: u64,
    /// Documents rejected by the doc mapping and dropped ([`InvalidDocPolicy::Skip`]).
    pub docs_invalid: u64,
    /// Top-level document fields dropped because the index mapping does not declare them
    /// (e.g. Odinson `norm` / `raw` when the mapping only covers the default token fields).
    pub unmapped_fields: BTreeSet<String>,
    pub splits_published: u64,
    pub bytes_processed: u64,
    pub batches: usize,
    /// Batches skipped because an identical batch was already published (checkpoint hit).
    pub batches_already_indexed: usize,
    /// The run stopped early because [`Indexer::request_stop`] was called.
    pub interrupted: bool,
}

impl IndexingStats {
    fn absorb(&mut self, other: IndexingStats) {
        self.files_seen += other.files_seen;
        self.files_failed += other.files_failed;
        self.failures.extend(other.failures);
        self.docs_submitted += other.docs_submitted;
        self.docs_processed += other.docs_processed;
        self.docs_invalid += other.docs_invalid;
        self.unmapped_fields.extend(other.unmapped_fields);
        self.splits_published += other.splits_published;
        self.bytes_processed += other.bytes_processed;
        self.batches += other.batches;
        self.batches_already_indexed += other.batches_already_indexed;
        self.interrupted |= other.interrupted;
    }
}

/// Outcome of [`Indexer::garbage_collect`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcReport {
    pub splits_removed: usize,
    pub bytes_removed: u64,
    pub splits_failed: usize,
}

/// Splits merged away (or abandoned while staged) stay on storage until collected. Searchers
/// may still be reading a just-merged-away split, so it is kept for this long first.
pub const DEFAULT_GC_DELETION_GRACE: std::time::Duration = std::time::Duration::from_secs(120);
/// Staged splits are uploads whose publish may still be in flight; Quickwit's janitor waits a
/// day before treating them as abandoned.
const GC_STAGED_GRACE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// See [`Indexer::stop_handle`].
#[derive(Debug, Clone)]
pub struct StopHandle(Arc<AtomicBool>);

impl StopHandle {
    pub fn request_stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// Indexes flattened Odinson sentences into a Quickwit index on MinIO.
pub struct Indexer {
    options: IndexerOptions,
    index_config: IndexConfig,
    doc_mapper: Arc<DocMapper>,
    mapped_fields: Arc<HashSet<String>>,
    metastore: MetastoreServiceClient,
    storage_resolver: StorageResolver,
    runtime: Option<PipelineRuntime>,
    stop: Arc<AtomicBool>,
    // Declared last so the scratch dir outlives the runtime during drop.
    _scratch: Option<tempfile::TempDir>,
}

impl Indexer {
    /// Resolve storage + metastore on MinIO, verify connectivity, and start the indexing
    /// actors. Fails fast (before any actor starts) if the endpoint or bucket is unusable.
    pub async fn connect(minio: MinioConfig, options: IndexerOptions) -> Result<Self> {
        options.validate()?;

        let (storage_resolver, metastore) = open_metastore(&minio, &options.metastore_uri).await?;

        let index_config = build_index_config(&options)?;
        let doc_mapper = build_doc_mapper(&index_config.doc_mapping, &index_config.search_settings)
            .map_err(|err| IndexerError::InvalidConfig(format!("invalid doc mapping: {err:#}")))?;
        let mapped_fields = index_config
            .doc_mapping
            .field_mappings
            .iter()
            .map(|entry| entry.name.clone())
            .collect();

        let (scratch, data_dir) = match &options.data_dir {
            Some(dir) => {
                std::fs::create_dir_all(dir).map_err(|err| IndexerError::io(dir, err))?;
                (None, dir.clone())
            }
            None => {
                let tmp = tempfile::Builder::new()
                    .prefix("rustie-indexer-")
                    .tempdir()
                    .map_err(|err| IndexerError::io(std::env::temp_dir(), err))?;
                let path = tmp.path().to_path_buf();
                (Some(tmp), path)
            }
        };

        let node_id = NodeId::from_str(&format!("rustie-indexer-{}", std::process::id()));
        let runtime = PipelineRuntime::start(
            node_id,
            &data_dir,
            metastore.clone(),
            storage_resolver.clone(),
        )
        .await?;

        Ok(Self {
            options,
            index_config,
            doc_mapper,
            mapped_fields: Arc::new(mapped_fields),
            metastore,
            storage_resolver,
            runtime: Some(runtime),
            stop: Arc::new(AtomicBool::new(false)),
            _scratch: scratch,
        })
    }

    pub fn options(&self) -> &IndexerOptions {
        &self.options
    }

    /// Ask a running [`index_odinson_dir`](Self::index_odinson_dir) to stop after the
    /// current batch. Batches are published atomically with their checkpoint, so a stopped
    /// run can simply be re-run.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// A cloneable handle for [`request_stop`](Self::request_stop), e.g. for a Ctrl-C task
    /// that must not keep the [`Indexer`] itself alive.
    pub fn stop_handle(&self) -> StopHandle {
        StopHandle(Arc::clone(&self.stop))
    }

    /// Create the index if it does not exist. If it does, verify that its doc mapping and
    /// URI match what this crate would create, so we never index into a stale schema.
    pub async fn ensure_index(&self) -> Result<IndexStatus> {
        let mut metastore = self.metastore.clone();
        let index_id = &self.options.index_id;
        if metastore
            .index_exists(index_id)
            .await
            .map_err(anyhow::Error::from)?
        {
            self.verify_existing_index().await?;
            return Ok(IndexStatus::AlreadyExisted);
        }
        let mut service = IndexService::new(self.metastore.clone(), self.storage_resolver.clone());
        match service.create_index(self.index_config.clone(), false).await {
            Ok(_) => {
                info!(index_id, index_uri = %self.index_config.index_uri, "created index");
                Ok(IndexStatus::Created)
            }
            // Lost a creation race with another process: fine, as long as it matches.
            Err(IndexServiceError::Metastore(MetastoreError::AlreadyExists(_))) => {
                self.verify_existing_index().await?;
                Ok(IndexStatus::AlreadyExisted)
            }
            Err(err) => Err(anyhow::Error::from(err)
                .context(format!("failed to create index `{index_id}`"))
                .into()),
        }
    }

    async fn index_metadata(&self) -> Result<IndexMetadata> {
        let response = self
            .metastore
            .clone()
            .index_metadata(IndexMetadataRequest::for_index_id(
                self.options.index_id.clone(),
            ))
            .await
            .map_err(anyhow::Error::from)?;
        Ok(response
            .deserialize_index_metadata()
            .map_err(anyhow::Error::from)?)
    }

    async fn verify_existing_index(&self) -> Result<()> {
        let existing = self.index_metadata().await?.into_index_config();
        let mismatch = |detail: String| IndexerError::IndexConfigMismatch {
            index_id: self.options.index_id.clone(),
            detail,
        };
        if existing.index_uri != self.index_config.index_uri {
            return Err(mismatch(format!(
                "index_uri is `{}`, expected `{}`",
                existing.index_uri, self.index_config.index_uri
            )));
        }
        if !same_doc_mapping(&existing, &self.index_config) {
            return Err(mismatch(
                "doc_mapping differs from rustie-schema's postings mapping; \
                 re-create the index (--overwrite) or migrate it"
                    .into(),
            ));
        }
        Ok(())
    }

    /// Delete the index and all its splits. Irreversible. Returns `false` if the index did
    /// not exist.
    pub async fn delete_index(&self) -> Result<bool> {
        let mut service = IndexService::new(self.metastore.clone(), self.storage_resolver.clone());
        match service.delete_index(&self.options.index_id, false).await {
            Ok(_) => {
                info!(index_id = %self.options.index_id, "deleted index");
                Ok(true)
            }
            Err(IndexServiceError::Metastore(MetastoreError::NotFound(_))) => Ok(false),
            Err(err) => Err(anyhow::Error::from(err).into()),
        }
    }

    /// Published split / document counts as recorded in the metastore.
    pub async fn summary(&self) -> Result<IndexSummary> {
        index_summary(&self.metastore, &self.options.index_id).await
    }

    /// Delete split files that are no longer part of the index: splits replaced by merges once
    /// they are older than `deletion_grace`, and staged splits abandoned for over a day.
    /// This is the storage reclamation that RustIE's `storage::gc` did by hand; it is safe to
    /// run while the index is being searched. `dry_run` reports without deleting.
    pub async fn garbage_collect(
        &self,
        deletion_grace: std::time::Duration,
        dry_run: bool,
    ) -> Result<GcReport> {
        let index_uid = self.index_metadata().await?.index_uid;
        let storage = self
            .storage_resolver
            .resolve(&self.index_config.index_uri)
            .await
            .map_err(anyhow::Error::from)?;
        let removal = run_garbage_collect(
            HashMap::from([(index_uid, storage)]),
            self.metastore.clone(),
            GC_STAGED_GRACE,
            deletion_grace,
            dry_run,
            None,
            None,
        )
        .await?;
        Ok(GcReport {
            splits_removed: removal.removed_split_entries.len(),
            bytes_removed: removal
                .removed_split_entries
                .iter()
                .map(|split| split.file_size_bytes.as_u64())
                .sum(),
            splits_failed: removal.failed_splits.len(),
        })
    }

    /// Index already-flattened sentence documents (one Quickwit doc each) and wait until
    /// their splits are published.
    ///
    /// The batch is checkpointed under a partition derived from its content, so submitting
    /// byte-identical documents again is a no-op (`batches_already_indexed`). Submitting
    /// *overlapping but different* batches indexes the overlap twice.
    pub async fn index_docs(&self, docs: Vec<JsonValue>) -> Result<IndexingStats> {
        let mut stats = IndexingStats::default();
        if docs.is_empty() {
            return Ok(stats);
        }
        let mapper = Arc::clone(&self.doc_mapper);
        let mapped_fields = Arc::clone(&self.mapped_fields);
        let prepared =
            tokio::task::spawn_blocking(move || prepare_docs(docs, &mapped_fields, &mapper))
                .await
                .map_err(|err| IndexerError::Pipeline(format!("doc validation panicked: {err}")))?;
        stats.unmapped_fields = prepared.unmapped_fields;
        stats.docs_invalid = prepared.num_invalid;
        if prepared.num_invalid > 0 && self.options.on_invalid_doc == InvalidDocPolicy::Fail {
            return Err(IndexerError::InvalidDocuments {
                count: prepared.num_invalid,
                sample: prepared.invalid_sample,
            });
        }
        for message in &prepared.invalid_sample {
            warn!(%message, "dropping document rejected by the doc mapping");
        }
        let docs = prepared.docs;
        if docs.is_empty() {
            return Ok(stats);
        }
        let num_docs = docs.len() as u64;
        let source_config = SourceConfig {
            // A default source registered on every index by `IndexService::create_index`.
            source_id: CLI_SOURCE_ID.to_string(),
            num_pipelines: NonZeroUsize::MIN,
            enabled: true,
            source_params: SourceParams::Vec(VecSourceParams {
                partition: content_partition(&docs),
                docs,
                batch_num_docs: self.options.source_batch_num_docs,
            }),
            transform_config: None,
            input_format: SourceInputFormat::Json,
        };
        let runtime = self
            .runtime
            .as_ref()
            .expect("runtime is present until drop");
        let pipeline_stats = runtime
            .run_source(&self.options.index_id, source_config)
            .await?;

        stats.batches = 1;
        stats.docs_submitted = num_docs;
        stats.docs_processed = pipeline_stats.num_docs;
        if pipeline_stats.num_invalid_docs > 0 {
            // Pre-validation uses the same doc mapper, so this indicates a bug or a Quickwit
            // behavior change. The batch's checkpoint is already advanced: surface loudly.
            return Err(IndexerError::Pipeline(format!(
                "{} document(s) were rejected inside the pipeline despite pre-validation",
                pipeline_stats.num_invalid_docs
            )));
        }
        stats.splits_published = pipeline_stats.num_published_splits;
        stats.bytes_processed = pipeline_stats.total_bytes_processed;
        if pipeline_stats.num_docs == 0 {
            stats.batches_already_indexed = 1;
        }
        Ok(stats)
    }

    /// Flatten Odinson JSON files under `dir` (recursively, sorted) and index them in
    /// batches of `batch_size` files. `limit` caps the number of files, for smoke runs.
    ///
    /// Files that fail to parse are skipped and reported in [`IndexingStats::failures`];
    /// a pipeline or storage failure aborts the run (earlier batches stay published).
    pub async fn index_odinson_dir(
        &self,
        dir: &std::path::Path,
        limit: Option<usize>,
        batch_size: usize,
    ) -> Result<IndexingStats> {
        if batch_size == 0 {
            return Err(IndexerError::InvalidConfig("batch_size must be > 0".into()));
        }
        self.ensure_index().await?;

        let dir = dir.to_path_buf();
        let files = tokio::task::spawn_blocking(move || list_odinson_files(&dir, limit))
            .await
            .map_err(|err| IndexerError::Pipeline(format!("file listing panicked: {err}")))??;
        let total_files = files.len();
        info!(total_files, batch_size, "indexing Odinson files");

        let (tx, mut rx) = mpsc::channel(PREFETCH_BATCHES);
        let producer = tokio::task::spawn_blocking(move || {
            for chunk in files.chunks(batch_size) {
                if tx.blocking_send(flatten_files(chunk)).is_err() {
                    break; // consumer stopped (error or interrupt)
                }
            }
        });

        let started = Instant::now();
        let mut totals = IndexingStats::default();
        while let Some(batch) = rx.recv().await {
            if self.stop.load(Ordering::SeqCst) {
                totals.interrupted = true;
                warn!("stop requested; halting before next batch");
                break;
            }
            let mut batch_stats = IndexingStats {
                files_seen: batch.num_files,
                files_failed: batch.num_failed_files,
                failures: batch.failures,
                ..Default::default()
            };
            if !batch.docs.is_empty() {
                batch_stats.absorb(self.index_docs(batch.docs).await?);
            }
            totals.absorb(batch_stats);

            let secs = started.elapsed().as_secs_f64().max(f64::EPSILON);
            info!(
                files = format_args!("{}/{}", totals.files_seen, total_files),
                docs = totals.docs_processed,
                invalid = totals.docs_invalid,
                splits = totals.splits_published,
                docs_per_sec = (totals.docs_processed as f64 / secs) as u64,
                "batch done"
            );
        }
        drop(rx);
        producer
            .await
            .map_err(|err| IndexerError::Pipeline(format!("flatten worker panicked: {err}")))?;
        totals
            .failures
            .truncate(crate::flatten_source::MAX_RECORDED_FAILURES);
        Ok(totals)
    }

    /// Stop the actor system. Prefer calling this over dropping so actors exit cleanly.
    pub async fn shutdown(mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown().await;
        }
    }
}

fn build_index_config(options: &IndexerOptions) -> Result<IndexConfig> {
    let yaml = postings_index_config_yaml(&IndexConfigOptions {
        index_id: options.index_id.clone(),
        index_uri: options.index_uri(),
        ..Default::default()
    });
    let root = Uri::from_str(&options.index_root_uri)
        .map_err(|err| IndexerError::InvalidConfig(err.to_string()))?;
    load_index_config_from_user_config(ConfigFormat::Yaml, yaml.as_bytes(), &root)
        .map_err(|err| IndexerError::InvalidConfig(format!("invalid index config: {err:#}")))
}

struct PreparedDocs {
    docs: Vec<Bytes>,
    num_invalid: u64,
    invalid_sample: Vec<String>,
    unmapped_fields: BTreeSet<String>,
}

const MAX_INVALID_SAMPLES: usize = 5;

/// Drop fields the mapping does not declare, then check each document against the doc
/// mapper exactly as the pipeline will. Quickwit advances a source's checkpoint past
/// invalid documents, so letting them reach the pipeline would silently and permanently
/// consume them.
fn prepare_docs(
    docs: Vec<JsonValue>,
    mapped_fields: &HashSet<String>,
    mapper: &DocMapper,
) -> PreparedDocs {
    let mut prepared = PreparedDocs {
        docs: Vec::with_capacity(docs.len()),
        num_invalid: 0,
        invalid_sample: Vec::new(),
        unmapped_fields: BTreeSet::new(),
    };
    for mut doc in docs {
        if let JsonValue::Object(map) = &mut doc {
            map.retain(|key, _| {
                let keep = mapped_fields.contains(key);
                if !keep {
                    prepared.unmapped_fields.insert(key.clone());
                }
                keep
            });
        }
        let bytes = Bytes::from(serde_json::to_vec(&doc).expect("Value serializes"));
        match mapper.doc_from_json_bytes(&bytes) {
            Ok(_) => prepared.docs.push(bytes),
            Err(err) => {
                prepared.num_invalid += 1;
                if prepared.invalid_sample.len() < MAX_INVALID_SAMPLES {
                    let id = doc
                        .get("sentence_id")
                        .and_then(JsonValue::as_str)
                        .unwrap_or("?");
                    prepared.invalid_sample.push(format!("{id}: {err}"));
                }
            }
        }
    }
    prepared
}

/// Doc mappings are equal up to `doc_mapping_uid`, which Quickwit randomizes every time a
/// config is parsed.
fn same_doc_mapping(a: &IndexConfig, b: &IndexConfig) -> bool {
    let mut b_mapping = b.doc_mapping.clone();
    b_mapping.doc_mapping_uid = a.doc_mapping.doc_mapping_uid;
    a.doc_mapping == b_mapping
}

/// Deterministic checkpoint partition id for a batch: SHA-256 over the length-prefixed docs.
fn content_partition(docs: &[Bytes]) -> String {
    let mut hasher = Sha256::new();
    for doc in docs {
        hasher.update((doc.len() as u64).to_le_bytes());
        hasher.update(doc);
    }
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("odinson-sha256-{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_is_stable_and_content_sensitive() {
        let a = vec![
            Bytes::from_static(b"{\"a\":1}"),
            Bytes::from_static(b"{\"b\":2}"),
        ];
        let b = vec![
            Bytes::from_static(b"{\"a\":1}"),
            Bytes::from_static(b"{\"b\":3}"),
        ];
        assert_eq!(content_partition(&a), content_partition(&a.clone()));
        assert_ne!(content_partition(&a), content_partition(&b));
        // Length prefixing: [ab, c] != [a, bc].
        let x = vec![Bytes::from_static(b"ab"), Bytes::from_static(b"c")];
        let y = vec![Bytes::from_static(b"a"), Bytes::from_static(b"bc")];
        assert_ne!(content_partition(&x), content_partition(&y));
    }

    #[test]
    fn options_layout_matches_ie_postings_yaml() {
        let opts = IndexerOptions::for_bucket("rustie-dev");
        assert_eq!(opts.index_uri(), "s3://rustie-dev/indexes/ie-postings");
        assert_eq!(opts.metastore_uri, "s3://rustie-dev/metastore");
        opts.validate().unwrap();
    }

    #[test]
    fn generated_config_matches_configs_ie_postings_yaml() {
        let opts = IndexerOptions::for_bucket("rustie-dev");
        let generated = build_index_config(&opts).unwrap();

        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../configs/ie_postings.yaml"
        );
        let checked_in = std::fs::read(path).unwrap();
        let root = Uri::from_str(&opts.index_root_uri).unwrap();
        let from_file =
            load_index_config_from_user_config(ConfigFormat::Yaml, &checked_in, &root).unwrap();

        assert_eq!(generated.index_id, from_file.index_id);
        assert_eq!(generated.index_uri, from_file.index_uri);
        assert!(same_doc_mapping(&generated, &from_file));
        assert!(!same_doc_mapping(&generated, &{
            let mut changed = from_file.clone();
            changed.doc_mapping.field_mappings.pop();
            changed
        }));
    }

    #[test]
    fn prepare_drops_unmapped_fields_and_rejects_bad_types() {
        let opts = IndexerOptions::for_bucket("b");
        let config = build_index_config(&opts).unwrap();
        let mapper = build_doc_mapper(&config.doc_mapping, &config.search_settings).unwrap();
        let mapped: HashSet<String> = config
            .doc_mapping
            .field_mappings
            .iter()
            .map(|f| f.name.clone())
            .collect();

        let good = serde_json::json!({
            "doc_id": "d", "sentence_id": "d_0", "sentence_length": 2,
            "word": "a|b", "norm": "a|b", "extra1": "x", "extra2": "y"
        });
        let bad =
            serde_json::json!({"doc_id": "d", "sentence_id": "d_1", "sentence_length": "many"});
        let prepared = prepare_docs(vec![good, bad], &mapped, &mapper);

        assert_eq!(prepared.docs.len(), 1);
        assert_eq!(prepared.num_invalid, 1);
        assert!(prepared.invalid_sample[0].starts_with("d_1:"));
        assert_eq!(
            prepared
                .unmapped_fields
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["extra1", "extra2"]
        );
        let kept: JsonValue = serde_json::from_slice(&prepared.docs[0]).unwrap();
        assert!(kept.get("extra1").is_none() && kept.get("norm").is_some());
    }

    #[test]
    fn rejects_bad_options() {
        let mut opts = IndexerOptions::for_bucket("b");
        opts.source_batch_num_docs = 0;
        assert!(matches!(
            opts.validate(),
            Err(IndexerError::InvalidConfig(_))
        ));
    }
}
