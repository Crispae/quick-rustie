//! Token-level node tests: the single lowering from surface [`Constraint`]s.
//!
//! Everything here is pure (no index, no Tantivy, no GPH2), so it can be unit
//! tested with plain strings.

use rustie_query::{Constraint, Matcher};

/// Negation-normal node test over token fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeTest {
    True,
    Field { name: String, matcher: NodeMatcher },
    Fuzzy { name: String, needle: String },
    And(Vec<NodeTest>),
    Or(Vec<NodeTest>),
    Not(Box<NodeTest>),
}

#[derive(Debug, Clone)]
pub enum NodeMatcher {
    Exact(String),
    Regex(regex::Regex),
}

impl PartialEq for NodeMatcher {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Exact(a), Self::Exact(b)) => a == b,
            (Self::Regex(a), Self::Regex(b)) => a.as_str() == b.as_str(),
            _ => false,
        }
    }
}

impl Eq for NodeMatcher {}

impl NodeTest {
    pub fn to_nnf(self) -> Self {
        match self {
            Self::Not(inner) => match *inner {
                Self::True => Self::Not(Box::new(Self::True)),
                Self::Not(x) => x.to_nnf(),
                Self::And(xs) => Self::Or(
                    xs.into_iter()
                        .map(|x| Self::Not(Box::new(x)).to_nnf())
                        .collect(),
                ),
                Self::Or(xs) => Self::And(
                    xs.into_iter()
                        .map(|x| Self::Not(Box::new(x)).to_nnf())
                        .collect(),
                ),
                other => Self::Not(Box::new(other.to_nnf())),
            },
            Self::And(xs) => Self::And(xs.into_iter().map(Self::to_nnf).collect()),
            Self::Or(xs) => Self::Or(xs.into_iter().map(Self::to_nnf).collect()),
            other => other,
        }
    }

    pub fn field_names(&self, out: &mut std::collections::BTreeSet<String>) {
        match self {
            Self::True => {}
            Self::Field { name, .. } | Self::Fuzzy { name, .. } => {
                out.insert(name.clone());
            }
            Self::And(xs) | Self::Or(xs) => {
                for x in xs {
                    x.field_names(out);
                }
            }
            Self::Not(x) => x.field_names(out),
        }
    }

    pub fn matches_token<'a>(&self, get: &dyn Fn(&str, usize) -> &'a str, tok: usize) -> bool {
        match self {
            Self::True => true,
            Self::Field { name, matcher } => {
                let val = get(name, tok);
                match matcher {
                    NodeMatcher::Exact(s) => val == s,
                    NodeMatcher::Regex(re) => full_match(re, val),
                }
            }
            Self::Fuzzy { name, needle } => get(name, tok)
                .to_lowercase()
                .contains(&needle.to_lowercase()),
            Self::And(xs) => xs.iter().all(|x| x.matches_token(get, tok)),
            Self::Or(xs) => xs.iter().any(|x| x.matches_token(get, tok)),
            Self::Not(x) => !x.matches_token(get, tok),
        }
    }
}

pub(crate) fn constraint_to_test(c: &Constraint) -> NodeTest {
    match c {
        Constraint::Wildcard => NodeTest::True,
        Constraint::Field { name, matcher } => NodeTest::Field {
            name: name.clone(),
            matcher: matcher_to_node(matcher),
        },
        Constraint::Fuzzy { name, matcher } => NodeTest::Fuzzy {
            name: name.clone(),
            needle: matcher.clone(),
        },
        Constraint::Negated(inner) => NodeTest::Not(Box::new(constraint_to_test(inner))),
        Constraint::Conjunctive(xs) => NodeTest::And(xs.iter().map(constraint_to_test).collect()),
        Constraint::Disjunctive(xs) => NodeTest::Or(xs.iter().map(constraint_to_test).collect()),
    }
}

pub(crate) fn matcher_to_node(m: &Matcher) -> NodeMatcher {
    match m {
        Matcher::String(s) => NodeMatcher::Exact(s.clone()),
        Matcher::Regex { regex, .. } => NodeMatcher::Regex(anchor(regex)),
    }
}

/// `re` anchored to the whole input. `regex::Regex::find` is leftmost-first, so an
/// unanchored `NN|NNS` would report "NN" (not the full "NNS") and fail a
/// `start == 0 && end == len` check; an anchored regex has no such ambiguity.
pub fn anchor(re: &regex::Regex) -> regex::Regex {
    regex::Regex::new(&format!(r"\A(?:{})\z", re.as_str())).unwrap_or_else(|_| re.clone())
}

pub fn full_match(re: &regex::Regex, s: &str) -> bool {
    re.find(s)
        .is_some_and(|m| m.start() == 0 && m.end() == s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tok<'a>(words: &'a [&'a str]) -> impl Fn(&str, usize) -> &'a str {
        move |_field, i| words[i]
    }

    #[test]
    fn nnf_pushes_negation_to_the_leaves() {
        let t = NodeTest::Not(Box::new(NodeTest::And(vec![
            NodeTest::Field {
                name: "w".into(),
                matcher: NodeMatcher::Exact("a".into()),
            },
            NodeTest::Field {
                name: "w".into(),
                matcher: NodeMatcher::Exact("b".into()),
            },
        ])))
        .to_nnf();
        assert!(
            matches!(t, NodeTest::Or(ref xs) if xs.iter().all(|x| matches!(x, NodeTest::Not(_))))
        );
    }

    #[test]
    fn regex_matches_the_whole_token_only() {
        let re = anchor(&regex::Regex::new("NN|NNS").unwrap());
        assert!(full_match(&re, "NNS"));
        assert!(!full_match(&re, "NNSX"));
    }

    #[test]
    fn negation_and_disjunction_evaluate() {
        let t = NodeTest::Or(vec![
            NodeTest::Field {
                name: "w".into(),
                matcher: NodeMatcher::Exact("a".into()),
            },
            NodeTest::Not(Box::new(NodeTest::Field {
                name: "w".into(),
                matcher: NodeMatcher::Exact("b".into()),
            })),
        ]);
        let words = ["a", "b", "c"];
        let get = tok(&words);
        assert!(t.matches_token(&get, 0));
        assert!(!t.matches_token(&get, 1));
        assert!(t.matches_token(&get, 2));
    }
}
