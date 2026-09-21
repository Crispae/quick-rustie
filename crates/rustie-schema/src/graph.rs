//! Dependency graph types for in-memory traversal and Quickwit storage.

use serde::{Deserialize, Serialize};

use crate::encoding::labels_by_direction;

/// Default Odinson / RustIE enhanced dependency field name.
pub const DEFAULT_GRAPH_FIELD: &str = "dependencies";

/// Optional basic-tree graph field name (RustIE `graph.basic_graph_field`).
pub const DEFAULT_BASIC_GRAPH_FIELD: &str = "dependencies_basic";

/// One directed dependency edge: `from --rel--> to` (0-based token indices).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyEdge {
    pub from: u32,
    pub to: u32,
    pub rel: String,
}

impl DependencyEdge {
    pub fn new(from: u32, to: u32, rel: impl Into<String>) -> Self {
        Self {
            from,
            to,
            rel: rel.into(),
        }
    }

    pub fn as_tuple(&self) -> (u32, u32, String) {
        (self.from, self.to, self.rel.clone())
    }
}

/// Dependency graph attached to one sentence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentenceGraph {
    /// Field name (`dependencies`, `dependencies_basic`, …).
    pub name: String,
    pub edges: Vec<DependencyEdge>,
    pub roots: Vec<u32>,
}

impl SentenceGraph {
    pub fn new(name: impl Into<String>, edges: Vec<DependencyEdge>, roots: Vec<u32>) -> Self {
        Self {
            name: name.into(),
            edges,
            roots,
        }
    }

    /// `(from, to, rel)` tuples for encoding helpers.
    pub fn edge_tuples(&self) -> Vec<(u32, u32, String)> {
        self.edges.iter().map(DependencyEdge::as_tuple).collect()
    }

    /// Outgoing and incoming labels per token position.
    pub fn labels_by_direction(&self, num_tokens: usize) -> (Vec<Vec<String>>, Vec<Vec<String>>) {
        labels_by_direction(num_tokens, &self.edge_tuples())
    }

    /// Compact adjacency for in-memory traversal: `out[from] = [(to, rel), …]`.
    pub fn outgoing_adjacency(&self, num_tokens: usize) -> Vec<Vec<(u32, String)>> {
        let mut out = vec![Vec::new(); num_tokens];
        for e in &self.edges {
            let f = e.from as usize;
            if f < num_tokens {
                out[f].push((e.to, e.rel.clone()));
            }
        }
        out
    }

    /// Compact adjacency: `inc[to] = [(from, rel), …]`.
    pub fn incoming_adjacency(&self, num_tokens: usize) -> Vec<Vec<(u32, String)>> {
        let mut inc = vec![Vec::new(); num_tokens];
        for e in &self.edges {
            let t = e.to as usize;
            if t < num_tokens {
                inc[t].push((e.from, e.rel.clone()));
            }
        }
        inc
    }

    /// Validate edge endpoints against `num_tokens`.
    pub fn validate(
        &self,
        num_tokens: usize,
        doc_id: &str,
        sentence_idx: usize,
    ) -> Result<(), String> {
        for e in &self.edges {
            if e.from as usize >= num_tokens {
                return Err(format!(
                    "Document '{doc_id}' sentence {sentence_idx}: edge {}->{}:{} has invalid 'from' index {} (token count: {num_tokens})",
                    e.from, e.to, e.rel, e.from
                ));
            }
            if e.to as usize >= num_tokens {
                return Err(format!(
                    "Document '{doc_id}' sentence {sentence_idx}: edge {}->{}:{} has invalid 'to' index {} (token count: {num_tokens})",
                    e.from, e.to, e.rel, e.to
                ));
            }
        }
        for &r in &self.roots {
            if r as usize >= num_tokens {
                return Err(format!(
                    "Document '{doc_id}' sentence {sentence_idx}: root index {r} out of range (token count: {num_tokens})"
                ));
            }
        }
        Ok(())
    }
}
