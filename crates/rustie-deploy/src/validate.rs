//! Validation: every problem carries the field, what is wrong and *what to provide*.

use std::collections::{BTreeSet, HashSet};
use std::fmt::Write as _;
use std::net::SocketAddr;

use url::Url;

use crate::config::{DeployConfig, Loaded, SUPPORTED_VERSION};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// The process refuses to start.
    Error,
    /// Starts, but something is probably wrong or risky.
    Warning,
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub severity: Severity,
    pub field: String,
    pub message: String,
    /// What the operator should provide / change.
    pub fix: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub issues: Vec<Issue>,
}

impl Report {
    pub fn has_errors(&self) -> bool {
        self.issues.iter().any(|i| i.severity == Severity::Error)
    }

    pub fn errors(&self) -> impl Iterator<Item = &Issue> {
        self.issues.iter().filter(|i| i.severity == Severity::Error)
    }

    pub fn push(
        &mut self,
        severity: Severity,
        field: &str,
        message: impl Into<String>,
        fix: impl Into<String>,
    ) {
        self.issues.push(Issue {
            severity,
            field: field.to_string(),
            message: message.into(),
            fix: fix.into(),
        });
    }

    fn err(&mut self, field: &str, message: impl Into<String>, fix: impl Into<String>) {
        self.push(Severity::Error, field, message, fix);
    }

    fn warn(&mut self, field: &str, message: impl Into<String>, fix: impl Into<String>) {
        self.push(Severity::Warning, field, message, fix);
    }

    /// Human-readable, errors first. Never contains secret values.
    pub fn render(&self, source: &str) -> String {
        let mut out = String::new();
        let n_err = self.errors().count();
        let n_warn = self.issues.len() - n_err;
        let _ = writeln!(
            out,
            "deploy config `{source}`: {n_err} error(s), {n_warn} warning(s)"
        );
        for sev in [Severity::Error, Severity::Warning] {
            for i in self.issues.iter().filter(|i| i.severity == sev) {
                let tag = if sev == Severity::Error {
                    "ERROR"
                } else {
                    "WARN "
                };
                let _ = writeln!(out, "  [{tag}] {}: {}", i.field, i.message);
                let _ = writeln!(out, "          -> {}", i.fix);
            }
        }
        if n_err == 0 && n_warn == 0 {
            let _ = writeln!(out, "  all checks passed");
        }
        out
    }
}

/// Which process is validating, so role-specific requirements are checked.
#[derive(Debug, Clone)]
pub enum Role {
    /// `rustie-node --node-id <id>`.
    Node { node_id: String },
    /// `rustie-serve`.
    Serve,
    /// `--check` without a node selected: validate everything.
    All,
}

const KNOWN_SERVICES: &[&str] = &[
    "searcher",
    "metastore",
    "metastore_read_replica",
    "indexer",
    "janitor",
    "control_plane",
    "compactor",
];
const CACHEY_SAFE_SERVICES: &[&str] = &["searcher", "metastore", "metastore_read_replica"];

fn is_loopback_or_any(host: &str) -> bool {
    matches!(host, "localhost" | "127.0.0.1" | "::1" | "0.0.0.0" | "::") || host.starts_with("127.")
}

fn is_placeholder(v: &str) -> bool {
    let l = v.to_ascii_lowercase();
    l.contains("change_me")
        || l.contains("changeme")
        || l.contains("<") && l.contains(">")
        || l == "todo"
}

/// Validate a loaded config. `loaded.issues` (unset env vars) are included.
pub fn validate(loaded: &Loaded, role: &Role) -> Report {
    let cfg = &loaded.config;
    let mut r = Report {
        issues: loaded.issues.clone(),
    };

    if cfg.version != SUPPORTED_VERSION {
        r.err(
            "version",
            format!("must be {SUPPORTED_VERSION} (got {})", cfg.version),
            format!("add `version: {SUPPORTED_VERSION}` at the top of the file"),
        );
    }
    require(
        &mut r,
        "cluster_id",
        &cfg.cluster_id,
        "a name shared by every node, e.g. `rustie-cluster`",
    );

    check_storage(&mut r, cfg, loaded);
    check_index_and_metastore(&mut r, cfg, loaded);
    check_nodes(&mut r, cfg, role);
    check_caches(&mut r, cfg, role);
    check_serve(&mut r, cfg, role);
    r
}

