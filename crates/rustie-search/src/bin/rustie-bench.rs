//! `rustie-bench`: Wiki5k index + search timing against the Quickwit/MinIO path.
//!
//! TSV columns (same as the Odinson harness):
//!   system  phase  query  iter  storage  hits  elapsed_ns  notes
//!
//! ```bash
//! cargo run --release -p rustie-search --bin rustie-bench -- index \
//!   --data data/wiki5k --index-id wiki5k --out benchmarks/wiki5k/results/rustie_index.tsv
//!
//! cargo run --release -p rustie-search --bin rustie-bench -- search \
//!   --index-id wiki5k --queries benchmarks/wiki5k/queries --reps 100 \
//!   --out benchmarks/wiki5k/results/rustie_search.tsv
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use clap::{Parser, Subcommand};
use rustie_indexer::{Indexer, IndexerOptions, MinioConfig};
use rustie_search::{SearchQuery, Searcher, SearcherOptions};
use tracing_subscriber::EnvFilter;

const SYSTEM: &str = "rustie";
const STORAGE: &str = "minio-quickwit";

#[derive(Parser, Debug)]
#[command(version, about = "Wiki5k index/search benchmark for quick-rustie")]
struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Index Odinson JSON into MinIO/Quickwit and time the run.
    Index {
        #[arg(long)]
        data: PathBuf,
        #[arg(long, default_value = "wiki5k")]
        index_id: String,
        #[arg(long, default_value_t = 1000)]
        batch_size: usize,
        #[arg(long, default_value_t = 2)]
        pipelines: usize,
        /// Keep an existing index (default: delete + recreate for a clean timing run).
        #[arg(long)]
        no_overwrite: bool,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, env = "MINIO_ENDPOINT", default_value = "http://127.0.0.1:9010")]
        endpoint: String,
        #[arg(long, env = "MINIO_BUCKET", default_value = "rustie-dev")]
        bucket: String,
        #[arg(long, env = "MINIO_ACCESS_KEY", default_value = "minioadmin")]
        access_key: String,
        #[arg(
            long,
            env = "MINIO_SECRET_KEY",
            default_value = "minioadmin",
            hide_env_values = true
        )]
        secret_key: String,
        #[arg(long, env = "RUSTIE_METASTORE_URI")]
        metastore_uri: Option<String>,
    },
    /// Run non-event patterns with exact counts (in-process Searcher).
    Search {
        #[arg(long, default_value = "wiki5k")]
        index_id: String,
        #[arg(long)]
        queries: PathBuf,
        #[arg(long, default_value_t = 100)]
        reps: usize,
        /// Optional on-disk split cache (when unset, every query hits object storage
        /// subject only to in-memory caches — that is the storage cost we want to see).
        #[arg(long)]
        split_cache_dir: Option<PathBuf>,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, env = "MINIO_ENDPOINT", default_value = "http://127.0.0.1:9010")]
        endpoint: String,
        #[arg(long, env = "MINIO_BUCKET", default_value = "rustie-dev")]
        bucket: String,
        #[arg(long, env = "MINIO_ACCESS_KEY", default_value = "minioadmin")]
        access_key: String,
        #[arg(
            long,
            env = "MINIO_SECRET_KEY",
            default_value = "minioadmin",
            hide_env_values = true
        )]
        secret_key: String,
        #[arg(long, env = "RUSTIE_METASTORE_URI")]
        metastore_uri: Option<String>,
    },
}

fn main() -> ExitCode {
    if std::env::var_os("AWS_EC2_METADATA_DISABLED").is_none() {
        // SAFETY: before any other threads (runtime built below).
        unsafe { std::env::set_var("AWS_EC2_METADATA_DISABLED", "true") };
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("error: cannot start tokio runtime: {err}");
            return ExitCode::from(1);
        }
    };
    match runtime.block_on(async_main()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

async fn async_main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("warn,rustie_indexer=info,rustie_search=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    match Args::parse().cmd {
        Cmd::Index {
            data,
            index_id,
            batch_size,
            pipelines,
            no_overwrite,
            out,
            endpoint,
            bucket,
            access_key,
            secret_key,
            metastore_uri,
        } => {
            run_index(
                data,
                index_id,
                batch_size,
                pipelines,
                !no_overwrite,
                out,
                endpoint,
                bucket,
                access_key,
                secret_key,
                metastore_uri,
            )
            .await
        }
        Cmd::Search {
            index_id,
            queries,
            reps,
            split_cache_dir,
            out,
            endpoint,
            bucket,
            access_key,
            secret_key,
            metastore_uri,
        } => {
            run_search(
                index_id,
                queries,
                reps,
                split_cache_dir,
                out,
                endpoint,
                bucket,
                access_key,
                secret_key,
                metastore_uri,
            )
            .await
        }
    }
}

