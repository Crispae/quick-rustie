# rustie-node

Quickwit node process with [`rustie_leaf::register()`](../rustie-leaf) installed before
`serve_quickwit`. Owns **cluster membership, gRPC leaf/root search, and optional indexing** —
not the Odinson HTTP API (`rustie-serve` is the gateway / embedded laptop searcher).

## Design

```
  rustie-serve --searcher-endpoint host:grpc
       │  gRPC root_search
       ▼
  rustie-node (this binary)
       ├─ SearchJobPlacer assigns splits (rendezvous hash)
       ├─ local leaf_search  → rustie-leaf (exact match in split)
       └─ gRPC leaf_search   → peer rustie-nodes
```

- Matching is Quickwit leaf search + the `rustie` extension; this binary only hosts that stack.
- **Every node** should enable `searcher` + `metastore` and open the **same** Postgres (default)
  or file/S3 metastore URI directly (no single-owner metastore proxy).
- Enable `searcher.split_cache` so live `ReportSplitsRequest`s from
  `rustie-index --cluster-config` fill the on-disk split cache.
- **Or** pass `--cachey-url http://127.0.0.1:9020` for a shared Cachey page cache (searcher /
  metastore / metastore-read-replica only — the process refuses to start if indexer/janitor/etc.
  are enabled). Do not combine with `searcher.split_cache` unless you accept the warning.
- Indexing remains batch `rustie-index` (optionally gossip-joined); do not enable `indexer` on
  this node unless you intentionally want Quickwit’s control-plane scheduling.

## Roles / config

Services come from the node YAML `enabled_services`. Pass `--service` only to **override**;
omitting the flag lets the config (and `QW_ENABLED_SERVICES`) win.

| Topology | How |
| --- | --- |
| Single node | `peer_seeds: []`; one process |
| Multi node | Shared `cluster_id`; unique `node_id`, ports, `data_dir`; `peer_seeds` = peers’ **gossip** addresses |
| Live split reports | `searcher.split_cache` on nodes + `rustie-index --cluster-config configs/rustie-indexer.yaml` |

## Run (single node)

```bash
mkdir -p /tmp/rustie-node-data
cargo run --release -p rustie-node -- --config configs/rustie-node.yaml
```

Gateway:

```bash
cargo run --release -p rustie-search --bin rustie-serve -- \
  --index-id pubmed-slots \
  --metastore-uri 'postgres://rustie:rustie@127.0.0.1:5433/rustie' \
  --searcher-endpoint 127.0.0.1:7281
```

## Run (two nodes on localhost)

Example configs already filled for MinIO `rustie-dev` + index `pubmed-slots`:

| | Node 1 | Node 2 |
| --- | --- | --- |
| Config | [`configs/rustie-node-1.yaml`](../../configs/rustie-node-1.yaml) | [`configs/rustie-node-2.yaml`](../../configs/rustie-node-2.yaml) |
| REST / gRPC / gossip | 7280 / **7281** / 7282 | 7380 / **7381** / 7382 |
| `peer_seeds` | `127.0.0.1:7382` | `127.0.0.1:7282` |

```bash
mkdir -p /tmp/rustie-node-1-data /tmp/rustie-node-2-data

cargo run --release -p rustie-node -- --config configs/rustie-node-1.yaml
cargo run --release -p rustie-node -- --config configs/rustie-node-2.yaml

cargo run --release -p rustie-search --bin rustie-serve -- \
  --index-id pubmed-slots \
  --metastore-uri 'postgres://rustie:rustie@127.0.0.1:5433/rustie' \
  --searcher-endpoint 127.0.0.1:7281 \
  --bind 127.0.0.1:8080
```

Point `--searcher-endpoint` at a node’s **gRPC** port (not REST). Leaf work is spread across
both nodes by Quickwit; the gateway only needs one root.

## Config checklist

- `metastore_uri` / `default_index_root_uri` / `storage.s3` must match `rustie-index`.
- `data_dir` must exist (the binary creates it when it can parse `data_dir` from YAML).
- Change `cluster_id` only if you intend a separate cluster; peers must share it.
