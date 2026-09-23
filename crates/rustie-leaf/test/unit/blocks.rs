use super::*;
use std::path::Path;

use rustie_graph_store::{Gph2Writer, SentenceRecord};
use tantivy::directory::{Directory, RamDirectory};

fn sentence(i: u32) -> SentenceRecord {
    let mut rec = SentenceRecord::new(2);
    rec.set_edges(vec![(1, 0, format!("nsubj{i}"))]);
    rec.set_attr("tag", vec!["NN".into(), "VBZ".into()]);
    rec.set_attr("entity", vec![String::new(); 2]);
    rec.set_attr("chunk", vec![String::new(); 2]);
    rec
}

/// Enough sentences for three GPH2 blocks (128 docs each), unique uuid per call.
fn encode_three_blocks(uuid: &str) -> Vec<u8> {
    let docs: Vec<_> = (0..BLOCK_DOCS as u32 * 3)
        .map(|i| Some(sentence(i)))
        .collect();
    Gph2Writer::encode(uuid, &docs).unwrap()
}

fn empty_file_slice() -> FileSlice {
    let dir = RamDirectory::create();
    dir.atomic_write(Path::new("x"), &[]).unwrap();
    dir.open_read(Path::new("x")).unwrap()
}

#[test]
fn block_cache_evicts_when_over_budget() {
    let bytes = encode_three_blocks("leaf-block-cache-evict");
    let dir = RamDirectory::create();
    dir.atomic_write(Path::new(GRAPH_FILE), &bytes).unwrap();
    let graph = futures::executor::block_on(SplitGraph::open(&dir))
        .unwrap()
        .unwrap();
    let loaded =
        futures::executor::block_on(graph.blocks_for_docs(&[0, BLOCK_DOCS as u32])).unwrap();
    let b0 = loaded.get(&0).unwrap().clone();
    let b1 = loaded.get(&1).unwrap().clone();
    let size0 = graph.block_range(0).len();
    let size1 = graph.block_range(1).len();

    let mut cache = BlockCache {
        entries: LruCache::unbounded(),
        bytes: 0,
        budget: size0, // room for exactly one of these blocks
    };
    cache.put((Arc::from("u"), 0), b0, size0);
    assert!(cache.get(&(Arc::from("u"), 0)).is_some());
    cache.put((Arc::from("u"), 1), b1, size1);
    assert!(
        cache.get(&(Arc::from("u"), 0)).is_none(),
        "first block must be evicted"
    );
    assert!(cache.get(&(Arc::from("u"), 1)).is_some());
    assert!(cache.bytes <= cache.budget);
}

#[test]
fn coalesce_merges_nearby_blocks_and_splits_far_ones() {
    let mut trailer = Gph2Trailer::empty();
    trailer.n_blocks = 3;
    // Contiguous layout; skipping the huge middle block leaves a hole > COALESCE_GAP.
    let mid = COALESCE_GAP_BYTES as u32 + 1;
    trailer.block_off = vec![0, 100, 100 + mid, 100 + mid + 50];
    let graph = SplitGraph {
        file: empty_file_slice(),
        trailer: Arc::new(trailer),
        uuid: Arc::from("coalesce"),
    };

    let adjacent = graph.coalesce(&[0, 1]);
    assert_eq!(adjacent.len(), 1);
    assert_eq!(adjacent[0].1, vec![0, 1]);
    assert_eq!(adjacent[0].0, 0..(100 + mid) as usize);

    let far = graph.coalesce(&[0, 2]);
    assert_eq!(far.len(), 2, "skipped middle exceeds COALESCE_GAP_BYTES");
    assert_eq!(far[0].1, vec![0]);
    assert_eq!(far[1].1, vec![2]);
}

#[tokio::test]
async fn open_returns_none_without_graph_file() {
    let dir = RamDirectory::create();
    assert!(SplitGraph::open(&dir).await.unwrap().is_none());
}

#[tokio::test]
async fn blocks_for_docs_loads_and_reuses_cache() {
    let bytes = encode_three_blocks("leaf-blocks-reuse");
    let dir = RamDirectory::create();
    dir.atomic_write(Path::new(GRAPH_FILE), &bytes).unwrap();
    let graph = SplitGraph::open(&dir).await.unwrap().unwrap();
    assert_eq!(graph.trailer.n_blocks, 3);

    let docs = [0u32, BLOCK_DOCS as u32, 2 * BLOCK_DOCS as u32 + 1];
    let first = graph.blocks_for_docs(&docs).await.unwrap();
    assert_eq!(first.len(), 3);
    assert!(first.contains_key(&0) && first.contains_key(&1) && first.contains_key(&2));

    // Second call: all hits from the process-wide block cache (same uuid).
    let second = graph.blocks_for_docs(&docs).await.unwrap();
    assert_eq!(second.len(), 3);
    assert!(Arc::ptr_eq(first.get(&0).unwrap(), second.get(&0).unwrap()));
}
