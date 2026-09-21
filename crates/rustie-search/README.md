# rustie-search

Run RustIE / Odinson patterns against the IE postings index (built by `rustie-indexer`) and
expose them as a Rust API and an HTTP API.

```
pattern ──rustie-compiler──▶ (1) Quickwit prefilter  ──▶ candidate sentences (stored tokens + graph)
                             (2) exact in-memory matcher ──▶ spans / captures per matching sentence
```

The prefilter is a *superset* computed by the index (term/regex clauses over `word`, `lemma`,
`outgoing_edges`, …); the in-memory matcher (`rustie-compiler`'s span VM and graph evaluator) makes
the result exact. Candidates are fetched in pages until `limit` matches are found, the candidates
run out, or `max_candidates` is reached.

## Run

```bash
cargo run --release -p rustie-search --bin rustie-serve      # http://127.0.0.1:8080

curl -G localhost:8080/v1/search \
  --data-urlencode 'q=[tag=/VB.*/] >nsubj [word=patients]' -d limit=5

curl -s localhost:8080/v1/search -H 'content-type: application/json' \
  -d '{"query": "[word=treatment]", "limit": 3}'

curl localhost:8080/v1/index      # published splits / docs
curl localhost:8080/health
```

| Route | |
| --- | --- |
| `POST /v1/search` | `{"query", "limit"?, "cursor"?, "max_candidates"?, "count"?}` |
| `GET /v1/search?q=&limit=&cursor=&max_candidates=&count=` | same |
| `GET /v1/index`, `GET /health` | |

Response (abridged):

```json
{ "kind": "graph", "hits": [{
    "doc_id": "…", "sentence_id": "…_3", "sentence_length": 21, "words": ["…"],
    "matches": [{ "spans": [{"start": 4, "end": 5, "text": "…", "captures": []}, …] }] }],
  "candidates_total": null, "candidates_scanned": 32, "exhausted": false, "truncated": false,
  "next_cursor": "7b22…", "prefilter_relaxed_clauses": 0, "candidate_query": {…}, "took_ms": 41 }
```

`matches[].spans` has one span for a token pattern and one per traversal endpoint (pattern order)
for a graph pattern. Errors are `{"error": "…"}`: `400` bad query/params, `502` index/backend
failure, `504` timeout.

Flags: `--bind` (default `127.0.0.1:8080`), `--index-id`, `--metastore-uri`, `--endpoint/--bucket/
--access-key/--secret-key` (or `MINIO_*` env), `--page-size`, `--max-limit`, `--max-candidates`,
`--timeout-secs`, `--max-concurrent-searches`, `--refresh-secs`.

## Library

```rust
let searcher = Searcher::connect(minio, SearcherOptions::for_bucket("rustie-dev")).await?;
let results = searcher.search(SearchQuery::new("[word=John] >nsubj [pos=VBZ]").limit(10)).await?;
```

## Behavior worth knowing

- **Prefilter = index-side, exact = in-memory.** Adjacent same-field tokens (`[word=the] [word=cat]`)
  become phrase queries on positions; different fields, wildcards and gaps become separate
  conjuncts. Fuzzy (`~`) constraints are prefiltered case-insensitively. Regexes are validated at
  plan time with Quickwit's own regex engine (`tantivy-fst`); `^`/`$` are stripped, and a regex it
  cannot run is dropped from the prefilter (`prefilter_relaxed_clauses`), never retried per request.
- **Cursor paging.** `next_cursor` resumes right after the last candidate examined (Quickwit
  `search_after` on the document address), so page N costs the same as page 1. A cursor is bound to
  its query. `truncated: true` means the call hit `max_candidates`; continue with `next_cursor`.
- **Counting is opt-in** (`count=true` → `candidates_total`): it forces a full prefilter pass.
- **Warm caches.** One long-lived Quickwit `SearcherContext` serves every query (split footers, fast
  fields, partial results); it survives metastore refreshes.
- **No authentication or TLS.** It binds to loopback by default; put a proxy in front to expose it.
- **Freshness.** The file-backed metastore does not poll, so a running server only sees splits that
  existed when it (re)opened the metastore. `--refresh-secs` (default 30) re-opens it periodically.
- **Scan-heavy patterns.** Matching runs here, not inside Quickwit, so a pattern with no lexical
  anchor ships many candidates; see `docs/ARCHITECTURE.md`.

## Tests

```bash
cargo test -p rustie-search
RUSTIE_MINIO_TEST=1 cargo test -p rustie-search --test search_integration -- --ignored
```
