//! Pattern / Constraint → [`CandidateFilter`] (document-level supersets).

use crate::candidate::CandidateFilter;
use crate::error::Result;
use rustie_query::{Constraint, Matcher, Pattern};

pub(crate) struct DocFilter;

impl DocFilter {
    pub(crate) fn for_pattern(pattern: &Pattern) -> Result<Option<CandidateFilter>> {
        match pattern {
            Pattern::Constraint(c) => Self::for_constraint(c),
            Pattern::NamedCapture { pattern, .. } => Self::for_pattern(pattern),
            Pattern::Repetition { pattern, min, .. } if *min > 0 => Self::for_pattern(pattern),
            Pattern::Repetition { .. } => Ok(None),
            Pattern::Assertion(_) => Ok(None),
            Pattern::Concatenated(parts) => {
                // Consecutive exact tokens on one field are adjacent tokens in the sentence:
                // require them as a phrase (positions) rather than as independent terms.
                let mut required = Vec::new();
                let mut run: Vec<(&str, &str)> = Vec::new();
                for part in parts {
                    if let Some(token) = exact_token(part) {
                        if run.last().is_some_and(|(field, _)| *field != token.0) {
                            Self::flush_run(&mut run, &mut required);
                        }
                        run.push(token);
                        continue;
                    }
                    Self::flush_run(&mut run, &mut required);
                    if let Some(q) = Self::for_pattern(part)? {
                        required.push(q);
                    }
                }
                Self::flush_run(&mut run, &mut required);
                Ok(Self::all_of(required))
            }
            Pattern::GraphTraversal { .. } => Err(crate::error::CompileError::compile(
                "Graph traversal patterns should be handled by GraphCompiler",
            )),
        }
    }

    pub(crate) fn for_constraint(constraint: &Constraint) -> Result<Option<CandidateFilter>> {
        match constraint {
            Constraint::Wildcard | Constraint::Negated(_) => Ok(None),
            Constraint::Field { name, matcher } => Ok(Some(Self::field_filter(name, matcher))),
            Constraint::Fuzzy { name, matcher } => Ok(Some(CandidateFilter::regex(
                name,
                format!(".*{}.*", case_insensitive(matcher)),
            ))),
            Constraint::Conjunctive(inner) => {
                for c in inner {
                    if let Some(q) = Self::for_constraint(c)? {
                        return Ok(Some(q));
                    }
                }
                Ok(None)
            }
            Constraint::Disjunctive(inner) => {
                let mut alts = Vec::with_capacity(inner.len());
                for c in inner {
                    match Self::for_constraint(c)? {
                        Some(q) => alts.push(q),
                        None => return Ok(None),
                    }
                }
                Ok(Self::any_of(alts))
            }
        }
    }

    pub(crate) fn field_filter(field_name: &str, matcher: &Matcher) -> CandidateFilter {
        match matcher {
            Matcher::String(s) => CandidateFilter::term(field_name, s),
            Matcher::Regex { pattern, .. } => {
                let clean = pattern.trim_start_matches('/').trim_end_matches('/');
                if is_simple_literal(clean) {
                    CandidateFilter::term(field_name, clean)
                } else {
                    CandidateFilter::regex(field_name, clean)
                }
            }
        }
    }

    /// Emit the pending run of adjacent same-field exact tokens.
    fn flush_run(run: &mut Vec<(&str, &str)>, out: &mut Vec<CandidateFilter>) {
        match run.as_slice() {
            [] => {}
            [(field, value)] => out.push(CandidateFilter::term(*field, *value)),
            [(field, _), ..] => out.push(CandidateFilter::phrase(
                *field,
                run.iter().map(|(_, value)| (*value).to_string()).collect(),
            )),
        }
        run.clear();
    }

    fn all_of(mut filters: Vec<CandidateFilter>) -> Option<CandidateFilter> {
        match filters.len() {
            0 => None,
            1 => filters.pop(),
            _ => Some(CandidateFilter::And(filters).normalize()),
        }
    }

    fn any_of(mut filters: Vec<CandidateFilter>) -> Option<CandidateFilter> {
        match filters.len() {
            0 => None,
            1 => filters.pop(),
            _ => Some(CandidateFilter::Or(filters).normalize()),
        }
    }
}

fn is_simple_literal(pattern: &str) -> bool {
    pattern
        .chars()
        .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
}

/// `[field=value]` with a literal value (possibly wrapped in a named capture).
fn exact_token(pattern: &Pattern) -> Option<(&str, &str)> {
    match pattern {
        Pattern::NamedCapture { pattern, .. } => exact_token(pattern),
        Pattern::Constraint(Constraint::Field {
            name,
            matcher: Matcher::String(value),
        }) => Some((name, value)),
        _ => None,
    }
}

/// Regex source matching `needle` in any letter case (`Foo` → `[Ff][Oo][Oo]`).
///
/// Fuzzy constraints are evaluated case-insensitively, but postings are case-sensitive, so a
/// literal prefilter would silently drop capitalised variants.
fn case_insensitive(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len() * 4);
    for c in needle.chars() {
        let mut variants: Vec<char> = vec![c];
        variants.extend(c.to_lowercase().take(1));
        variants.extend(c.to_uppercase().take(1));
        variants.sort_unstable();
        variants.dedup();
        if variants.len() == 1 {
            out.push_str(&regex::escape(&c.to_string()));
        } else {
            out.push('[');
            out.extend(variants);
            out.push(']');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustie_query::QueryParser;

    fn filter(q: &str) -> CandidateFilter {
        let pattern = QueryParser::new().parse_query(q).unwrap();
        DocFilter::for_pattern(&pattern)
            .unwrap()
            .unwrap()
            .normalize()
    }

    #[test]
    fn adjacent_same_field_tokens_become_a_phrase() {
        assert_eq!(
            filter("[word=the] [word=cat]"),
            CandidateFilter::phrase("word", vec!["the".into(), "cat".into()])
        );
        assert_eq!(
            filter("[word=the] [word=big] [word=cat]"),
            CandidateFilter::phrase("word", vec!["the".into(), "big".into(), "cat".into()])
        );
    }

    #[test]
    fn runs_break_on_field_change_and_wildcards() {
        // Different fields cannot share a phrase: separate conjuncts.
        assert_eq!(
            filter("[word=the] [pos=NN]"),
            CandidateFilter::And(vec![
                CandidateFilter::term("word", "the"),
                CandidateFilter::term("pos", "NN"),
            ])
        );
        // A wildcard between two tokens means they are not adjacent.
        assert_eq!(
            filter("[word=a] [] [word=b]"),
            CandidateFilter::And(vec![
                CandidateFilter::term("word", "a"),
                CandidateFilter::term("word", "b"),
            ])
        );
        // Two runs on the same field separated by a wildcard: two phrases.
        assert_eq!(
            filter("[word=a] [word=b] [] [word=c] [word=d]"),
            CandidateFilter::And(vec![
                CandidateFilter::phrase("word", vec!["a".into(), "b".into()]),
                CandidateFilter::phrase("word", vec!["c".into(), "d".into()]),
            ])
        );
    }

    #[test]
    fn fuzzy_prefilter_is_case_insensitive() {
        let f = filter("[word=foo~]");
        assert_eq!(f, CandidateFilter::regex("word", ".*[Ff][Oo][Oo].*"));
        // The in-memory test lowercases both sides; the prefilter must admit every variant.
        let re = regex::Regex::new(r"\A(?:.*[Ff][Oo][Oo].*)\z").unwrap();
        assert!(["foo", "Foo", "xFOOy"].iter().all(|w| re.is_match(w)));
    }
}