fn require(r: &mut Report, field: &str, value: &str, fix: &str) -> bool {
    // An unset ${VAR} was already reported for this field with the exact variable to export.
    if r.issues
        .iter()
        .any(|i| i.field == field && i.severity == Severity::Error)
    {
        return false;
    }
    if value.trim().is_empty() {
        r.err(field, "is required but empty", format!("provide {fix}"));
        return false;
    }
    if is_placeholder(value) {
        r.err(
            field,
            "still contains a placeholder",
            format!("replace it with {fix}"),
        );
        return false;
    }
    true
}

fn check_storage(r: &mut Report, cfg: &DeployConfig, loaded: &Loaded) {
    let s = &cfg.storage;
    if s.endpoint.trim().is_empty() {
        r.warn(
            "storage.endpoint",
            "empty: the AWS SDK default endpoint (real AWS S3) will be used",
            "set your provider's S3 URL if you are not on AWS (Contabo, MinIO, ...)",
        );
    } else {
        let already = r.issues.iter().any(|i| i.field == "storage.endpoint");
        match Url::parse(&s.endpoint) {
            _ if already => {}
            _ if is_placeholder(&s.endpoint) => {
                r.err(
                    "storage.endpoint",
                    "still contains a placeholder",
                    "replace it with the real endpoint URL",
                );
            }
            Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => {
                if u.scheme() == "http" && !u.host_str().is_some_and(is_loopback_or_any) {
                    r.warn(
                        "storage.endpoint",
                        "plain http to a remote host sends credentials unencrypted",
                        "use an https:// endpoint",
                    );
                }
            }
            _ => r.err(
                "storage.endpoint",
                format!("`{}` is not a valid http(s) URL", s.endpoint),
                "use the full URL, e.g. https://<region>.<provider>.com",
            ),
        }
    }
    require(
        r,
        "storage.region",
        &s.region,
        "the region name your provider signs requests with",
    );
    if require(
        r,
        "storage.bucket",
        &s.bucket,
        "the bucket that holds the index splits",
    ) {
        let ok = (3..=63).contains(&s.bucket.len())
            && s.bucket
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.');
        if !ok {
            r.warn(
                "storage.bucket",
                format!(
                    "`{}` is not a valid S3 bucket name (3-63 chars, lowercase, digits, `-`, `.`)",
                    s.bucket
                ),
                "check the bucket name in your provider's console",
            );
        }
    }
    require(
        r,
        "storage.access_key_id",
        &s.access_key_id,
        "the S3 access key, e.g. `${S3_ACCESS_KEY}` with the variable exported",
    );
    require(
        r,
        "storage.secret_access_key",
        &s.secret_access_key,
        "the S3 secret key, e.g. `${S3_SECRET_KEY}` with the variable exported",
    );
    for field in loaded
        .literal_secrets
        .iter()
        .filter(|f| **f != "metastore_uri")
    {
        r.warn(
            field,
            "a credential is written literally in this file",
            "reference an environment variable instead (`${NAME}`) so the file can be committed / shared",
        );
    }
    if let Some(c) = &s.checksum
        && !["crc32c", "md5", "disabled"].contains(&c.as_str())
    {
        r.err(
            "storage.checksum",
            format!("`{c}` is not supported"),
            "use one of: crc32c, md5, disabled",
        );
    }
    if !cfg.is_aws() {
        match s.path_style {
            None => r.warn(
                "storage.path_style",
                "not set on a non-AWS endpoint; defaulting to path-style (`endpoint/bucket/key`)",
                "set `path_style: true` explicitly (required by Contabo and MinIO; Cachey needs it too)",
            ),
            Some(false) => r.warn(
                "storage.path_style",
                "virtual-host style on a non-AWS endpoint usually fails (DNS / TLS for `<bucket>.<endpoint>`)",
                "set `path_style: true` unless your provider documents virtual-host addressing",
            ),
            Some(true) => {}
        }
    }
}

