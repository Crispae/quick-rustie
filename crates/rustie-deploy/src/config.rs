//! The deployment file schema and `${ENV}` expansion.
//!
//! Every field has a default so that a missing or empty value becomes a *validation issue with
//! a fix instruction* (see [`crate::validate`]) rather than an opaque serde error.
//! Unknown keys are rejected so typos do not silently fall back to defaults.

use std::fmt;
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;

use crate::validate::{Issue, Severity};

pub const SUPPORTED_VERSION: u32 = 1;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeployConfig {
    pub version: u32,
    pub cluster_id: String,
    pub storage: StorageSection,
    pub index: IndexSection,
    /// `postgres://user:pass@host:port/db` (recommended) or an `s3://` file metastore.
    pub metastore_uri: String,
    pub cachey: CacheySection,
    pub split_cache: SplitCacheSection,
    pub nodes: Vec<NodeSection>,
    pub serve: ServeSection,
}

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StorageSection {
    /// S3 endpoint URL, e.g. `https://eu2.contabostorage.com`. Empty is only valid for AWS.
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    /// Path-style addressing (`endpoint/bucket/key`). Unset: on for non-AWS endpoints.
    pub path_style: Option<bool>,
    /// Upload checksum: `crc32c` | `md5` | `disabled`. Unset: `md5` for non-AWS endpoints.
    pub checksum: Option<String>,
}

