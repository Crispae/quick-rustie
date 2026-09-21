//! Exhaustive catalog of RustIE query-language variations.
//!
//! Each case must parse (or intentionally fail). Categories follow the RustIE
//! query-language docs: tokens, booleans, sequences, quantifiers, graph hops,
//! named captures, look-around, and complex combinations.

use rustie_query::{
    Assertion, Constraint, Matcher, Pattern, QuantifierKind, QueryError, QueryParser, Traversal,
};

fn parser() -> QueryParser {
    QueryParser::new()
}

fn parse(query: &str) -> Pattern {
    parser()
        .parse_query(query)
        .unwrap_or_else(|e| panic!("expected OK for `{query}`: {e}"))
}

fn assert_parses(cases: &[(&str, &str)]) {
    for (label, query) in cases {
        let result = parser().parse_query(query);
        assert!(
            result.is_ok(),
            "{label}: `{query}` should parse, got {:?}",
            result.err()
        );
    }
}

fn assert_rejects(cases: &[(&str, &str)]) {
    for (label, query) in cases {
        let result = parser().parse_query(query);
        assert!(
            result.is_err(),
            "{label}: `{query}` should be rejected, got Ok({:?})",
            result.ok()
        );
        assert!(
            matches!(result.err().unwrap(), QueryError::Parse(_)),
            "{label}: expected Parse error"
        );
    }
}

// ---------------------------------------------------------------------------
// Bare / default-field
// ---------------------------------------------------------------------------

