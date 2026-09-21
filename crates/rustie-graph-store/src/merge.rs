//! Merge several GPH2 sidecars into one, for segment-compaction column
//! transcode.
//!
//! `rel_dict` / `attr_dicts` / `colocated` / `label_w` / `attr_w` are
//! file-global (see `format.rs`), so a source block's bytes are only
//! meaningful against its own trailer's dictionaries. Merging therefore
//! cannot be a byte-level block copy: every token's `rel`/attr id must be
//! remapped from its source dictionary into a union dictionary, and — because
//! the union can cross the 255-entry width threshold — columns must be
//! re-emitted at whatever width the union needs, never `memcpy`'d.
//!
//! Dictionary order is cosmetic: widths derive from dict length, and every
//! consumer (`SentenceView::label`/`attr`, `LabelSet`, `AttrSet`) resolves by
//! string against its own trailer. So the union dictionary can be built by
//! straightforward first-seen concatenation across sources, with no frequency
//! pass over records required.

use crate::format::Gph2Trailer;
use crate::reader::{Gph2File, RawSentenceColumns};
use crate::writer::{Gph2StreamWriter, PreparedSentence};
use std::collections::HashMap;
use std::io::Write;

/// Union of one dictionary across sources, plus a per-source id remap.
struct DictUnion {
    dict: Vec<String>,
    index: HashMap<String, u16>,
}

impl DictUnion {
    /// `reserved_empty`: seed with `""` at id 0, matching attr dicts (never
    /// matches an empty `rel_dict`, which has no reserved slot).
    fn new(reserved_empty: bool) -> Self {
        let mut u = Self {
            dict: Vec::new(),
            index: HashMap::new(),
        };
        if reserved_empty {
            u.intern("");
        }
        u
    }

    fn intern(&mut self, s: &str) -> u16 {
        if let Some(&id) = self.index.get(s) {
            return id;
        }
        let id = self.dict.len() as u16;
        self.index.insert(s.to_string(), id);
        self.dict.push(s.to_string());
        id
    }

    /// Remap table from `source_dict`'s local ids into this union's ids.
    /// Every string in `source_dict` must already have been `intern`ed.
    fn remap_from(&self, source_dict: &[String]) -> Result<Vec<u16>, String> {
        source_dict
            .iter()
            .map(|s| {
                self.index
                    .get(s)
                    .copied()
                    .ok_or_else(|| format!("merge_sidecars: '{s}' missing from union dict"))
            })
            .collect()
    }

    fn check_capacity(&self, what: &str) -> Result<(), String> {
        if self.dict.len() > u16::MAX as usize + 1 {
            return Err(format!(
                "merge_sidecars: union {what} dict has {} entries, exceeds u16 id space",
                self.dict.len()
            ));
        }
        Ok(())
    }
}

fn remap_sentence(
    raw: RawSentenceColumns,
    rel_map: &[u16],
    attr_maps: &[Vec<u16>],
) -> PreparedSentence {
    let remap_rel = |id: u16| rel_map.get(id as usize).copied().unwrap_or(0);
    let rel_ids = raw.rel_ids.iter().copied().map(remap_rel).collect();
    let overlay = raw
        .overlay
        .iter()
        .map(|&(g, d, r)| (g, d, remap_rel(r)))
        .collect();
    let attr_ids = raw
        .attr_ids
        .into_iter()
        .enumerate()
        .map(|(k, col)| {
            let map = attr_maps.get(k);
            col.into_iter()
                .map(|id| map.and_then(|m| m.get(id as usize).copied()).unwrap_or(0))
                .collect()
        })
        .collect();
    PreparedSentence {
        n_tokens: raw.n_tokens,
        head: raw.head,
        rel_ids,
        attr_ids,
        overlay,
    }
}

/// One input of a merge.
pub struct MergeSource<'a> {
    /// The source's complete GPH2 file, or `None` for a source that has no graph component
    /// (splits written before the component existed): its rows become holes.
    pub data: Option<&'a [u8]>,
    /// Rows in the source. Must equal the file's `max_doc` when `data` is present.
    pub num_docs: u32,
    /// Ascending local row ids that survive the merge (`None` keeps every row). Rows are
    /// emitted in this order, so the output row of the k-th survivor of source `j` is
    /// `Σ_{i<j} survivors(i) + k`, the numbering a search-index merge uses when it stacks
    /// segments and drops deleted documents.
    pub alive: Option<&'a [u32]>,
}

