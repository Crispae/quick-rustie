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
use quickwit_config::{
    ChecksumAlgorithm, ConfigFormat, MetastoreConfigs, NodeConfig, S3StorageConfig, StorageConfig,
    StorageConfigs,
};
use quickwit_metastore::{
    IndexMetadataResponseExt, ListSplitsRequestExt, MetastoreResolver,
    MetastoreServiceStreamSplitsExt, SplitState,
};
use quickwit_proto::metastore::{IndexMetadataRequest, ListSplitsRequest, MetastoreService};
use rustie_deploy::{DeployConfig, Loaded, Report, Role, Severity};
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

    /// One YAML file describing the whole deployment (S3, index, metastore, Cachey, all nodes).
    /// Replaces `--config`; requires `--node-id` (or `--check`). See
    /// `configs/rustie-deploy.example.yaml`.
    #[arg(long, env = "RUSTIE_DEPLOY_CONFIG")]
    deploy_config: Option<PathBuf>,

    /// Which entry of the deploy file's `nodes:` this process is.
    #[arg(long, env = "RUSTIE_NODE_ID")]
    node_id: Option<String>,

    /// Validate `--deploy-config` (static checks, then live checks against S3, the metastore,
    /// the index and Cachey) and exit; nothing is started. Exit code 1 when there are errors.
    #[arg(long, default_value_t = false)]
    check: bool,

    /// Cachey base URL for searcher `.split` range reads (e.g. `http://127.0.0.1:9020`).
    /// Only allowed with searcher / metastore / metastore-read-replica roles.
    #[arg(long, env = "RUSTIE_CACHEY_URL")]
    cachey_url: Option<String>,
    #[arg(long)]
    cachey_c0_config: Option<String>,
    #[arg(long, default_value_t = false)]
    no_cachey_fallback: bool,
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

    // Only override when --service was passed; otherwise YAML / QW_ENABLED_SERVICES apply.
    let cli_services = parse_cli_services(&args.services)?;

    let deploy = match &args.deploy_config {
        Some(path) => Some(prepare_deploy(&args, path)?),
        None if args.check => anyhow::bail!("--check requires --deploy-config"),
        None => None,
    };
    if args.check {
        let (loaded, report) = deploy.expect("checked above");
        let label = args.deploy_config.as_ref().map(|p| p.display().to_string()).unwrap_or_default();
        return run_check(&loaded, report, &label).await;
    }

    let (config_content, config_format, config_label) = match &deploy {
        Some((loaded, _)) => {
            let node_id = args.node_id.as_deref().expect("validated by prepare_deploy");
            (
                rustie_deploy::render_node_yaml(&loaded.config, node_id)?.into_bytes(),
                ConfigFormat::Yaml,
                format!("deploy:{}#{node_id}", args.deploy_config.as_ref().unwrap().display()),
            )
        }
        None => {
            let config_uri = Uri::from_str(&args.config)
                .with_context(|| format!("invalid --config `{}`", args.config))?;
            let content = load_file(&StorageResolver::unconfigured(), &config_uri)
                .await
                .context("failed to load node config")?;
            (content.as_slice().to_vec(), ConfigFormat::sniff_from_uri(&config_uri)?, config_uri.to_string())
        }
    };
    // NodeConfig::load requires data_dir to exist already.
    ensure_data_dir_from_config_bytes(&config_content)?;

    let node_config = NodeConfig::load_with_enabled_services(
        config_format,
        config_content.as_slice(),
        cli_services.as_ref(),
    )
    .await
    .with_context(|| format!("failed to parse node config `{config_label}`"))?;

    warn_if_no_local_metastore(&node_config.enabled_services);
    info!(
        services = %node_config.enabled_services.iter().join(", "),
        config = %config_label,
        "starting rustie-node"
    );

    let deploy_cachey = deploy
        .as_ref()
        .map(|(l, _)| &l.config.cachey)
        .filter(|c| c.enabled);
    let cachey_cfg = match (&args.cachey_url, deploy_cachey) {
        (None, None) => None,
        (url, section) => {
            ensure_cachey_services_allowed(&node_config.enabled_services)?;
            let url = url.clone().or_else(|| section.map(|c| c.url.clone())).expect("one is set");
            let mut cfg = rustie_cachey::CacheyConfig::new(
                url.parse().with_context(|| format!("invalid cachey url `{url}`"))?,
            );
            cfg.c0_config = args
                .cachey_c0_config
                .clone()
                .or_else(|| section.and_then(|c| c.c0_config.clone()));
            cfg.fallback = !args.no_cachey_fallback && section.is_none_or(|c| c.fallback);
            Some(cfg)
        }
    };
    if let Some(cfg) = &cachey_cfg {
        rustie_cachey::check_stats(cfg)
            .await
            .context("cachey /stats check failed")?;
    }

    let (storage_resolver, metastore_resolver) = get_resolvers(
        &node_config.storage_configs,
        &node_config.metastore_configs,
        cachey_cfg.as_ref(),
    );

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

