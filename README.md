# quick_rustie

Boilerplate Rust library for **Quickwit indexing on S3** (Amazon S3 or S3-compatible stores like MinIO / Garage).

Depends on [quickwit-oss/quickwit](https://github.com/quickwit-oss/quickwit) `v0.9.0`.

## Crates

| Crate | Role |
| --- | --- |
| **`rustie-query`** (`crates/rustie-query`) | RustIE query language — grammar, AST, `QueryParser` |
| **`rustie-schema`** (`crates/rustie-schema`) | Odinson → sentence docs (tokens + graphs) + Quickwit mapping |
| **`rustie-search`** (`crates/rustie-search`) | Run RustIE patterns against the index: `Searcher` library + `rustie-serve` HTTP API |
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

# put/get smoke test via Quickwit storage
export PROTOC=/path/to/protoc   # if not on PATH
cargo run --example minio_ping
```

In code:

```rust
use quick_rustie::{connect_minio, MinioConfig};

let storage = connect_minio(&MinioConfig::default()).await?;
```

## Index Odinson documents into MinIO

```bash
cargo run -p rustie-indexer --bin rustie-index -- --data "data/processed_pubmed22n0008 (2)" --limit 10
```

See [`crates/rustie-indexer/README.md`](crates/rustie-indexer/README.md) for flags, idempotency and
failure behavior. `quick_rustie::minio` re-exports the MinIO helpers now owned by `rustie-indexer`.

## Query the index

```bash
cargo run --release -p rustie-search --bin rustie-serve &
curl -G localhost:8080/v1/search --data-urlencode 'q=[word=John] >nsubj [pos=VBZ]' -d limit=5
```

See [`crates/rustie-search/README.md`](crates/rustie-search/README.md) and, for the design
(what RustIE's storage layer maps to in Quickwit, and the limits), [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

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
