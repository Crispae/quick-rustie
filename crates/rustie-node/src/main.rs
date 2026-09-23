//! `rustie-node`: a Quickwit node with [`rustie_leaf::register`] installed.
//!
//! Owns chitchat, gRPC search, and (when enabled) indexing. Every node should run with
//! **searcher + metastore** so each process opens the shared file/S3 metastore URI directly
//! (no single-owner metastore proxy). Empty `peer_seeds` is a valid single-node cluster.
//!
//! ```bash
//! cargo run -p rustie-node -- --config configs/rustie-node.yaml
//! ```

use std::collections::HashSet;
use std::path::PathBuf;
use std::pin::pin;
use std::process::ExitCode;
use std::str::FromStr;

use anyhow::Context;
use clap::{ArgAction, Parser};
use futures::future::select;
use itertools::Itertools;
use quickwit_common::runtimes::RuntimesConfig;
use quickwit_common::uri::Uri;
use quickwit_config::service::QuickwitService;
use quickwit_config::{ConfigFormat, MetastoreConfigs, NodeConfig, StorageConfigs};
use quickwit_metastore::MetastoreResolver;
use quickwit_serve::tcp_listener::DefaultTcpListenerResolver;
use quickwit_serve::{do_nothing_env_filter_reload_fn, serve_quickwit};
use quickwit_storage::{StorageResolver, load_file};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Quickwit node with RustIE leaf extensions (services from config unless --service overrides)"
)]
struct Args {
    /// Quickwit node config (YAML / TOML / JSON). See `configs/rustie-node.yaml`.
    #[arg(long, env = "RUSTIE_NODE_CONFIG", default_value = "configs/rustie-node.yaml")]
    config: String,

    /// Override `enabled_services` from the node config. Repeatable. When omitted, the
    /// config file (and `QW_ENABLED_SERVICES`) win — see `configs/rustie-node.yaml`.
    #[arg(long = "service", action = ArgAction::Append)]
    services: Vec<String>,
}

fn main() -> ExitCode {
    if std::env::var_os("AWS_EC2_METADATA_DISABLED").is_none() {
        // SAFETY: sole thread before the runtime exists.
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
    // Sidecar (index) + query extension (search) before any Quickwit actor starts.
    rustie_leaf::register();

    let config_uri = Uri::from_str(&args.config)
        .with_context(|| format!("invalid --config `{}`", args.config))?;
    // Only override when --service was passed; otherwise YAML / QW_ENABLED_SERVICES apply.
    let cli_services = parse_cli_services(&args.services)?;

    let config_content = load_file(&StorageResolver::unconfigured(), &config_uri)
        .await
        .context("failed to load node config")?;
    // NodeConfig::load requires data_dir to exist already.
    ensure_data_dir_from_config_bytes(&config_content)?;

    let config_format = ConfigFormat::sniff_from_uri(&config_uri)?;
    let node_config = NodeConfig::load_with_enabled_services(
        config_format,
        config_content.as_slice(),
        cli_services.as_ref(),
    )
    .await
    .with_context(|| format!("failed to parse node config `{config_uri}`"))?;

    warn_if_no_local_metastore(&node_config.enabled_services);
    info!(
        services = %node_config.enabled_services.iter().join(", "),
        config = %config_uri,
        "starting rustie-node"
    );

    let (storage_resolver, metastore_resolver) =
        get_resolvers(&node_config.storage_configs, &node_config.metastore_configs);

    let runtimes_config = RuntimesConfig::default();
    start_actor_runtimes(runtimes_config, &node_config.enabled_services)?;

    let shutdown_signal = Box::pin(async {
        select(pin!(listen_interrupt()), pin!(listen_sigterm())).await;
    });

    serve_quickwit(
        node_config,
        runtimes_config,
        metastore_resolver,
        storage_resolver,
        DefaultTcpListenerResolver,
        shutdown_signal,
        do_nothing_env_filter_reload_fn(),
    )
    .await?;
    info!("rustie-node terminated");
    Ok(())
}

/// Parses `--service` flags. `None` when the flag was omitted so the config file wins.
fn parse_cli_services(cli: &[String]) -> anyhow::Result<Option<HashSet<QuickwitService>>> {
    if cli.is_empty() {
        return Ok(None);
    }
    let services: HashSet<QuickwitService> = cli
        .iter()
        .map(|s| QuickwitService::from_str(s))
        .collect::<Result<_, _>>()
        .map_err(|err| anyhow::anyhow!("invalid --service: {err}"))?;
    Ok(Some(services))
}

fn warn_if_no_local_metastore(services: &HashSet<QuickwitService>) {
    if !services.contains(&QuickwitService::Metastore)
        && !services.contains(&QuickwitService::MetastoreReadReplica)
    {
        warn!(
            "no metastore role enabled; this node will try to discover a remote metastore over \
             gRPC. Prefer `enabled_services: [searcher, metastore]` in the node config (or \
             `--service searcher --service metastore`) so every node opens the shared URI \
             directly (file/S3-backed)."
        );
    }
}

fn ensure_data_dir_from_config_bytes(config_content: &[u8]) -> anyhow::Result<()> {
    #[derive(serde::Deserialize)]
    struct DataDirHint {
        data_dir: Option<PathBuf>,
    }
    // Best-effort: YAML/JSON both deserialize; TOML configs should create data_dir manually.
    if let Ok(DataDirHint {
        data_dir: Some(path),
    }) = serde_yaml::from_slice::<DataDirHint>(config_content)
    {
        ensure_data_dir(&path)?;
    }
    Ok(())
}

fn get_resolvers(
    storage_configs: &StorageConfigs,
    metastore_configs: &MetastoreConfigs,
) -> (StorageResolver, MetastoreResolver) {
    if storage_configs.is_empty() && metastore_configs.is_empty() {
        return (
            StorageResolver::unconfigured(),
            MetastoreResolver::unconfigured(),
        );
    }
    let storage_resolver = StorageResolver::configured(storage_configs);
    let metastore_resolver =
        MetastoreResolver::configured(storage_resolver.clone(), metastore_configs);
    (storage_resolver, metastore_resolver)
}

fn start_actor_runtimes(
    runtimes_config: RuntimesConfig,
    services: &HashSet<QuickwitService>,
) -> anyhow::Result<()> {
    if services.contains(&QuickwitService::Indexer)
        || services.contains(&QuickwitService::Janitor)
        || services.contains(&QuickwitService::ControlPlane)
        || services.contains(&QuickwitService::Compactor)
    {
        quickwit_common::runtimes::initialize_runtimes(runtimes_config)
            .context("failed to start actor runtimes")?;
    }
    Ok(())
}

fn ensure_data_dir(path: &PathBuf) -> anyhow::Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)
            .with_context(|| format!("cannot create data_dir `{}`", path.display()))?;
        info!(path = %path.display(), "created data_dir");
    }
    Ok(())
}

async fn listen_interrupt() {
    tokio::signal::ctrl_c()
        .await
        .expect("registering SIGINT handler");
    print!("\r");
    info!("SIGINT: graceful shutdown");
}

async fn listen_sigterm() {
    use tokio::signal::unix::{SignalKind, signal};
    signal(SignalKind::terminate())
        .expect("registering SIGTERM handler")
        .recv()
        .await;
    info!("SIGTERM: graceful shutdown");
}