fn check_index_and_metastore(r: &mut Report, cfg: &DeployConfig, loaded: &Loaded) {
    require(
        r,
        "index.id",
        &cfg.index.id,
        "the id of the index to search, e.g. `wiki5k`",
    );
    if let Some(root) = &cfg.index.root_uri
        && !root.starts_with("s3://")
    {
        r.err(
            "index.root_uri",
            format!("`{root}` must start with s3://"),
            "e.g. s3://<bucket>/indexes",
        );
    } else if let Some(root) = &cfg.index.root_uri
        && !cfg.storage.bucket.is_empty()
        && !root.starts_with(&format!("s3://{}/", cfg.storage.bucket))
        && root != &format!("s3://{}", cfg.storage.bucket)
    {
        r.warn(
            "index.root_uri",
            "points at a different bucket than storage.bucket",
            "make sure the index really lives in that bucket, and the credentials can read it",
        );
    }

    if !require(
        r,
        "metastore_uri",
        &cfg.metastore_uri,
        "the metastore holding the index metadata, e.g. postgres://user:pass@host:5432/db",
    ) {
        return;
    }
    match Url::parse(&cfg.metastore_uri) {
        Ok(u) if u.scheme().starts_with("postgres") => {
            if u.host_str().is_none() {
                r.err(
                    "metastore_uri",
                    "has no host",
                    "postgres://user:pass@HOST:PORT/DB",
                );
            }
            let remote_nodes = distinct_hosts(cfg) > 1;
            if remote_nodes && u.host_str().is_some_and(is_loopback_or_any) {
                r.err(
                    "metastore_uri",
                    "points at a loopback host but the nodes run on different machines",
                    "use an address every node can reach (a DNS name or LAN/public IP of the Postgres)",
                );
            }
            if loaded.literal_secrets.contains(&"metastore_uri") {
                r.warn(
                    "metastore_uri",
                    "the Postgres password is written literally in this file",
                    "use `postgres://user:${PG_PASSWORD}@host:5432/db` and export PG_PASSWORD",
                );
            }
        }
        Ok(u) if u.scheme() == "s3" => {
            r.warn(
                "metastore_uri",
                "the s3:// file metastore is not refreshed automatically and is not safe for concurrent writers",
                "prefer Postgres for multi-node deployments",
            );
        }
        _ => r.err(
            "metastore_uri",
            format!(
                "unsupported metastore URI scheme in `{}`",
                redact_uri(&cfg.metastore_uri)
            ),
            "use postgres://... (recommended) or s3://<bucket>/<path>",
        ),
    }
}

