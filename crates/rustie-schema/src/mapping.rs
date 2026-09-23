//! Emit Quickwit index / doc_mapping YAML for IE postings (+ graph storage).

use crate::fields::{
    DEFAULT_INDEXED_TOKEN_FIELDS, FIELD_INCOMING_EDGES, FIELD_OUTGOING_EDGES,
    RUSTIE_EDGE_TOKENIZER_NAME, RUSTIE_TOKEN_TOKENIZER_NAME,
};
use crate::graph::{DEFAULT_BASIC_GRAPH_FIELD, DEFAULT_GRAPH_FIELD};

/// Options for the `doc_mapping` fragment only.
#[derive(Debug, Clone)]
pub struct PostingsMappingOptions {
    /// Token fields to include (defaults to every Odinson token field: word, lemma, pos, tag,
    /// entity, chunk, norm, raw).
    pub token_fields: Vec<String>,
    /// Store token field values in the doc store.
    pub store_tokens: bool,
    /// Also index token fields as fast (columnar) fields. Off by default: nothing in the
    /// query path reads them, and they multiply split size and S3 traffic.
    pub fast_tokens: bool,
    /// Include `incoming_edges` / `outgoing_edges` postings fields.
    pub include_edge_postings: bool,
    /// Store full dependency graph JSON (`dependencies`, optionally basic).
    pub include_graph_json: bool,
    /// Also map `dependencies_basic` JSON if present at index time.
    pub include_basic_graph_json: bool,
}

impl Default for PostingsMappingOptions {
    fn default() -> Self {
        Self {
            token_fields: DEFAULT_INDEXED_TOKEN_FIELDS
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            store_tokens: true,
            fast_tokens: false,
            include_edge_postings: true,
            include_graph_json: true,
            include_basic_graph_json: true,
        }
    }
}

/// Documents per split above which Quickwit stops merging it.
///
/// Quickwit's own default is 10M, which merges a corpus of a few million sentences into a single
/// split. A split is searched by one thread, so that caps a query at one core; splits of about
/// this size keep every core of a search node busy while still amortising per-split costs
/// (dictionary, hotcache, graph trailer). A merged split ends up between one and two times this.
pub const DEFAULT_SPLIT_NUM_DOCS_TARGET: usize = 500_000;

/// Full Quickwit index config options.
#[derive(Debug, Clone)]
pub struct IndexConfigOptions {
    pub index_id: String,
    pub index_uri: String,
    pub mapping: PostingsMappingOptions,
    /// Tag `doc_id` for split pruning. Off by default: Quickwit only records tag values for
    /// fields with at most 1000 distinct values per split, and a split of sentences has far
    /// more distinct documents, so the tag is never registered and only costs work.
    pub tag_doc_id: bool,
    /// Splits with at least this many documents are mature and never merged again.
    pub split_num_docs_target: usize,
}

impl Default for IndexConfigOptions {
    fn default() -> Self {
        Self {
            index_id: "ie-postings".into(),
            index_uri: "s3://rustie-dev/indexes/ie-postings".into(),
            mapping: PostingsMappingOptions::default(),
            tag_doc_id: false,
            split_num_docs_target: DEFAULT_SPLIT_NUM_DOCS_TARGET,
        }
    }
}

fn push_token_field_mappings(lines: &mut Vec<String>, opts: &PostingsMappingOptions) {
    for field in &opts.token_fields {
        lines.push(format!("    - name: {field}"));
        lines.push("      type: text".to_string());
        lines.push(format!("      tokenizer: {RUSTIE_TOKEN_TOKENIZER_NAME}"));
        lines.push("      record: position".to_string());
        lines.push(format!("      stored: {}", opts.store_tokens));
        if opts.fast_tokens {
            lines.push("      fast: true".to_string());
        }
    }
}

fn push_edge_posting_fields(lines: &mut Vec<String>) {
    for field in [FIELD_OUTGOING_EDGES, FIELD_INCOMING_EDGES] {
        lines.push(format!("    - name: {field}"));
        lines.push("      type: text".to_string());
        lines.push(format!("      tokenizer: {RUSTIE_EDGE_TOKENIZER_NAME}"));
        // Positional (see `SentenceGraph::edge_label_slots` / `encoding::slot_terms`): every
        // label at a token's slot lands at that token's Tantivy position, so a same-token
        // candidate filter can require e.g. `word:cat AND incoming_edges:nsubj` on one token
        // instead of just "somewhere in this sentence". Not stored: the graph JSON already
        // carries every edge for rendering.
        lines.push("      record: position".to_string());
        lines.push("      stored: false".to_string());
    }
}

fn push_graph_json_fields(lines: &mut Vec<String>, opts: &PostingsMappingOptions) {
    if opts.include_graph_json {
        lines.push(format!("    - name: {DEFAULT_GRAPH_FIELD}"));
        lines.push("      type: json".to_string());
        lines.push("      stored: true".to_string());
        lines.push("      indexed: false".to_string());
    }
    if opts.include_basic_graph_json {
        lines.push(format!("    - name: {DEFAULT_BASIC_GRAPH_FIELD}"));
        lines.push("      type: json".to_string());
        lines.push("      stored: true".to_string());
        lines.push("      indexed: false".to_string());
    }
}

