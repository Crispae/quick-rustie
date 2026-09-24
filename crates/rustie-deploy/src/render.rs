//! Render a per-node Quickwit config (YAML) from the deployment file.

use anyhow::{Context, bail};
use serde_json::json;

use crate::config::DeployConfig;

/// Quickwit `NodeConfig` YAML for `node_id`. Peers are every other node's gossip address.
///
/// Uses `force_path_style_access` / `region` / `checksum_algorithm` explicitly instead of
/// `flavor: minio`, because the MinIO flavor overwrites the region with `minio`.
pub fn render_node_yaml(cfg: &DeployConfig, node_id: &str) -> anyhow::Result<String> {
    let Some(node) = cfg.node(node_id) else {
        bail!("node `{node_id}` is not defined in `nodes:`");
    };
    let peers: Vec<String> = cfg
        .nodes
        .iter()
        .filter(|n| n.node_id != node.node_id)
        .map(|n| format!("{}:{}", n.host, n.gossip_port))
        .collect();

    let mut s3 = serde_json::Map::new();
    if !cfg.storage.endpoint.trim().is_empty() {
        s3.insert("endpoint".into(), json!(cfg.storage.endpoint));
    }
    s3.insert("region".into(), json!(cfg.storage.region));
    s3.insert("access_key_id".into(), json!(cfg.storage.access_key_id));
    s3.insert(
        "secret_access_key".into(),
        json!(cfg.storage.secret_access_key),
    );
    s3.insert(
        "force_path_style_access".into(),
        json!(cfg.effective_path_style()),
    );
    s3.insert("checksum_algorithm".into(), json!(cfg.effective_checksum()));

    let mut doc = json!({
        "version": 0.8,
        "cluster_id": cfg.cluster_id,
        "node_id": node.node_id,
        "peer_seeds": peers,
        "listen_address": node.listen_address.clone().unwrap_or_else(|| node.host.clone()),
        "advertise_address": node.host,
        "rest": { "listen_port": node.rest_port },
        "grpc_listen_port": node.grpc_port,
        "gossip_listen_port": node.gossip_port,
        "data_dir": node.data_dir,
        "metastore_uri": cfg.metastore_uri,
        "default_index_root_uri": cfg.index_root_uri(),
        "storage": { "s3": s3 },
        "enabled_services": node.services,
    });
    if cfg.split_cache.enabled {
        doc["searcher"] = json!({
            "split_cache": {
                "max_num_bytes": format!("{}GB", cfg.split_cache.max_gb),
                "max_num_splits": cfg.split_cache.max_splits,
                "num_concurrent_downloads": 1,
            }
        });
    }
    serde_yaml::to_string(&doc).context("cannot serialize node config")
}

/// `host:grpc_port` of the node `rustie-serve` should dial in gateway mode.
pub fn gateway_endpoint(cfg: &DeployConfig) -> Option<String> {
    let id = cfg.serve.gateway_node.as_deref()?;
    let n = cfg.node(id)?;
    Some(format!("{}:{}", n.host, n.grpc_port))
}