fn check_nodes(r: &mut Report, cfg: &DeployConfig, role: &Role) {
    if cfg.nodes.is_empty() {
        r.err(
            "nodes",
            "no nodes defined",
            "add at least one entry under `nodes:` with node_id, host, data_dir",
        );
        return;
    }
    let mut ids = HashSet::new();
    let mut endpoints = HashSet::new();
    let multi_host = distinct_hosts(cfg) > 1;
    for (i, n) in cfg.nodes.iter().enumerate() {
        let p = |f: &str| format!("nodes[{i}].{f}");
        if require(
            r,
            &p("node_id"),
            &n.node_id,
            "a unique id, e.g. `rustie-node-1`",
        ) && !ids.insert(n.node_id.clone())
        {
            r.err(
                &p("node_id"),
                format!("duplicate node_id `{}`", n.node_id),
                "give every node a unique id",
            );
        }
        if require(
            r,
            &p("host"),
            &n.host,
            "the address other nodes use to reach this node",
        ) {
            if multi_host && is_loopback_or_any(&n.host) {
                r.err(
                    &p("host"),
                    format!(
                        "`{}` is a loopback/wildcard address but nodes are on different machines",
                        n.host
                    ),
                    "use the machine's LAN / public IP or DNS name (peers gossip to this address)",
                );
            }
            for (kind, port) in [
                ("rest_port", n.rest_port),
                ("grpc_port", n.grpc_port),
                ("gossip_port", n.gossip_port),
            ] {
                if port == 0 {
                    r.err(
                        &p(kind),
                        "must be non-zero",
                        "pick a free port (defaults: 7280/7281/7282)",
                    );
                } else if !endpoints.insert((n.host.clone(), port)) {
                    r.err(
                        &p(kind),
                        format!(
                            "port {port} on `{}` is used by more than one port setting / node",
                            n.host
                        ),
                        "give each node on the same host different rest/grpc/gossip ports",
                    );
                }
            }
        }
        if n.data_dir.trim().is_empty() {
            r.err(
                &p("data_dir"),
                "is required but empty",
                "a persistent local directory for this node, e.g. /var/lib/rustie/node-1",
            );
        } else if n.data_dir.starts_with("/tmp") {
            r.warn(
                &p("data_dir"),
                "is under /tmp and may be wiped on reboot",
                "use a persistent path such as /var/lib/rustie/<node_id>",
            );
        }
        for s in &n.services {
            if !KNOWN_SERVICES.contains(&s.as_str()) {
                r.err(
                    &p("services"),
                    format!("unknown service `{s}`"),
                    format!("use: {}", KNOWN_SERVICES.join(", ")),
                );
            }
        }
        if !n.services.iter().any(|s| s == "searcher") {
            r.warn(
                &p("services"),
                "no `searcher` role: this node cannot answer searches",
                "add `searcher` unless it is intentionally a helper node",
            );
        }
        if !n
            .services
            .iter()
            .any(|s| s == "metastore" || s == "metastore_read_replica")
        {
            r.warn(
                &p("services"),
                "no `metastore` role: the node must discover a remote metastore over gRPC",
                "use `[searcher, metastore]` so every node opens the shared metastore directly",
            );
        }
    }
    if let Role::Node { node_id } = role
        && cfg.node(node_id).is_none()
    {
        let known: Vec<_> = cfg.nodes.iter().map(|n| n.node_id.as_str()).collect();
        r.err(
            "--node-id",
            format!("`{node_id}` is not defined in `nodes:`"),
            format!("pass one of: {}", known.join(", ")),
        );
    }
    if cfg.nodes.len() == 1 {
        r.warn(
            "nodes",
            "a single node: there is no fan-out or failover",
            "add nodes for a multi-node deployment",
        );
    }
}

fn check_caches(r: &mut Report, cfg: &DeployConfig, role: &Role) {
    let c = &cfg.cachey;
    if c.enabled {
        if require(
            r,
            "cachey.url",
            &c.url,
            "the base URL of your Cachey, e.g. http://cachey.internal:9020",
        ) {
            match Url::parse(&c.url) {
                Ok(u) if matches!(u.scheme(), "http" | "https") && u.host_str().is_some() => {
                    if distinct_hosts(cfg) > 1 && u.host_str().is_some_and(is_loopback_or_any) {
                        r.err(
                            "cachey.url",
                            "points at a loopback host but nodes run on different machines",
                            "use an address every node can reach",
                        );
                    }
                }
                _ => r.err(
                    "cachey.url",
                    format!("`{}` is not a valid http(s) URL", c.url),
                    "e.g. http://127.0.0.1:9020",
                ),
            }
        }
        if !cfg.effective_path_style() && !cfg.is_aws() {
            r.warn(
                "cachey",
                "Cachey needs path-style addressing on non-AWS endpoints",
                "set storage.path_style: true",
            );
        }
        if cfg.split_cache.enabled {
            r.warn(
                "cachey / split_cache",
                "both are enabled; they are alternative disk layers and dilute each other",
                "enable only one: Cachey for several/stateless searchers, split_cache for one searcher with a local disk",
            );
        }
        for (i, n) in cfg.nodes.iter().enumerate() {
            if let Some(bad) = n
                .services
                .iter()
                .find(|s| !CACHEY_SAFE_SERVICES.contains(&s.as_str()))
            {
                r.err(
                    &format!("nodes[{i}].services"),
                    format!("`{bad}` is not allowed on a node that reads through Cachey (it would route indexer/janitor I/O through it)"),
                    "run indexer/janitor on a separate node with cachey disabled, or drop the role",
                );
            }
        }
    }
    let s = &cfg.split_cache;
    if s.enabled {
        if s.max_gb == 0 {
            r.err("split_cache.max_gb", "must be > 0", "e.g. 10");
        }
        if s.max_splits == 0 {
            r.err("split_cache.max_splits", "must be > 0", "e.g. 10000");
        }
        if matches!(role, Role::Serve)
            && cfg.serve.gateway_node.is_none()
            && s.dir.as_deref().is_none_or(|d| d.trim().is_empty())
        {
            r.err(
                "split_cache.dir",
                "required for rustie-serve embedded mode",
                "a persistent directory, e.g. /var/lib/rustie/split-cache",
            );
        }
    }
}

