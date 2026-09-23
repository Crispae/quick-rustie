//! Reading a split's GPH2 file: trailer from the hotcache, blocks by range reads, both cached
//! process-wide.

use std::collections::{BTreeSet, HashMap};
use std::num::NonZeroUsize;
use std::ops::Range;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::Context;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use lru::LruCache;
use rustie_graph_store::BLOCK_DOCS;
use rustie_graph_store::format::{END_RECORD_LEN, EndRecord, Gph2Trailer};
use rustie_graph_store::reader::Gph2Block;
use tantivy::HasLen;
use tantivy::directory::{Directory, FileSlice};

use crate::GRAPH_FILE;

/// Byte budget of the process-wide block cache (`RUSTIE_GRAPH_CACHE_MB`, default 512).
fn block_cache_budget() -> usize {
    std::env::var("RUSTIE_GRAPH_CACHE_MB")
        .ok()
        .and_then(|mb| mb.parse::<usize>().ok())
        .unwrap_or(512)
        * 1024
        * 1024
}

/// Two nearby block ranges are fetched in one request when the gap between them is smaller
/// than this: on object storage a request costs far more than a few extra kilobytes.
const COALESCE_GAP_BYTES: usize = 256 * 1024;

/// A PROBE touching more than this fraction of the blocks reads the whole body instead.
const SCAN_FRACTION: f64 = 0.5;

/// A request whose newly cached blocks fill at least this fraction of its bytes is shared by
/// those blocks instead of copied per block (see `SplitGraph::blocks_for_docs`).
const MIN_SHARED_COVERAGE: f64 = 0.9;

/// Concurrent range requests per split.
const FETCH_CONCURRENCY: usize = 8;

type BlockKey = (Arc<str>, u32);

struct BlockCache {
    entries: LruCache<BlockKey, (Arc<Gph2Block>, usize)>,
    bytes: usize,
    budget: usize,
}

impl BlockCache {
    fn get(&mut self, key: &BlockKey) -> Option<Arc<Gph2Block>> {
        self.entries.get(key).map(|(block, _)| block.clone())
    }

    /// `size` is the block's own bytes; a block may share a request buffer with the other blocks
    /// of that request, but only when they fill most of it (see `SplitGraph::blocks_for_docs`).
    fn put(&mut self, key: BlockKey, block: Arc<Gph2Block>, size: usize) {
        if let Some((_, old)) = self.entries.put(key, (block, size)) {
            self.bytes -= old;
        }
        self.bytes += size;
        while self.bytes > self.budget {
            match self.entries.pop_lru() {
                Some((_, (_, size))) => self.bytes -= size,
                None => break,
            }
        }
    }
}

fn block_cache() -> &'static Mutex<BlockCache> {
    static CACHE: OnceLock<Mutex<BlockCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        Mutex::new(BlockCache {
            entries: LruCache::unbounded(),
            bytes: 0,
            budget: block_cache_budget(),
        })
    })
}

fn trailer_cache() -> &'static Mutex<LruCache<Arc<str>, Arc<Gph2Trailer>>> {
    static CACHE: OnceLock<Mutex<LruCache<Arc<str>, Arc<Gph2Trailer>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(LruCache::new(NonZeroUsize::new(4096).unwrap())))
}

/// One split's graph file.
pub(crate) struct SplitGraph {
    file: FileSlice,
    pub(crate) trailer: Arc<Gph2Trailer>,
    uuid: Arc<str>,
}

