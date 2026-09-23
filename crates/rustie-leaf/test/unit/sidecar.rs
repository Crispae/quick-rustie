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

#[test]
fn out_of_range_edges_reject_the_document() {
    let mut d = doc(1);
    d.insert(
        "dependencies".into(),
        serde_json::json!({"edges": [[9, 0, "nsubj"]], "roots": [0]}),
    );
    assert!(sentence_record(&d).is_none());
}

#[test]
fn merge_keeps_alive_rows_in_order() {
    use std::sync::Arc;

    let sidecar = GraphSidecar;
    let dir = tempfile::tempdir().unwrap();
    // Two sources of 3 docs. Keep {0,2} from the first and {0,1} from the second.
    let mut file_bytes = Vec::new();
    for source in 0..2u32 {
        let mut writer = sidecar.new_writer().unwrap();
        for i in 0..3u32 {
            writer
                .push(sidecar.extract(&mut doc((source * 10 + i) as usize)))
                .unwrap();
        }
        let path = dir.path().join(format!("src-{source}.gph2"));
        writer.finish(&path).unwrap();
        file_bytes.push(std::fs::read(&path).unwrap());
    }
    let alives = [vec![0u32, 2], vec![0u32, 1]];
    let sources: Vec<SidecarMergeSource> = file_bytes
        .iter()
        .zip(alives)
        .map(|(data, alive)| SidecarMergeSource {
            data: Some(Arc::new(data.clone())),
            num_docs: 3,
            alive_docs: Some(alive),
        })
        .collect();
    let out = dir.path().join("merged.gph2");
    sidecar.merge(&sources, &out).unwrap();
    let merged = std::fs::read(&out).unwrap();
    let file = Gph2File::open(&merged).unwrap();
    assert_eq!(file.trailer.max_doc, 4);
    let mut scratch = SentenceScratch::default();
    // Surviving labels: l0, l2 from src0; l10, l11 from src1 (doc() uses i in label).
    let mut labels = Vec::new();
    for row in 0..4 {
        let view = file.sentence(row, &mut scratch).unwrap();
        let mut edge_labels: Vec<String> = view.edges().iter().map(|e| e.2.to_string()).collect();
        edge_labels.sort();
        labels.push(edge_labels);
    }
    assert_eq!(
        labels,
        [
            vec!["l0".to_string(), "nsubj".to_string()],
            vec!["l2".to_string(), "nsubj".to_string()],
            vec!["l10".to_string(), "nsubj".to_string()],
            vec!["l11".to_string(), "nsubj".to_string()],
        ]
    );
}
