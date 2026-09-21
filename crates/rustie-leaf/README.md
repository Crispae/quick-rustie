# rustie-leaf

RustIE pattern matching inside Quickwit leaf search, through the extension hooks of the Quickwit
fork (<https://github.com/Crispae/quickwit-custom>, branch `rustie-ext`). Call `rustie_leaf::register()` once at start-up in
every process that indexes or searches (`rustie-indexer` and `rustie-search` do).

## Graph component (`GraphSidecar`)

Every split carries `rustie.gph2`: each sentence's dependency graph (backbone + overlay edges,
dictionary-encoded labels) and its `tag` / `entity` / `chunk` ids, in blocks of 128 documents.
Row *i* is document *i*: rows are appended in the same call as the document, and merges rebuild
the file from the alive documents of each input, in merge order. The trailer (block offsets +
dictionaries) is kept in the split hotcache.

## `rustie` query

```json
{"type": "extension", "kind": "rustie", "payload": {"pattern": "[tag=/VB.*/] >nsubj [word=patients]"}}
```

Per split, the warmup:
1. evaluates the compiler's candidate filter on postings (a superset of the matches);
2. binds a graph plan to the split's GPH2 dictionaries;
3. expands each token test on `word`, `lemma`, … to its terms and warms their positions;
4. fetches the sentence lengths (token patterns) or the candidates' graph blocks (graph
   patterns: the blocks touched, or the whole body when that is over half of it).

The scorer then yields only the candidates the pattern matches exactly, so `num_hits`, top-k and
`search_after` are exact and non-matching sentences are never fetched.

**Postings-decided patterns skip steps 2–4.** When the pattern is one token test on one field (a
literal or a regex) or alternatives of them (`[tag=/VB.*/]`, `[word=cat | tag=NN]`), the
candidates already are the matches (`SurfacePlan::exact`, decided in the compiler). No positions,
sentence lengths or per-sentence checks are read, so `[tag=VBD]` over 5M sentences counts in tens
of milliseconds rather than a second. Sequences, phrases, quantifiers, conjunctions on one
token, fuzzy tests and negation still go through the exact matcher; the differential test in
`tests/leaf_search.rs` covers both kinds.

Set `RUST_LOG=rustie_leaf=debug` to log, per segment, the candidate count and the time spent on
candidates, leaf expansion and graph blocks.

## Tests

```bash
cargo test -p rustie-leaf    # includes a differential test over real Quickwit splits
```