/// Merge `sources` into one GPH2 file for `target_uuid`, streaming it to `sink`.
///
/// Source order is the caller's row order: pass sources in the order the search index stacks
/// its segments. Every source with data must share the same `colocated` attribute list; that
/// comes from schema configuration, so a mismatch means the caller picked an invalid merge set.
pub fn merge_sources<W: Write>(
    sources: &[MergeSource],
    target_uuid: &str,
    sink: W,
) -> Result<W, String> {
    if sources.is_empty() {
        return Err("merge_sidecars: no source sidecars".into());
    }
    let files: Vec<Option<Gph2File>> = sources
        .iter()
        .map(|s| s.data.map(Gph2File::open).transpose())
        .collect::<Result<_, _>>()?;
    for (j, (src, file)) in sources.iter().zip(&files).enumerate() {
        if let Some(f) = file {
            if f.trailer.max_doc != src.num_docs {
                return Err(format!(
                    "merge_sidecars: source {j} has {} rows but caller says {}",
                    f.trailer.max_doc, src.num_docs
                ));
            }
        }
        if let Some(alive) = src.alive {
            let ordered = alive.windows(2).all(|w| w[0] < w[1]);
            if !ordered || alive.last().is_some_and(|&d| d >= src.num_docs) {
                return Err(format!(
                    "merge_sidecars: source {j} alive list is not ascending / in range"
                ));
            }
        }
    }

    let with_data: Vec<&Gph2File> = files.iter().flatten().collect();
    let colocated = with_data
        .first()
        .map(|f| f.trailer.colocated.clone())
        .unwrap_or_default();
    for f in &with_data[1.min(with_data.len())..] {
        if f.trailer.colocated != colocated {
            return Err(format!(
                "merge_sidecars: colocated attr fields differ ({:?} vs {:?})",
                colocated, f.trailer.colocated
            ));
        }
    }
    let n_attrs = colocated.len();

    let mut rel_union = DictUnion::new(false);
    let mut attr_unions: Vec<DictUnion> = (0..n_attrs).map(|_| DictUnion::new(true)).collect();
    for f in &with_data {
        for s in &f.trailer.rel_dict {
            rel_union.intern(s);
        }
        for (k, union) in attr_unions.iter_mut().enumerate() {
            for s in &f.trailer.attr_dicts[k] {
                union.intern(s);
            }
        }
    }
    rel_union.check_capacity("rel")?;
    for (k, union) in attr_unions.iter().enumerate() {
        union.check_capacity(&format!("attr[{k}]"))?;
    }

    let label_w: u8 = if rel_union.dict.len() > 255 { 2 } else { 1 };
    let attr_w: Vec<u8> = attr_unions
        .iter()
        .map(|u| if u.dict.len() > 255 { 2 } else { 1 })
        .collect();

    let mut writer = Gph2StreamWriter::new(sink, label_w, attr_w);
    for (j, (src, file)) in sources.iter().zip(&files).enumerate() {
        let survivors = |local: u32| match src.alive {
            None => true,
            Some(alive) => alive.binary_search(&local).is_ok(),
        };
        let Some(file) = file else {
            let kept = src.alive.map_or(src.num_docs as usize, |a| a.len());
            for _ in 0..kept {
                writer.push_sentence(None)?;
            }
            continue;
        };
        let rel_map = rel_union.remap_from(&file.trailer.rel_dict)?;
        let attr_maps: Vec<Vec<u16>> = (0..n_attrs)
            .map(|k| attr_unions[k].remap_from(&file.trailer.attr_dicts[k]))
            .collect::<Result<_, _>>()?;

        let mut seen = 0u32;
        for b in 0..file.trailer.n_blocks {
            let block = file.block(b as usize)?;
            for local in 0..block.hdr.n_docs as usize {
                if survivors(seen) {
                    let raw = block.raw_columns(local)?;
                    writer.push_sentence(Some(remap_sentence(raw, &rel_map, &attr_maps)))?;
                }
                seen += 1;
            }
        }
        if seen != src.num_docs {
            return Err(format!(
                "merge_sidecars: source {j} block doc count {seen} != trailer.max_doc {}",
                src.num_docs
            ));
        }
    }

    writer.finish(
        target_uuid,
        colocated,
        rel_union.dict,
        attr_dicts(attr_unions),
    )
}

