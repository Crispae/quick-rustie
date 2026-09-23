//! Flatten Odinson documents into [`SentenceDoc`]s for Quickwit.

use crate::error::{Result, SchemaError};
use crate::fields::TOKEN_FIELDS;
use crate::graph::{DependencyEdge, SentenceGraph, DEFAULT_BASIC_GRAPH_FIELD, DEFAULT_GRAPH_FIELD};
use crate::odinson::{Document, EdgeTriple, Field};
use crate::sentence::SentenceDoc;
use std::collections::BTreeMap;

/// Flatten a parsed Odinson [`Document`] into sentence docs.
///
/// - Token fields must have length `numTokens`. `|`, `,` and `\` in a token are escaped by
///   [`crate::encoding::encode_tokens`] and recovered by the `rustie_tokens` tokenizer, so they
///   no longer need to be rejected here (unlike Quickwit's stock regex tokenizer, which never
///   unescaped them).
/// - Graph fields are validated and attached; primary `dependencies` also
///   populate `incoming_edges` / `outgoing_edges` for postings.
pub fn flatten_document(doc: &Document) -> Result<Vec<SentenceDoc>> {
    let mut out = Vec::with_capacity(doc.sentences.len());

    for (sentence_idx, sentence) in doc.sentences.iter().enumerate() {
        let n = sentence.numTokens as usize;
        let mut tokens: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut graphs: BTreeMap<String, SentenceGraph> = BTreeMap::new();

        for field in &sentence.fields {
            match field {
                Field::TokensField {
                    name,
                    tokens: field_tokens,
                } => {
                    if !is_indexable_token_field(name) {
                        continue;
                    }
                    if field_tokens.len() != n {
                        return Err(SchemaError::validate(format!(
                            "Document '{}' sentence {}: field '{}' has {} tokens but numTokens is {}",
                            doc.id,
                            sentence_idx,
                            name,
                            field_tokens.len(),
                            n
                        )));
                    }
                    tokens.insert(name.clone(), field_tokens.clone());
                }
                Field::GraphField { name, edges, roots } => {
                    let graph = SentenceGraph::new(
                        name.clone(),
                        edges
                            .iter()
                            .cloned()
                            .map(EdgeTriple::into_edge)
                            .collect::<Vec<DependencyEdge>>(),
                        roots.clone(),
                    );
                    if let Err(msg) = graph.validate(n, &doc.id, sentence_idx) {
                        return Err(SchemaError::validate(msg));
                    }
                    // A relation label containing `|` or `,` (enhanced UD puts lemmas into
                    // labels, e.g. `obl:in`) is escaped by `encode_edges` and recovered by the
                    // `rustie_edges` tokenizer, so it no longer needs to be rejected here.
                    graphs.insert(name.clone(), graph);
                }
            }
        }

        if !tokens.contains_key("word") {
            return Err(SchemaError::validate(format!(
                "Document '{}' sentence {}: missing required token field 'word'",
                doc.id, sentence_idx
            )));
        }

        // Same primary-graph choice GPH2 is built from (`dependencies`, falling back to
        // `dependencies_basic`; see `rustie-leaf`'s `GraphSidecar`), so the edge postings cover
        // every label a graph traversal can actually follow.
        let (outgoing_edges, incoming_edges) = if let Some(g) = graphs
            .get(DEFAULT_GRAPH_FIELD)
            .or_else(|| graphs.get(DEFAULT_BASIC_GRAPH_FIELD))
        {
            g.edge_label_slots(n)
        } else {
            (Vec::new(), Vec::new())
        };

        out.push(SentenceDoc {
            doc_id: doc.id.clone(),
            sentence_id: format!("{}_{}", doc.id, sentence_idx),
            sentence_length: sentence.numTokens as u64,
            tokens,
            graphs,
            outgoing_edges,
            incoming_edges,
        });
    }

    Ok(out)
}

fn is_indexable_token_field(name: &str) -> bool {
    TOKEN_FIELDS.iter().any(|f| *f == name)
}