async fn run_index(
    data: PathBuf,
    index_id: String,
    batch_size: usize,
    pipelines: usize,
    overwrite: bool,
    out: PathBuf,
    endpoint: String,
    bucket: String,
    access_key: String,
    secret_key: String,
    metastore_uri: Option<String>,
) -> anyhow::Result<()> {
    let minio = MinioConfig {
        endpoint,
        bucket: bucket.clone(),
        access_key,
        secret_key,
        prefix: String::new(),
    };
    let mut options = IndexerOptions::for_bucket(&bucket);
    options.index_id = index_id.clone();
    options.pipelines = pipelines.max(1);
    if let Some(uri) = metastore_uri {
        options.metastore_uri = uri;
    }

    let indexer = Indexer::connect(minio, options).await?;
    if overwrite {
        eprintln!("deleting index `{index_id}` for a clean timing run");
        indexer.delete_index().await?;
    }
    indexer.ensure_index().await?;

    let wall = Instant::now();
    let stats = indexer.index_odinson_dir(&data, None, batch_size).await?;
    let elapsed = wall.elapsed();
    let summary = indexer.summary().await?;
    indexer.shutdown().await;

    let notes = format!(
        "docs_processed={} splits={} batches={} flatten_s={:.1} indexing_s={:.1} merge_s={:.1}",
        stats.docs_processed,
        stats.splits_published,
        stats.batches,
        stats.timings.flatten.as_secs_f64(),
        stats.timings.indexing.as_secs_f64(),
        stats.timings.merge_drain.as_secs_f64(),
    );
    write_tsv(
        &out,
        &[TsvRow {
            system: SYSTEM,
            phase: "index",
            query: "-",
            iter: 0,
            storage: STORAGE,
            hits: stats.docs_processed,
            elapsed_ns: elapsed.as_nanos() as u64,
            notes,
        }],
    )?;
    eprintln!(
        "[rustie] indexed {} docs / {} splits in {:.2}s (metastore num_docs={}) → {}",
        stats.docs_processed,
        summary.num_published_splits,
        elapsed.as_secs_f64(),
        summary.num_docs,
        out.display()
    );
    Ok(())
}

async fn run_search(
    index_id: String,
    queries: PathBuf,
    reps: usize,
    split_cache_dir: Option<PathBuf>,
    out: PathBuf,
    endpoint: String,
    bucket: String,
    access_key: String,
    secret_key: String,
    metastore_uri: Option<String>,
) -> anyhow::Result<()> {
    let minio = MinioConfig {
        endpoint,
        bucket: bucket.clone(),
        access_key,
        secret_key,
        prefix: String::new(),
    };
    let mut options = SearcherOptions::for_bucket(&bucket);
    options.index_id = index_id;
    options.timeout = std::time::Duration::from_secs(120);
    if let Some(uri) = metastore_uri {
        options.metastore_uri = uri;
    }
    if let Some(dir) = split_cache_dir {
        options = options.with_split_cache(dir, 10_000, 10_000)?;
    }

    let load = Instant::now();
    let searcher = Searcher::connect(minio, options).await?;
    let load_ns = load.elapsed().as_nanos() as u64;
    let summary = searcher.summary().await?;

    let mut rows = vec![TsvRow {
        system: SYSTEM,
        phase: "load",
        query: "-",
        iter: 0,
        storage: STORAGE,
        hits: summary.num_docs,
        elapsed_ns: load_ns,
        notes: "searcher_open".into(),
    }];

    let patterns = load_queries(&queries)?;
    for (name, pattern) in &patterns {
        // Warm-up: exact count over the corpus (not recorded).
        let warm = SearchQuery::new(pattern.clone()).limit(1).count(true);
        let _ = searcher.search(warm).await?;

        for i in 0..reps {
            let q = SearchQuery::new(pattern.clone()).limit(1).count(true);
            let t0 = Instant::now();
            let res = searcher.search(q).await?;
            rows.push(TsvRow {
                system: SYSTEM,
                phase: "search",
                query: name,
                iter: i as u64,
                storage: STORAGE,
                hits: res.total_hits,
                elapsed_ns: t0.elapsed().as_nanos() as u64,
                notes: format!(
                    "backend_us={} render_us={} exact={}",
                    res.timing.backend_us, res.timing.render_us, res.total_is_exact
                ),
            });
        }
        eprintln!("[rustie] search {name} x{reps} done");
    }

    write_tsv(&out, &rows)?;
    eprintln!("[rustie] wrote {}", out.display());
    Ok(())
}

struct TsvRow<'a> {
    system: &'a str,
    phase: &'a str,
    query: &'a str,
    iter: u64,
    storage: &'a str,
    hits: u64,
    elapsed_ns: u64,
    notes: String,
}

fn write_tsv(path: &Path, rows: &[TsvRow<'_>]) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut body = String::from("system\tphase\tquery\titer\tstorage\thits\telapsed_ns\tnotes\n");
    for r in rows {
        body.push_str(&format!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
            r.system, r.phase, r.query, r.iter, r.storage, r.hits, r.elapsed_ns, r.notes
        ));
    }
    fs::write(path, body)?;
    Ok(())
}

fn load_queries(dir: &Path) -> anyhow::Result<Vec<(String, String)>> {
    let mut out = Vec::new();
    let mut entries: Vec<_> = fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|x| x.to_str())
                .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
        })
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("query")
            .to_string();
        let pattern = fs::read_to_string(&path)?.trim().to_string();
        if pattern.is_empty() || pattern.starts_with("trigger") {
            continue;
        }
        out.push((name, pattern));
    }
    if out.is_empty() {
        anyhow::bail!("no non-event *.txt queries under {}", dir.display());
    }
    Ok(out)
}
