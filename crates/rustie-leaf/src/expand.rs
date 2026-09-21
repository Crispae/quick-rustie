//! Resolve a token test to the terms of a field that satisfy it, warming their postings.
//!
//! Exact tests are one term. Regex and fuzzy tests are expanded over the term dictionary: with
//! the dictionary's own automaton when the pattern is within its regex dialect (only the
//! dictionary blocks the automaton visits are read), otherwise by scanning the whole
//! dictionary. Either way every candidate term is re-checked with the exact matcher, so the
//! result is exactly the set of terms the in-memory test accepts.

use std::sync::Arc;

use futures::{StreamExt, TryStreamExt};
use rustie_compiler::LeafTest;
use tantivy::schema::Field;
use tantivy::{InvertedIndexReader, Term};
use tantivy_fst::Automaton;

/// Warm concurrency for the postings of expanded terms.
const WARM_CONCURRENCY: usize = 16;

/// Terms of `field` satisfying `test`, with their postings (and positions when
/// `with_positions`) available for synchronous reads afterwards.
pub(crate) async fn expand_terms(
    inverted_index: &InvertedIndexReader,
    field: Field,
    test: &LeafTest,
    with_positions: bool,
) -> anyhow::Result<Vec<Term>> {
    let terms = match test {
        LeafTest::Exact(value) => vec![Term::from_field_text(field, value)],
        LeafTest::Regex(_) | LeafTest::Fuzzy(_) => {
            let values = match dictionary_regex(test) {
                Some(fst_regex) => {
                    // Reads the dictionary blocks the automaton can match, and those terms'
                    // postings; the synchronous search below then runs on cached bytes.
                    inverted_index
                        .warm_postings_automaton(fst_regex.clone(), |task| async move { task() })
                        .await?;
                    let mut stream = inverted_index.terms().search(fst_regex).into_stream()?;
                    let mut values = Vec::new();
                    while stream.advance() {
                        values.push(stream.key().to_vec());
                    }
                    values
                }
                None => {
                    inverted_index.terms().warm_up_dictionary().await?;
                    let mut stream = inverted_index.terms().stream()?;
                    let mut values = Vec::new();
                    while stream.advance() {
                        values.push(stream.key().to_vec());
                    }
                    values
                }
            };
            values
                .into_iter()
                .filter_map(|bytes| String::from_utf8(bytes).ok())
                .filter(|value| test.matches(value))
                .map(|value| Term::from_field_text(field, &value))
                .collect()
        }
    };
    // Keep only terms present in the split, warming what the scorer will read.
    let present: Vec<Option<Term>> = futures::stream::iter(terms.into_iter().map(|term| async {
        let exists = inverted_index.warm_postings(&term, with_positions).await?;
        anyhow::Ok(exists.then_some(term))
    }))
    .buffer_unordered(WARM_CONCURRENCY)
    .try_collect()
    .await?;
    let mut present: Vec<Term> = present.into_iter().flatten().collect();
    present.sort();
    Ok(present)
}

/// A cheaply clonable dictionary regex (the dictionary API needs `Clone` automatons).
#[derive(Clone)]
struct SharedRegex(Arc<tantivy_fst::Regex>);

impl Automaton for SharedRegex {
    type State = Option<usize>;

    fn start(&self) -> Self::State {
        self.0.start()
    }

    fn is_match(&self, state: &Self::State) -> bool {
        self.0.is_match(state)
    }

    fn can_match(&self, state: &Self::State) -> bool {
        self.0.can_match(state)
    }

    fn will_always_match(&self, state: &Self::State) -> bool {
        self.0.will_always_match(state)
    }

    fn accept(&self, state: &Self::State, byte: u8) -> Self::State {
        self.0.accept(state, byte)
    }
}

/// The test as a term-dictionary automaton, when its dialect allows it. It must accept every
/// value the exact test accepts (a superset is fine: results are re-checked).
fn dictionary_regex(test: &LeafTest) -> Option<SharedRegex> {
    let pattern = match test {
        LeafTest::Exact(_) => return None,
        LeafTest::Regex(regex) => unanchor(regex.as_str()).to_string(),
        // Case folding by character classes is only a faithful superset for ASCII.
        LeafTest::Fuzzy(needle) if needle.is_ascii() => {
            format!(".*{}.*", case_insensitive(needle))
        }
        LeafTest::Fuzzy(_) => return None,
    };
    // The dictionary matches whole terms, so explicit anchors are redundant (and not in its
    // dialect).
    let mut p = pattern.as_str();
    p = p.strip_prefix('^').unwrap_or(p);
    if p.ends_with('$') && !p.ends_with("\\$") {
        p = &p[..p.len() - 1];
    }
    tantivy_fst::Regex::new(p)
        .ok()
        .map(|regex| SharedRegex(Arc::new(regex)))
}

/// Leaf regexes are stored anchored as `\A(?:…)\z`; recover the user pattern.
fn unanchor(pattern: &str) -> &str {
    pattern
        .strip_prefix(r"\A(?:")
        .and_then(|p| p.strip_suffix(r")\z"))
        .unwrap_or(pattern)
}

fn case_insensitive(needle: &str) -> String {
    let mut out = String::with_capacity(needle.len() * 4);
    for c in needle.chars() {
        let (lower, upper) = (c.to_ascii_lowercase(), c.to_ascii_uppercase());
        if lower != upper {
            out.push('[');
            out.push(lower);
            out.push(upper);
            out.push(']');
        } else {
            out.push_str(&regex::escape(&c.to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anchored(p: &str) -> LeafTest {
        LeafTest::Regex(regex::Regex::new(&format!(r"\A(?:{p})\z")).unwrap())
    }

    #[test]
    fn dictionary_dialect() {
        assert!(dictionary_regex(&anchored("J.*")).is_some());
        assert!(dictionary_regex(&anchored("^[Cc]ancer.*$")).is_some());
        // Look-around and word boundaries are beyond the dictionary automaton: full scan.
        assert!(dictionary_regex(&anchored(r"\bJohn")).is_none());
        assert!(dictionary_regex(&LeafTest::Fuzzy("foo".into())).is_some());
        assert!(dictionary_regex(&LeafTest::Fuzzy("straße".into())).is_none());
        assert_eq!(unanchor(r"\A(?:a|b)\z"), "a|b");
        assert_eq!(case_insensitive("a1-"), r"[aA]1\-");
    }
}
