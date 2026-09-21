# quick-rustie architecture

quick-rustie is RustIE's query language and graph matching on top of **Quickwit's** S3 indexing,
instead of RustIE's own object-storage layer.

```
Odinson JSON ─rustie-schema─▶ sentence docs ─rustie-indexer─▶ Quickwit indexing pipeline
                                                                   │ splits, merges, checkpoints
                                                                   ▼
                          s3://bucket/indexes/ie-postings  +  s3://bucket/metastore
                                                                   ▲
pattern ─rustie-compiler─▶ (1) prefilter  ─────────────────────────┘  Quickwit search (split
         │                 (2) exact matcher ◀── stored tokens + graph   caches kept warm)
         └────────────────────────────▶ rustie-search (library + HTTP API)
```

## What replaced what

| RustIE (custom, ~10k lines in `src/storage/`) | quick-rustie |
| --- | --- |
| Epoch manifests, `_head` pointer, writer lease | Quickwit metastore + per-source checkpoints (atomic publish) |
| Compaction worker | Quickwit merge policy / merge pipeline |
| `storage::gc` | `Indexer::garbage_collect` (Quickwit's GC) |
| Hotcache, NVMe ring, chunk singleflight | Quickwit split footer/hotcache, searcher caches (`SearcherContext`, kept for the process lifetime) |
| Segment / shard blooms, term-presence pruning | Quickwit warm-up aborts a split when a required term is absent; prefilter runs on the term dictionary |
| Sharded namespaces | Splits |
| `S3CachedDirectory` | Quickwit storage layer (`quickwit-storage`), MinIO flavor |

## What was kept from RustIE

- **Token-aware indexing.** Every token field is one pipe-joined string tokenized by `pipe_tokens`,
  so a term's position is its token index. Adjacent-token patterns become phrase queries.
- **The query language and matchers** (`rustie-query`, `rustie-compiler`): span VM for sequences,
  repetitions and look-arounds; hop NFAs with forward/backward pruning for graph traversal.
- **Superset-prefilter + exact evaluation**, the same soundness rule as RustIE's pruning: a
  prefilter may admit false positives but never drop a match.

## Where it deliberately differs, and why

1. **Edge labels are indexed as a per-sentence label set**, not per token position. RustIE's
   `edge_positions` tokenizer puts several labels at one position; Quickwit's tokenizers cannot.
   Encoding slots as comma-joined tokens hid labels from the index, so the encoding was changed at
   the source (`rustie-schema`), not compensated for at query time.
2. **The graph is stored as JSON in the doc store**, not as a GPH2 columnar sidecar. Graph
   evaluation happens after candidates are fetched.
3. **No fast columns, no `doc_id` tag.** Nothing in the query path reads them, and Quickwit only
   registers tags with ≤ 1000 distinct values per split.

## Limits that come from using stock Quickwit

- **Matching cannot run inside Quickwit's leaf search.** It has no plug-in point for custom
  scorers (`open_split_bundle` and `warmup` are crate-private; the storage directory panics on
  synchronous reads). Custom tantivy queries over splits would mean forking Quickwit.
  Consequence: candidates are shipped to the search tier. Patterns with no lexical anchor
  (`[] >nsubj []`) therefore scan roughly the corpus, where RustIE's local SCAN mode would not
  cross a network. Anchored queries are unaffected; the prefilter keeps candidate counts small.
- **Positions are only usable within one field.** A phrase cannot span `word` and `pos`; such
  sequences prefilter with one phrase per field plus the exact matcher.
- **The file-backed metastore has no polling** and assumes one writer.