fn push_field_mappings_body(lines: &mut Vec<String>, opts: &PostingsMappingOptions) {
    lines.push("  field_mappings:".to_string());
    lines.push("    - name: doc_id".to_string());
    lines.push("      type: text".to_string());
    lines.push("      tokenizer: raw".to_string());
    lines.push("      stored: true".to_string());
    lines.push("    - name: sentence_id".to_string());
    lines.push("      type: text".to_string());
    lines.push("      tokenizer: raw".to_string());
    lines.push("      stored: true".to_string());
    lines.push("    - name: sentence_length".to_string());
    lines.push("      type: u64".to_string());
    lines.push("      stored: true".to_string());
    lines.push("      fast: true".to_string());
    push_token_field_mappings(lines, opts);
    if opts.include_edge_postings {
        push_edge_posting_fields(lines);
    }
    push_graph_json_fields(lines, opts);
}

/// YAML for the `doc_mapping:` block.
///
/// No `tokenizers:` section: `rustie_tokens` / `rustie_edges` are registered process-wide by
/// `rustie_leaf::register()` through the Quickwit fork's tokenizer-registration hook, not
/// declared per-mapping (Quickwit's config tokenizers are a closed list that cannot place
/// several terms at one token position). Naming either as a custom tokenizer here would be
/// rejected as shadowing a built-in name.
pub fn postings_doc_mapping_yaml(opts: &PostingsMappingOptions) -> String {
    let mut lines = Vec::new();
    lines.push("doc_mapping:".to_string());
    lines.push("  mode: strict".to_string());
    push_field_mappings_body(&mut lines, opts);
    lines.join("\n") + "\n"
}

/// Full Quickwit index config YAML for IE indexing.
pub fn postings_index_config_yaml(opts: &IndexConfigOptions) -> String {
    let mut lines = Vec::new();
    lines.push("version: 0.7".to_string());
    lines.push(String::new());
    lines.push(format!("index_id: \"{}\"", opts.index_id));
    lines.push(String::new());
    lines.push(format!("index_uri: \"{}\"", opts.index_uri));
    lines.push(String::new());
    lines.push("doc_mapping:".to_string());
    lines.push("  mode: strict".to_string());
    if opts.tag_doc_id {
        lines.push("  tag_fields: [doc_id]".to_string());
    }
    push_field_mappings_body(&mut lines, &opts.mapping);
    lines.push(String::new());
    lines.push("indexing_settings:".to_string());
    lines.push(format!(
        "  split_num_docs_target: {}",
        opts.split_num_docs_target
    ));
    lines.push(String::new());
    lines.push("search_settings:".to_string());
    lines.push("  default_search_fields: [word]".to_string());
    lines.push(String::new());
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaml_includes_graph_and_edge_fields() {
        let yaml = postings_index_config_yaml(&IndexConfigOptions::default());
        assert!(yaml.contains(RUSTIE_TOKEN_TOKENIZER_NAME));
        assert!(yaml.contains(RUSTIE_EDGE_TOKENIZER_NAME));
        assert!(!yaml.contains("pipe_tokens"));
        assert!(!yaml.contains("tokenizers:"));
        assert!(yaml.contains("incoming_edges"));
        assert!(yaml.contains("outgoing_edges"));
        assert!(yaml.contains("dependencies"));
        assert!(yaml.contains("dependencies_basic"));
        assert!(yaml.contains("type: json"));
        let _: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("valid yaml");
    }

    #[test]
    fn edge_fields_are_positional() {
        let yaml = postings_index_config_yaml(&IndexConfigOptions::default());
        let config: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        let fields = config["doc_mapping"]["field_mappings"].as_sequence().unwrap();
        for name in ["incoming_edges", "outgoing_edges"] {
            let field = fields
                .iter()
                .find(|f| f["name"].as_str() == Some(name))
                .unwrap_or_else(|| panic!("field '{name}' missing from mapping"));
            assert_eq!(field["record"].as_str(), Some("position"));
            assert_eq!(field["tokenizer"].as_str(), Some(RUSTIE_EDGE_TOKENIZER_NAME));
        }
    }

    #[test]
    fn yaml_caps_split_size() {
        let yaml = postings_index_config_yaml(&IndexConfigOptions {
            split_num_docs_target: 123_456,
            ..Default::default()
        });
        let config: serde_yaml::Value = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(
            config["indexing_settings"]["split_num_docs_target"].as_u64(),
            Some(123_456)
        );
        let default: serde_yaml::Value =
            serde_yaml::from_str(&postings_index_config_yaml(&IndexConfigOptions::default()))
                .unwrap();
        assert_eq!(
            default["indexing_settings"]["split_num_docs_target"].as_u64(),
            Some(DEFAULT_SPLIT_NUM_DOCS_TARGET as u64)
        );
    }
}
