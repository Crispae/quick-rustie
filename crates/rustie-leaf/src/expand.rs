//! Resolve a token test to the terms of a field that satisfy it, warming their postings, and
//! cache each `(field, test)` pair's resolution per segment.
//!
//! Exact tests are one term. Regex and fuzzy tests are expanded over the term dictionary: with
//! the dictionary's own automaton when the pattern is within its regex dialect (only the
//! dictionary blocks the automaton visits are read), otherwise by scanning the whole
//! dictionary. Either way every candidate term is re-checked with the exact matcher, so the
//! result is exactly the set of terms the in-memory test accepts.
//!
//! A pattern's endpoint constraint is usually resolved twice per segment: once as a `SameToken`
//! child (`rustie-leaf`'s `refine_same_token`) and once as an `ExternalLeaf` of the bound graph
//! plan (the exact matcher). [`SegmentTerms`] caches the resolution — the dictionary walk and the
//! postings/positions warming — so the second caller reuses the first's work instead of repeating
//! it (previously the whole cost, including re-warming positions, ran twice).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures::{StreamExt, TryStreamExt};
use rustie_compiler::LeafTest;
use tantivy::postings::TermInfo;
use tantivy::schema::Field;
use tantivy::{InvertedIndexReader, SegmentReader, Term};
use tantivy_fst::Automaton;

/// Warm concurrency for coalesced postings/positions range reads.
const WARM_CONCURRENCY: usize = 16;

/// Merge position-range holes under this many bytes into one read: the same heuristic tantivy's
/// own `warm_postings_automaton` uses for postings ranges (`MERGE_HOLES_UNDER_BYTES`, roughly
/// what a 50ms-TTFB S3 read can absorb for free).
const POSITIONS_MERGE_GAP: usize = 4 * 1024 * 1024;

/// A test's matched terms in one field: resolved once per segment, then shared by every caller
/// that needs them.
pub(crate) struct Resolved {
    /// Matched terms with their dictionary entry, in dictionary (and therefore postings-file and
    /// positions-file) order.
    terms: Vec<(Term, TermInfo)>,
    positions_warmed: AtomicBool,
}

impl Resolved {
    /// Sum of each term's document frequency: an upper bound on how many documents this test
    /// alone admits. No I/O — every matched term's `TermInfo` is already local.
    pub(crate) fn doc_freq(&self) -> u64 {
        self.terms
            .iter()
            .map(|(_, info)| u64::from(info.doc_freq))
            .sum()
    }

    pub(crate) fn len(&self) -> usize {
        self.terms.len()
    }

    pub(crate) fn terms(&self) -> impl Iterator<Item = &Term> {
        self.terms.iter().map(|(term, _)| term)
    }
}

/// Cache key for one `(field, test)` pair. Two tests that produce the same key always produce the
/// same answer, so under-merging (two keys for what turns out to be an equivalent test) only
/// costs a redundant resolve; it can never share a wrong result.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum TestKey {
    Exact(String),
    /// The already-anchored pattern's source (`regex.as_str()`), not the raw user pattern: both
    /// callers anchor via `rustie_compiler::matching::node_test::anchor` before building a
    /// `LeafTest::Regex`, so the same user pattern always anchors to the same string here.
    Regex(String),
    Fuzzy(String),
}

impl TestKey {
    fn of(test: &LeafTest) -> Self {
        match test {
            LeafTest::Exact(v) => Self::Exact(v.clone()),
            LeafTest::Regex(re) => Self::Regex(re.as_str().to_string()),
            LeafTest::Fuzzy(v) => Self::Fuzzy(v.clone()),
        }
    }
}

/// Resolves `(field, test)` pairs against one segment, caching each pair's matched terms so
/// every caller within one `prepare_segment` call pays for the dictionary walk and the range
/// reads once. Not `Sync` across segments: build one per segment.
pub(crate) struct SegmentTerms<'a> {
    reader: &'a SegmentReader,
    cache: Mutex<HashMap<(Field, TestKey), Arc<Resolved>>>,
    resolved_count: AtomicUsize,
    position_group_count: AtomicUsize,
}

impl<'a> SegmentTerms<'a> {
    pub(crate) fn new(reader: &'a SegmentReader) -> Self {
        Self {
            reader,
            cache: Mutex::new(HashMap::new()),
            resolved_count: AtomicUsize::new(0),
            position_group_count: AtomicUsize::new(0),
        }
    }

    /// Terms of `field` satisfying `test`, with postings (not positions) warm. Cached: a second
    /// call with an equivalent `(field, test)` in this segment returns the same `Resolved`
    /// without touching the dictionary or postings again. `field` must already be checked present
    /// and indexed (callers do this while resolving the field name to a `Field`).
    pub(crate) async fn resolve(
        &self,
        field: Field,
        test: &LeafTest,
    ) -> anyhow::Result<Arc<Resolved>> {
        let key = (field, TestKey::of(test));
        if let Some(hit) = self.cache.lock().expect("poisoned").get(&key) {
            return Ok(hit.clone());
        }
        let inverted_index = self.reader.inverted_index(field)?;
        let resolved = Arc::new(resolve_uncached(&inverted_index, field, test).await?);
        self.resolved_count.fetch_add(1, Ordering::Relaxed);
        // `or_insert`, not a plain insert: if another call resolved the same key while this one
        // was awaiting I/O, every caller must share the one `Resolved` that won, so its
        // `positions_warmed` flag is shared too.
        Ok(self
            .cache
            .lock()
            .expect("poisoned")
            .entry(key)
            .or_insert(resolved)
            .clone())
    }