#[test]
fn bare_default_field_queries() {
    assert_parses(&[
        ("bare word", "cat"),
        ("bare alnum", "gene123"),
        ("bare regex", "/gene.*/"),
        ("bare regex anchors", "/^[A-Z].*$/"),
        ("bare regex braces", "/a{2,4}/"),
        ("bare regex char class", "/[A-Za-z]+/"),
    ]);

    let p = parse("cat");
    match p {
        Pattern::Constraint(Constraint::Field { name, matcher }) => {
            assert_eq!(name, "word");
            assert!(matches!(matcher, Matcher::String(s) if s == "cat"));
        }
        other => panic!("expected word constraint, got {other:?}"),
    }

    let p = parse("/gene.*/");
    match p {
        Pattern::Constraint(Constraint::Field { name, matcher }) => {
            assert_eq!(name, "word");
            assert!(matches!(matcher, Matcher::Regex { pattern, .. } if pattern == "gene.*"));
        }
        other => panic!("expected regex constraint, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Single-token constraints
// ---------------------------------------------------------------------------

#[test]
fn single_token_constraints() {
    assert_parses(&[
        ("exact field", "[word=John]"),
        ("entity tag", "[entity=B-Gene]"),
        ("negation !=", "[word!=the]"),
        ("field regex", "[word=/^[A-Z].*/]"),
        ("empty wildcard", "[]"),
        ("star wildcard", "[*]"),
        ("fuzzy", "[word=diabetes~]"),
        ("pos tag", "[pos=VBZ]"),
        ("chunk", "[chunk=B-NP]"),
        ("tag regex", "[tag=/VB.*/]"),
        ("underscore value", "[lemma=look_up]"),
        ("hyphen value", "[word=state-of-the-art]"),
    ]);

    match parse("[word=diabetes~]") {
        Pattern::Constraint(Constraint::Fuzzy { name, matcher }) => {
            assert_eq!(name, "word");
            assert_eq!(matcher, "diabetes");
        }
        other => panic!("expected Fuzzy, got {other:?}"),
    }

    match parse("[word!=the]") {
        Pattern::Constraint(Constraint::Negated(inner)) => match *inner {
            Constraint::Field { name, .. } => assert_eq!(name, "word"),
            other => panic!("expected Field inside Negated, got {other:?}"),
        },
        other => panic!("expected Negated, got {other:?}"),
    }

    match parse("[]") {
        Pattern::Constraint(Constraint::Wildcard) => {}
        other => panic!("expected Wildcard, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Token-level boolean combinators
// ---------------------------------------------------------------------------

#[test]
fn token_boolean_combinators() {
    assert_parses(&[
        ("OR two words", "[word=cat | word=dog]"),
        ("AND word+pos", "[word=run & tag=/VB.*/]"),
        ("prefix !", "[!word=the]"),
        ("AND with !", "[word=cat & !pos=DT]"),
        ("OR of ANDs", "[word=a & pos=NN | word=b & pos=VB]"),
        ("grouped OR", "[(word=cat | word=dog) & pos=NN]"),
        ("triple OR", "[word=a | word=b | word=c]"),
        ("triple AND", "[word=a & pos=NN & tag=NN]"),
    ]);

    match parse("[word=cat | word=dog]") {
        Pattern::Constraint(Constraint::Disjunctive(parts)) => assert_eq!(parts.len(), 2),
        other => panic!("expected Disjunctive, got {other:?}"),
    }

    match parse("[word=run & tag=/VB.*/]") {
        Pattern::Constraint(Constraint::Conjunctive(parts)) => assert_eq!(parts.len(), 2),
        other => panic!("expected Conjunctive, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Sequences (adjacent tokens / gaps)
// ---------------------------------------------------------------------------

#[test]
fn sequence_patterns() {
    assert_parses(&[
        ("two adjacent", "[word=the] [word=cat]"),
        ("three adjacent", "[word=the] [pos=JJ] [word=cat]"),
        ("gap wildcard", "[word=the] [] [word=cat]"),
        ("bounded gap", "[word=start] []{0,3} [word=end]"),
        ("bounded gap exact-ish", "[word=a] []{1,2} [word=b]"),
        ("OR then verb", "[word=cat | word=dog] [pos=/V.*/]"),
        ("mixed regex sequence", "[pos=DT] [pos=/J.*/] [pos=/N.*/]"),
    ]);

    match parse("[word=the] [word=cat]") {
        Pattern::Concatenated(parts) => assert_eq!(parts.len(), 2),
        other => panic!("expected Concatenated, got {other:?}"),
    }

    match parse("[word=start] []{0,3} [word=end]") {
        Pattern::Concatenated(parts) => {
            assert_eq!(parts.len(), 3);
            match &parts[1] {
                Pattern::Repetition { min, max, kind, .. } => {
                    assert_eq!(*min, 0);
                    assert_eq!(*max, Some(3));
                    assert_eq!(*kind, QuantifierKind::Greedy);
                }
                other => panic!("expected Repetition gap, got {other:?}"),
            }
        }
        other => panic!("expected Concatenated, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Quantifiers (greedy + lazy + groups)
// ---------------------------------------------------------------------------

#[test]
fn quantifier_variations() {
    assert_parses(&[
        ("star greedy", "[pos=JJ]*"),
        ("plus greedy", "[pos=JJ]+"),
        ("optional greedy", "[pos=JJ]?"),
        ("range {n,m}", "[]{2,5}"),
        ("range {,m}", "[]{,3}"),
        ("range {n,}", "[]{2,}"),
        ("star lazy", "[]*?"),
        ("plus lazy", "[]+?"),
        ("optional lazy", "[pos=JJ]??"),
        ("range lazy", "[]{0,3}?"),
        ("lazy gap sequence", "[word=cat] []*? [word=dog]"),
        ("quantified group", "([pos=JJ] [word=and])+ [pos=NN]"),
        ("optional group", "([pos=DT] [pos=JJ])? [pos=NN]"),
        ("star group", "([word=and] [pos=JJ])* [pos=NN]"),
    ]);

    match parse("[pos=JJ]+") {
        Pattern::Repetition {
            min,
            max,
            kind,
            pattern,
        } => {
            assert_eq!(min, 1);
            assert_eq!(max, None);
            assert_eq!(kind, QuantifierKind::Greedy);
            assert!(matches!(
                *pattern,
                Pattern::Constraint(Constraint::Field { .. })
            ));
        }
        other => panic!("expected Repetition, got {other:?}"),
    }

    match parse("[]*?") {
        Pattern::Repetition { min, max, kind, .. } => {
            assert_eq!(min, 0);
            assert_eq!(max, None);
            assert_eq!(kind, QuantifierKind::Lazy);
        }
        other => panic!("expected lazy Repetition, got {other:?}"),
    }

    match parse("([pos=JJ] [word=and])+ [pos=NN]") {
        Pattern::Concatenated(parts) => {
            assert_eq!(parts.len(), 2);
            assert!(matches!(
                &parts[0],
                Pattern::Repetition {
                    min: 1,
                    max: None,
                    kind: QuantifierKind::Greedy,
                    ..
                }
            ));
        }
        other => panic!("expected Concatenated quantified group, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Graph traversals
// ---------------------------------------------------------------------------

#[test]
fn graph_single_hop() {
    assert_parses(&[
        ("outgoing label", "[word=eats] >nsubj [word=John]"),
        ("incoming label", "[word=John] <nsubj [word=eats]"),
        ("outgoing wildcard", "[word=eats] >> []"),
        ("incoming wildcard", "[word=pizza] << []"),
        ("outgoing regex hop", "[word=eats] >/nsubj|dobj/ []"),
        ("outgoing alt labels", "[word=eats] >nsubj|dobj []"),
        ("incoming alt labels", "[word=John] <nsubj|dobj []"),
        ("label with underscore", "[pos=NN] >nmod_of [pos=NN]"),
        ("label with colon", "[pos=VB] >nsubj:xsubj []"),
        ("label with hyphen", "[pos=VB] >obl-agent []"),
        ("trailing hop → wildcard dst", "[word=eats] >nsubj"),
        ("noun subject of verb", "[pos=/N.*/] <nsubj [pos=/V.*/]"),
    ]);

    match parse("[word=eats] >nsubj [word=John]") {
        Pattern::GraphTraversal {
            traversal: Traversal::Outgoing(Matcher::String(label)),
            ..
        } => assert_eq!(label, "nsubj"),
        other => panic!("expected Outgoing nsubj, got {other:?}"),
    }

    match parse("[word=eats] >> []") {
        Pattern::GraphTraversal {
            traversal: Traversal::OutgoingWildcard,
            ..
        } => {}
        other => panic!("expected OutgoingWildcard, got {other:?}"),
    }

    match parse("[word=eats] >nsubj|dobj []") {
        Pattern::GraphTraversal {
            traversal: Traversal::Disjunctive(parts),
            ..
        } => assert_eq!(parts.len(), 2),
        other => panic!("expected Disjunctive hop, got {other:?}"),
    }
}

#[test]
fn graph_quantified_hops() {
    assert_parses(&[
        ("optional hop", "[pos=NN] >amod? [pos=JJ]"),
        ("star hop", "[pos=NN] >nmod* [pos=VB]"),
        ("plus hop", "[pos=NN] >conj+ [pos=NN]"),
        ("optional wildcard hop", "[pos=NN] >>? []"),
        ("star wildcard hop", "[pos=NN] >>* []"),
        ("plus wildcard hop", "[pos=NN] >>+ []"),
        ("optional incoming", "[pos=VB] <nsubj? [pos=NN]"),
        ("star incoming", "[pos=VB] <nmod* [pos=NN]"),
    ]);

    match parse("[pos=NN] >nmod* [pos=VB]") {
        Pattern::GraphTraversal {
            traversal: Traversal::KleeneStar(inner),
            ..
        } => assert!(matches!(
            *inner,
            Traversal::Outgoing(Matcher::String(ref s)) if s == "nmod"
        )),
        other => panic!("expected KleeneStar hop, got {other:?}"),
    }

    match parse("[pos=NN] >amod? [pos=JJ]") {
        Pattern::GraphTraversal {
            traversal: Traversal::Optional(inner),
            ..
        } => assert!(matches!(*inner, Traversal::Outgoing(_))),
        other => panic!("expected Optional hop, got {other:?}"),
    }

    // `>conj+` desugars to `>conj` then `>conj*`
    match parse("[pos=NN] >conj+ [pos=NN]") {
        Pattern::GraphTraversal {
            traversal: Traversal::Concatenated(steps),
            ..
        } => {
            assert_eq!(steps.len(), 2);
            assert!(matches!(steps[0], Traversal::Outgoing(_)));
            assert!(matches!(steps[1], Traversal::KleeneStar(_)));
        }
        other => panic!("expected + hop as Concatenated, got {other:?}"),
    }
}

#[test]
fn graph_hop_chains_and_spans() {
    assert_parses(&[
        ("hop chain two", "[word=a] >nsubj >dobj [word=b]"),
        ("hop chain three", "[word=eats] >nsubj >poss []"),
        ("multi-node chain", "[pos=NNP] <nsubj [pos=VB] >dobj [pos=NN]"),
        ("span endpoint src", "[pos=DT] [pos=NN]+ <nsubj [pos=VB]"),
        ("span after hop", "[pos=VB] >dobj [pos=DT] [pos=NN]+"),
        ("adjacent then hop", "[word=the] [word=cat] <nsubj [pos=VB]"),
        ("hop then trailing atoms", "[pos=VB] >dobj [pos=DT] [pos=NN]"),
        ("middle span between hops", "[pos=NNP] <nsubj [pos=VB] [pos=RB] >dobj [pos=NN]"),
    ]);

    match parse("[word=a] >nsubj >dobj [word=b]") {
        Pattern::GraphTraversal {
            traversal: Traversal::Concatenated(steps),
            ..
        } => assert_eq!(steps.len(), 2),
        other => panic!("expected hop chain Concatenated, got {other:?}"),
    }

    match parse("[pos=DT] [pos=NN]+ <nsubj [pos=VB]") {
        Pattern::GraphTraversal { src, .. } => {
            assert!(matches!(*src, Pattern::Concatenated(_)));
        }
        other => panic!("expected GraphTraversal with span src, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Named captures
// ---------------------------------------------------------------------------

#[test]
fn named_capture_variations() {
    assert_parses(&[
        ("plain capture", "(?<subject> [word=John])"),
        ("token typed", "(?<subj:token> [pos=NN])"),
        ("phrase typed", "(?<arg1:phrase> [pos=NN])"),
        ("pred typed", "(?<rel:pred> [pos=VB])"),
        ("capture sequence", "(?<np> [pos=DT] [pos=NN])"),
        ("capture on graph node", "(?<rel:pred> [pos=VB]) >nsubj (?<arg1:phrase> [])"),
        ("capture wraps traversal", "(?<m> [pos=NN] >amod [pos=JJ])"),
        ("nested name alnum", "(?<arg2> [pos=NN])"),
    ]);

    match parse("(?<subject> [word=John])") {
        Pattern::NamedCapture { name, pattern } => {
            assert_eq!(name, "subject");
            assert!(matches!(*pattern, Pattern::Constraint(_)));
        }
        other => panic!("expected NamedCapture, got {other:?}"),
    }

    match parse("(?<arg1:phrase> [pos=NN])") {
        Pattern::NamedCapture { name, .. } => assert_eq!(name, "arg1:phrase"),
        other => panic!("expected NamedCapture with typed name, got {other:?}"),
    }

    match parse("(?<rel:pred> [pos=VB]) >nsubj (?<arg1:phrase> [])") {
        Pattern::GraphTraversal { src, dst, .. } => {
            assert!(matches!(*src, Pattern::NamedCapture { .. }));
            assert!(matches!(*dst, Pattern::NamedCapture { .. }));
        }
        other => panic!("expected GraphTraversal with captures, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Look-around assertions
// ---------------------------------------------------------------------------

#[test]
fn lookaround_assertions() {
    assert_parses(&[
        ("pos lookahead", "(?= [word=test])"),
        ("neg lookahead", "(?! [word=cat])"),
        ("pos lookbehind", "(?<= [word=the])"),
        ("neg lookbehind", "(?<! [word=a])"),
        ("lookahead after token", "[word=the] (?! [word=cat])"),
        ("lookbehind before token", "(?<= [pos=DT]) [pos=NN]"),
        ("lookahead sequence", "(?= [pos=JJ] [pos=NN])"),
    ]);

    match parse("(?= [word=test])") {
        Pattern::Assertion(Assertion::PositiveLookahead(_)) => {}
        other => panic!("expected PositiveLookahead, got {other:?}"),
    }
    match parse("(?! [word=cat])") {
        Pattern::Assertion(Assertion::NegativeLookahead(_)) => {}
        other => panic!("expected NegativeLookahead, got {other:?}"),
    }
    match parse("(?<= [word=the])") {
        Pattern::Assertion(Assertion::PositiveLookbehind(_)) => {}
        other => panic!("expected PositiveLookbehind, got {other:?}"),
    }
    match parse("(?<! [word=a])") {
        Pattern::Assertion(Assertion::NegativeLookbehind(_)) => {}
        other => panic!("expected NegativeLookbehind, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Complex combinations (doc examples)
// ---------------------------------------------------------------------------

#[test]
fn complex_doc_examples() {
    assert_parses(&[
        ("the cat", "[word=the] [word=cat]"),
        ("cat|dog then verb", "[word=cat | word=dog] [pos=/V.*/]"),
        ("noun subject of verb", "[pos=/N.*/] <nsubj [pos=/V.*/]"),
        ("verb with nsubj John", "[word=John] <nsubj [pos=VBZ]"),
        ("born-style chain", "[word=born] >nsubj >amod []"),
        (
            "OpenIE-ish triple",
            "(?<rel:pred> [pos=VB]) >nsubj (?<arg1:phrase> [pos=/N.*/]) >dobj (?<arg2:phrase> [])",
        ),
        (
            "quantified adj then noun subject",
            "([pos=JJ] [word=and])+ [pos=NN] <nsubj [pos=VB]",
        ),
        (
            "lazy scan to end",
            "[word=start] []*? [word=end]",
        ),
        (
            "grouped then hop",
            "([pos=NN] >amod [pos=JJ]) >nsubj [pos=VB]",
        ),
    ]);
}

// ---------------------------------------------------------------------------
// Rejected / invalid syntax
// ---------------------------------------------------------------------------

#[test]
fn invalid_queries_are_rejected() {
    assert_rejects(&[
        ("empty", ""),
        ("whitespace only", "   "),
        ("unmatched [", "[word=test"),
        ("unmatched (", "(?<a> [word=x]"),
        ("unclosed regex", "[word=/abc"),
        ("trailing garbage", "[word=cat] !!!"),
        ("lonely hop", ">nsubj"),
        ("double pipe junk", "[word=a || word=b]"),
    ]);
}

// ---------------------------------------------------------------------------
// Mega catalog — one table, every variation must parse
// ---------------------------------------------------------------------------

#[test]
fn mega_catalog_all_must_parse() {
    // Flat list used as a regression checkpoint for the whole language surface.
    const ALL: &[&str] = &[
        // bare
        "cat",
        "/gene.*/",
        "/^[A-Z].*$/",
        // tokens
        "[word=John]",
        "[entity=B-Gene]",
        "[word!=the]",
        "[word=/^[A-Z].*/]",
        "[]",
        "[*]",
        "[word=diabetes~]",
        "[pos=VBZ]",
        "[lemma=look_up]",
        "[word=state-of-the-art]",
        // booleans
        "[word=cat | word=dog]",
        "[word=run & tag=/VB.*/]",
        "[!word=the]",
        "[word=cat & !pos=DT]",
        "[(word=cat | word=dog) & pos=NN]",
        "[word=a | word=b | word=c]",
        "[word=a & pos=NN & tag=NN]",
        // sequences
        "[word=the] [word=cat]",
        "[word=the] [pos=JJ] [word=cat]",
        "[word=the] [] [word=cat]",
        "[word=start] []{0,3} [word=end]",
        "[word=cat | word=dog] [pos=/V.*/]",
        // quantifiers
        "[pos=JJ]*",
        "[pos=JJ]+",
        "[pos=JJ]?",
        "[]{2,5}",
        "[]{,3}",
        "[]{2,}",
        "[]*?",
        "[]+?",
        "[pos=JJ]??",
        "[]{0,3}?",
        "[word=cat] []*? [word=dog]",
        "([pos=JJ] [word=and])+ [pos=NN]",
        "([pos=DT] [pos=JJ])? [pos=NN]",
        // graph
        "[word=eats] >nsubj [word=John]",
        "[word=John] <nsubj [word=eats]",
        "[word=eats] >> []",
        "[word=pizza] << []",
        "[word=eats] >/nsubj|dobj/ []",
        "[word=eats] >nsubj|dobj []",
        "[word=John] <nsubj|dobj []",
        "[pos=NN] >nmod_of [pos=NN]",
        "[pos=VB] >nsubj:xsubj []",
        "[word=eats] >nsubj",
        "[pos=/N.*/] <nsubj [pos=/V.*/]",
        "[pos=NN] >amod? [pos=JJ]",
        "[pos=NN] >nmod* [pos=VB]",
        "[pos=NN] >conj+ [pos=NN]",
        "[pos=NN] >>? []",
        "[pos=NN] >>* []",
        "[pos=NN] >>+ []",
        "[pos=VB] <nsubj? [pos=NN]",
        "[pos=VB] <nmod* [pos=NN]",
        "[word=a] >nsubj >dobj [word=b]",
        "[word=eats] >nsubj >poss []",
        "[pos=NNP] <nsubj [pos=VB] >dobj [pos=NN]",
        "[pos=DT] [pos=NN]+ <nsubj [pos=VB]",
        "[pos=VB] >dobj [pos=DT] [pos=NN]+",
        "[word=the] [word=cat] <nsubj [pos=VB]",
        "[pos=VB] >dobj [pos=DT] [pos=NN]",
        "[pos=NNP] <nsubj [pos=VB] [pos=RB] >dobj [pos=NN]",
        // captures
        "(?<subject> [word=John])",
        "(?<subj:token> [pos=NN])",
        "(?<arg1:phrase> [pos=NN])",
        "(?<rel:pred> [pos=VB])",
        "(?<np> [pos=DT] [pos=NN])",
        "(?<rel:pred> [pos=VB]) >nsubj (?<arg1:phrase> [])",
        "(?<m> [pos=NN] >amod [pos=JJ])",
        // look-around
        "(?= [word=test])",
        "(?! [word=cat])",
        "(?<= [word=the])",
        "(?<! [word=a])",
        "[word=the] (?! [word=cat])",
        "(?<= [pos=DT]) [pos=NN]",
        "(?= [pos=JJ] [pos=NN])",
        // complex
        "[word=John] <nsubj [pos=VBZ]",
        "[word=born] >nsubj >amod []",
        "(?<rel:pred> [pos=VB]) >nsubj (?<arg1:phrase> [pos=/N.*/]) >dobj (?<arg2:phrase> [])",
        "([pos=JJ] [word=and])+ [pos=NN] <nsubj [pos=VB]",
        "[word=start] []*? [word=end]",
        "([pos=NN] >amod [pos=JJ]) >nsubj [pos=VB]",
    ];

    let mut failures = Vec::new();
    for query in ALL {
        if let Err(e) = parser().parse_query(query) {
            failures.push(format!("`{query}` → {e}"));
        }
    }
    assert!(
        failures.is_empty(),
        "{} catalog queries failed to parse:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
