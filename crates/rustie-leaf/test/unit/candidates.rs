use super::*;

#[test]
fn bit_operations() {
    let mut a = DocBits::empty(130);
    for d in [0, 64, 129] {
        a.insert(d);
    }
    assert_eq!(a.docs(), [0, 64, 129]);
    assert!(a.contains(64) && !a.contains(65) && !a.contains(500));
}

#[test]
fn intersect_sorted_keeps_common_positions() {
    let mut acc = vec![0, 2, 5, 9];
    intersect_sorted(&mut acc, &[1, 2, 9, 12]);
    assert_eq!(acc, [2, 9]);
    intersect_sorted(&mut acc, &[3]);
    assert!(acc.is_empty());
}

use tantivy::schema::{TextFieldIndexing, TextOptions};
use tantivy::{Index, IndexReader, doc};

/// A one-segment in-RAM index with the `rustie_tokens` / `rustie_edges` tokenizers:
/// `tag` and `incoming_edges` indexed with positions, `basic_edges` indexed like the
/// pre-`rustie_edges` edge fields (`record: basic`, no positions).
fn slot_index(docs: &[(&str, &str)]) -> (Index, IndexReader) {
    let positional = |tokenizer: &str, option| {
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(tokenizer)
                .set_index_option(option),
        )
    };
    let mut builder = Schema::builder();
    let tag = builder.add_text_field(
        "tag",
        positional("rustie_tokens", IndexRecordOption::WithFreqsAndPositions),
    );
    let incoming = builder.add_text_field(
        "incoming_edges",
        positional("rustie_edges", IndexRecordOption::WithFreqsAndPositions),
    );
    let basic = builder.add_text_field(
        "basic_edges",
        positional("rustie_edges", IndexRecordOption::Basic),
    );
    let index = Index::create_in_ram(builder.build());
    index
        .tokenizers()
        .register("rustie_tokens", crate::tokenizer::slot_text_analyzer(false));
    index
        .tokenizers()
        .register("rustie_edges", crate::tokenizer::slot_text_analyzer(true));
    let mut writer = index.writer_with_num_threads(1, 15_000_000).unwrap();
    for (tags, edges) in docs {
        writer
            .add_document(doc!(tag => *tags, incoming => *edges, basic => *edges))
            .unwrap();
    }
    writer.commit().unwrap();
    let reader = index.reader().unwrap();
    (index, reader)
}

/// doc 0: `NNS` + `nsubj` on token 1 (same token). doc 1: `NN`/`NNS` on tokens 0-1, `nsubj`
/// only on token 2 (`DT`). doc 2: `NN` + `nsubj` on token 0.
const DOCS: [(&str, &str); 3] = [
    ("NN|NNS|DT", "|nsubj|"),
    ("NNS|NN|DT", "||nsubj"),
    ("NN|DT", "nsubj|"),
];

async fn refine(filter: &CandidateFilter, ratio: f64) -> Vec<DocId> {
    let (index, reader) = slot_index(&DOCS);
    let searcher = reader.searcher();
    let segment_reader = searcher.segment_reader(0);
    let schema = index.schema();
    let terms = SegmentTerms::new(segment_reader);
    refine_same_token_with(
        filter,
        &terms,
        segment_reader,
        &schema,
        vec![0, 1, 2],
        ratio,
    )
    .await
    .unwrap()
}

fn tag_nn_with_nsubj(edge_field: &str) -> CandidateFilter {
    CandidateFilter::SameToken(vec![
        CandidateFilter::regex("tag", "NN.*"),
        CandidateFilter::term(edge_field, "nsubj"),
    ])
}

#[tokio::test]
async fn regex_child_is_a_union_of_its_terms() {
    // `tag=/NN.*/` expands to `NN` and `NNS`. Doc 0's matching token is `NNS` (never `NN`):
    // the child's positions must be NN ∪ NNS, then intersected with `nsubj`'s. Intersecting
    // term by term (NN ∩ NNS ∩ nsubj) would be empty on every doc and drop 0 and 2.
    assert_eq!(
        refine(&tag_nn_with_nsubj("incoming_edges"), 1.0).await,
        [0, 2]
    );
}

#[tokio::test]
async fn only_top_level_same_tokens_narrow() {
    let node = tag_nn_with_nsubj("incoming_edges");
    let conjunct = CandidateFilter::And(vec![node.clone(), CandidateFilter::term("tag", "DT")]);
    assert_eq!(refine(&conjunct, 1.0).await, [0, 2]);
    // Under an `Or`, another branch may hold instead: left alone.
    let disjunct = CandidateFilter::Or(vec![node, CandidateFilter::term("tag", "DT")]);
    assert_eq!(refine(&disjunct, 1.0).await, [0, 1, 2]);
}

#[tokio::test]
async fn field_without_positions_skips_the_node() {
    // An old split's edge field (`record: basic`) cannot answer a same-token check.
    assert_eq!(
        refine(&tag_nn_with_nsubj("basic_edges"), 1.0).await,
        [0, 1, 2]
    );
}

#[tokio::test]
async fn cost_guard_skips_a_node_whose_rarest_child_is_too_common() {
    // With the threshold at 0 no node is worth refining: candidates are left unchanged.
    assert_eq!(
        refine(&tag_nn_with_nsubj("incoming_edges"), 0.0).await,
        [0, 1, 2]
    );
}

