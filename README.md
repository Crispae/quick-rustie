# quick_rustie

Boilerplate Rust library for **Quickwit indexing on S3** (Amazon S3 or S3-compatible stores like MinIO / Garage).

Depends on [quickwit-oss/quickwit](https://github.com/quickwit-oss/quickwit) `v0.9.0`.

## Crates

| Crate | Role |
| --- | --- |
| **`rustie-query`** (`crates/rustie-query`) | RustIE query language — grammar, AST, `QueryParser` |
| **`rustie-schema`** (`crates/rustie-schema`) | Odinson → sentence docs (tokens + graphs) + Quickwit mapping |
| **`rustie-search`** (`crates/rustie-search`) | Run RustIE patterns against the index: `Searcher` library + `rustie-serve` HTTP API (embedded or gateway) |
| **`rustie-node`** (`crates/rustie-node`) | Quickwit node with `rustie-leaf` registered (`serve_quickwit`; searcher + metastore) |
| **`rustie-cachey`** (`crates/rustie-cachey`) | Optional Cachey read-through wrapper for S3 `.split` `get_slice` (searchers only) |
| **`rustie-leaf`** (`crates/rustie-leaf`) | RustIE inside Quickwit leaf search: GPH2 split component + `rustie` query extension |
| **`rustie-graph-store`** (`crates/rustie-graph-store`) | GPH2 graph file: blocks, range reads, streaming writer, merge |
| `quickwit-indexing` | Indexing pipeline |
| `quickwit-storage` | Object storage (`s3://` always available) |
| `quickwit-aws` | AWS credentials / S3 clients |
| `quickwit-config` | Storage & index config (`s3://bucket/...`) |
| `quickwit-metastore` | Index metadata (can use `s3://` file metastore) |
| `quickwit-doc-mapper` | Document schema for indexing |
| `quickwit-common` | Shared utilities |

```rust
use quick_rustie::{flatten_odinson_json, QueryParser};

let pattern = QueryParser::new()
    .parse_query("[word=John] >nsubj [pos=VBZ]")?;

let sentences = flatten_odinson_json(odinson_json)?;
// each sentence → Quickwit JSON with word: "The|cat|sat"
```

Postings index config: `configs/ie_postings.yaml` (also via `postings_index_config_yaml`).

## Deployment config (multi-node)

One YAML file describes the whole deployment: S3 endpoint/region/bucket/keys, index id,
Postgres metastore, Cachey or split cache, every node, and `rustie-serve`. Start from
[`configs/rustie-deploy.example.yaml`](configs/rustie-deploy.example.yaml) (copy it to
`configs/rustie-deploy.yaml`, which is gitignored) and reference secrets as `${ENV_VAR}`.

```bash
export S3_ACCESS_KEY=... S3_SECRET_KEY=... PG_PASSWORD=...
rustie-node  --deploy-config configs/rustie-deploy.yaml --check                  # validate
rustie-node  --deploy-config configs/rustie-deploy.yaml --node-id rustie-node-1  # run a node
rustie-serve --deploy-config configs/rustie-deploy.yaml                          # HTTP API
```

Every start validates the file: **errors stop the process and each one says what to provide**
(missing/placeholder values, unset env vars, loopback hosts across machines, port clashes,
Cachey with an indexer role, ...); warnings (literal secrets, missing `path_style` off-AWS,
Cachey together with the split cache) are printed and startup continues.
`rustie-node --check` additionally talks to the real services (bucket, metastore, index and
split count, Cachey `/stats` + a footer-byte comparison against direct S3) without starting a node.
`rustie-serve --check` runs the static checks only.

## Local MinIO

This machine already has **`rustie-minio`** healthy on:

| | |
| --- | --- |
| API | `http://127.0.0.1:9010` |
| Console | `http://127.0.0.1:9011` |
| Bucket | `rustie-dev` |
| User / pass | `minioadmin` / `minioadmin` |

(Host `:9000` is ClickHouse — do not use it for MinIO.)

```bash
# start MinIO if needed
docker compose -f docker-compose.minio.yml up -d

# optional: shared Cachey in front of MinIO (searchers: --cachey-url http://127.0.0.1:9020)
docker compose -f docker-compose.minio.yml up -d cachey

# put/get smoke test via Quickwit storage
export PROTOC=/path/to/protoc   # if not on PATH
cargo run --example minio_ping
```

**Cachey vs on-disk split cache:** use `--split-cache-dir` for a single searcher with enough
local disk; use `--cachey-url` when several searchers should share page-grained caching (or the
corpus is larger than one machine’s disk). Cachey’s `AWS_ENDPOINT_URL` must match rustie’s MinIO/S3
endpoint (compose uses `http://rustie-minio:9000` inside the network). See
[`crates/rustie-search/README.md`](crates/rustie-search/README.md) and
[`crates/rustie-node/README.md`](crates/rustie-node/README.md).

In code:

```rust
use quick_rustie::{connect_minio, MinioConfig};

let storage = connect_minio(&MinioConfig::default()).await?;
```

## Index Odinson documents into MinIO

Indexing is still a **batch** job (`rustie-index`): one process writes splits to object storage.
With `--cluster-config`, it also joins the search cluster as a gossip-only member so live
split-cache reports reach `rustie-node` searchers.

```bash
# Postgres metastore (default); Quickwit runs migrations on connect
docker run -d --name rustie-postgres -p 5433:5432 \
  -e POSTGRES_USER=rustie -e POSTGRES_PASSWORD=rustie -e POSTGRES_DB=rustie postgres:16

cargo run --release -p rustie-indexer --bin rustie-index -- \
  --data "data/processed_pubmed22n0008 (2)" \
  --index-id pubmed-slots \
  --cluster-config configs/rustie-indexer.yaml
```

Defaults: metastore `postgres://rustie:rustie@127.0.0.1:5433/rustie` (override with
`RUSTIE_METASTORE_URI` / `--metastore-uri`), index root `s3://<bucket>/indexes/<index-id>`.
Old file-backed metastores stay readable via `--metastore-uri s3://…` until deleted; switching
to Postgres means **re-indexing**, not migrating.
See [`crates/rustie-indexer/README.md`](crates/rustie-indexer/README.md) for flags, idempotency and
failure behavior. `quick_rustie::minio` re-exports the MinIO helpers now owned by `rustie-indexer`.

## Query the index (embedded, one process)

```bash
cargo run --release -p rustie-search --bin rustie-serve -- \
  --index-id pubmed-slots &
curl -G localhost:8080/v1/search --data-urlencode 'q=[word=John] >nsubj [pos=VBZ]' -d limit=5
```

## Multi-node search

### Design

RustIE matching runs **inside Quickwit leaf search** (`rustie-leaf`). Distributed fan-out is
Quickwit’s own root/leaf path — we do not reimplement clustering in `rustie-serve`.

```
  pattern
    │
    ▼
  rustie-serve (HTTP gateway)
    │  compile → Extension QueryAst { kind: "rustie" }
    │  gRPC SearchService::root_search  (one --searcher-endpoint)
    ▼
  rustie-node A (root + leaf) ──chitchat── rustie-node B (leaf)
    │                                      │
    ├─ local leaf_search (rustie-leaf)     └─ gRPC leaf_search
    └─ merge hits / counts
         │
         ▼
  MinIO: s3://…/indexes/<index-id>   Postgres: metastore (default)
```

| Process | Role |
| --- | --- |
| **`rustie-index`** | Batch Odinson → splits on MinIO. With `--cluster-config`, gossip-only cluster member that reports new splits into searchers' split caches. |
| **`rustie-node`** | `register()` + `serve_quickwit`: gossip, gRPC search, exact leaf match. Every node runs **searcher + metastore** and opens the **same** Postgres (or s3://) metastore URI. Enable `searcher.split_cache` for live reports. |
| **`rustie-serve`** | Odinson HTTP API. With `--searcher-endpoint`, only dials remote `root_search` and renders spans; without it, embeds the search stack (laptop default). |

- **N = 1:** one `rustie-node`, `peer_seeds: []`.
- **N ≥ 2:** shared `cluster_id`, each node unique `node_id` / ports / `data_dir`; `peer_seeds` = peers’ **gossip** `host:port`.
- Gateway dials **one** node’s **gRPC** port (root entry SPOF); that node fans leaf jobs across the pool via rendezvous hashing.
- Details: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md), [`crates/rustie-node/README.md`](crates/rustie-node/README.md), [`crates/rustie-search/README.md`](crates/rustie-search/README.md).

### Run two nodes on one machine (example: `pubmed-slots`)

Requires MinIO + Postgres up and an index already in the metastore (e.g. `pubmed-slots` under
`postgres://…` / `s3://rustie-dev/indexes/pubmed-slots`).

```bash
export PROTOC=/path/to/protoc   # if needed
cargo build --release -p rustie-node -p rustie-search

mkdir -p /tmp/rustie-node-1-data /tmp/rustie-node-2-data

# terminal 1 — searcher + metastore (gRPC :7281, gossip :7282)
cargo run --release -p rustie-node -- --config configs/rustie-node-1.yaml

# terminal 2 — peer (gRPC :7381, gossip :7382; peer_seeds → node 1 gossip)
cargo run --release -p rustie-node -- --config configs/rustie-node-2.yaml

# terminal 3 — Odinson HTTP gateway → node 1 root
cargo run --release -p rustie-search --bin rustie-serve -- \
  --index-id pubmed-slots \
  --metastore-uri 'postgres://rustie:rustie@127.0.0.1:5433/rustie' \
  --searcher-endpoint 127.0.0.1:7281 \
  --bind 127.0.0.1:8080
```

```bash
curl -s localhost:8080/v1/index
curl -G localhost:8080/v1/search \
  --data-urlencode 'q=[entity=/B-gen.*/] >nsubj [tag=VBZ]' -d limit=5 -d count=true
```

Configs: [`configs/rustie-node-1.yaml`](configs/rustie-node-1.yaml),
[`configs/rustie-node-2.yaml`](configs/rustie-node-2.yaml). Align `metastore_uri` /
`storage.s3` with wherever you indexed; `--index-id` must match the metastore entry
(`pubmed-slots`, not `ie-postings`, unless that is what you built).

See [`crates/rustie-search/README.md`](crates/rustie-search/README.md) for the HTTP API and, for
storage/leaf design limits, [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Quickwit fork

quick-rustie builds against a fork of Quickwit with three generic extension hooks (split sidecar,
sidecar hotcache listing, query extension): <https://github.com/Crispae/quickwit-custom>, branch
`rustie-ext`, based on upstream `cc420c3`.

The root `Cargo.toml` `[patch]` section redirects every Quickwit crate to that repository at a
pinned `rev`, and `rustie-leaf` takes `quickwit-extensions` from it. Nothing needs to be checked
out next to this repository. To use a new fork commit, change the `rev` in all of those entries
(and in `crates/rustie-leaf/Cargo.toml`) to the same commit. To develop the fork locally, clone it
and temporarily point the entries at `path = "<clone>/quickwit/<crate>"`. The hooks and how to
rebase them are in the fork's `docs/rustie-fork.md`.

## Wiki5k vs Odinson benchmark

Index + search timing on `data/wiki5k` against Odinson (`v0.5.0-api.zip`, local Lucene)
vs this stack (Quickwit → MinIO). Event rules skipped.

```bash
./benchmarks/wiki5k/run.sh                 # both (downloads JDK 8 into .cache if needed)
./benchmarks/wiki5k/run.sh --rustie-only
REPS=20 ./benchmarks/wiki5k/run.sh
```

See [`benchmarks/wiki5k/README.md`](benchmarks/wiki5k/README.md).

## Build

Requirements:

1. Tokio unstable cfg — already set in `.cargo/config.toml`
2. `protoc` on `PATH` (or set `PROTOC`) — needed to build `quickwit-proto`

```bash
# example if protoc is not on PATH:
export PROTOC=/path/to/protoc

cargo check
cargo test
```
