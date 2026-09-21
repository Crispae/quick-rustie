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
| `--data` | required | directory of Odinson `*.json` / `*.json.gz` (recursive) |
| `--limit` | – | max files (smoke runs) |
| `--batch-size` | 200 | files per pipeline run (≈ one split before merges) |
| `--endpoint` / `--bucket` / `--access-key` / `--secret-key` | `MINIO_*` env, else local `rustie-minio` | prefer env for the secret |
| `--index-id` | `ie-postings` | |
| `--metastore-uri` / `--index-root-uri` | `s3://<bucket>/metastore`, `s3://<bucket>/indexes` | |
| `--threads` | 0 (all cores) | threads reading, decompressing, flattening and validating input; `1` = serial. Batch content, order and checkpoints are identical for any value |
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
- **Where indexing time goes.** Input preparation (read, gunzip, flatten, validate, hash) runs on
  `--threads` threads and two batches ahead of the pipeline, so it is hidden (14 s serial vs 1.4 s
  parallel on a 240k-sentence sample). What remains is Quickwit's pipeline (one indexing thread per
  pipeline: about 2 of 20 cores in use). Stock Quickwit also spends about 25 µs per document
  spawning a vec-source pipeline (it round-trips the documents through JSON); the fork fixes that.
  The run's final line reports the breakdown. Merges are not overlapped with the next batch:
  Quickwit shuts a merge pipeline down when no indexing pipeline is attached, so the next batch
  could plan splits still being merged (the metastore rejects the stale publish atomically, so it
  costs work, not correctness). Mature splits make this moot.
- **Single writer.** The file-backed metastore on S3 assumes one writer per metastore URI; do not run
  two indexers against the same metastore concurrently.

## Tests

```bash
cargo test -p rustie-indexer                          # unit tests, no services needed
RUSTIE_MINIO_TEST=1 cargo test -p rustie-indexer --test minio_integration -- --ignored
```

The integration test uses an isolated `it-<pid>-<ts>/` prefix and deletes its index afterwards
(a 58-byte `metastore/manifest.json` is left behind).
