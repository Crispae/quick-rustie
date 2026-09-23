//! Integration checks: tokens + graph → Quickwit JSON / index YAML.

use rustie_schema::{
    flatten_odinson_json, postings_index_config_yaml, IndexConfigOptions,
    RUSTIE_EDGE_TOKENIZER_NAME, RUSTIE_TOKEN_TOKENIZER_NAME,
};

const ODINSON: &str = r#"{
  "id": "pmid:1",
  "metadata": [],
  "sentences": [
    {
      "numTokens": 4,
      "fields": [
        {
          "name": "word",
          "$type": "ai.lum.odinson.TokensField",
          "tokens": ["John", "eats", "pizza", "."]
        },
        {
          "name": "lemma",
          "$type": "ai.lum.odinson.TokensField",
          "tokens": ["john", "eat", "pizza", "."]
        },
        {
          "name": "pos",
          "$type": "ai.lum.odinson.TokensField",
          "tokens": ["NNP", "VBZ", "NN", "."]
        },
        {
          "name": "dependencies",
          "$type": "ai.lum.odinson.GraphField",
          "edges": [[1, 0, "nsubj"], [1, 2, "dobj"]],
          "roots": [1]
        }
      ]
    }
  ]
}"#;

#[test]
fn odinson_to_quickwit_with_graph() {
    let sentences = flatten_odinson_json(ODINSON).expect("flatten");
    assert_eq!(sentences.len(), 1);
    assert_eq!(sentences[0].sentence_length, 4);

    let g = sentences[0].primary_graph().expect("graph");
    assert_eq!(g.edges.len(), 2);
    assert_eq!(g.roots, vec![1]);
    // `edge_label_slots` dedupes/sorts each slot (see `SentenceGraph::edge_label_slots`).
    assert_eq!(
        sentences[0].outgoing_edges[1],
        vec!["dobj".to_string(), "nsubj".to_string()]
    );

    let line = sentences[0].to_ndjson_line();
    assert!(line.contains("\"word\":\"John|eats|pizza|.\""));
    assert!(line.contains("\"pos\":\"NNP|VBZ|NN|.\""));
    assert!(line.contains("dependencies"));
    assert!(line.contains("incoming_edges"));
    assert!(line.contains("outgoing_edges"));
    assert!(line.contains("nsubj"));

    // Edge postings are positional: "eats" (token 1) is the head of both `nsubj` and `dobj`,
    // and both labels land at its own slot (comma-joined); "eats" is also the root, so it gets
    // "root" added to its own incoming slot.
    let json = sentences[0].to_quickwit_json();
    assert_eq!(json["outgoing_edges"], "|dobj,nsubj||");
    assert_eq!(json["incoming_edges"], "nsubj|root|dobj|");
}

#[test]
fn quickwit_yaml_uses_rustie_tokenizers_not_pipe_tokens() {
    let yaml = postings_index_config_yaml(&IndexConfigOptions::default());
    assert!(yaml.contains(RUSTIE_TOKEN_TOKENIZER_NAME));
    assert!(yaml.contains(RUSTIE_EDGE_TOKENIZER_NAME));
    assert!(!yaml.contains("pipe_tokens"));
    assert!(yaml.contains("incoming_edges"));
    assert!(yaml.contains("dependencies"));
}
