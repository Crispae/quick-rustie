//! `rustie-serve`: HTTP API for RustIE queries over the IE postings index on MinIO.
//!
//! ```bash
//! cargo run -p rustie-search --bin rustie-serve
//! curl -G localhost:8080/v1/search --data-urlencode 'q=[word=John] >nsubj [pos=VBZ]' -d limit=5
//! ```

use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use rustie_search::{MinioConfig, Searcher, SearcherOptions, server};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Args {
    /// Listen address. Loopback by default: the API has no authentication.
    #[arg(long, default_value = "127.0.0.1:8080")]
    bind: SocketAddr,

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

    #[arg(long, default_value = "ie-postings")]
    index_id: String,
    /// Metastore URI. Default: `s3://<bucket>/metastore`.
    #[arg(long)]
    metastore_uri: Option<String>,

    /// Re-open the metastore this often (seconds) to see newly indexed splits; 0 disables.
    #[arg(long, default_value_t = 30)]
    refresh_secs: u64,
    #[arg(long, default_value_t = 1_000)]
    max_limit: usize,
    #[arg(long, default_value_t = 30)]
    timeout_secs: u64,
    #[arg(long, default_value_t = 16)]
    max_concurrent_searches: usize,
}

fn main() -> ExitCode {
    // Skip the AWS SDK's EC2-metadata probing (seconds of timeouts against MinIO).
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
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,quickwit=warn,tantivy=warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    match runtime.block_on(run(Args::parse())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::from(1)
        }
    }
}

async fn run(args: Args) -> anyhow::Result<()> {
    let minio = MinioConfig {
        endpoint: args.endpoint,
        bucket: args.bucket,
        access_key: args.access_key,
        secret_key: args.secret_key,
        prefix: String::new(),
    };
    let mut options = SearcherOptions::for_bucket(&minio.bucket);
    options.index_id = args.index_id;
    options.max_limit = args.max_limit;
    options.timeout = Duration::from_secs(args.timeout_secs);
    if let Some(uri) = args.metastore_uri {
        options.metastore_uri = uri;
    }

    let searcher = Arc::new(Searcher::connect(minio, options).await?);
    match searcher.summary().await {
        Ok(s) => info!(
            splits = s.num_published_splits,
            docs = s.num_docs,
            "index ready"
        ),
        Err(err) => warn!(%err, "index not readable yet; searches will fail until it exists"),
    }

    if args.refresh_secs > 0 {
        let searcher = Arc::clone(&searcher);
        let period = Duration::from_secs(args.refresh_secs);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(period);
            ticker.tick().await; // first tick fires immediately; we just connected
            loop {
                ticker.tick().await;
                if let Err(err) = searcher.refresh().await {
                    warn!(%err, "metastore refresh failed; keeping previous view");
                }
            }
        });
    }

    let app = server::router(searcher, args.max_concurrent_searches);
    let listener = tokio::net::TcpListener::bind(args.bind).await?;
    info!(addr = %listener.local_addr()?, "listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            info!("shutting down");
        })
        .await?;
    Ok(())
}
