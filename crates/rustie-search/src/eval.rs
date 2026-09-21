//! Stored Quickwit sentence → in-memory match evaluation.

use std::collections::HashMap;

use rustie_compiler::matching::SpanProg;
use rustie_compiler::{
    CompiledQuery, DEFAULT_SENTENCE_CAP, EndpointSpan, GraphCompiled, SurfacePlan,
    evaluate_on_sentence,
};
use rustie_schema::{
    DEFAULT_BASIC_GRAPH_FIELD, DEFAULT_GRAPH_FIELD, DependencyEdge, SentenceGraph, TOKEN_FIELDS,
    decode_tokens_from_quickwit,
};
use serde_json::Value;

use crate::error::{Result, SearchError};
use crate::model::{CaptureOut, MatchOut, SentenceHit, SpanOut};

/// A sentence as stored in the index (`stored: true` on every field).
pub struct StoredSentence {
    pub doc_id: String,
    pub sentence_id: String,
    pub sentence_length: u64,
    pub tokens: HashMap<String, Vec<String>>,
    pub graph: Option<SentenceGraph>,
}

impl StoredSentence {
    pub fn from_hit_json(json: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(json).ok()?;
        let text = |key: &str| first_string(value.get(key)).map(str::to_string);

        let mut tokens = HashMap::new();
        for field in TOKEN_FIELDS {
            if let Some(encoded) = first_string(value.get(*field)) {
                tokens.insert((*field).to_string(), decode_tokens_from_quickwit(encoded));
            }
        }
        let graph = [DEFAULT_GRAPH_FIELD, DEFAULT_BASIC_GRAPH_FIELD]
            .into_iter()
            .find_map(|name| parse_graph(name, value.get(name)?));
        Some(Self {
            doc_id: text("doc_id")?,
            sentence_id: text("sentence_id")?,
            sentence_length: first_u64(value.get("sentence_length"))?,
            tokens,
            graph,
        })
    }

    fn words(&self) -> Vec<String> {
        self.tokens.get("word").cloned().unwrap_or_default()
    }

    fn token(&self, field: &str, idx: usize) -> &str {
        self.tokens
            .get(field)
            .and_then(|toks| toks.get(idx))
            .map_or("", String::as_str)
    }
}

/// Quickwit returns stored fields as arrays (`["value"]`) or bare scalars depending on the
/// path taken; accept both.
fn first_string(value: Option<&Value>) -> Option<&str> {
    match value? {
        Value::String(s) => Some(s),
        Value::Array(items) => items.first()?.as_str(),
        _ => None,
    }
}

fn first_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Array(items) => items.first()?.as_u64(),
        other => other.as_u64(),
    }
}

fn parse_graph(name: &str, value: &Value) -> Option<SentenceGraph> {
    let value = match value {
        Value::Array(items) => items.first()?,
        other => other,
    };
    let edges = value
        .get("edges")?
        .as_array()?
        .iter()
        .map(|edge| {
            let edge = edge.as_array()?;
            Some(DependencyEdge::new(
                u32::try_from(edge.first()?.as_u64()?).ok()?,
                u32::try_from(edge.get(1)?.as_u64()?).ok()?,
                edge.get(2)?.as_str()?,
            ))
        })
        .collect::<Option<Vec<_>>>()?;
    let roots = value
        .get("roots")
        .and_then(Value::as_array)
        .map(|roots| {
            roots
                .iter()
                .filter_map(|r| u32::try_from(r.as_u64()?).ok())
                .collect()
        })
        .unwrap_or_default();
    Some(SentenceGraph::new(name, edges, roots))
}

/// A compiled query ready to be evaluated against many sentences.
pub enum Evaluator {
    Surface { prog: SpanProg },
    Graph(GraphCompiled),
}

impl Evaluator {
    pub fn new(compiled: &CompiledQuery) -> Result<Self> {
        match compiled {
            CompiledQuery::Surface(SurfacePlan { pattern, .. }) => {
                let prog = SpanProg::compile(pattern).map_err(SearchError::InvalidQuery)?;
                Ok(Self::Surface { prog })
            }
            CompiledQuery::Graph(graph) => Ok(Self::Graph(graph.clone())),
        }
    }

