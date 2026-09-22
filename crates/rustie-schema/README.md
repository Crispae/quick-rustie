# rustie-schema

Postings + dependency graph document model for RustIE-style indexing on **Quickwit**.

- Flatten Odinson JSON → one Quickwit document per sentence
- Encode token arrays as `tok0|tok1|…` (aligned positions)
- Capture `GraphField` edges/roots; store as JSON for **in-memory** traversal
- Emit `incoming_edges` / `outgoing_edges` per token (`|dobj,nsubj||`: one slot per token,
  comma-joined labels, `root` on the root's incoming slot) for same-token prefilters
- Emit a Quickwit index config (`doc_mapping` using the `rustie_tokens` / `rustie_edges`
  tokenizers that `rustie_leaf::register()` installs)

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
  `rustie_tokens`, so the Tantivy position of a term is the token index. `|`, `,` and `\` inside a
  token are escaped by `encode_tokens` and unescaped by the tokenizer (`slot_terms`). Sequences can
  therefore be prefiltered with phrase queries. No `fast` columns by default (nothing reads them).
- **Edge labels** are indexed per token by `rustie_edges`: each slot's comma-joined labels all land
  at that slot's position, deduplicated. Quickwit's config tokenizers cannot place several terms at
  one position, which is why the tokenizer is registered through the fork's hook. An empty slot
  emits nothing but still advances the position, keeping every field aligned; `SentenceDoc::validate`
  rejects a vector whose slot count differs from `sentence_length`.
- **No `doc_id` tag**: Quickwit registers tag values only for ≤1000 distinct values per split.

Regenerate the checked-in config with
`cargo run -p rustie-schema --example emit_index_config > configs/ie_postings.yaml`.
