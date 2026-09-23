use super::*;
use tantivy::schema::TEXT;

#[test]
fn required_terms_include_top_level_same_token_children_only() {
    let mut builder = Schema::builder();
    let word = builder.add_text_field("word", TEXT);
    let incoming = builder.add_text_field("incoming_edges", TEXT);
    let schema = builder.build();

    let filter = CandidateFilter::And(vec![
        CandidateFilter::SameToken(vec![
            CandidateFilter::term("word", "cat"),
            CandidateFilter::term("incoming_edges", "nsubj"),
        ]),
        // Any one alternative may hold: none is required.
        CandidateFilter::Or(vec![
            CandidateFilter::SameToken(vec![CandidateFilter::term("word", "dog")]),
            CandidateFilter::term("word", "bird"),
        ]),
    ]);
    assert_eq!(
        required_terms(&filter, &schema),
        [
            Term::from_field_text(word, "cat"),
            Term::from_field_text(incoming, "nsubj"),
        ]
    );
}

#[test]
fn required_terms_skip_regex_children() {
    let mut builder = Schema::builder();
    builder.add_text_field("tag", TEXT);
    builder.add_text_field("incoming_edges", TEXT);
    let schema = builder.build();
    let filter = CandidateFilter::SameToken(vec![
        CandidateFilter::regex("tag", "NN.*"),
        CandidateFilter::term("incoming_edges", "nsubj"),
    ]);
    let incoming = schema.get_field("incoming_edges").unwrap();
    assert_eq!(
        required_terms(&filter, &schema),
        [Term::from_field_text(incoming, "nsubj")],
        "regex terms are not required-term early-abort keys"
    );
}

#[test]
fn pattern_payload_round_trips_and_rejects_bad_shape() {
    let payload = PatternPayload {
        pattern: "[tag=/VB.*/]".into(),
    };
    let json = payload.to_json();
    let back = PatternPayload::from_json(&json).unwrap();
    assert_eq!(back.pattern, payload.pattern);
    assert!(PatternPayload::from_json(&serde_json::json!({})).is_err());
    assert!(PatternPayload::from_json(&serde_json::json!({"pattern": 1})).is_err());
}

#[test]
fn compile_marks_exact_surface_and_caches() {
    let ext = RustieQueryExtension::default();
    let exact = ext.compile("[tag=/NN.*/]").unwrap();
    assert!(exact.exact);
    assert!(exact.surface.is_some());
    assert!(matches!(exact.compiled, CompiledQuery::Surface(_)));

    let again = ext.compile("[tag=/NN.*/]").unwrap();
    assert!(Arc::ptr_eq(&exact, &again), "compiled-query LRU must hit");

    let sequence = ext.compile("[word=cat] [word=sat]").unwrap();
    assert!(!sequence.exact);
    assert!(sequence.surface.is_some());

    let graph = ext.compile("[tag=/V.*/] >nsubj [word=cat]").unwrap();
    assert!(!graph.exact);
    assert!(graph.surface.is_none());
    assert!(matches!(graph.compiled, CompiledQuery::Graph(_)));
}

#[test]
fn cache_wrapped_query_ast_is_cache_of_rustie_extension() {
    let ast = cache_wrapped_query_ast("[word=cat]");
    match ast {
        QueryAst::Cache(node) => match &*node.inner {
            QueryAst::Extension(ext) => {
                assert_eq!(ext.kind, crate::QUERY_KIND);
                let payload = PatternPayload::from_json(&ext.payload).unwrap();
                assert_eq!(payload.pattern, "[word=cat]");
            }
            other => panic!("expected Extension inside Cache, got {other:?}"),
        },
        other => panic!("expected Cache node, got {other:?}"),
    }
}

#[test]
fn build_returns_query_warmup_and_required_terms() {
    let mut builder = Schema::builder();
    builder.add_text_field("word", TEXT);
    let schema = builder.build();
    let ext = RustieQueryExtension::default();
    let built = ext
        .build(&serde_json::json!({"pattern": "[word=cat]"}), &schema)
        .unwrap();
    assert!(!built.required_terms.is_empty());
    assert!(built.warmup.is_some());
    let _ = built
        .query
        .weight(EnableScoring::disabled_from_schema(&schema))
        .unwrap();
}
