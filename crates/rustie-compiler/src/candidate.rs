//! Document-level candidate filters for Quickwit search.
//!
//! A filter is always a **superset** of the documents a pattern can match: it
//! only narrows on positive terms. Position-level / graph matching decides the
//! real hits after documents are retrieved.

use serde::{Deserialize, Serialize};

/// Postings fields used by the IE Quickwit mapping (`configs/ie_postings.yaml`).
pub const POSTINGS_TOKEN_FIELDS: &[&str] = rustie_schema::DEFAULT_INDEXED_TOKEN_FIELDS;

pub const FIELD_OUTGOING_EDGES: &str = "outgoing_edges";
pub const FIELD_INCOMING_EDGES: &str = "incoming_edges";

/// Boolean combination of term / regex clauses over Quickwit text fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CandidateFilter {
    /// No document-level restriction (still requires a hit doc to exist).
    All,
    Term {
        field: String,
        value: String,
    },
    Regex {
        field: String,
        pattern: String,
    },
    /// `terms` occur at consecutive token positions of `field` (≥ 2 terms). Only meaningful
    /// on fields indexed with positions, which is what makes token-aware indexing pay off:
    /// adjacent-token patterns are pruned by the index instead of by post-filtering.
    Phrase {
        field: String,
        terms: Vec<String>,
    },
    And(Vec<CandidateFilter>),
    Or(Vec<CandidateFilter>),
    /// Every child (`Term` or `Regex`, on any positionally-indexed field) must be satisfied by
    /// *one* token, not just somewhere in the sentence. Built only from a graph endpoint's own
    /// constraints plus its adjacent hop's label (see `GraphCompiler::endpoint_candidate`):
    /// every such endpoint is a token that must exist in any match, so requiring its clauses
    /// stays a sound superset. As a document-level test this is weaker than same-token — it only
    /// requires each child somewhere in the sentence, same as `And` — the leaf tightens it to the
    /// real same-token check on postings positions before reading graph data (see
    /// `rustie-leaf`'s `refine_same_token`).
    SameToken(Vec<CandidateFilter>),
}

impl CandidateFilter {
    pub fn term(field: impl Into<String>, value: impl Into<String>) -> Self {
        Self::Term {
            field: field.into(),
            value: value.into(),
        }
    }

    pub fn phrase(field: impl Into<String>, terms: Vec<String>) -> Self {
        debug_assert!(terms.len() >= 2, "a phrase needs at least two terms");
        Self::Phrase {
            field: field.into(),
            terms,
        }
    }

    pub fn regex(field: impl Into<String>, pattern: impl Into<String>) -> Self {
        Self::Regex {
            field: field.into(),
            pattern: pattern.into(),
        }
    }

    /// Collapse nested single-child And/Or and empty lists.
    pub fn normalize(self) -> Self {
        match self {
            Self::And(xs) => {
                let mut out: Vec<Self> = xs
                    .into_iter()
                    .map(Self::normalize)
                    .flat_map(|x| match x {
                        Self::And(inner) => inner,
                        Self::All => Vec::new(),
                        other => vec![other],
                    })
                    .collect();
                match out.len() {
                    0 => Self::All,
                    1 => out.pop().unwrap(),
                    _ => Self::And(out),
                }
            }
            Self::Or(xs) => {
                let mut out: Vec<Self> = xs
                    .into_iter()
                    .map(Self::normalize)
                    .flat_map(|x| match x {
                        Self::Or(inner) => inner,
                        other => vec![other],
                    })
                    .collect();
                if out.iter().any(|x| matches!(x, Self::All)) {
                    return Self::All;
                }
                match out.len() {
                    0 => Self::All,
                    1 => out.pop().unwrap(),
                    _ => Self::Or(out),
                }
            }
            Self::SameToken(xs) => {
                let mut out: Vec<Self> = xs
                    .into_iter()
                    .map(Self::normalize)
                    .flat_map(|x| match x {
                        Self::SameToken(inner) => inner,
                        Self::All => Vec::new(),
                        other => vec![other],
                    })
                    .collect();
                match out.len() {
                    0 => Self::All,
                    1 => out.pop().unwrap(),
                    _ => Self::SameToken(out),
                }
            }
            other => other,
        }
    }

    /// Emit a Quickwit query-string fragment (boolean + fielded terms / regex).
    ///
    /// Uses Quickwit/Tantivy-style syntax: `field:value`, `field:/regex/`,
    /// and `(a AND b)` / `(a OR b)`.
    pub fn to_quickwit_query(&self) -> String {
        match self {
            Self::All => "*".to_string(),
            Self::Term { field, value } => {
                format!("{field}:{}", quote_term(value))
            }
            Self::Regex { field, pattern } => {
                format!("{field}:/{pattern}/")
            }
            Self::Phrase { field, terms } => {
                let text = terms.join(" ");
                format!(
                    "{field}:\"{}\"",
                    text.replace('\\', "\\\\").replace('"', "\\\"")
                )
            }
            Self::And(xs) => {
                let parts: Vec<String> = xs.iter().map(|x| x.to_quickwit_query()).collect();
                if parts.len() == 1 {
                    parts.into_iter().next().unwrap()
                } else {
                    format!("({})", parts.join(" AND "))
                }
            }
            Self::Or(xs) => {
                let parts: Vec<String> = xs.iter().map(|x| x.to_quickwit_query()).collect();
                if parts.len() == 1 {
                    parts.into_iter().next().unwrap()
                } else {
                    format!("({})", parts.join(" OR "))
                }
            }
            // As a document-level Quickwit query, "one token satisfies every child" has no
            // representation weaker than "every child holds somewhere": same rendering as `And`.
            // The real same-token check happens in the leaf, over postings positions.
            Self::SameToken(xs) => {
                let parts: Vec<String> = xs.iter().map(|x| x.to_quickwit_query()).collect();
                if parts.len() == 1 {
                    parts.into_iter().next().unwrap()
                } else {
                    format!("({})", parts.join(" AND "))
                }
            }
        }
    }
}

fn quote_term(value: &str) -> String {
    if value
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.')
    {
        value.to_string()
    } else {
        format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phrase_query_string() {
        let f = CandidateFilter::phrase("word", vec!["the".into(), "cat".into()]);
        assert_eq!(f.to_quickwit_query(), "word:\"the cat\"");
    }

    #[test]
    fn and_of_terms() {
        let f = CandidateFilter::And(vec![
            CandidateFilter::term("word", "John"),
            CandidateFilter::term("outgoing_edges", "nsubj"),
        ])
        .normalize();
        assert_eq!(
            f.to_quickwit_query(),
            "(word:John AND outgoing_edges:nsubj)"
        );
    }

    #[test]
    fn same_token_renders_as_and() {
        let f = CandidateFilter::SameToken(vec![
            CandidateFilter::term("word", "cat"),
            CandidateFilter::term("incoming_edges", "nsubj"),
        ]);
        assert_eq!(f.to_quickwit_query(), "(word:cat AND incoming_edges:nsubj)");
    }

    #[test]
    fn same_token_normalizes_like_and() {
        // Nested SameToken flattens; All drops out; one child collapses to itself.
        let f = CandidateFilter::SameToken(vec![
            CandidateFilter::SameToken(vec![CandidateFilter::term("word", "cat")]),
            CandidateFilter::All,
        ])
        .normalize();
        assert_eq!(f, CandidateFilter::term("word", "cat"));

        assert_eq!(CandidateFilter::SameToken(vec![]).normalize(), CandidateFilter::All);
    }
}