/// Load, validate and report the deploy file. Warnings are always printed; errors abort
/// unless `--check` (which prints them itself and continues to the live checks).
fn prepare_deploy(args: &Args, path: &std::path::Path) -> anyhow::Result<(Loaded, Report)> {
    let loaded = DeployConfig::load(path)?;
    let role = match (&args.node_id, args.check) {
        (Some(id), _) => Role::Node { node_id: id.clone() },
        (None, true) => Role::All,
        (None, false) => {
            let ids: Vec<_> = loaded.config.nodes.iter().map(|n| n.node_id.as_str()).collect();
            anyhow::bail!(
                "--deploy-config needs --node-id (or RUSTIE_NODE_ID) to pick which node this is; \
                 available: [{}]",
                ids.join(", ")
            );
        }
    };
    let report = rustie_deploy::validate(&loaded, &role);
    if !args.check {
        if !report.issues.is_empty() {
            eprint!("{}", report.render(&path.display().to_string()));
        }
        if report.has_errors() {
            anyhow::bail!("invalid deploy config: fix the errors above (or run with --check)");
        }
    }
    Ok((loaded, report))
}

/// `--check`: static report + live checks. Errors => exit code 1.
async fn run_check(loaded: &Loaded, mut report: Report, label: &str) -> anyhow::Result<()> {
    let mut notes = Vec::new();
    if report.has_errors() {
        notes.push("live checks skipped: fix the errors first".to_string());
    } else {
        live_checks(&loaded.config, &mut report, &mut notes).await;
    }
    eprint!("{}", report.render(label));
    for n in &notes {
        eprintln!("  [INFO ] {n}");
    }
    if report.has_errors() {
        anyhow::bail!("deploy config check failed");
    }
    eprintln!("check passed");
    Ok(())
}

fn s3_config(cfg: &DeployConfig) -> S3StorageConfig {
    S3StorageConfig {
        endpoint: (!cfg.storage.endpoint.trim().is_empty()).then(|| cfg.storage.endpoint.clone()),
        region: Some(cfg.storage.region.clone()),
        access_key_id: Some(cfg.storage.access_key_id.clone()),
        secret_access_key: Some(cfg.storage.secret_access_key.clone()),
        force_path_style_access: cfg.effective_path_style(),
        checksum_algorithm: match cfg.effective_checksum() {
            "md5" => ChecksumAlgorithm::Md5,
            "disabled" => ChecksumAlgorithm::Disabled,
            _ => ChecksumAlgorithm::Crc32c,
        },
        ..Default::default()
    }
}

const LIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

fn live_error(report: &mut Report, field: &str, what: &str, err: impl std::fmt::Display, fix: &str) {
    report.push(Severity::Error, field, format!("{what}: {err}"), fix);
}

