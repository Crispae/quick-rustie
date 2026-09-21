//! Differential test: RustIE patterns run inside Quickwit leaf search (real indexing pipeline,
//! real split bundle with its GPH2 component, real single-node search) must return exactly the
//! sentences the string-based reference evaluator accepts, with exact hit counts.

use std::collections::{BTreeSet, HashMap};

use quickwit_indexing::TestSandbox;
use quickwit_proto::search::SearchRequest;
use quickwit_query::query_ast::{ExtensionQuery, QueryAst};
use quickwit_search::single_node_search;
use rustie_compiler::matching::SpanProg;
use rustie_compiler::{CompiledQuery, DEFAULT_SENTENCE_CAP, QueryCompiler, evaluate_on_sentence};
use rustie_schema::{
    IndexConfigOptions, SentenceDoc, flatten_odinson_json, postings_index_config_yaml,
};
use serde_json::{Value as JsonValue, json};

const WORDS: [&str; 6] = ["the", "cat", "sat", "Dogs", "bark", "loudly"];
const LEMMAS: [&str; 6] = ["the", "cat", "sit", "dog", "bark", "loud"];
const TAGS: [&str; 4] = ["DT", "NN", "VBD", "RB"];
const LABELS: [&str; 4] = ["nsubj", "det", "advmod", "dobj"];

/// Deterministic xorshift, so failures are reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n as u64) as usize
    }
}

/// One Odinson document of 1–3 sentences with a random tree plus a few extra edges.
fn odinson_doc(id: usize, rng: &mut Rng) -> String {
    let sentences: Vec<JsonValue> = (0..1 + rng.next(3))
        .map(|_| {
            let n = 1 + rng.next(9);
            let picks: Vec<usize> = (0..n).map(|_| rng.next(WORDS.len())).collect();
            let root = rng.next(n);
            let mut edges = Vec::new();
            for dep in 0..n {
                if dep != root {
                    let gov = rng.next(n);
                    if gov != dep {
                        edges.push(json!([gov, dep, LABELS[rng.next(LABELS.len())]]));
                    }
                }
            }
            for _ in 0..rng.next(3) {
                edges.push(json!([rng.next(n), rng.next(n), LABELS[rng.next(LABELS.len())]]));
            }
            let field = |name: &str, values: Vec<&str>| {
                json!({"name": name, "$type": "ai.lum.odinson.TokensField", "tokens": values})
            };
            json!({
                "numTokens": n,
                "fields": [
                    field("word", picks.iter().map(|&i| WORDS[i]).collect()),
                    field("lemma", picks.iter().map(|&i| LEMMAS[i]).collect()),
                    field("tag", picks.iter().map(|_| TAGS[rng.next(TAGS.len())]).collect()),
                    {"name": "dependencies", "$type": "ai.lum.odinson.GraphField",
                     "edges": edges, "roots": [root]}
                ]
            })
        })
        .collect();
    json!({"id": format!("d{id}"), "sentences": sentences}).to_string()
}

/// The `doc_mapping` section of the IE index config.
fn doc_mapping_yaml() -> String {
    let config: serde_yaml::Value =
        serde_yaml::from_str(&postings_index_config_yaml(&IndexConfigOptions::default())).unwrap();
    serde_yaml::to_string(&config["doc_mapping"]).unwrap()
}

/// Reference: does `pattern` match `sentence`, evaluated on strings?
fn reference_matches(compiled: &CompiledQuery, sentence: &SentenceDoc) -> bool {
    let fields: HashMap<String, Vec<String>> = sentence
        .tokens
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    match compiled {
        CompiledQuery::Surface(plan) => {
            let prog = SpanProg::compile(&plan.pattern).unwrap();
            let n = sentence.sentence_length as usize;
            let get = |field: &str, tok: usize| {
                fields
                    .get(field)
                    .and_then(|values| values.get(tok))
                    .map_or("", String::as_str)
            };
            !prog.spans_over_tokens(n, &get, true).is_empty()
        }
        CompiledQuery::Graph(graph) => sentence.primary_graph().is_some_and(|g| {
            !evaluate_on_sentence(&graph.plan, g, &fields, DEFAULT_SENTENCE_CAP).is_empty()
        }),
    }
}

