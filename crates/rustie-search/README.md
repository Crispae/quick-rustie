# rustie-search

Run RustIE / Odinson patterns against the IE postings index (built by `rustie-indexer`) and
expose them as a Rust API and an HTTP API.

A query is one Quickwit search: the pattern travels as a `rustie` extension query and every split
matches it exactly (see `rustie-leaf`), so only matching sentences are counted, ranked and
fetched. This crate renders the matched spans and captures of the returned page.

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
| `POST /v1/search` | `{"query", "limit"?, "cursor"?, "count"?}` |
| `GET /v1/search?q=&limit=&cursor=&count=` | same |
| `GET /v1/index`, `GET /health` | |

Response (abridged):

```json
{ "kind": "graph", "hits": [{
    "doc_id": "…", "sentence_id": "…_3", "sentence_length": 21, "words": ["…"],
    "matches": [{ "spans": [{"start": 4, "end": 5, "text": "…", "captures": []}, …] }] }],
  "total_hits": 528, "total_is_exact": false, "exhausted": false, "next_cursor": "7b22…",
  "took_ms": 22, "timing": {"backend_us": 21000, "render_us": 500} }
```

`matches[].spans` has one span for a token pattern and one per traversal endpoint (pattern order)
for a graph pattern. Errors are `{"error": "…"}`: `400` bad query/params, `502` index/backend
failure, `504` timeout.

Flags: `--bind` (default `127.0.0.1:8080`), `--index-id`, `--metastore-uri`, `--endpoint/--bucket/
--access-key/--secret-key` (or `MINIO_*` env), `--max-limit`, `--timeout-secs`,
`--max-concurrent-searches`, `--refresh-secs`.

## Library

```rust
let searcher = Searcher::connect(minio, SearcherOptions::for_bucket("rustie-dev")).await?;
let results = searcher.search(SearchQuery::new("[word=John] >nsubj [pos=VBZ]").limit(10)).await?;
```

## Behavior worth knowing

- **Exact, in the split.** Hits are only sentences the pattern matches; `total_hits` is exact
  with `count=true`, otherwise a lower bound (Quickwit may stop once the page is full).
- **Cursor paging.** `next_cursor` resumes right after the last hit (Quickwit `search_after`), so
  page N costs the same as page 1. A cursor is bound to its query.
- **Warm caches.** One long-lived Quickwit `SearcherContext` serves every query and survives
  metastore refreshes; graph blocks are cached process-wide (`RUSTIE_GRAPH_CACHE_MB`, default 512).
- **No authentication or TLS.** It binds to loopback by default; put a proxy in front to expose it.
- **Freshness.** The file-backed metastore does not poll; `--refresh-secs` (default 30) re-opens it.
- **Old splits.** Splits indexed before the graph component existed match no graph pattern:
  re-index them (`rustie-index --overwrite`).

## Tests

```bash
cargo test -p rustie-search
RUSTIE_MINIO_TEST=1 cargo test -p rustie-search --test search_integration -- --ignored
```
