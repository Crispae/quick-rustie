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
}
