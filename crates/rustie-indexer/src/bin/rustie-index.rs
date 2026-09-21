//! `rustie-index`: index a directory of Odinson JSON documents into the IE postings
//! index on MinIO.
//!
//! ```bash
//! cargo run -p rustie-indexer --bin rustie-index -- --data "data/processed_pubmed22n0008 (2)" --limit 10
//! ```
//!
//! Exit codes: `0` success, `1` fatal error, `2` finished but some files/documents were
//! rejected, `130` interrupted.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use rustie_indexer::{Indexer, IndexerOptions, InvalidDocPolicy, MinioConfig};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Directory of Odinson `*.json` documents (searched recursively).
    #[arg(long, required_unless_present = "gc_only")]
    data: Option<PathBuf>,

    /// Index at most this many files (smoke runs).
    #[arg(long)]
    limit: Option<usize>,

    /// Odinson files per indexing pipeline run (= per published split, before merges).
    #[arg(long, default_value_t = 200)]
    batch_size: usize,

    /// MinIO / S3 endpoint.
    #[arg(long, env = "MINIO_ENDPOINT", default_value = "http://127.0.0.1:9010")]
    endpoint: String,

    #[arg(long, env = "MINIO_BUCKET", default_value = "rustie-dev")]
    bucket: String,

    #[arg(long, env = "MINIO_ACCESS_KEY", default_value = "minioadmin")]
    access_key: String,

    /// Prefer the env var over the flag: flags are visible in `ps`.
    #[arg(
        long,
        env = "MINIO_SECRET_KEY",
        default_value = "minioadmin",
        hide_env_values = true
    )]
    secret_key: String,

    #[arg(long, default_value = "ie-postings")]
    index_id: String,

    /// Metastore URI. Default: `s3://<bucket>/metastore`.
    #[arg(long)]
    metastore_uri: Option<String>,

    /// Index root URI. Default: `s3://<bucket>/indexes`.
    #[arg(long)]
    index_root_uri: Option<String>,

    /// Scratch directory for split building. Default: a temporary directory.
    #[arg(long)]
    data_dir: Option<PathBuf>,

    /// Drop documents the index mapping rejects instead of aborting the run.
    #[arg(long)]
    skip_invalid: bool,

    /// After indexing, delete split files replaced by merges (older than 2 minutes).
    #[arg(long)]
    gc: bool,

    /// Only run garbage collection on the index; do not index anything (`--data` is ignored).
    #[arg(long)]
    gc_only: bool,

    /// Delete and re-create the index before indexing. DESTROYS existing index data.
    #[arg(long)]
    overwrite: bool,
}

fn main() -> ExitCode {
    // The AWS SDK probes the EC2 metadata service for a region/credentials unless told
    // not to; against MinIO that only adds ~3s of timeouts and warnings at startup.
    // SAFETY: called before any other thread exists (the runtime is built below).
    if std::env::var_os("AWS_EC2_METADATA_DISABLED").is_none() {
        unsafe { std::env::set_var("AWS_EC2_METADATA_DISABLED", "true") };
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("error: cannot start tokio runtime: {err}");
            return ExitCode::from(1);
        }
    };
    runtime.block_on(async_main())
}

async fn async_main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,quickwit=warn,tantivy=warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    match run(Args::parse()).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

async fn run(args: Args) -> anyhow::Result<ExitCode> {
    let minio = MinioConfig {
        endpoint: args.endpoint,
        bucket: args.bucket,
        access_key: args.access_key,
        secret_key: args.secret_key,
        prefix: String::new(),
    };
    let mut options = IndexerOptions::for_bucket(&minio.bucket);
    options.index_id = args.index_id;
    options.data_dir = args.data_dir;
    if args.skip_invalid {
        options.on_invalid_doc = InvalidDocPolicy::Skip;
    }
    if let Some(uri) = args.metastore_uri {
        options.metastore_uri = uri;
    }
    if let Some(uri) = args.index_root_uri {
        options.index_root_uri = uri;
    }

    let indexer = Indexer::connect(minio, options).await?;

    // First Ctrl-C: finish the current batch and stop (safe to re-run). Second: hard exit.
    let stop = indexer.stop_handle();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("interrupt: finishing current batch (Ctrl-C again to abort)");
            stop.request_stop();
            if tokio::signal::ctrl_c().await.is_ok() {
                std::process::exit(130);
            }
        }
    });

    if args.overwrite {
        eprintln!(
            "--overwrite: deleting index `{}`",
            indexer.options().index_id
        );
        indexer.delete_index().await?;
    }

    if args.gc_only {
        report_gc(&indexer).await?;
        indexer.shutdown().await;
        return Ok(ExitCode::SUCCESS);
    }
    let data = args.data.expect("clap requires --data unless --gc-only");
    let stats = indexer
        .index_odinson_dir(&data, args.limit, args.batch_size)
        .await?;
    let summary = indexer.summary().await?;

    eprintln!(
        "files: {} seen, {} failed | docs: {} submitted, {} processed, {} invalid | \
         splits published this run: {} | batches: {} ({} already indexed)",
        stats.files_seen,
        stats.files_failed,
        stats.docs_submitted,
        stats.docs_processed,
        stats.docs_invalid,
        stats.splits_published,
        stats.batches,
        stats.batches_already_indexed,
    );
    eprintln!(
        "index `{}` now has {} published split(s), {} docs",
        indexer.options().index_id,
        summary.num_published_splits,
        summary.num_docs
    );
    if !stats.unmapped_fields.is_empty() {
        eprintln!(
            "note: fields not declared in the index mapping were not indexed: {}",
            stats
                .unmapped_fields
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    for failure in stats.failures.iter().take(10) {
        eprintln!("  failed: {}: {}", failure.path.display(), failure.error);
    }

    if args.gc {
        report_gc(&indexer).await?;
    }
    let interrupted = stats.interrupted;
    let rejected = stats.files_failed > 0 || stats.docs_invalid > 0;
    indexer.shutdown().await;
    Ok(if interrupted {
        ExitCode::from(130)
    } else if rejected {
        ExitCode::from(2)
    } else {
        ExitCode::SUCCESS
    })
}

async fn report_gc(indexer: &Indexer) -> anyhow::Result<()> {
    let report = indexer
        .garbage_collect(rustie_indexer::DEFAULT_GC_DELETION_GRACE, false)
        .await?;
    eprintln!(
        "gc: removed {} split file(s), {:.1} MiB reclaimed, {} failed",
        report.splits_removed,
        report.bytes_removed as f64 / (1024.0 * 1024.0),
        report.splits_failed
    );
    Ok(())
}