impl SplitGraph {
    /// Open the split's graph file, or `None` if the split has none (indexed before the
    /// component existed).
    pub(crate) async fn open(split_directory: &dyn Directory) -> anyhow::Result<Option<Self>> {
        let path = Path::new(GRAPH_FILE);
        if !split_directory.exists(path)? {
            return Ok(None);
        }
        let file = split_directory.open_read(path)?;
        let len = file.len();
        anyhow::ensure!(len >= END_RECORD_LEN, "{GRAPH_FILE} is truncated");
        // Both reads are served by the hotcache (see `GraphSidecar::hotcache_ranges`).
        let end = file
            .read_bytes_slice_async(len - END_RECORD_LEN..len)
            .await?;
        let record = EndRecord::decode(end.as_slice()).map_err(anyhow::Error::msg)?;
        let range = EndRecord::trailer_range(len as u64, record);
        let range = range.start as usize..range.end as usize;
        anyhow::ensure!(range.len() >= 32, "{GRAPH_FILE} trailer is truncated");
        let uuid_bytes = file
            .read_bytes_slice_async(range.start..range.start + 32)
            .await?;
        let uuid: Arc<str> = {
            let raw = uuid_bytes.as_slice();
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[..end]).into()
        };
        let cached = trailer_cache()
            .lock()
            .expect("poisoned")
            .get(&uuid)
            .cloned();
        let trailer = match cached {
            Some(trailer) => trailer,
            None => {
                let bytes = file.read_bytes_slice_async(range).await?;
                let trailer =
                    Arc::new(Gph2Trailer::decode(bytes.as_slice()).map_err(anyhow::Error::msg)?);
                trailer_cache()
                    .lock()
                    .expect("poisoned")
                    .put(uuid.clone(), trailer.clone());
                trailer
            }
        };
        Ok(Some(Self {
            file,
            trailer,
            uuid,
        }))
    }

    fn block_range(&self, block: u32) -> Range<usize> {
        let offsets = &self.trailer.block_off;
        offsets[block as usize] as usize..offsets[block as usize + 1] as usize
    }

    /// Parsed blocks holding `docs` (ascending), from the cache or storage.
    pub(crate) async fn blocks_for_docs(
        &self,
        docs: &[u32],
    ) -> anyhow::Result<HashMap<u32, Arc<Gph2Block>>> {
        let n_blocks = self.trailer.n_blocks;
        let wanted: BTreeSet<u32> = docs
            .iter()
            .map(|&doc| doc / BLOCK_DOCS as u32)
            .filter(|&block| block < n_blocks)
            .collect();
        let mut found = HashMap::with_capacity(wanted.len());
        let mut missing = Vec::new();
        {
            let mut cache = block_cache().lock().expect("poisoned");
            for &block in &wanted {
                match cache.get(&(self.uuid.clone(), block)) {
                    Some(parsed) => {
                        found.insert(block, parsed);
                    }
                    None => missing.push(block),
                }
            }
        }
        if missing.is_empty() {
            return Ok(found);
        }
        let requests = if missing.len() as f64 > SCAN_FRACTION * n_blocks as f64 {
            // SCAN: the whole body in one request.
            let body_end = *self.trailer.block_off.last().unwrap_or(&0) as usize;
            vec![(0..body_end, missing.clone())]
        } else {
            self.coalesce(&missing)
        };
        let started = std::time::Instant::now();
        let fetched: Vec<(Range<usize>, Vec<u32>, tantivy::directory::OwnedBytes)> =
            futures::stream::iter(requests.into_iter().map(|(range, blocks)| async move {
                let bytes = self
                    .file
                    .read_bytes_slice_async(range.clone())
                    .await
                    .with_context(|| format!("reading {GRAPH_FILE} bytes {range:?}"))?;
                anyhow::Ok((range, blocks, bytes))
            }))
            .buffer_unordered(FETCH_CONCURRENCY)
            .try_collect()
            .await?;

        let fetch_took = started.elapsed();
        let fetched_bytes: usize = fetched.iter().map(|(range, ..)| range.len()).sum();
        let body = |range: &Range<usize>, owned: &tantivy::directory::OwnedBytes| {
            Bytes::from_owner(owned.clone()).slice(0..range.len())
        };
        // The cache counts each block's own bytes, but a slice of the request buffer keeps the
        // whole request alive: coalescing gaps, and on a SCAN every block already cached. When a
        // request is mostly bytes nobody counts, each block gets its own copy; when its new
        // blocks fill it (a SCAN of a cold file, a run of adjacent blocks), slicing wastes at most
        // `1 - MIN_SHARED_COVERAGE` of it and saves copying hundreds of MiB.
        // Parsed outside the cache lock: it is shared by every concurrent split search.
        let mut parsed = Vec::with_capacity(missing.len());
        for (range, blocks, owned) in &fetched {
            let covered: usize = blocks.iter().map(|&b| self.block_range(b).len()).sum();
            let share = covered as f64 >= MIN_SHARED_COVERAGE * range.len() as f64;
            let request = body(range, owned);
            for &block in blocks {
                let block_range = self.block_range(block);
                let local = block_range.start - range.start..block_range.end - range.start;
                let bytes = if share {
                    request.slice(local)
                } else {
                    Bytes::copy_from_slice(&request[local])
                };
                let block_data =
                    Gph2Block::parse(bytes, &self.trailer).map_err(anyhow::Error::msg)?;
                parsed.push((block, Arc::new(block_data), block_range.len()));
            }
        }
        drop(fetched);
        {
            let mut cache = block_cache().lock().expect("poisoned");
            for (block, block_data, size) in parsed {
                cache.put((self.uuid.clone(), block), block_data.clone(), size);
                found.insert(block, block_data);
            }
        }
        tracing::debug!(
            wanted = wanted.len(),
            missing = missing.len(),
            n_blocks,
            fetched_mib = fetched_bytes >> 20,
            fetch_ms = fetch_took.as_millis() as u64,
            parse_ms = (started.elapsed() - fetch_took).as_millis() as u64,
            "graph blocks loaded"
        );
        Ok(found)
    }

    /// Group consecutive-ish blocks into requests.
    fn coalesce(&self, blocks: &[u32]) -> Vec<(Range<usize>, Vec<u32>)> {
        let mut requests: Vec<(Range<usize>, Vec<u32>)> = Vec::new();
        for &block in blocks {
            let range = self.block_range(block);
            match requests.last_mut() {
                Some((current, members)) if range.start <= current.end + COALESCE_GAP_BYTES => {
                    current.end = range.end;
                    members.push(block);
                }
                _ => requests.push((range, vec![block])),
            }
        }
        requests
    }
}

#[cfg(test)]
#[path = "../test/unit/blocks.rs"]
mod tests;
