# rustie-node

Quickwit node process with [`rustie_leaf::register()`](../rustie-leaf) installed before
`serve_quickwit`. This is the process that owns **cluster membership, gRPC leaf/root search,
and optional indexing** — not `rustie-serve` (which is the Odinson HTTP gateway / embedded
laptop searcher).

## Roles

Services come from the **node config** `enabled_services` (example: **`searcher` + `metastore`**).
Pass `--service` only to override that list; omitting the flag lets the YAML win.

Every node should open the shared file/S3 metastore URI **directly**. Do not adopt upstream’s
single-owner metastore-role topology for this stack.

| Topology | How |
| --- | --- |
| Single node | `peer_seeds: []` in the node config; one `rustie-node` |
| Multi node | Shared `cluster_id`, peer gossip seeds; each node unique `node_id` / advertise addrs |
| Indexer (later) | Add `indexer` under `enabled_services` in the YAML (or `--service indexer --service searcher --service metastore`) |

## Run

```bash
# create data dir path from the config if needed (binary also creates it)
mkdir -p /tmp/rustie-node-data

cargo run --release -p rustie-node -- --config configs/rustie-node.yaml

# override services
cargo run --release -p rustie-node -- \
  --config configs/rustie-node.yaml \
  --service searcher --service metastore
```

Point `rustie-serve` at the node’s **gRPC** port (not REST):

```bash
cargo run --release -p rustie-search --bin rustie-serve -- \
  --searcher-endpoint 127.0.0.1:7281
```

(Default gRPC listen in `configs/rustie-node.yaml` is `7281`; adjust to match.)

## Config

See [`configs/rustie-node.yaml`](../../configs/rustie-node.yaml). Set MinIO/`storage.s3` and
`metastore_uri` / `default_index_root_uri` to the same bucket layout `rustie-index` uses.