/// Talks to the real services with the file's own settings, without starting a node.
async fn live_checks(cfg: &DeployConfig, report: &mut Report, notes: &mut Vec<String>) {
    let s3 = s3_config(cfg);
    let storage_resolver =
        StorageResolver::configured(&StorageConfigs::new(vec![StorageConfig::S3(s3.clone())]));

    // 1. Bucket reachable with these credentials / addressing.
    let bucket_uri = match Uri::from_str(&format!("s3://{}/", cfg.storage.bucket)) {
        Ok(u) => u,
        Err(err) => return live_error(report, "storage.bucket", "invalid bucket URI", err, "check the bucket name"),
    };
    match tokio::time::timeout(LIVE_TIMEOUT, async {
        storage_resolver.resolve(&bucket_uri).await?.check_connectivity().await
    })
    .await
    {
        Ok(Ok(())) => notes.push(format!("S3: bucket `{}` reachable", cfg.storage.bucket)),
        Ok(Err(err)) => {
            return live_error(
                report,
                "storage",
                "cannot reach the bucket",
                format!("{err:#}"),
                "check storage.endpoint / region / access keys / path_style, and that the bucket exists",
            );
        }
        Err(_) => {
            return live_error(report, "storage", "timed out reaching the bucket", "", "check network access to storage.endpoint from this machine");
        }
    }

    // 2. Metastore reachable (read-only open: no migrations).
    let metastore_uri = match Uri::from_str(&cfg.metastore_uri) {
        Ok(u) => u,
        Err(err) => return live_error(report, "metastore_uri", "invalid URI", err, "postgres://user:pass@host:5432/db"),
    };
    let metastore_resolver =
        MetastoreResolver::configured(storage_resolver.clone(), &MetastoreConfigs::default());
    let metastore = match tokio::time::timeout(LIVE_TIMEOUT, metastore_resolver.resolve_read_only(&metastore_uri)).await {
        Ok(Ok(m)) => {
            notes.push("metastore: reachable".to_string());
            m
        }
        Ok(Err(err)) => {
            return live_error(report, "metastore_uri", "cannot open the metastore", err, "check host/port/credentials, and that this machine can reach the database");
        }
        Err(_) => {
            return live_error(report, "metastore_uri", "timed out opening the metastore", "", "check network access to the database from this machine");
        }
    };

    // 3. The index exists and has published splits.
    let index_id = &cfg.index.id;
    let metadata = match metastore
        .index_metadata(IndexMetadataRequest::for_index_id(index_id.clone()))
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r.deserialize_index_metadata().map_err(anyhow::Error::from))
    {
        Ok(m) => m,
        Err(err) => {
            return live_error(report, "index.id", &format!("index `{index_id}` not found in the metastore"), format!("{err:#}"), "check index.id, or index the data first with rustie-index against this metastore");
        }
    };
    let index_uri = metadata.index_uri().clone();
    let splits = match ListSplitsRequest::try_from_index_uid(metadata.index_uid.clone()) {
        Ok(req) => match metastore.list_splits(req).await {
            Ok(stream) => stream.collect_splits().await.unwrap_or_default(),
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    };
    let published: Vec<_> = splits.iter().filter(|s| s.split_state == SplitState::Published).collect();
    if published.is_empty() {
        report.push(Severity::Warning, "index.id", format!("index `{index_id}` has no published splits"), "index data into it before searching");
    } else {
        let docs: usize = published.iter().map(|s| s.split_metadata.num_docs).sum();
        notes.push(format!("index `{index_id}`: {} published splits, {docs} docs, uri {index_uri}", published.len()));
    }
    if !index_uri.as_str().starts_with(&format!("s3://{}/", cfg.storage.bucket)) {
        report.push(
            Severity::Warning,
            "storage.bucket",
            format!("the index lives at {index_uri}, outside bucket `{}`", cfg.storage.bucket),
            "the credentials must be able to read that bucket too; check storage.bucket",
        );
    }

    // 4. Cachey: /stats, then the same footer probe rustie-serve runs at startup.
    if cfg.cachey.enabled {
        let mut ccfg = match cfg.cachey.url.parse() {
            Ok(url) => rustie_cachey::CacheyConfig::new(url),
            Err(err) => return live_error(report, "cachey.url", "invalid URL", err, "e.g. http://host:9020"),
        };
        ccfg.c0_config = cfg.cachey.c0_config.clone();
        ccfg.fallback = false; // a check must not hide a broken Cachey behind the fallback
        let direct = match storage_resolver.resolve(&index_uri).await {
            Ok(d) => d,
            Err(err) => return live_error(report, "index", "cannot open the index storage", err, "check storage settings"),
        };
        let probe = match published.first() {
            Some(split) => {
                let path = std::path::PathBuf::from(format!("{}.split", split.split_metadata.split_id));
                match direct.file_num_bytes(&path).await {
                    Ok(len) if len > 0 => {
                        let end = len as usize;
                        Some((path, end.saturating_sub(64 * 1024)..end))
                    }
                    _ => None,
                }
            }
            None => None,
        };
        let probe_ref = probe.as_ref().map(|(p, r)| (p.as_path(), r.clone()));
        match tokio::time::timeout(LIVE_TIMEOUT, rustie_cachey::check(&ccfg, &s3, &index_uri, direct, probe_ref)).await {
            Ok(Ok(())) => notes.push(format!(
                "cachey: {} reachable{}",
                cfg.cachey.url,
                if probe.is_some() { ", footer bytes identical to direct S3" } else { " (no split to probe)" }
            )),
            Ok(Err(err)) => live_error(report, "cachey", "check failed", format!("{err:#}"), "start Cachey, and make sure it points at the same S3 endpoint/bucket/credentials (AWS_ENDPOINT_URL, AWS_*), reachable from this machine"),
            Err(_) => live_error(report, "cachey", "timed out", "", "check network access to cachey.url"),
        }
    }
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
    cachey: Option<&rustie_cachey::CacheyConfig>,
) -> (StorageResolver, MetastoreResolver) {
    let unconfigured = storage_configs.is_empty() && metastore_configs.is_empty();
    let base = if unconfigured {
        StorageResolver::unconfigured()
    } else {
        StorageResolver::configured(storage_configs)
    };
    let storage_resolver = match cachey {
        Some(cfg) => {
            if unconfigured {
                warn!(
                    "--cachey-url set but the node config has no `storage:` / `metastore:` \
                     section; wrapping the default S3 config (no MinIO path-style addressing)"
                );
            }
            let s3 = storage_configs.find_s3().cloned().unwrap_or_default();
            rustie_cachey::storage_resolver(base, s3, cfg.clone())
        }
        None => base,
    };
    let metastore_resolver = if unconfigured {
        MetastoreResolver::unconfigured()
    } else {
        MetastoreResolver::configured(storage_resolver.clone(), metastore_configs)
    };
    (storage_resolver, metastore_resolver)
}

