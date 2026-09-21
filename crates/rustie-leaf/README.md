# rustie-leaf

RustIE pattern matching inside Quickwit leaf search, through the extension hooks of the Quickwit
fork (`../quickwit-fork`, branch `rustie-ext`). Call `rustie_leaf::register()` once at start-up in
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

## Tests

```bash
cargo test -p rustie-leaf    # includes a differential test over real Quickwit splits
```