// Never print the secret key.
impl fmt::Debug for StorageSection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StorageSection")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("access_key_id", &self.access_key_id)
            .field("secret_access_key", &"<redacted>")
            .field("path_style", &self.path_style)
            .field("checksum", &self.checksum)
            .finish()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexSection {
    /// Index id to search (e.g. `wiki5k`).
    pub id: String,
    /// Quickwit `default_index_root_uri`. Unset: `s3://{bucket}/indexes`.
    pub root_uri: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheySection {
    pub enabled: bool,
    /// Base URL every searcher can reach, e.g. `http://cachey.internal:9020`.
    pub url: String,
    /// Extra `C0-Config` overrides (space-separated), appended after `fps=…`.
    pub c0_config: Option<String>,
    /// Fall back to direct S3 when Cachey fails (default true).
    pub fallback: bool,
}

impl Default for CacheySection {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            c0_config: None,
            fallback: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SplitCacheSection {
    pub enabled: bool,
    /// Directory for `rustie-serve` embedded mode (nodes manage their own under `data_dir`).
    pub dir: Option<String>,
    pub max_gb: u64,
    pub max_splits: u32,
}

impl Default for SplitCacheSection {
    fn default() -> Self {
        Self {
            enabled: false,
            dir: None,
            max_gb: 10,
            max_splits: 10_000,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NodeSection {
    pub node_id: String,
    /// Address the *other* nodes (and `rustie-serve`) use to reach this node. Must not be a
    /// loopback address when nodes run on different machines.
    pub host: String,
    /// Bind address. Unset: same as `host`. Use `0.0.0.0` inside containers.
    pub listen_address: Option<String>,
    pub rest_port: u16,
    pub grpc_port: u16,
    pub gossip_port: u16,
    pub data_dir: String,
    /// Quickwit roles. Default: `[searcher, metastore]`.
    pub services: Vec<String>,
}

impl Default for NodeSection {
    fn default() -> Self {
        Self {
            node_id: String::new(),
            host: String::new(),
            listen_address: None,
            rest_port: 7280,
            grpc_port: 7281,
            gossip_port: 7282,
            data_dir: String::new(),
            services: vec!["searcher".into(), "metastore".into()],
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServeSection {
    /// `rustie-serve` listen address. The API has no authentication.
    pub bind: String,
    /// `node_id` whose gRPC port `rustie-serve` dials (gateway mode). Unset: embedded search.
    pub gateway_node: Option<String>,
}

impl Default for ServeSection {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".into(),
            gateway_node: None,
        }
    }
}

/// A parsed file plus problems found while loading (unset `${VAR}`s).
pub struct Loaded {
    pub config: DeployConfig,
    pub issues: Vec<Issue>,
    /// Fields whose *raw* value was a literal secret (not a `${VAR}` reference).
    pub literal_secrets: Vec<&'static str>,
}

impl DeployConfig {
    /// Read `path`, parse it and expand `${VAR}` references in every string value.
    pub fn load(path: &Path) -> anyhow::Result<Loaded> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read deploy config `{}`", path.display()))?;
        Self::parse(&text).with_context(|| format!("invalid deploy config `{}`", path.display()))
    }

    pub fn parse(text: &str) -> anyhow::Result<Loaded> {
        let mut config: DeployConfig = serde_yaml::from_str(text).map_err(|err| {
            anyhow::anyhow!(
                "{err}\n  hint: check indentation and spelling (see configs/rustie-deploy.example.yaml). \
                 Inside `{{ ... }}` one-line maps, quote values that contain `${{VAR}}`: \
                 `key: \"${{VAR}}\"`"
            )
        })?;
        let mut literal_secrets = Vec::new();
        for (field, raw) in [
            (
                "storage.secret_access_key",
                &config.storage.secret_access_key,
            ),
            ("storage.access_key_id", &config.storage.access_key_id),
        ] {
            if !raw.is_empty() && !raw.contains("${") {
                literal_secrets.push(field);
            }
        }
        // Judge the RAW string: the url crate percent-encodes `${VAR}` in the password.
        if !config.metastore_uri.contains("${")
            && postgres_password(&config.metastore_uri).is_some_and(|pw| !pw.is_empty())
        {
            literal_secrets.push("metastore_uri");
        }
        let mut ex = Expander::default();
        config.expand(&mut ex);
        Ok(Loaded {
            config,
            issues: ex.into_issues(),
            literal_secrets,
        })
    }

    fn expand(&mut self, ex: &mut Expander) {
        ex.string("cluster_id", &mut self.cluster_id);
        ex.string("metastore_uri", &mut self.metastore_uri);
        ex.string("storage.endpoint", &mut self.storage.endpoint);
        ex.string("storage.region", &mut self.storage.region);
        ex.string("storage.bucket", &mut self.storage.bucket);
        ex.string("storage.access_key_id", &mut self.storage.access_key_id);
        ex.string(
            "storage.secret_access_key",
            &mut self.storage.secret_access_key,
        );
        ex.string("index.id", &mut self.index.id);
        ex.opt("index.root_uri", &mut self.index.root_uri);
        ex.string("cachey.url", &mut self.cachey.url);
        ex.opt("cachey.c0_config", &mut self.cachey.c0_config);
        ex.opt("split_cache.dir", &mut self.split_cache.dir);
        ex.string("serve.bind", &mut self.serve.bind);
        for (i, node) in self.nodes.iter_mut().enumerate() {
            ex.string(&format!("nodes[{i}].node_id"), &mut node.node_id);
            ex.string(&format!("nodes[{i}].host"), &mut node.host);
            ex.opt(
                &format!("nodes[{i}].listen_address"),
                &mut node.listen_address,
            );
            ex.string(&format!("nodes[{i}].data_dir"), &mut node.data_dir);
        }
    }

    pub fn node(&self, node_id: &str) -> Option<&NodeSection> {
        self.nodes.iter().find(|n| n.node_id == node_id)
    }

    /// Endpoint host is AWS (or empty, meaning the SDK default AWS endpoint).
    pub fn is_aws(&self) -> bool {
        let e = self.storage.endpoint.trim();
        e.is_empty() || e.contains("amazonaws.com")
    }

    pub fn effective_path_style(&self) -> bool {
        self.storage.path_style.unwrap_or(!self.is_aws())
    }

    pub fn effective_checksum(&self) -> &str {
        match self.storage.checksum.as_deref() {
            Some(c) => c,
            None if self.is_aws() => "crc32c",
            None => "md5",
        }
    }

    pub fn index_root_uri(&self) -> String {
        self.index
            .root_uri
            .clone()
            .unwrap_or_else(|| format!("s3://{}/indexes", self.storage.bucket))
    }
}

pub(crate) fn postgres_password(uri: &str) -> Option<String> {
    let u = url::Url::parse(uri).ok()?;
    if !u.scheme().starts_with("postgres") {
        return None;
    }
    u.password().map(str::to_string)
}

/// Expands `${VAR}` in strings and remembers which variables were unset.
#[derive(Default)]
struct Expander {
    issues: Vec<Issue>,
}

impl Expander {
    fn into_issues(self) -> Vec<Issue> {
        self.issues
    }

    fn opt(&mut self, field: &str, value: &mut Option<String>) {
        if let Some(v) = value {
            self.string(field, v);
        }
    }

    fn string(&mut self, field: &str, value: &mut String) {
        if !value.contains("${") {
            return;
        }
        let mut out = String::with_capacity(value.len());
        let mut rest = value.as_str();
        while let Some(start) = rest.find("${") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let Some(end) = after.find('}') else {
                out.push_str(&rest[start..]);
                rest = "";
                break;
            };
            let name = &after[..end];
            match std::env::var(name) {
                Ok(v) if !v.is_empty() => out.push_str(&v),
                _ => self.issues.push(Issue {
                    severity: Severity::Error,
                    field: field.to_string(),
                    message: format!("environment variable `{name}` is not set (or empty)"),
                    fix: format!(
                        "export {name}=<value> before starting, or put the value in the file"
                    ),
                }),
            }
            rest = &after[end + 1..];
        }
        out.push_str(rest);
        *value = out;
    }
}