fn check_serve(r: &mut Report, cfg: &DeployConfig, role: &Role) {
    if !matches!(role, Role::Serve | Role::All) {
        return;
    }
    if matches!(role, Role::Serve) {
        if cfg.storage.endpoint.trim().is_empty() {
            r.err(
                "storage.endpoint",
                "rustie-serve needs an explicit S3 endpoint (its storage layer is the S3-compatible/MinIO one)",
                "set storage.endpoint to your provider's URL",
            );
        }
        if cfg.storage.path_style == Some(false) {
            r.warn(
                "storage.path_style",
                "rustie-serve always uses path-style addressing and ignores `path_style: false`",
                "keep `path_style: true` (nodes and serve then agree)",
            );
        }
        if cfg.storage.checksum.as_deref().is_some_and(|c| c != "md5") {
            r.warn(
                "storage.checksum",
                "rustie-serve always uses md5 upload checksums; the setting only applies to nodes",
                "no action needed unless you index through rustie-serve's storage layer",
            );
        }
    }
    match cfg.serve.bind.parse::<SocketAddr>() {
        Ok(a) if !a.ip().is_loopback() => r.warn(
            "serve.bind",
            "listens on a non-loopback address and the API has no authentication",
            "put it behind a reverse proxy / firewall, or bind 127.0.0.1",
        ),
        Ok(_) => {}
        Err(_) => r.err(
            "serve.bind",
            format!("`{}` is not a host:port socket address", cfg.serve.bind),
            "e.g. 127.0.0.1:8080",
        ),
    }
    match &cfg.serve.gateway_node {
        Some(id) if cfg.node(id).is_none() => {
            let known: Vec<_> = cfg.nodes.iter().map(|n| n.node_id.as_str()).collect();
            r.err(
                "serve.gateway_node",
                format!("`{id}` is not defined in `nodes:`"),
                format!("use one of: {}", known.join(", ")),
            );
        }
        Some(_) => {
            if cfg.cachey.enabled || cfg.split_cache.enabled {
                // Not an error: caches live on the nodes in gateway mode.
            }
        }
        None if cfg.nodes.len() > 1 => r.warn(
            "serve.gateway_node",
            "not set: rustie-serve will search embedded and will not use the cluster",
            "set it to a node_id so searches fan out across the nodes",
        ),
        None => {}
    }
}

fn distinct_hosts(cfg: &DeployConfig) -> usize {
    cfg.nodes
        .iter()
        .map(|n| n.host.as_str())
        .filter(|h| !h.is_empty())
        .collect::<BTreeSet<_>>()
        .len()
}

/// `postgres://user:pass@h/db` → `postgres://user:***@h/db`.
pub fn redact_uri(uri: &str) -> String {
    match Url::parse(uri) {
        Ok(mut u) if u.password().is_some() => {
            let _ = u.set_password(Some("***"));
            u.to_string()
        }
        _ => uri.to_string(),
    }
}