/// Merge complete GPH2 file buffers, keeping every row and concatenating in the given order.
pub fn merge_sidecars(sources: &[Vec<u8>], target_uuid: &str) -> Result<Vec<u8>, String> {
    let counts: Vec<u32> = sources
        .iter()
        .map(|b| crate::format::trailer_from_file_bytes(b).map(|(t, _)| t.max_doc))
        .collect::<Result<_, _>>()?;
    let merge_sources_list: Vec<MergeSource> = sources
        .iter()
        .zip(&counts)
        .map(|(bytes, &num_docs)| MergeSource {
            data: Some(bytes),
            num_docs,
            alive: None,
        })
        .collect();
    merge_sources(&merge_sources_list, target_uuid, Vec::new())
}

fn attr_dicts(unions: Vec<DictUnion>) -> Vec<Vec<String>> {
    unions.into_iter().map(|u| u.dict).collect()
}

/// Sum of `max_doc` across GPH2 file byte buffers, without merging them.
/// Lets a caller assert `merged.max_doc == Σ source.max_doc` against the
/// Tantivy-side merge independently of this module.
pub fn sidecar_doc_count(sources: &[Vec<u8>]) -> Result<u64, String> {
    let mut total = 0u64;
    for bytes in sources {
        let (trailer, _): (Gph2Trailer, _) = crate::format::trailer_from_file_bytes(bytes)?;
        total += trailer.max_doc as u64;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backbone::ROOT;
    use crate::record::SentenceRecord;
    use crate::view::SentenceScratch;
    use crate::writer::Gph2Writer;
    use proptest::prelude::*;
    use std::collections::BTreeSet;

    fn sentence(n_tok: u32, edges: Vec<(u32, u32, &str)>, tag: &str) -> SentenceRecord {
        let mut rec = SentenceRecord::new(n_tok);
        rec.set_edges(
            edges
                .into_iter()
                .map(|(g, d, r)| (g, d, r.to_string()))
                .collect(),
        );
        rec.set_attr("tag", vec![tag.to_string(); n_tok as usize]);
        rec
    }

    fn encode(uuid: &str, docs: &[Option<SentenceRecord>]) -> Vec<u8> {
        Gph2Writer::encode(uuid, docs).unwrap()
    }

    /// Assert `merged.sentence(offset + i)` and `source.sentence(i)` agree on
    /// n_tokens, head, resolved rel label per token, resolved attr per token,
    /// and the sorted edge-triple set — a view-level differential rather than
    /// a byte or round-trip comparison, since dict order and block layout are
    /// both allowed to differ between merged and source.
    fn assert_sentences_equal(
        merged: &Gph2File,
        merged_doc: u32,
        source: &Gph2File,
        source_doc: u32,
    ) {
        let mut ms = SentenceScratch::default();
        let mut ss = SentenceScratch::default();
        let mv = merged.sentence(merged_doc, &mut ms).unwrap();
        let sv = source.sentence(source_doc, &mut ss).unwrap();
        assert_eq!(
            mv.n_tokens, sv.n_tokens,
            "n_tokens at merged doc {merged_doc}"
        );
        for t in 0..sv.n_tokens {
            let m_head_label = if mv.head[t] == ROOT {
                None
            } else {
                Some(mv.label(mv.rel[t]).to_string())
            };
            let s_head_label = if sv.head[t] == ROOT {
                None
            } else {
                Some(sv.label(sv.rel[t]).to_string())
            };
            assert_eq!(
                mv.head[t] == ROOT,
                sv.head[t] == ROOT,
                "token {t} ROOT-ness at merged doc {merged_doc}"
            );
            assert_eq!(
                m_head_label, s_head_label,
                "token {t} rel label at merged doc {merged_doc}"
            );
            assert_eq!(
                mv.attr("tag", t),
                sv.attr("tag", t),
                "token {t} tag at merged doc {merged_doc}"
            );
        }
        let m_edges: BTreeSet<(u32, u32, String)> = mv
            .edges()
            .into_iter()
            .map(|(g, d, r)| (g, d, r.to_string()))
            .collect();
        let s_edges: BTreeSet<(u32, u32, String)> = sv
            .edges()
            .into_iter()
            .map(|(g, d, r)| (g, d, r.to_string()))
            .collect();
        assert_eq!(m_edges, s_edges, "edge set at merged doc {merged_doc}");
    }

    #[test]
    fn merges_disjoint_dicts_two_sources() {
        let a = encode(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &[
                Some(sentence(3, vec![(1, 0, "nsubj"), (1, 2, "dobj")], "NN")),
                Some(sentence(2, vec![(1, 0, "amod")], "JJ")),
            ],
        );
        let b = encode(
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            &[Some(sentence(4, vec![(2, 0, "nmod"), (2, 1, "det")], "VB"))],
        );
        let merged_bytes =
            merge_sidecars(&[a.clone(), b.clone()], "cccccccccccccccccccccccccccccccc").unwrap();

        let source_a = Gph2File::open(&a).unwrap();
        let source_b = Gph2File::open(&b).unwrap();
        let merged = Gph2File::open(&merged_bytes).unwrap();
        assert_eq!(merged.trailer.max_doc, 3);

        assert_sentences_equal(&merged, 0, &source_a, 0);
        assert_sentences_equal(&merged, 1, &source_a, 1);
        assert_sentences_equal(&merged, 2, &source_b, 0);
    }

    #[test]
    fn merges_overlapping_dicts_shares_ids() {
        let a = encode(
            "11111111111111111111111111111111",
            &[Some(sentence(3, vec![(1, 0, "nsubj")], "NN"))],
        );
        let b = encode(
            "22222222222222222222222222222222",
            &[Some(sentence(3, vec![(1, 0, "nsubj")], "NN"))],
        );
        let merged_bytes =
            merge_sidecars(&[a.clone(), b.clone()], "33333333333333333333333333333333").unwrap();
        let merged = Gph2File::open(&merged_bytes).unwrap();
        // A shared label should intern to exactly one union id.
        assert_eq!(merged.trailer.rel_dict.len(), 1);
        assert_eq!(merged.trailer.attr_dicts[0].len(), 2); // "" + "NN"

        let source_a = Gph2File::open(&a).unwrap();
        let source_b = Gph2File::open(&b).unwrap();
        assert_sentences_equal(&merged, 0, &source_a, 0);
        assert_sentences_equal(&merged, 1, &source_b, 0);
    }

    #[test]
    fn merge_forces_label_width_from_1_to_2() {
        // Each source alone stays under 255 labels; the union does not.
        let mk_docs = |prefix: &str| -> Vec<Option<SentenceRecord>> {
            (0..200u32)
                .map(|i| {
                    let mut rec = SentenceRecord::new(2);
                    rec.set_edges(vec![(1, 0, format!("{prefix}{i}"))]);
                    rec.set_attr("tag", vec!["T".to_string(); 2]);
                    Some(rec)
                })
                .collect()
        };
        let docs_a = mk_docs("labA");
        let docs_b = mk_docs("labB");
        let a = encode("44444444444444444444444444444444", &docs_a);
        let b = encode("55555555555555555555555555555555", &docs_b);
        let source_a = Gph2File::open(&a).unwrap();
        let source_b = Gph2File::open(&b).unwrap();
        assert_eq!(source_a.trailer.label_w, 1);
        assert_eq!(source_b.trailer.label_w, 1);

        let merged_bytes =
            merge_sidecars(&[a.clone(), b.clone()], "66666666666666666666666666666666").unwrap();
        let merged = Gph2File::open(&merged_bytes).unwrap();
        assert_eq!(merged.trailer.label_w, 2);
        assert_eq!(merged.trailer.max_doc, 400);

        for i in 0..200u32 {
            assert_sentences_equal(&merged, i, &source_a, i);
            assert_sentences_equal(&merged, 200 + i, &source_b, i);
        }
    }

    #[test]
    fn merge_preserves_empty_doc_holes() {
        let a = encode(
            "77777777777777777777777777777777",
            &[
                Some(sentence(3, vec![(1, 0, "nsubj")], "NN")),
                None,
                Some(sentence(2, vec![(1, 0, "amod")], "JJ")),
            ],
        );
        let merged_bytes =
            merge_sidecars(std::slice::from_ref(&a), "88888888888888888888888888888888").unwrap();
        let source_a = Gph2File::open(&a).unwrap();
        let merged = Gph2File::open(&merged_bytes).unwrap();
        assert_eq!(merged.trailer.max_doc, 3);

        let mut scratch = SentenceScratch::default();
        let v = merged.sentence(1, &mut scratch).unwrap();
        assert_eq!(v.n_tokens, 0);

        assert_sentences_equal(&merged, 0, &source_a, 0);
        assert_sentences_equal(&merged, 2, &source_a, 2);
    }

    #[test]
    fn rejects_colocated_mismatch() {
        let mut a_rec = SentenceRecord::new(2);
        a_rec.set_edges(vec![(1, 0, "nsubj".to_string())]);
        a_rec.set_attr("tag", vec!["NN".to_string(); 2]);
        let a = encode("99999999999999999999999999999999", &[Some(a_rec)]);

        let mut b_rec = SentenceRecord::new(2);
        b_rec.set_edges(vec![(1, 0, "nsubj".to_string())]);
        b_rec.set_attr("pos", vec!["NN".to_string(); 2]);
        let b = encode("aa999999999999999999999999999999", &[Some(b_rec)]);

        let err = merge_sidecars(&[a, b], "bb999999999999999999999999999999").unwrap_err();
        assert!(err.contains("colocated"), "{err}");
    }

    #[test]
    fn rejects_empty_source_list() {
        let err = merge_sidecars(&[], "cc999999999999999999999999999999").unwrap_err();
        assert!(err.contains("no source sidecars"), "{err}");
    }

    fn arb_label(prefix: &'static str) -> impl Strategy<Value = String> {
        (0u32..12).prop_map(move |i| format!("{prefix}{i}"))
    }

    fn arb_sentence(prefix: &'static str) -> impl Strategy<Value = SentenceRecord> {
        (2u32..6, prop::collection::vec(arb_label(prefix), 0..4)).prop_map(|(n, labels)| {
            let mut rec = SentenceRecord::new(n);
            let edges: Vec<(u32, u32, String)> = labels
                .into_iter()
                .enumerate()
                .map(|(i, lab)| ((i as u32 + 1) % n, i as u32 % n, lab))
                .collect();
            rec.set_edges(edges);
            rec.set_attr("tag", (0..n).map(|i| format!("T{}", i % 3)).collect());
            rec
        })
    }

    /// At least one real sentence (forced at index 0), since
    /// `Gph2Writer::encode` derives `colocated` from the first non-`None` doc
    /// and an all-`None` source would make `colocated` come out `[]` — a
    /// genuine mismatch `merge_sidecars` is right to reject, but not one this
    /// differential test is trying to exercise. Other positions may still be
    /// `None`, covering the empty-doc-hole case.
    fn arb_docs(
        prefix: &'static str,
        max_n: usize,
    ) -> impl Strategy<Value = Vec<Option<SentenceRecord>>> {
        (
            arb_sentence(prefix),
            prop::collection::vec(prop::option::of(arb_sentence(prefix)), 0..max_n),
        )
            .prop_map(|(first, rest)| {
                let mut docs = vec![Some(first)];
                docs.extend(rest);
                docs
            })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(48))]

        #[test]
        fn merged_sentences_match_sources(
            docs_a in arb_docs("a", 20),
            docs_b in arb_docs("b", 20),
            docs_c in arb_docs("ab", 20), // overlapping label namespace with both
        ) {
            let a = encode("d0000000000000000000000000000000", &docs_a);
            let b = encode("d1111111111111111111111111111111", &docs_b);
            let c = encode("d2222222222222222222222222222222", &docs_c);
            let merged_bytes = merge_sidecars(
                &[a.clone(), b.clone(), c.clone()],
                "d3333333333333333333333333333333",
            ).unwrap();

            let source_a = Gph2File::open(&a).unwrap();
            let source_b = Gph2File::open(&b).unwrap();
            let source_c = Gph2File::open(&c).unwrap();
            let merged = Gph2File::open(&merged_bytes).unwrap();

            let expected_total = docs_a.len() + docs_b.len() + docs_c.len();
            prop_assert_eq!(merged.trailer.max_doc as usize, expected_total);

            let mut offset = 0u32;
            for (source, len) in [(&source_a, docs_a.len()), (&source_b, docs_b.len()), (&source_c, docs_c.len())] {
                for i in 0..len as u32 {
                    assert_sentences_equal(&merged, offset + i, source, i);
                }
                offset += len as u32;
            }
        }
    }
}

