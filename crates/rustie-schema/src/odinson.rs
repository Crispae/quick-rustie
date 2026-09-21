//! Odinson document types (token fields + dependency graphs).

use serde::{Deserialize, Serialize};

use crate::graph::{DependencyEdge, SentenceGraph};

/// Complete Odinson document with sentences.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Document {
    pub id: String,
    #[serde(default)]
    pub metadata: Vec<serde_json::Value>,
    pub sentences: Vec<Sentence>,
}

/// One sentence with parallel fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(non_snake_case)]
pub struct Sentence {
    #[serde(rename = "numTokens")]
    pub numTokens: u32,
    pub fields: Vec<Field>,
}

/// Odinson sentence field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "$type")]
pub enum Field {
    #[serde(rename = "ai.lum.odinson.TokensField")]
    TokensField { name: String, tokens: Vec<String> },
    #[serde(rename = "ai.lum.odinson.GraphField")]
    GraphField {
        name: String,
        #[serde(default)]
        edges: Vec<EdgeTriple>,
        #[serde(default)]
        roots: Vec<u32>,
    },
}

/// JSON edge as `[from, to, rel]` (Odinson wire format).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "EdgeTripleDe", into = "EdgeTripleSer")]
pub struct EdgeTriple {
    pub from: u32,
    pub to: u32,
    pub rel: String,
}

impl EdgeTriple {
    pub fn into_edge(self) -> DependencyEdge {
        DependencyEdge::new(self.from, self.to, self.rel)
    }
}

#[derive(Deserialize)]
struct EdgeTripleDe(u32, u32, String);

#[derive(Serialize)]
struct EdgeTripleSer(u32, u32, String);

impl From<EdgeTripleDe> for EdgeTriple {
    fn from(EdgeTripleDe(from, to, rel): EdgeTripleDe) -> Self {
        Self { from, to, rel }
    }
}

impl From<EdgeTriple> for EdgeTripleSer {
    fn from(e: EdgeTriple) -> Self {
        Self(e.from, e.to, e.rel)
    }
}

impl Document {
    pub fn get_tokens(&self, sentence_idx: usize, field_name: &str) -> Option<&[String]> {
        let sentence = self.sentences.get(sentence_idx)?;
        for field in &sentence.fields {
            if let Field::TokensField { name, tokens } = field {
                if name == field_name {
                    return Some(tokens);
                }
            }
        }
        None
    }

    pub fn get_graph(&self, sentence_idx: usize, field_name: &str) -> Option<SentenceGraph> {
        let sentence = self.sentences.get(sentence_idx)?;
        for field in &sentence.fields {
            if let Field::GraphField { name, edges, roots } = field {
                if name == field_name {
                    return Some(SentenceGraph::new(
                        name.clone(),
                        edges.iter().cloned().map(EdgeTriple::into_edge).collect(),
                        roots.clone(),
                    ));
                }
            }
        }
        None
    }
}
