//! One Quickwit document = one sentence (tokens + dependency graphs).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::encoding::{encode_edges, encode_tokens};
use crate::error::{Result, SchemaError};
use crate::fields::{FIELD_INCOMING_EDGES, FIELD_OUTGOING_EDGES, TOKEN_FIELDS};
use crate::graph::SentenceGraph;

/// Flattened sentence ready for Quickwit ingest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentenceDoc {
    pub doc_id: String,
    pub sentence_id: String,
    pub sentence_length: u64,
    /// Parallel token fields (each `Vec` length == `sentence_length`).
    pub tokens: BTreeMap<String, Vec<String>>,
    /// Dependency graphs keyed by field name (`dependencies`, `dependencies_basic`, …).
    #[serde(default)]
    pub graphs: BTreeMap<String, SentenceGraph>,
    /// Outgoing edge labels per token position (from primary `dependencies` graph; see
    /// [`SentenceGraph::edge_label_slots`]). Empty (zero-length) when there is no graph;
    /// otherwise one (possibly empty) label set per token, length == `sentence_length`.
    #[serde(default)]
    pub outgoing_edges: Vec<Vec<String>>,
    /// Incoming edge labels per token position, `"root"` included for a root token. Same
    /// shape as `outgoing_edges`.
    #[serde(default)]
    pub incoming_edges: Vec<Vec<String>>,
}

impl SentenceDoc {
    /// Primary enhanced graph (`dependencies`), if present.
    pub fn primary_graph(&self) -> Option<&SentenceGraph> {
        self.graphs
            .get(crate::graph::DEFAULT_GRAPH_FIELD)
            .or_else(|| self.graphs.get(crate::graph::DEFAULT_BASIC_GRAPH_FIELD))
    }

    /// Every token vector, and both edge-label vectors when a graph is present, must have
    /// exactly `sentence_length` slots: the `rustie_tokens` / `rustie_edges` tokenizers assign
    /// each slot's Tantivy position by its index in the pipe-joined string, so a short or long
    /// vector silently shifts every later token's position instead of failing loudly.
    pub fn validate(&self) -> Result<()> {
        let n = self.sentence_length as usize;
        for (field, toks) in &self.tokens {
            if toks.len() != n {
                return Err(SchemaError::validate(format!(
                    "Document '{}' sentence '{}': field '{field}' has {} slots but \
                     sentence_length is {n}",
                    self.doc_id,
                    self.sentence_id,
                    toks.len()
                )));
            }
        }
        if self.primary_graph().is_some() {
            for (field, edges) in [
                (FIELD_OUTGOING_EDGES, &self.outgoing_edges),
                (FIELD_INCOMING_EDGES, &self.incoming_edges),
            ] {
                if edges.len() != n {
                    return Err(SchemaError::validate(format!(
                        "Document '{}' sentence '{}': field '{field}' has {} slots but \
                         sentence_length is {n}",
                        self.doc_id,
                        self.sentence_id,
                        edges.len()
                    )));
                }
            }
        }
        Ok(())
    }

    /// JSON object for Quickwit indexing.
    pub fn to_quickwit_json(&self) -> Value {
        let mut map = Map::new();
        map.insert("doc_id".into(), Value::String(self.doc_id.clone()));
        map.insert(
            "sentence_id".into(),
            Value::String(self.sentence_id.clone()),
        );
        map.insert("sentence_length".into(), json!(self.sentence_length));

        for (name, toks) in &self.tokens {
            map.insert(name.clone(), Value::String(encode_tokens(toks)));
        }

        // Position-aware edge-label postings, read by the `rustie_edges` tokenizer (see
        // `SentenceGraph::edge_label_slots`): omitted when there is no graph, same as before.
        for (field, edges) in [
            (FIELD_OUTGOING_EDGES, &self.outgoing_edges),
            (FIELD_INCOMING_EDGES, &self.incoming_edges),
        ] {
            if !edges.is_empty() {
                map.insert(field.into(), Value::String(encode_edges(edges)));
            }
        }

        // Store full graph payload(s) as JSON for in-memory traversal after retrieval.
        for (name, graph) in &self.graphs {
            map.insert(
                name.clone(),
                json!({
                    "edges": graph.edges.iter().map(|e| {
                        json!([e.from, e.to, e.rel])
                    }).collect::<Vec<_>>(),
                    "roots": graph.roots,
                }),
            );
        }

        Value::Object(map)
    }

    /// NDJSON line (one sentence document).
    pub fn to_ndjson_line(&self) -> String {
        self.to_quickwit_json().to_string()
    }

    /// Known token field names present on this sentence, in canonical order.
    pub fn present_token_fields(&self) -> Vec<&str> {
        TOKEN_FIELDS
            .iter()
            .copied()
            .filter(|f| self.tokens.contains_key(*f))
            .collect()
    }
}