    /// All matches of the query in `sentence` (empty when the sentence is only a
    /// prefilter false positive).
    pub fn evaluate(&self, sentence: &StoredSentence) -> Option<SentenceHit> {
        let n = sentence.sentence_length as usize;
        let words = sentence.words();
        let matches: Vec<MatchOut> = match self {
            Self::Surface { prog } => prog
                .spans_over_tokens(n, &|field, tok| sentence.token(field, tok), true)
                .iter()
                .map(|span| MatchOut {
                    spans: vec![span_out(span, &words)],
                })
                .collect(),
            Self::Graph(compiled) => {
                let graph = sentence.graph.as_ref()?;
                evaluate_on_sentence(
                    &compiled.plan,
                    graph,
                    &sentence.tokens,
                    DEFAULT_SENTENCE_CAP,
                )
                .iter()
                .map(|tuple| MatchOut {
                    spans: tuple.iter().map(|span| span_out(span, &words)).collect(),
                })
                .collect()
            }
        };
        if matches.is_empty() {
            return None;
        }
        Some(SentenceHit {
            doc_id: sentence.doc_id.clone(),
            sentence_id: sentence.sentence_id.clone(),
            sentence_length: sentence.sentence_length,
            words,
            matches,
        })
    }
}

fn span_out(span: &EndpointSpan, words: &[String]) -> SpanOut {
    let text = |start: usize, end: usize| {
        words
            .get(start..end.min(words.len()))
            .map(|w| w.join(" "))
            .unwrap_or_default()
    };
    SpanOut {
        start: span.start,
        end: span.end,
        text: text(span.start, span.end),
        captures: span
            .captures
            .iter()
            .map(|c| CaptureOut {
                name: c.name.clone(),
                start: c.span.start,
                end: c.span.end,
                text: text(c.span.start, c.span.end),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustie_compiler::QueryCompiler;

    fn sentence() -> StoredSentence {
        // "John runs fast": John --nsubj--> runs is encoded as edge (head=1, dep=0).
        let json = serde_json::json!({
            "doc_id": "d1", "sentence_id": "d1_0", "sentence_length": 3,
            "word": "John|runs|fast", "pos": "NNP|VBZ|RB",
            "dependencies": {"edges": [[1, 0, "nsubj"], [1, 2, "advmod"]], "roots": [1]}
        });
        StoredSentence::from_hit_json(&json.to_string()).unwrap()
    }

    fn run(query: &str) -> Option<SentenceHit> {
        let compiled = QueryCompiler::new().compile(query).unwrap();
        Evaluator::new(&compiled).unwrap().evaluate(&sentence())
    }

    #[test]
    fn surface_match_and_prefilter_false_positive() {
        let hit = run("[pos=VBZ]").expect("match");
        assert_eq!(hit.matches[0].spans[0].text, "runs");
        assert_eq!(
            (hit.matches[0].spans[0].start, hit.matches[0].spans[0].end),
            (1, 2)
        );
        assert!(run("[pos=NN]").is_none());
    }

    #[test]
    fn graph_match_returns_one_span_per_endpoint() {
        let hit = run("[pos=VBZ] >nsubj [word=John]").expect("match");
        let spans = &hit.matches[0].spans;
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].text, "runs");
        assert_eq!(spans[1].text, "John");
        assert!(run("[pos=VBZ] >dobj []").is_none());
    }

    #[test]
    fn scalar_and_array_stored_values_both_parse() {
        let json = r#"{"doc_id":["d"],"sentence_id":["d_0"],"sentence_length":[1],"word":["a"]}"#;
        let s = StoredSentence::from_hit_json(json).unwrap();
        assert_eq!((s.sentence_length, s.tokens["word"][0].as_str()), (1, "a"));
        assert!(s.graph.is_none());
    }
}