/// Parse Odinson JSON and flatten to sentence docs.
pub fn flatten_odinson_json(json: &str) -> Result<Vec<SentenceDoc>> {
    let doc: Document = serde_json::from_str(json)?;
    flatten_document(&doc)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "id": "doc1",
      "metadata": [],
      "sentences": [
        {
          "numTokens": 3,
          "fields": [
            {
              "name": "word",
              "$type": "ai.lum.odinson.TokensField",
              "tokens": ["The", "cat", "sat"]
            },
            {
              "name": "pos",
              "$type": "ai.lum.odinson.TokensField",
              "tokens": ["DT", "NN", "VBD"]
            },
            {
              "name": "dependencies",
              "$type": "ai.lum.odinson.GraphField",
              "edges": [[1, 0, "det"], [2, 1, "nsubj"]],
              "roots": [2]
            }
          ]
        }
      ]
    }"#;

    #[test]
    fn flattens_tokens_and_graph() {
        let docs = flatten_odinson_json(SAMPLE).unwrap();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].doc_id, "doc1");
        assert_eq!(docs[0].sentence_id, "doc1_0");
        assert_eq!(docs[0].sentence_length, 3);
        assert_eq!(
            docs[0].tokens.get("word").unwrap(),
            &vec!["The".to_string(), "cat".to_string(), "sat".to_string()]
        );
        assert!(docs[0].tokens.contains_key("pos"));

        let g = docs[0].primary_graph().expect("dependencies");
        assert_eq!(g.edges.len(), 2);
        assert_eq!(g.roots, vec![2]);
        assert_eq!(docs[0].outgoing_edges[1], vec!["det".to_string()]);
        assert_eq!(docs[0].incoming_edges[0], vec!["det".to_string()]);
        assert_eq!(docs[0].outgoing_edges[2], vec!["nsubj".to_string()]);
        // Token 2 ("sat") is the root: `edge_label_slots` adds "root" to its incoming labels.
        assert_eq!(docs[0].incoming_edges[2], vec!["root".to_string()]);

        assert!(docs[0].validate().is_ok());

        let json = docs[0].to_quickwit_json();
        assert_eq!(json["word"], "The|cat|sat");
        assert_eq!(json["pos"], "DT|NN|VBD");
        assert!(json.get("dependencies").is_some());
        assert_eq!(json["incoming_edges"], "det|nsubj|root");
        assert_eq!(json["outgoing_edges"], "|det|nsubj");

        let adj = g.outgoing_adjacency(3);
        assert_eq!(adj[2][0].0, 1);
        assert_eq!(adj[2][0].1, "nsubj");
    }

    #[test]
    fn rejects_bad_edge_endpoint() {
        let bad = r#"{
          "id": "x",
          "sentences": [{
            "numTokens": 2,
            "fields": [
              {
                "name": "word",
                "$type": "ai.lum.odinson.TokensField",
                "tokens": ["a", "b"]
              },
              {
                "name": "dependencies",
                "$type": "ai.lum.odinson.GraphField",
                "edges": [[0, 5, "nsubj"]],
                "roots": [0]
              }
            ]
          }]
        }"#;
        assert!(flatten_odinson_json(bad).is_err());
    }

    #[test]
    fn rejects_length_mismatch() {
        let bad = r#"{
          "id": "x",
          "sentences": [{
            "numTokens": 2,
            "fields": [{
              "name": "word",
              "$type": "ai.lum.odinson.TokensField",
              "tokens": ["a", "b", "c"]
            }]
          }]
        }"#;
        assert!(flatten_odinson_json(bad).is_err());
    }

    #[test]
    fn pipe_and_comma_in_token_round_trip() {
        // The stock Quickwit regex tokenizer could never unescape these, so they used to be
        // rejected outright; `rustie_tokens` (see `encoding::slot_terms`) recovers them exactly.
        let doc = r#"{
          "id": "x",
          "sentences": [{
            "numTokens": 2,
            "fields": [{
              "name": "word",
              "$type": "ai.lum.odinson.TokensField",
              "tokens": ["a|b", "1,000"]
            }]
          }]
        }"#;
        let docs = flatten_odinson_json(doc).unwrap();
        assert!(docs[0].validate().is_ok());
        let json = docs[0].to_quickwit_json();
        let encoded = json["word"].as_str().unwrap();
        assert_eq!(
            crate::encoding::decode_tokens(encoded),
            vec!["a|b".to_string(), "1,000".to_string()]
        );
    }

    #[test]
    fn pipe_in_edge_relation_round_trips() {
        let doc = r#"{
          "id": "x",
          "sentences": [{
            "numTokens": 2,
            "fields": [
              {
                "name": "word",
                "$type": "ai.lum.odinson.TokensField",
                "tokens": ["a", "b"]
              },
              {
                "name": "dependencies",
                "$type": "ai.lum.odinson.GraphField",
                "edges": [[1, 0, "nsubj|weird,rel"]],
                "roots": [1]
              }
            ]
          }]
        }"#;
        let docs = flatten_odinson_json(doc).unwrap();
        assert!(docs[0].validate().is_ok());
        assert_eq!(
            docs[0].outgoing_edges[1],
            vec!["nsubj|weird,rel".to_string()]
        );
    }
}
