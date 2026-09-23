# rustie-indexer

Creates the IE postings index on MinIO / S3 and ingests flattened Odinson sentences into it by
running Quickwit's real `IndexingService` pipeline (splits, merges, checkpoints), pinned to the
same Quickwit tag as the root crate.

```
data/*.json (Odinson) ──rustie-schema flatten──▶ sentence docs
        ──prune/validate vs doc mapping──▶ Vec source ──▶ IndexingService ──▶ s3://<bucket>/indexes/ie-postings
                                                           Postgres metastore (default) / optional s3:// file-backed
```

## CLI

```bash
docker compose -f docker-compose.minio.yml up -d      # if rustie-minio is not running
docker run -d --name rustie-postgres -p 5433:5432 \
  -e POSTGRES_USER=rustie -e POSTGRES_PASSWORD=rustie -e POSTGRES_DB=rustie postgres:16
export PROTOC=/path/to/protoc                         # needed to build quickwit-proto

# smoke run (Postgres metastore by default)
cargo run -p rustie-indexer --bin rustie-index -- --data "data/processed_pubmed22n0008 (2)" --limit 10

# with live split-cache reports to a running rustie-node
cargo run -p rustie-indexer --bin rustie-index -- \
  --cluster-config configs/rustie-indexer.yaml \
  --data "data/processed_pubmed22n0008 (2)" --limit 10

# full run (re-runnable, see "Idempotency")
cargo run --release -p rustie-indexer --bin rustie-index -- --data "data/processed_pubmed22n0008 (2)"
```

| Flag | Default | |
| --- | --- | --- |
| `--data` | required | directory of Odinson `*.json` / `*.json.gz` (recursive) |
| `--limit` | – | max files (smoke runs) |
| `--batch-size` | 200 | files per pipeline run (≈ one split before merges) |
| `--endpoint` / `--bucket` / `--access-key` / `--secret-key` | `MINIO_*` env, else local `rustie-minio` | prefer env for the secret |
| `--index-id` | `ie-postings` | |
| `--metastore-uri` / `--index-root-uri` | `RUSTIE_METASTORE_URI` or `postgres://rustie:rustie@127.0.0.1:5433/rustie`, `s3://<bucket>/indexes` | |
| `--cluster-config` | – | join rustie-cluster as gossip-only (`RUSTIE_CLUSTER_CONFIG`); live split reports via Quickwit's uploader → placer (no per-batch metastore re-list; cache is best-effort prefetch + query-time warming) |
| `--threads` | 0 (all cores) | threads reading, decompressing, flattening and validating input; `1` = serial. Batch content, order and checkpoints are identical for any value |
| `--pipelines` | 1 | indexing pipelines at once, each on its own Quickwit source ("lane"); each holds a batch in memory. Re-runs skip or resume batches by content in every lane, so the value may differ between runs |
| `--split-num-docs-target` | 500000 | splits at or above this many documents are never merged again; applies when the index is created (`--overwrite` to change) |
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

- **Graph component.** `Indexer::connect` registers `rustie-leaf`, so every split (and every merged
  split) carries `rustie.gph2`, used by graph patterns at search time. Splits indexed before that
  have none: re-index with `--overwrite`.

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
- **Split size = search parallelism.** A split is searched by one thread. Quickwit merges up to
  10M documents by default, which turns a few-million-sentence corpus into one split and one busy
  core; the index config caps it at 500k (`--split-num-docs-target`), giving splits of 0.5–1M
  documents. On 5.1M PubMed sentences that gave 10 splits and 1.3–4.5× faster graph queries than
  one 3.9M-document split. Smaller targets (more splits than cores helps little) trade per-split
  overhead for parallelism.
- **Batch size decides the merge tax.** A batch becomes its own pipeline run. Batches much smaller
  than `--split-num-docs-target` yield small splits that Quickwit then re-merges up to the target,
  and each run waits for its merges to finish. Batches of about the target size yield splits that
  are already mature and never merged: on 1.49M PubMed sentences, 5,000-file batches took 214 s
  (62 s waiting on merges) and 67,000-file batches (≈ 500k sentences, 7.5 per file) about 145 s
  with no merge wait. Pick `--batch-size ≈ target / sentences-per-file`. A batch is all-or-nothing
  (checkpoint per batch), so an interrupt loses at most one batch of work, and it is held in
  memory: 500k sentences peak at about 7 GB with the fork's vec-source fix (rev containing
  `fix(indexing): create the vec source from its typed params`), about 17 GB without it.
- **Lanes: several pipelines at once (`--pipelines N`).** One Quickwit pipeline uses about two
  cores (its indexing thread is serial). With `N > 1` the indexer runs N pipelines concurrently,
  each on its own Quickwit source (`rustie-lane-1`, …; lane 0 is the default CLI source).
  Quickwit runs one merge pipeline per source and it only plans merges of its own source's
  splits, so lanes cannot plan the same merge twice. Which lane indexed a batch is not part of
  its identity: before submitting, the batch's content hash is looked up in *every* source's
  checkpoint, so a re-run skips finished batches and resumes a partly published one on the lane
  holding its progress, whatever `--pipelines` was before. Measured on PubMed (67,000-file
  batches ≈ 500k sentences): 200k files 116 s with one lane, 54 s with three; the full corpus
  (5.14M sentences, 660k files) in 2 min 12 s with four lanes (about 16 min originally), peak
  memory 16 GB (a batch is held per lane). Results are identical: the same document count, and
  12 test queries return the same totals as the serially built index. Each lane leaves a small
  tail split per batch (a batch of ≥ 500k documents is cut at the target); tails are merged
  only within their lane.
- **Where indexing time goes.** Input preparation (read, gunzip, flatten, validate, hash) runs on
  `--threads` threads and two batches ahead of the pipelines, so it is hidden (14 s serial vs
  1.4 s parallel on a 240k-sentence sample). With one lane the rest is Quickwit's pipeline (about
  2 of 20 cores). Stock Quickwit also spends about 25 µs per document spawning a vec-source
  pipeline (it round-trips the documents through JSON); the fork fixes that. The run's final
  line reports the breakdown (with several lanes the `indexing` figure sums the lanes).
- **Cluster join (`--cluster-config`).** The indexer joins as a gossip-only member so the
  uploader's early `ReportSplitsRequest` reaches searchers via Quickwit's `SearchJobPlacer`.
  There is no per-batch metastore re-list: the on-disk split cache is best-effort prefetch;
  misses fall back to object storage and a later query re-enqueues the download.
- **Single writer.** Postgres supports concurrent readers; prefer one indexer writer per index.
  File-backed (`s3://`) metastores assume one writer per metastore URI.

## Tests

```bash
cargo test -p rustie-indexer                          # unit tests, no services needed
RUSTIE_MINIO_TEST=1 cargo test -p rustie-indexer --test minio_integration -- --ignored
RUSTIE_PG_TEST=1 cargo test -p rustie-indexer --test postgres_integration -- --ignored
```

The MinIO integration test uses an isolated `it-<pid>-<ts>/` prefix and deletes its index afterwards
(a 58-byte `metastore/manifest.json` is left behind). The Postgres test needs MinIO for split
files and Postgres for the metastore; it deletes its index afterwards.
