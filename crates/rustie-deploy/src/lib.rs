//! Deployment config shared by `rustie-node` and `rustie-serve`.
//!
//! One YAML file describes the whole deployment (S3, index, metastore, Cachey, every node).
//! [`validate`](validate::validate) reports what is missing or risky together with what to
//! provide; [`render_node_yaml`] turns it into the Quickwit config of one node.
//! See `configs/rustie-deploy.example.yaml`.

mod config;
mod render;
mod validate;

pub use config::{
    CacheySection, DeployConfig, IndexSection, Loaded, NodeSection, SUPPORTED_VERSION,
    ServeSection, SplitCacheSection, StorageSection,
};
pub use render::{gateway_endpoint, render_node_yaml};
pub use validate::{Issue, Report, Role, Severity, redact_uri, validate};

#[cfg(test)]
mod tests;