fn ensure_cachey_services_allowed(services: &HashSet<QuickwitService>) -> anyhow::Result<()> {
    let allowed = |s: &QuickwitService| {
        matches!(
            s,
            QuickwitService::Searcher
                | QuickwitService::Metastore
                | QuickwitService::MetastoreReadReplica
        )
    };
    let bad: Vec<_> = services.iter().filter(|s| !allowed(s)).collect();
    if bad.is_empty() {
        return Ok(());
    }
    anyhow::bail!(
        "--cachey-url is only allowed on searcher/metastore nodes; refusing services [{}]. \
         Run a separate searcher-only node for Cachey.",
        bad.iter().join(", ")
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The YAML rendered from a deploy file must load through Quickwit's real `NodeConfig`
    /// loader, with the S3 settings (explicit region / path-style / checksum) intact.
    #[tokio::test]
    async fn rendered_node_config_loads_through_quickwit() {
        let data_dir = std::env::temp_dir().join(format!("rustie-deploy-test-{}", std::process::id()));
        std::fs::create_dir_all(&data_dir).unwrap();
        let text = format!(
            r#"
version: 1
cluster_id: rustie-cluster
storage:
  endpoint: https://s3.example.com
  region: eu2
  bucket: wiki-bucket
  access_key_id: AK
  secret_access_key: SK
  path_style: true
index: {{ id: wiki5k }}
metastore_uri: postgres://u:p@10.0.0.5:5432/db
split_cache: {{ enabled: true, max_gb: 5 }}
nodes:
  - {{ node_id: n1, host: 10.0.0.1, data_dir: "{d}/n1" }}
  - {{ node_id: n2, host: 10.0.0.2, data_dir: "{d}/n2" }}
"#,
            d = data_dir.display()
        );
        let loaded = DeployConfig::parse(&text).unwrap();
        let yaml = rustie_deploy::render_node_yaml(&loaded.config, "n1").unwrap();
        ensure_data_dir_from_config_bytes(yaml.as_bytes()).unwrap();
        let node = NodeConfig::load_with_enabled_services(ConfigFormat::Yaml, yaml.as_bytes(), None)
            .await
            .unwrap_or_else(|err| panic!("{err:#}\n--- rendered ---\n{yaml}"));
        assert_eq!(node.node_id.as_str(), "n1");
        assert_eq!(node.peer_seeds, vec!["10.0.0.2:7282".to_string()]);
        let s3 = node.storage_configs.find_s3().expect("s3 config");
        assert_eq!(s3.region.as_deref(), Some("eu2"));
        assert_eq!(s3.endpoint.as_deref(), Some("https://s3.example.com"));
        assert!(s3.force_path_style_access);
        assert_eq!(s3.checksum_algorithm, ChecksumAlgorithm::Md5);
        assert!(node.enabled_services.contains(&QuickwitService::Searcher));
        let _ = std::fs::remove_dir_all(&data_dir);
    }
}
