# rustie-schema

Postings + dependency graph document model for RustIE-style indexing on **Quickwit**.

- Flatten Odinson JSON → one Quickwit document per sentence
- Encode token arrays as `tok0|tok1|…` (aligned positions)
- Capture `GraphField` edges/roots; store as JSON for **in-memory** traversal
- Emit `incoming_edges` / `outgoing_edges` as per-sentence edge-label *sets* (`dobj|nsubj`, one
  index term per label) for document-level prefilters
- Emit a Quickwit index config (`doc_mapping` + `pipe_tokens`)

## Example

```rust
use rustie_schema::{flatten_odinson_json, postings_index_config_yaml, IndexConfigOptions};

let sentences = flatten_odinson_json(odinson_json)?;
let doc = &sentences[0];
let json = doc.to_quickwit_json();
// json["word"], json["dependencies"], json["incoming_edges"], …

let adj = doc.primary_graph().unwrap().outgoing_adjacency(doc.sentence_length as usize);
let yaml = postings_index_config_yaml(&IndexConfigOptions::default());
```

## Index encoding decisions

- **Token fields** keep RustIE's token-aware layout: one pipe-joined string per field, tokenized by
  `pipe_tokens`, so the Tantivy position of a term is the token index. Sequences can therefore be
  prefiltered with phrase queries. No `fast` columns by default (nothing reads them).
- **Edge labels** are indexed as a label set, not per token: Quickwit tokenizers cannot place several
  tokens at one position (RustIE's `edge_positions` tokenizer can), and comma-joined slots would hide
  labels. Per-token structure lives in the stored `dependencies` JSON.
- **No `doc_id` tag**: Quickwit registers tag values only for ≤1000 distinct values per split.

Regenerate the checked-in config with
`cargo run -p rustie-schema --example emit_index_config > configs/ie_postings.yaml`.
