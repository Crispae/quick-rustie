//! The GPH2 split component: built from each sentence document while indexing.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

use bytes::Bytes;
use quickwit_extensions::{SidecarMergeSource, SidecarWriter, SplitSidecar};
use rustie_graph_store::format::{END_RECORD_LEN, EndRecord};
use rustie_graph_store::{MergeSource, ROOT, SentenceRecord, SpoolWriter, merge_sources};
use rustie_schema::{DEFAULT_BASIC_GRAPH_FIELD, DEFAULT_GRAPH_FIELD, decode_tokens_from_quickwit};
use serde_json::{Map, Value as JsonValue};

use crate::{COLOCATED_FIELDS, GRAPH_FILE};

/// Builds `rustie.gph2` for every split.
pub struct GraphSidecar;

impl SplitSidecar for GraphSidecar {
    fn file_name(&self) -> &str {
        GRAPH_FILE
    }

    /// Reads, without removing, the sentence's graph (still stored in the document so hits can
    /// be rendered) and its colocated token fields.
    fn extract(&self, doc: &mut Map<String, JsonValue>) -> Option<Bytes> {
        sentence_record(doc).map(|record| Bytes::from(record.to_bytes()))
    }

    fn new_writer(&self) -> io::Result<Box<dyn SidecarWriter>> {
        Ok(Box::new(GraphWriter {
            spool: SpoolWriter::new().map_err(io::Error::other)?,
        }))
    }

    fn merge(&self, sources: &[SidecarMergeSource], out: &Path) -> io::Result<()> {
        let sources: Vec<MergeSource> = sources
            .iter()
            .map(|source| MergeSource {
                data: source.data.as_ref().map(|data| (**data).as_ref()),
                num_docs: source.num_docs,
                alive: source.alive_docs.as_deref(),
            })
            .collect();
        let sink = BufWriter::new(File::create(out)?);
        merge_sources(&sources, &new_uuid(), sink)
            .map_err(io::Error::other)?
            .flush()
    }

    /// The trailer (block offsets + dictionaries) and end record: needed by every query that
    /// touches the graph, so they live in the hotcache.
    #[allow(clippy::single_range_in_vec_init)] // one range by design; the API allows several
    fn hotcache_ranges(&self, file: &[u8]) -> Vec<std::ops::Range<usize>> {
        if file.len() < END_RECORD_LEN {
            return Vec::new();
        }
        match EndRecord::decode(&file[file.len() - END_RECORD_LEN..]) {
            Ok(record) => {
                let trailer = EndRecord::trailer_range(file.len() as u64, record);
                vec![trailer.start as usize..file.len()]
            }
            Err(_) => Vec::new(),
        }
    }
}

struct GraphWriter {
    spool: SpoolWriter,
}

impl SidecarWriter for GraphWriter {
    fn push(&mut self, payload: Option<Bytes>) -> io::Result<()> {
        let record = payload
            .map(|bytes| SentenceRecord::from_bytes(&bytes))
            .transpose()
            .map_err(io::Error::other)?;
        self.spool.push(record.as_ref()).map_err(io::Error::other)
    }

    fn num_rows(&self) -> u32 {
        self.spool.len()
    }

    fn finish(self: Box<Self>, out: &Path) -> io::Result<()> {
        let sink = BufWriter::new(File::create(out)?);
        self.spool
            .finish(sink, &new_uuid())
            .map_err(io::Error::other)?
            .flush()
    }
}

/// Every GPH2 file gets a fresh identity: caches key blocks by it.
fn new_uuid() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The graph record of a flattened sentence document (see `rustie_schema::SentenceDoc`), or
/// `None` when it has no dependency graph.
pub(crate) fn sentence_record(doc: &Map<String, JsonValue>) -> Option<SentenceRecord> {
    let n = doc.get("sentence_length")?.as_u64()? as usize;
    let edges = graph_edges(
        doc.get(DEFAULT_GRAPH_FIELD)
            .or(doc.get(DEFAULT_BASIC_GRAPH_FIELD))?,
        n,
    )?;
    let mut record = SentenceRecord::new(n as u32);
    record.set_edges(edges);
    if let Some(basic) = doc
        .get(DEFAULT_BASIC_GRAPH_FIELD)
        .and_then(|g| graph_edges(g, n))
    {
        let mut heads = vec![ROOT; n];
        for (gov, dep, _) in basic {
            heads[dep as usize] = gov;
        }
        record.set_basic_heads(heads);
    }
    // Every record carries every colocated field (empty values when absent), so all rows of a
    // file agree on the attribute columns.
    for field in COLOCATED_FIELDS {
        let mut values = doc
            .get(*field)
            .and_then(JsonValue::as_str)
            .map(decode_tokens_from_quickwit)
            .unwrap_or_default();
        values.resize(n, String::new());
        record.set_attr(*field, values);
    }
    Some(record)
}

/// `{"edges": [[gov, dep, label], ...]}` with in-range endpoints.
fn graph_edges(graph: &JsonValue, n: usize) -> Option<Vec<(u32, u32, String)>> {
    graph
        .get("edges")?
        .as_array()?
        .iter()
        .map(|edge| {
            let edge = edge.as_array()?;
            let gov = edge.first()?.as_u64()?;
            let dep = edge.get(1)?.as_u64()?;
            let label = edge.get(2)?.as_str()?;
            ((gov as usize) < n && (dep as usize) < n)
                .then(|| (gov as u32, dep as u32, label.to_string()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustie_graph_store::SentenceScratch;
    use rustie_graph_store::reader::Gph2File;

    fn doc(i: usize) -> Map<String, JsonValue> {
        let JsonValue::Object(map) = serde_json::json!({
            "sentence_length": 3,
            "word": format!("w{i}|runs|fast"),
            "tag": "NNP|VBZ|RB",
            "dependencies": {"edges": [[1, 0, "nsubj"], [1, 2, format!("l{i}")]], "roots": [1]}
        }) else {
            unreachable!()
        };
        map
    }

    #[test]
    fn writer_output_is_a_valid_graph_file_with_colocated_tags() {
        let sidecar = GraphSidecar;
        let mut writer = sidecar.new_writer().unwrap();
        for i in 0..300 {
            let payload = if i % 5 == 0 {
                None
            } else {
                sidecar.extract(&mut doc(i))
            };
            writer.push(payload).unwrap();
        }
        assert_eq!(writer.num_rows(), 300);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(GRAPH_FILE);
        writer.finish(&path).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        let file = Gph2File::open(&bytes).unwrap();
        assert_eq!(file.trailer.max_doc, 300);
        assert_eq!(file.trailer.colocated, COLOCATED_FIELDS);
        let mut scratch = SentenceScratch::default();
        assert_eq!(file.sentence(5, &mut scratch).unwrap().n_tokens, 0, "hole");
        let view = file.sentence(7, &mut scratch).unwrap();
        assert_eq!(view.attr("tag", 1), "VBZ");
        let mut labels: Vec<&str> = view.edges().iter().map(|e| e.2).collect();
        labels.sort();
        assert_eq!(labels, ["l7", "nsubj"]);

        // The hot range is exactly trailer + end record.
        let hot = sidecar.hotcache_ranges(&bytes);
        assert_eq!(hot.len(), 1);
        assert_eq!(hot[0].end, bytes.len());
        assert_eq!(hot[0].start as u64, file.trailer_range().start);
    }

    #[test]
    fn documents_without_a_graph_have_no_record() {
        let mut d = doc(1);
        d.remove("dependencies");
        assert!(sentence_record(&d).is_none());
    }
}
