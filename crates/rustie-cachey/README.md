# rustie-cachey

Read-through [Cachey](https://github.com/s2-streamstore/cachey) wrapper for Quickwit S3
`.split` range reads. Used by `rustie-search` (embedded) and searcher-only `rustie-node`.

Only `Storage::get_slice` on paths with extension `split` goes through Cachey. Writes,
`copy_to` / whole-split downloads, and metastore objects stay on the inner S3 client.

## Fork note

Implementing `Storage` outside `quickwit-storage` requires a public `SendableAsync` re-export
(used by `copy_to`). The Crispae `rustie-ext` fork needs:

```rust
pub use self::storage::{SendableAsync, Storage};
```

Local commit on `quickwit-fork` (`2bc4fe5`) has this one-line change; push it and bump the
workspace patch rev when ready. Until then, a matching edit in the cargo git checkout is enough
to build.

## Tests

```bash
cargo test -p rustie-cachey
RUSTIE_CACHEY_TEST=1 cargo test -p rustie-cachey --test minio_identity -- --ignored
```