    /// Warms `resolved`'s positions, a no-op if already warmed (by this call or an earlier one
    /// for the same `Resolved`). Merges nearby terms' position ranges into a few contiguous reads
    /// instead of one request per term.
    pub(crate) async fn warm_positions(
        &self,
        field: Field,
        resolved: &Resolved,
    ) -> anyhow::Result<()> {
        if resolved.terms.is_empty() {
            return Ok(());
        }
        if resolved.positions_warmed.swap(true, Ordering::Relaxed) {
            return Ok(());
        }
        let inverted_index = self.reader.inverted_index(field)?;
        let ranges: Vec<_> = resolved
            .terms
            .iter()
            .map(|(_, info)| info.positions_range.clone())
            .collect();
        let groups = group_runs(&ranges, POSITIONS_MERGE_GAP);
        self.position_group_count
            .fetch_add(groups.len(), Ordering::Relaxed);
        futures::stream::iter(groups.into_iter().map(|(start, end)| {
            let inverted_index = &inverted_index;
            let terms = &resolved.terms;
            async move {
                let lo = terms[start].0.clone();
                let hi = terms[end].0.clone();
                inverted_index.warm_postings_range(lo..=hi, None, true).await
            }
        }))
        .buffer_unordered(WARM_CONCURRENCY)
        .try_for_each(|_| async { Ok(()) })
        .await?;
        Ok(())
    }

    /// `(distinct (field, test) pairs resolved, position-range reads issued)` in this segment, for
    /// the `rustie segment warmed` debug event.
    pub(crate) fn stats(&self) -> (usize, usize) {
        (
            self.resolved_count.load(Ordering::Relaxed),
            self.position_group_count.load(Ordering::Relaxed),
        )
    }
}

async fn resolve_uncached(
    inverted_index: &InvertedIndexReader,
    field: Field,
    test: &LeafTest,
) -> anyhow::Result<Resolved> {
    let terms: Vec<(Term, TermInfo)> = match test {
        LeafTest::Exact(value) => {
            let term = Term::from_field_text(field, value);
            match inverted_index
                .terms()
                .get_async(term.serialized_value_bytes())
                .await?
            {
                Some(info) => {
                    inverted_index.warm_postings(&term, false).await?;
                    vec![(term, info)]
                }
                None => Vec::new(),
            }
        }
        LeafTest::Regex(_) | LeafTest::Fuzzy(_) => {
            let entries: Vec<(Vec<u8>, TermInfo)> = match dictionary_regex(test) {
                Some(fst_regex) => {
                    // Reads the dictionary blocks the automaton can match, and those terms'
                    // postings (not positions); the streams below then run on cached bytes.
                    inverted_index
                        .warm_postings_automaton(fst_regex.clone(), |task| async move { task() })
                        .await?;
                    let mut stream = inverted_index.terms().search(fst_regex).into_stream()?;
                    let mut entries = Vec::new();
                    while stream.advance() {
                        entries.push((stream.key().to_vec(), stream.value().clone()));
                    }
                    entries
                }
                None => {
                    // Outside the dictionary automaton's dialect (look-around, word boundaries):
                    // a full scan needs every term's postings warm too, in one coalesced read.
                    inverted_index.terms().warm_up_dictionary().await?;
                    inverted_index.warm_postings_full(false).await?;
                    let mut stream = inverted_index.terms().stream()?;
                    let mut entries = Vec::new();
                    while stream.advance() {
                        entries.push((stream.key().to_vec(), stream.value().clone()));
                    }
                    entries
                }
            };
            entries
                .into_iter()
                .filter_map(|(bytes, info)| String::from_utf8(bytes).ok().map(|v| (v, info)))
                .filter(|(value, _)| test.matches(value))
                .map(|(value, info)| (Term::from_field_text(field, &value), info))
                .collect()
        }
    };
    Ok(Resolved {
        terms,
        positions_warmed: AtomicBool::new(false),
    })
}

/// Groups `ranges` (already in ascending byte order, as dictionary order guarantees) into runs
/// where consecutive ranges are at most `gap` bytes apart. Each group is returned as the first
/// and last index into `ranges` it covers.
fn group_runs(ranges: &[std::ops::Range<usize>], gap: usize) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut start = 0;
    for i in 1..ranges.len() {
        if ranges[i].start.saturating_sub(ranges[i - 1].end) > gap {
            groups.push((start, i - 1));
            start = i;
        }
    }
    if !ranges.is_empty() {
        groups.push((start, ranges.len() - 1));
    }
    groups
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

    #[test]
    fn group_runs_merges_within_gap_and_splits_beyond_it() {
        assert_eq!(group_runs(&[], 10), Vec::<(usize, usize)>::new());
        assert_eq!(group_runs(&[0..5], 10), vec![(0, 0)]);
        // Both gaps (5->7 and 8->10) are 2 bytes, within the 10-byte budget -> one group.
        assert_eq!(group_runs(&[0..5, 7..8, 10..20], 10), vec![(0, 2)]);
        // Same ranges, budget 1: neither 2-byte gap fits -> three singleton groups.
        assert_eq!(group_runs(&[0..5, 7..8, 10..20], 1), vec![(0, 0), (1, 1), (2, 2)]);
    }

    #[test]
    fn test_key_matches_across_independently_anchored_regexes() {
        // The two call sites that build a `LeafTest::Regex` (a `SameToken` child in
        // `candidates.rs`, and a bound graph plan's `ExternalLeaf`) each anchor their own
        // `regex::Regex` from the same raw pattern; the keys must still collide.
        use rustie_compiler::matching::node_test::anchor;
        let a = regex::Regex::new(".*ase").unwrap();
        let b = regex::Regex::new(".*ase").unwrap();
        assert_eq!(
            TestKey::of(&LeafTest::Regex(anchor(&a))),
            TestKey::of(&LeafTest::Regex(anchor(&b)))
        );
    }
}
