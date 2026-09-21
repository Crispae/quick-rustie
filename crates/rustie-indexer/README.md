# rustie-indexer

Creates the IE postings index on MinIO / S3 and ingests flattened Odinson sentences into it by
running Quickwit's real `IndexingService` pipeline (splits, merges, checkpoints), pinned to the
same Quickwit tag as the root crate.

```
data/*.json (Odinson) ──rustie-schema flatten──▶ sentence docs
        ──prune/validate vs doc mapping──▶ Vec source ──▶ IndexingService ──▶ s3://<bucket>/indexes/ie-postings
                                                           file-backed metastore ──▶ s3://<bucket>/metastore
```

## CLI

```bash
docker compose -f docker-compose.minio.yml up -d      # if rustie-minio is not running
export PROTOC=/path/to/protoc                         # needed to build quickwit-proto

# smoke run
cargo run -p rustie-indexer --bin rustie-index -- --data "data/processed_pubmed22n0008 (2)" --limit 10

# full run (re-runnable, see "Idempotency")
cargo run --release -p rustie-indexer --bin rustie-index -- --data "data/processed_pubmed22n0008 (2)"
```

| Flag | Default | |
| --- | --- | --- |
| `--data` | required | directory of Odinson `*.json` (recursive) |
| `--limit` | – | max files (smoke runs) |
| `--batch-size` | 200 | files per pipeline run (≈ one split before merges) |
| `--endpoint` / `--bucket` / `--access-key` / `--secret-key` | `MINIO_*` env, else local `rustie-minio` | prefer env for the secret |
| `--index-id` | `ie-postings` | |
| `--metastore-uri` / `--index-root-uri` | `s3://<bucket>/metastore`, `s3://<bucket>/indexes` | |
| `--data-dir` | temp dir | scratch for split building |
| `--skip-invalid` | off | drop documents the mapping rejects instead of aborting |
| `--overwrite` | off | **deletes** the index and re-creates it first (needed after a mapping change) |
| `--gc` / `--gc-only` | off | delete split files replaced by merges (older than 2 min); `--gc-only` skips indexing |

Exit codes: `0` ok · `1` fatal · `2` finished but files/documents were rejected · `130` interrupted
(first Ctrl-C finishes the current batch; re-run to resume).

## Library

```rust
use rustie_indexer::{Indexer, IndexerOptions, MinioConfig};

let minio = MinioConfig::from_env();
let indexer = Indexer::connect(minio.clone(), IndexerOptions::for_bucket(&minio.bucket)).await?;
indexer.ensure_index().await?;                        // create, or verify mapping matches
let stats = indexer.index_odinson_dir(dir, Some(10), 200).await?;
let summary = indexer.summary().await?;               // published splits / docs per metastore
indexer.shutdown().await;
```

`index_docs(Vec<serde_json::Value>)` indexes already-flattened sentence documents.

## Behavior worth knowing

- **Idempotency.** Each batch is checkpointed under a partition derived from a SHA-256 of its
  documents, so re-running over the same files with the same `--batch-size` skips everything already
  published. Changing `--batch-size`/`--limit` re-groups files into different batches and will
  index the overlap again (documents have no unique key). Use `--overwrite` to start clean.
- **Mapping drift.** If the index exists with a different `doc_mapping`/`index_uri` than
  `rustie-schema` generates, `ensure_index` fails instead of indexing into a stale schema.
- **Unmapped fields.** The mapping declares every Odinson token field (`word/lemma/pos/tag/entity/chunk/norm/raw`).
  Any other top-level field is pruned before indexing and reported (`unmapped_fields`).
- **Invalid documents.** Quickwit advances checkpoints past documents it rejects, which would lose
  them permanently. Documents are therefore validated against the doc mapper first; by default a
  rejected document aborts the run before its batch is submitted (`--skip-invalid` to drop instead).
  Files that fail to parse as Odinson are skipped and listed.
- **Single writer.** The file-backed metastore on S3 assumes one writer per metastore URI; do not run
  two indexers against the same metastore concurrently.

## Tests

```bash
cargo test -p rustie-indexer                          # unit tests, no services needed
RUSTIE_MINIO_TEST=1 cargo test -p rustie-indexer --test minio_integration -- --ignored
```

The integration test uses an isolated `it-<pid>-<ts>/` prefix and deletes its index afterwards
(a 58-byte `metastore/manifest.json` is left behind).