async fn candidates(filter: &CandidateFilter) -> Vec<DocId> {
    candidates_in(&DOCS, filter).await
}

async fn candidates_in(docs: &[(&str, &str)], filter: &CandidateFilter) -> Vec<DocId> {
    let (index, reader) = slot_index(docs);
    let searcher = reader.searcher();
    let segment_reader = searcher.segment_reader(0);
    let schema = index.schema();
    let terms = SegmentTerms::new(segment_reader);
    candidate_docs(filter, &terms, segment_reader, &schema)
        .await
        .unwrap()
        .docs()
}

#[tokio::test]
async fn candidate_docs_term_regex_and_or_phrase() {
    // DOCS tags: 0=NN|NNS|DT  1=NNS|NN|DT  2=NN|DT
    assert_eq!(
        candidates(&CandidateFilter::term("tag", "NNS")).await,
        [0, 1]
    );
    assert_eq!(
        candidates(&CandidateFilter::regex("tag", "NN.*")).await,
        [0, 1, 2],
        "NN and NNS both match NN.*"
    );
    assert_eq!(
        candidates(&CandidateFilter::And(vec![
            CandidateFilter::term("tag", "NNS"),
            CandidateFilter::term("incoming_edges", "nsubj"),
        ]))
        .await,
        [0, 1],
        "doc-level And: both terms somewhere in the sentence"
    );
    assert_eq!(
        candidates(&CandidateFilter::Or(vec![
            CandidateFilter::term("tag", "missing"),
            CandidateFilter::term("tag", "DT"),
        ]))
        .await,
        [0, 1, 2]
    );
    // Phrase is document-level presence of every term (adjacency left to the matcher).
    assert_eq!(
        candidates(&CandidateFilter::phrase(
            "tag",
            vec!["NN".into(), "DT".into()],
        ))
        .await,
        [0, 1, 2]
    );
    assert_eq!(
        candidates(&CandidateFilter::phrase(
            "tag",
            vec!["NNS".into(), "missing".into()],
        ))
        .await,
        Vec::<DocId>::new()
    );
}

#[tokio::test]
async fn candidate_docs_missing_field_is_empty_and_all_is_full() {
    assert_eq!(
        candidates(&CandidateFilter::term("no_such_field", "NN")).await,
        Vec::<DocId>::new()
    );
    assert_eq!(candidates(&CandidateFilter::All).await, [0, 1, 2]);
    // A conjunction with nothing but `All` in it constrains nothing.
    assert_eq!(
        candidates(&CandidateFilter::And(vec![CandidateFilter::All])).await,
        [0, 1, 2]
    );
    // SameToken at document level is And of its children.
    assert_eq!(
        candidates(&CandidateFilter::SameToken(vec![
            CandidateFilter::term("tag", "NNS"),
            CandidateFilter::term("incoming_edges", "nsubj"),
        ]))
        .await,
        [0, 1]
    );
}

#[tokio::test]
async fn conjunction_mixes_leaves_nested_or_and_both_probe_strategies() {
    // DOCS: 0=NN|NNS|DT/-,nsubj,-  1=NNS|NN|DT/-,-,nsubj  2=NN|DT/nsubj,-
    // Rarest leaf `tag=A` (1 doc), then `tag=X` (201 docs): 1 candidate x 1 term x 128 <= 201
    // -> seek path.
    let mut sparse = vec![("A|X", "")];
    sparse.extend(std::iter::repeat_n(("X", ""), 200));
    assert_eq!(
        candidates_in(
            &sparse,
            &CandidateFilter::And(vec![
                CandidateFilter::term("tag", "X"),
                CandidateFilter::term("tag", "A"),
            ])
        )
        .await,
        [0]
    );
    let docs = [("A|X", ""), ("B|X", ""), ("C|X", ""), ("D", "")];
    // Rarest `tag=X` (3 docs), then `tag=/A|B|C|D/`: 3 x 4 x 128 > df 4 -> bitmap.
    assert_eq!(
        candidates_in(
            &docs,
            &CandidateFilter::And(vec![
                CandidateFilter::regex("tag", "A|B|C|D"),
                CandidateFilter::term("tag", "X"),
            ])
        )
        .await,
        [0, 1, 2]
    );
    // A nested `Or` intersects with the leaves.
    assert_eq!(
        candidates(&CandidateFilter::And(vec![
            CandidateFilter::term("tag", "DT"),
            CandidateFilter::Or(vec![
                CandidateFilter::term("tag", "NNS"),
                CandidateFilter::term("incoming_edges", "missing"),
            ]),
        ]))
        .await,
        [0, 1]
    );
    // An `Or` with an unconstrained branch admits everything.
    assert_eq!(
        candidates(&CandidateFilter::Or(vec![
            CandidateFilter::term("tag", "NNS"),
            CandidateFilter::All,
        ]))
        .await,
        [0, 1, 2]
    );
    // A missing term empties the conjunction regardless of the others.
    assert_eq!(
        candidates(&CandidateFilter::And(vec![
            CandidateFilter::term("tag", "DT"),
            CandidateFilter::term("tag", "missing"),
        ]))
        .await,
        Vec::<DocId>::new()
    );
}
