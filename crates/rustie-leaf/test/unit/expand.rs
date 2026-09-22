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
    assert_eq!(
        group_runs(&[0..5, 7..8, 10..20], 1),
        vec![(0, 0), (1, 1), (2, 2)]
    );
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

use tantivy::schema::{IndexRecordOption, Schema, TextFieldIndexing, TextOptions};
use tantivy::{Index, IndexReader, doc};

fn word_index(docs: &[&str]) -> (Index, IndexReader) {
    let opts = TextOptions::default().set_indexing_options(
        TextFieldIndexing::default()
            .set_tokenizer("rustie_tokens")
            .set_index_option(IndexRecordOption::WithFreqsAndPositions),
    );
    let mut builder = Schema::builder();
    let word = builder.add_text_field("word", opts);
    let index = Index::create_in_ram(builder.build());
    index
        .tokenizers()
        .register("rustie_tokens", crate::tokenizer::slot_text_analyzer(false));
    let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
    for text in docs {
        writer.add_document(doc!(word => *text)).unwrap();
    }
    writer.commit().unwrap();
    let reader = index.reader().unwrap();
    (index, reader)
}

fn term_text(term: &Term) -> String {
    String::from_utf8(term.serialized_value_bytes().to_vec()).unwrap()
}

#[tokio::test]
async fn resolve_exact_regex_and_cache_per_segment() {
    use rustie_compiler::matching::node_test::anchor;
    // doc0: protease|ase|run  doc1: kinase|ase|walk  doc2: other|foo
    let (index, reader) = word_index(&["protease|ase|run", "kinase|ase|walk", "other|foo"]);
    let searcher = reader.searcher();
    let segment = searcher.segment_reader(0);
    let field = index.schema().get_field("word").unwrap();
    let terms = SegmentTerms::new(segment);

    let exact = terms
        .resolve(field, &LeafTest::Exact("protease".into()))
        .await
        .unwrap();
    assert_eq!(exact.len(), 1);
    assert_eq!(exact.doc_freq(), 1);
    assert_eq!(term_text(exact.terms().next().unwrap()), "protease");

    let regex_test = LeafTest::Regex(anchor(&regex::Regex::new(".*ase").unwrap()));
    let regex = terms.resolve(field, &regex_test).await.unwrap();
    // Anchored `.*ase` full-matches protease, kinase, and ase — not other/foo/run/walk.
    let mut values: Vec<_> = regex.terms().map(term_text).collect();
    values.sort();
    assert_eq!(values, ["ase", "kinase", "protease"]);

    let (resolved_before, _) = terms.stats();
    // Second resolve of the same regex must reuse the cache entry.
    let again = terms.resolve(field, &regex_test).await.unwrap();
    assert!(Arc::ptr_eq(&regex, &again));
    let (resolved_after, _) = terms.stats();
    assert_eq!(resolved_after, resolved_before);

    terms.warm_positions(field, &again).await.unwrap();
    let (_, groups_after_first) = terms.stats();
    terms.warm_positions(field, &again).await.unwrap(); // idempotent
    let (_, groups_after_second) = terms.stats();
    assert_eq!(groups_after_first, groups_after_second);
}

#[tokio::test]
async fn resolve_missing_term_is_empty() {
    let (index, reader) = word_index(&["cat|dog"]);
    let searcher = reader.searcher();
    let segment = searcher.segment_reader(0);
    let field = index.schema().get_field("word").unwrap();
    let terms = SegmentTerms::new(segment);
    let resolved = terms
        .resolve(field, &LeafTest::Exact("missing".into()))
        .await
        .unwrap();
    assert_eq!(resolved.len(), 0);
    assert_eq!(resolved.doc_freq(), 0);
}