const PATTERNS: &[&str] = &[
    // Token patterns: exact, regex, fuzzy, negation, sequences, gaps, quantifiers.
    "[word=cat]",
    "[word=cat] [word=sat]",
    "[lemma=dog] []{0,2} [word=/b.*/]",
    "[word=/[A-Z].*/]",
    "[word=dog~]",
    "[!word=the] [tag=NN]",
    "[tag=/NN|VBD/]+ [word=loudly]",
    "(?<x> [word=the]) (?= [tag=NN])",
    "[word=nothing]",
    // Graph patterns: labels, directions, wildcards, quantifiers, colocated tags, spans.
    "[tag=VBD] >nsubj [word=cat]",
    "[] >nsubj []",
    "[word=bark] <dobj []",
    "[tag=NN] >> [tag=DT]",
    "[] >nsubj|dobj [lemma=/d.*/]",
    "[tag=VBD] >advmod? [word=loudly]",
    "[word=the] >det* [tag=NN]",
    "[tag=NN] >nsubj >dobj []",
    "[word=cat] [word=sat] >nsubj []",
    "[word=Dogs~] >nsubj [!tag=DT]",
    "[tag=NN] >/nsubj|det/ [word=nothing]",
];

async fn search(
    sandbox: &TestSandbox,
    pattern: &str,
    max_hits: u64,
) -> anyhow::Result<(u64, BTreeSet<String>)> {
    let query_ast = QueryAst::Extension(ExtensionQuery {
        kind: rustie_leaf::QUERY_KIND.to_string(),
        payload: json!({ "pattern": pattern }),
    });
    let response = single_node_search(
        SearchRequest {
            index_id_patterns: vec!["rustie-leaf".to_string()],
            query_ast: serde_json::to_string(&query_ast)?,
            max_hits,
            ..Default::default()
        },
        sandbox.metastore(),
        sandbox.storage_resolver(),
    )
    .await?;
    anyhow::ensure!(
        response.failed_splits.is_empty() && response.errors.is_empty(),
        "{:?} {:?}",
        response.failed_splits,
        response.errors
    );
    let ids = response
        .hits
        .iter()
        .map(|hit| {
            let doc: JsonValue = serde_json::from_str(&hit.json).unwrap();
            doc["sentence_id"].as_str().unwrap().to_string()
        })
        .collect();
    Ok((response.num_hits, ids))
}

#[tokio::test]
async fn leaf_search_equals_reference_evaluator() -> anyhow::Result<()> {
    rustie_leaf::register();
    let sandbox = TestSandbox::create("rustie-leaf", &doc_mapping_yaml(), "{}", &["word"]).await?;

    let mut rng = Rng(0x9e3779b97f4a7c15);
    let mut sentences: Vec<SentenceDoc> = Vec::new();
    // Three splits of different sizes; the largest spans several GPH2 blocks.
    let mut next_id = 0;
    for num_docs in [40, 150, 7] {
        let mut docs = Vec::new();
        for _ in 0..num_docs {
            let flattened = flatten_odinson_json(&odinson_doc(next_id, &mut rng))?;
            next_id += 1;
            docs.extend(flattened.iter().map(SentenceDoc::to_quickwit_json));
            sentences.extend(flattened);
        }
        sandbox.add_documents(docs).await?;
    }
    assert!(sentences.len() > 300, "{}", sentences.len());

    let compiler = QueryCompiler::new();
    for pattern in PATTERNS {
        let compiled = compiler.compile(pattern)?;
        let expected: BTreeSet<String> = sentences
            .iter()
            .filter(|s| reference_matches(&compiled, s))
            .map(|s| s.sentence_id.clone())
            .collect();
        let (num_hits, got) = search(&sandbox, pattern, 10_000).await?;
        assert_eq!(got, expected, "pattern {pattern:?}");
        assert_eq!(
            num_hits,
            expected.len() as u64,
            "exact count for {pattern:?}"
        );

        // A page smaller than the result still reports the exact total.
        let (num_hits, page) = search(&sandbox, pattern, 3).await?;
        assert_eq!(num_hits, expected.len() as u64);
        assert_eq!(page.len(), expected.len().min(3));
        assert!(page.is_subset(&expected));
        eprintln!("{pattern:40} {} hits", expected.len());
    }
    sandbox.assert_quit().await;
    Ok(())
}
