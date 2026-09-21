# quick-rustie architecture

quick-rustie is RustIE's query language and matching engine running **inside Quickwit**, which
provides the object-storage index (splits on S3/MinIO, metastore, merges, caches). A small fork of
Quickwit adds generic extension points; all RustIE code stays in this repository.

```
indexing
  Odinson JSON ─rustie-schema─▶ sentence docs ─rustie-indexer─▶ Quickwit indexing pipeline
                                                                  │  + GPH2 sidecar (rustie-leaf):
                                                                  │    row i = document i, rebuilt
                                                                  ▼    on every merge
                        split = tantivy segment + rustie.gph2 + hotcache   ──▶  s3://…/indexes

search
  pattern ─rustie-search─▶ Quickwit root search ─▶ leaf search, per split (rustie-leaf):
                             QueryAst::Extension      1. candidates from postings (compiler's filter)
                             {kind: "rustie"}         2. positions of every tested term
                                                      3. GPH2 blocks of the candidates (range reads)
                                                      4. exact match per candidate (span VM /
                                                         bound graph evaluator); only matches reach
                                                         the collector
                           ◀─ exact num_hits, top-k, search_after, fetched docs = matches only
         rustie-search renders the page's spans / captures from the stored sentences
```

## Crates

| Crate | Role |
| --- | --- |
| `rustie-query` | Pattern language: grammar, AST, parser |
| `rustie-compiler` | Pattern → candidate filter (superset) + span program / graph plan; `BoundPlan` / `BoundSurface` evaluate over dictionary ids and caller-supplied token sets |
| `rustie-schema` | Odinson → sentence documents; Quickwit doc mapping |
| `rustie-graph-store` | GPH2: block-structured, range-readable, mergeable per-split graph file (ported from RustIE) |
| `rustie-leaf` | The Quickwit extensions: GPH2 split sidecar + `rustie` query (candidates, warmup, scorer) |
| `rustie-indexer` | Index Odinson data into MinIO/S3 through Quickwit's indexing service |
| `rustie-search` | `Searcher` library + `rustie-serve` HTTP API |

The Quickwit fork is <https://github.com/Crispae/quickwit-custom> (branch `rustie-ext`); see its
`docs/rustie-fork.md` for the three hooks and how to rebase them. Every Quickwit crate is
redirected to it, at a pinned commit, by the `[patch]` section of the root `Cargo.toml`.

## What replaced what (RustIE → quick-rustie)

| RustIE | quick-rustie |
| --- | --- |
| Epoch manifests, `_head`, writer lease | Quickwit metastore, per-source checkpoints |
| Compaction worker, GPH2 sidecar merge | Quickwit merge pipeline; the fork's sidecar hook merges GPH2 with the alive documents |
| `storage::gc` | `Indexer::garbage_collect` (Quickwit GC) |
| Hotcache, NVMe ring, chunk singleflight | Split hotcache (holds the GPH2 trailer), Quickwit searcher caches, process-wide GPH2 block LRU (`RUSTIE_GRAPH_CACHE_MB`) |
| Segment / shard blooms | Required-term early abort per split |
| Custom tantivy scorers (concat, graph V2) | `rustie-leaf` scorer: postings positions + `BoundPlan` on GPH2 |
| GPH2 PROBE / SCAN access planner | Same rule: fetch the candidates' blocks, or the whole body above half the blocks |

## Design decisions

- **Filter in the scorer, not after the collector.** Counts, top-k and `search_after` stay exact,
  and non-matching documents are never fetched or shipped.
- **Token positions come from postings; tag/entity/chunk come from GPH2.** Every token field is
  one pipe-joined string tokenized by `pipe_tokens`, so a term's position is its token index.
  Regex and fuzzy tests are expanded over the term dictionary (automaton when the pattern fits its
  dialect, full scan otherwise), re-checked with the exact matcher.
- **Edge labels are indexed as a per-sentence label set** (`outgoing_edges` / `incoming_edges`):
  only membership is useful for candidates; the graph itself is in GPH2.
- **The stored document keeps its tokens and graph JSON** so a page can be rendered without a
  second graph read. Only the returned page is rendered.
- **Sentence length** is a fast field, read for token patterns (wildcards, negation, look-arounds
  need it); graph patterns get it from GPH2.

## Correctness checks

- `rustie-compiler`: bound evaluators vs the string reference, 3,000 random graphs + surface
  patterns (proptest).
- `rustie-graph-store`: round trips, spool = in-memory encoding, merge with deletes and missing
  components.
- Fork: sidecar rows follow document ids through indexing, merge and delete-and-merge.
- `rustie-leaf/tests/leaf_search.rs`: 20 token and graph patterns over three real splits vs the
  reference evaluator, including exact `num_hits` and paging.
- On the PubMed sample (127,052 sentences, 8 merged splits), results equal a Python oracle over
  the raw JSON for every page size.