#[cfg(test)]
mod filtered_tests {
    use super::*;
    use crate::reader::Gph2File;
    use crate::record::SentenceRecord;
    use crate::view::SentenceScratch;
    use crate::writer::Gph2Writer;

    /// Doc `i` has a marker: its single edge label is `l{i}`.
    fn file(range: std::ops::Range<u32>, holes: &[u32]) -> Vec<u8> {
        let docs: Vec<Option<SentenceRecord>> = range
            .map(|i| {
                if holes.contains(&i) {
                    return None;
                }
                let mut r = SentenceRecord::new(2);
                r.set_edges(vec![(0, 1, format!("l{i}"))]);
                Some(r)
            })
            .collect();
        Gph2Writer::encode("u", &docs).unwrap()
    }

    /// The marker label of each row, `None` for a row without a graph.
    fn labels(bytes: &[u8]) -> Vec<Option<String>> {
        let f = Gph2File::open(bytes).unwrap();
        let mut scratch = SentenceScratch::default();
        (0..f.trailer.max_doc)
            .map(|d| {
                let view = f.sentence(d, &mut scratch).unwrap();
                view.edges().first().map(|&(_, _, label)| label.to_string())
            })
            .collect()
    }

    #[test]
    fn dead_rows_are_dropped_and_survivors_keep_source_order() {
        let a = file(0..200, &[]); // markers l0..l199, spans two blocks
        let b = file(1000..1010, &[1003]);
        let alive_a: Vec<u32> = (0..200).filter(|d| d % 3 != 0).collect();
        let out = merge_sources(
            &[
                // Segment order is the caller's: b first, then a.
                MergeSource {
                    data: Some(&b),
                    num_docs: 10,
                    alive: None,
                },
                MergeSource {
                    data: Some(&a),
                    num_docs: 200,
                    alive: Some(&alive_a),
                },
            ],
            "m",
            Vec::new(),
        )
        .unwrap();
        let got = labels(&out);
        let mut expected: Vec<Option<String>> = (1000..1010)
            .map(|i| (i != 1003).then(|| format!("l{i}")))
            .collect();
        expected.extend(alive_a.iter().map(|d| Some(format!("l{d}"))));
        assert_eq!(got, expected);
    }

    #[test]
    fn source_without_a_graph_component_becomes_holes() {
        let a = file(0..5, &[]);
        let out = merge_sources(
            &[
                MergeSource {
                    data: None,
                    num_docs: 4,
                    alive: Some(&[0, 2]),
                },
                MergeSource {
                    data: Some(&a),
                    num_docs: 5,
                    alive: None,
                },
            ],
            "m",
            Vec::new(),
        )
        .unwrap();
        let got = labels(&out);
        assert_eq!(got.len(), 2 + 5);
        assert!(got[..2].iter().all(Option::is_none));
        assert_eq!(got[2], Some("l0".into()));
    }

    #[test]
    fn bad_alive_lists_are_rejected() {
        let a = file(0..5, &[]);
        for bad in [vec![3, 1], vec![1, 1], vec![9]] {
            let err = merge_sources(
                &[MergeSource {
                    data: Some(&a),
                    num_docs: 5,
                    alive: Some(&bad),
                }],
                "m",
                Vec::new(),
            );
            assert!(err.is_err(), "{bad:?}");
        }
    }
}
