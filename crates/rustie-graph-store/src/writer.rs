//! Two-pass GPH2 writer: frequency-sorted dictionaries, then columnar blocks.
//!
//! [`Gph2Writer::encode`] is a thin wrapper over [`Gph2StreamWriter`], which
//! flushes one `BLOCK_DOCS`-sized block at a time instead of materializing every
//! prepared sentence up front. The streaming writer is the reusable piece: a
//! caller that already knows its dictionaries (e.g. a segment merge, which
//! unions source trailers rather than re-deriving frequencies) can push
//! sentences directly without a frequency pass.

use crate::backbone::{ROOT, split_backbone};
use crate::format::{
    BLOCK_DOCS, BlockHeader, EndRecord, GPH2_VERSION, Gph2Trailer, encode_head, write_width,
};
use crate::normalize_uuid;
use crate::record::SentenceRecord;
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone)]
pub struct PreparedSentence {
    pub n_tokens: u32,
    pub head: Vec<u32>,
    pub rel_ids: Vec<u16>,
    pub attr_ids: Vec<Vec<u16>>,
    pub overlay: Vec<(u32, u32, u16)>,
}

/// O(1) string-to-dict-id lookup, replacing a linear scan per token.
///
/// Ties resolve to the first occurrence, matching the old `position()` scan
/// (dictionaries built by [`freq_sorted_dict`] never contain duplicates, so
/// this only matters defensively).
pub struct DictIndex<'a> {
    ids: HashMap<&'a str, u16>,
}

impl<'a> DictIndex<'a> {
    pub fn build(dict: &'a [String]) -> Self {
        let mut ids = HashMap::with_capacity(dict.len());
        for (i, s) in dict.iter().enumerate() {
            ids.entry(s.as_str()).or_insert(i as u16);
        }
        Self { ids }
    }

    /// Falls back to id 0 for an unknown string, matching the prior linear
    /// scan's `unwrap_or(0)`.
    pub fn get(&self, s: &str) -> u16 {
        self.ids.get(s).copied().unwrap_or(0)
    }
}

/// Streams `PreparedSentence`s into GPH2 blocks, flushing every `BLOCK_DOCS`
/// sentences so memory stays bounded regardless of segment size.
///
/// Dictionaries (`label_w`/`attr_w` widths, and the dicts themselves passed to
/// [`finish`](Self::finish)) must be final before the first sentence is
/// pushed: they are file-global, not per-block.
pub struct Gph2StreamWriter<W: Write> {
    label_w: u8,
    attr_w: Vec<u8>,
    sink: W,
    block_off: Vec<u32>,
    pending: Vec<Option<PreparedSentence>>,
    max_doc: u32,
}

impl<W: Write> Gph2StreamWriter<W> {
    /// `sink` receives the file front to back: blocks as they fill, then trailer and end
    /// record on [`finish`](Self::finish).
    pub fn new(sink: W, label_w: u8, attr_w: Vec<u8>) -> Self {
        Self {
            label_w,
            attr_w,
            sink,
            block_off: vec![0u32],
            pending: Vec::with_capacity(BLOCK_DOCS),
            max_doc: 0,
        }
    }

    pub fn push_sentence(&mut self, sentence: Option<PreparedSentence>) -> Result<(), String> {
        self.pending.push(sentence);
        self.max_doc += 1;
        if self.pending.len() == BLOCK_DOCS {
            self.flush_block()?;
        }
        Ok(())
    }

    fn flush_block(&mut self) -> Result<(), String> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let bytes = encode_block(&self.pending, self.label_w, &self.attr_w)?;
        self.pending.clear();
        self.append_block(bytes)
    }

    fn append_block(&mut self, bytes: Vec<u8>) -> Result<(), String> {
        let next_off = self.block_off.last().copied().unwrap() as u64 + bytes.len() as u64;
        if next_off > u32::MAX as u64 {
            return Err(format!(
                "GPH2 body would exceed the 4 GiB block_off cap ({next_off} bytes)"
            ));
        }
        self.sink.write_all(&bytes).map_err(|e| e.to_string())?;
        self.block_off.push(next_off as u32);
        Ok(())
    }

    /// Flushes any partial trailing block, writes the trailer and end record,
    /// and returns the sink.
    ///
    /// A zero-sentence writer still emits one (empty) block, matching
    /// [`Gph2Writer::encode`]'s `max_doc.div_ceil(BLOCK_DOCS).max(1)`.
    pub fn finish(
        mut self,
        uuid: &str,
        colocated: Vec<String>,
        rel_dict: Vec<String>,
        attr_dicts: Vec<Vec<String>>,
    ) -> Result<W, String> {
        self.flush_block()?;
        if self.max_doc == 0 {
            let bytes = encode_block(&[], self.label_w, &self.attr_w)?;
            self.append_block(bytes)?;
        }
        let n_blocks = (self.block_off.len() - 1) as u32;

        let trailer = Gph2Trailer {
            uuid: normalize_uuid(uuid),
            max_doc: self.max_doc,
            n_blocks,
            label_w: self.label_w,
            colocated,
            attr_w: self.attr_w,
            block_off: self.block_off,
            rel_dict,
            attr_dicts,
        };
        let trailer_bytes = trailer.encode();
        let rec = EndRecord {
            trailer_len: trailer_bytes.len() as u32,
            version: GPH2_VERSION,
            flags: 0,
        };
        self.sink
            .write_all(&trailer_bytes)
            .and_then(|()| self.sink.write_all(&rec.encode()))
            .and_then(|()| self.sink.flush())
            .map_err(|e| e.to_string())?;
        Ok(self.sink)
    }
}

/// Label / attribute frequencies, which decide dictionary order and id widths.
pub struct FreqCounter {
    pub rel_freq: HashMap<String, u64>,
    pub attr_freq: Vec<HashMap<String, u64>>,
}

impl FreqCounter {
    pub fn new(n_attrs: usize) -> Self {
        Self {
            rel_freq: HashMap::new(),
            attr_freq: vec![HashMap::new(); n_attrs],
        }
    }

    pub fn add(&mut self, rec: &SentenceRecord) {
        let split = split_backbone(
            rec.n_tokens as usize,
            &rec.edges,
            rec.basic_heads.as_deref(),
        );
        for r in &split.rel {
            if !r.is_empty() {
                *self.rel_freq.entry(r.clone()).or_insert(0) += 1;
            }
        }
        for (_, _, r) in &split.overlay {
            *self.rel_freq.entry(r.clone()).or_insert(0) += 1;
        }
        for (k, vals) in rec.attrs.iter().enumerate() {
            let Some(freq) = self.attr_freq.get_mut(k) else {
                continue;
            };
            for v in vals {
                if !v.is_empty() {
                    *freq.entry(v.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    /// Frequency-sorted dictionaries and the id widths they need.
    pub fn dictionaries(&self) -> (Vec<String>, Vec<Vec<String>>, u8, Vec<u8>) {
        let rel_dict = freq_sorted_dict(&self.rel_freq, false);
        let attr_dicts: Vec<Vec<String>> = self
            .attr_freq
            .iter()
            .map(|f| freq_sorted_dict(f, true))
            .collect();
        let label_w = if rel_dict.len() > 255 { 2 } else { 1 };
        let attr_w = attr_dicts
            .iter()
            .map(|d| if d.len() > 255 { 2 } else { 1 })
            .collect();
        (rel_dict, attr_dicts, label_w, attr_w)
    }
}

pub struct Gph2Writer;

impl Gph2Writer {
    /// Encode a dense document list (one record per doc id `0..max_doc`).
    pub fn encode(uuid: &str, docs: &[Option<SentenceRecord>]) -> Result<Vec<u8>, String> {
        let colocated = docs
            .iter()
            .find_map(|d| d.as_ref().map(|r| r.attr_names.clone()))
            .unwrap_or_default();

        let mut freq = FreqCounter::new(colocated.len());
        for rec in docs.iter().flatten() {
            freq.add(rec);
        }
        let FreqCounter {
            rel_freq,
            attr_freq,
        } = freq;

        let (rel_dict, attr_dicts, label_w, attr_w) = FreqCounter {
            rel_freq,
            attr_freq,
        }
        .dictionaries();
        let rel_index = DictIndex::build(&rel_dict);
        let attr_indexes: Vec<DictIndex> = attr_dicts.iter().map(|d| DictIndex::build(d)).collect();

        let mut writer = Gph2StreamWriter::new(Vec::new(), label_w, attr_w);
        for rec in docs {
            let prepared = rec
                .as_ref()
                .map(|r| prepare(r, &rel_index, &attr_indexes, &colocated));
            writer.push_sentence(prepared)?;
        }
        writer.finish(uuid, colocated, rel_dict, attr_dicts)
    }

    pub fn write_file(
        path: &Path,
        uuid: &str,
        docs: &[Option<SentenceRecord>],
    ) -> Result<(), String> {
        let bytes = Self::encode(uuid, docs)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let tmp = path.with_extension(format!("gph2.tmp.{}", std::process::id()));
        {
            let mut f = File::create(&tmp).map_err(|e| e.to_string())?;
            f.write_all(&bytes).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
        }
        std::fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn freq_sorted_dict(freq: &HashMap<String, u64>, reserved_none: bool) -> Vec<String> {
    let mut items: Vec<(String, u64)> = freq.iter().map(|(k, v)| (k.clone(), *v)).collect();
    items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut dict = Vec::new();
    if reserved_none {
        dict.push(String::new());
    }
    for (s, _) in items {
        if reserved_none && s.is_empty() {
            continue;
        }
        dict.push(s);
    }
    if dict.is_empty() {
        dict.push(String::new());
    }
    dict
}

pub fn prepare(
    rec: &SentenceRecord,
    rel_index: &DictIndex,
    attr_indexes: &[DictIndex],
    colocated: &[String],
) -> PreparedSentence {
    let split = split_backbone(
        rec.n_tokens as usize,
        &rec.edges,
        rec.basic_heads.as_deref(),
    );
    let rel_ids: Vec<u16> = split
        .rel
        .iter()
        .map(|r| if r.is_empty() { 0 } else { rel_index.get(r) })
        .collect();
    let overlay: Vec<(u32, u32, u16)> = split
        .overlay
        .iter()
        .map(|(g, d, r)| (*g, *d, rel_index.get(r)))
        .collect();
    let mut attr_ids = Vec::with_capacity(colocated.len());
    for (k, name) in colocated.iter().enumerate() {
        let index = attr_indexes.get(k);
        let values = rec
            .attr_names
            .iter()
            .position(|n| n == name)
            .and_then(|i| rec.attrs.get(i));
        let mut ids = vec![0u16; rec.n_tokens as usize];
        if let Some(vals) = values {
            for (i, v) in vals.iter().enumerate() {
                if i < ids.len() && !v.is_empty() {
                    ids[i] = index.map(|idx| idx.get(v)).unwrap_or(0);
                }
            }
        }
        attr_ids.push(ids);
    }
    PreparedSentence {
        n_tokens: rec.n_tokens,
        head: split.head,
        rel_ids,
        attr_ids,
        overlay,
    }
}

fn encode_block(
    docs: &[Option<PreparedSentence>],
    label_w: u8,
    attr_w: &[u8],
) -> Result<Vec<u8>, String> {
    let n_docs = docs.len() as u16;
    let mut n_tok = 0u32;
    let mut n_ov = 0u32;
    let mut max_len = 0u32;
    for d in docs {
        if let Some(s) = d {
            n_tok += s.n_tokens;
            n_ov += s.overlay.len() as u32;
            max_len = max_len.max(s.n_tokens);
        }
    }
    let head_w: u8 = if max_len > 255 { 2 } else { 1 };
    let hdr = BlockHeader {
        n_docs,
        head_w,
        flags: 0,
        n_tok,
        n_ov,
    };
    let mut tok_start = Vec::with_capacity(docs.len() + 1);
    let mut ov_start = Vec::with_capacity(docs.len() + 1);
    tok_start.push(0u32);
    ov_start.push(0u32);
    let mut heads = Vec::new();
    let mut rels = Vec::new();
    let mut attrs: Vec<Vec<u16>> = vec![Vec::new(); attr_w.len()];
    let mut ov_gov = Vec::new();
    let mut ov_dep = Vec::new();
    let mut ov_rel = Vec::new();
    for d in docs {
        match d {
            Some(s) => {
                for i in 0..s.n_tokens as usize {
                    heads.push(encode_head(s.head.get(i).copied().unwrap_or(ROOT), head_w));
                    rels.push(s.rel_ids.get(i).copied().unwrap_or(0));
                    for (k, col) in s.attr_ids.iter().enumerate() {
                        attrs[k].push(col.get(i).copied().unwrap_or(0));
                    }
                }
                let mut ov = s.overlay.clone();
                ov.sort_by(|a, b| a.1.cmp(&b.1).then(a.0.cmp(&b.0)).then(a.2.cmp(&b.2)));
                for (g, dep, r) in ov {
                    ov_gov.push(g);
                    ov_dep.push(dep);
                    ov_rel.push(r);
                }
                tok_start.push(tok_start.last().copied().unwrap() + s.n_tokens);
                ov_start.push(ov_start.last().copied().unwrap() + s.overlay.len() as u32);
            }
            None => {
                tok_start.push(*tok_start.last().unwrap());
                ov_start.push(*ov_start.last().unwrap());
            }
        }
    }
    let mut out = Vec::new();
    out.extend_from_slice(&hdr.encode());
    for v in &tok_start {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for v in &ov_start {
        out.extend_from_slice(&v.to_le_bytes());
    }
    for h in &heads {
        write_width(&mut out, *h, head_w);
    }
    for r in &rels {
        write_width(&mut out, *r as u32, label_w);
    }
    for (k, col) in attrs.iter().enumerate() {
        let w = attr_w.get(k).copied().unwrap_or(1);
        for v in col {
            write_width(&mut out, *v as u32, w);
        }
    }
    for g in &ov_gov {
        write_width(&mut out, *g, head_w);
    }
    for d in &ov_dep {
        write_width(&mut out, *d, head_w);
    }
    for r in &ov_rel {
        write_width(&mut out, *r as u32, label_w);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Rebuild the pre-streaming `encode` exactly (dense `PreparedSentence`
    /// slice, one call to `encode_block` per full block) so its output can be
    /// compared byte-for-byte against `Gph2StreamWriter`.
    fn encode_dense(uuid: &str, docs: &[Option<SentenceRecord>]) -> Result<Vec<u8>, String> {
        let max_doc = docs.len() as u32;
        let colocated = docs
            .iter()
            .find_map(|d| d.as_ref().map(|r| r.attr_names.clone()))
            .unwrap_or_default();

        let mut rel_freq: HashMap<String, u64> = HashMap::new();
        let mut attr_freq: Vec<HashMap<String, u64>> = vec![HashMap::new(); colocated.len()];
        for rec in docs.iter().flatten() {
            let split = split_backbone(
                rec.n_tokens as usize,
                &rec.edges,
                rec.basic_heads.as_deref(),
            );
            for r in &split.rel {
                if !r.is_empty() {
                    *rel_freq.entry(r.clone()).or_insert(0) += 1;
                }
            }
            for (_, _, r) in &split.overlay {
                *rel_freq.entry(r.clone()).or_insert(0) += 1;
            }
            for (k, vals) in rec.attrs.iter().enumerate() {
                if k >= attr_freq.len() {
                    continue;
                }
                for v in vals {
                    if !v.is_empty() {
                        *attr_freq[k].entry(v.clone()).or_insert(0) += 1;
                    }
                }
            }
        }

        let rel_dict = freq_sorted_dict(&rel_freq, false);
        let attr_dicts: Vec<Vec<String>> = attr_freq
            .iter()
            .map(|f| freq_sorted_dict(f, true))
            .collect();
        let label_w: u8 = if rel_dict.len() > 255 { 2 } else { 1 };
        let attr_w: Vec<u8> = attr_dicts
            .iter()
            .map(|d| if d.len() > 255 { 2 } else { 1 })
            .collect();

        let rel_index = DictIndex::build(&rel_dict);
        let attr_indexes: Vec<DictIndex> = attr_dicts.iter().map(|d| DictIndex::build(d)).collect();

        let mut prepared: Vec<Option<PreparedSentence>> = Vec::with_capacity(docs.len());
        for rec in docs {
            prepared.push(
                rec.as_ref()
                    .map(|r| prepare(r, &rel_index, &attr_indexes, &colocated)),
            );
        }

        let n_blocks = max_doc.div_ceil(BLOCK_DOCS as u32).max(1);
        let mut blocks = Vec::new();
        let mut block_off = vec![0u32];
        for b in 0..n_blocks {
            let start = (b as usize) * BLOCK_DOCS;
            let end = ((b as usize + 1) * BLOCK_DOCS).min(docs.len());
            let slice = if start < prepared.len() {
                &prepared[start..end]
            } else {
                &[]
            };
            let bytes = encode_block(slice, label_w, &attr_w)?;
            block_off.push(block_off.last().copied().unwrap() + bytes.len() as u32);
            blocks.extend_from_slice(&bytes);
        }

        let trailer = Gph2Trailer {
            uuid: normalize_uuid(uuid),
            max_doc,
            n_blocks,
            label_w,
            colocated,
            attr_w,
            block_off,
            rel_dict,
            attr_dicts,
        };
        let trailer_bytes = trailer.encode();
        let rec = EndRecord {
            trailer_len: trailer_bytes.len() as u32,
            version: GPH2_VERSION,
            flags: 0,
        };
        let mut out = blocks;
        out.extend_from_slice(&trailer_bytes);
        out.extend_from_slice(&rec.encode());
        Ok(out)
    }

    fn arb_docs(max_n: usize) -> impl Strategy<Value = Vec<Option<SentenceRecord>>> {
        prop::collection::vec(
            prop::option::of((1u32..6, prop::collection::vec("[a-z]{1,4}", 0..3))),
            0..max_n,
        )
        .prop_map(|opts| {
            opts.into_iter()
                .map(|opt| {
                    opt.map(|(n_tok, labels)| {
                        let mut rec = SentenceRecord::new(n_tok);
                        let edges: Vec<(u32, u32, String)> = labels
                            .into_iter()
                            .enumerate()
                            .map(|(i, lab)| {
                                let dep = (i as u32 + 1) % n_tok;
                                let gov = i as u32 % n_tok;
                                (gov, dep, lab)
                            })
                            .collect();
                        rec.set_edges(edges);
                        rec.set_attr("tag", vec!["NN".to_string(); n_tok as usize]);
                        rec
                    })
                })
                .collect()
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]

        #[test]
        fn stream_writer_matches_dense_encode(docs in arb_docs(300)) {
            let uuid = "0123456789abcdef0123456789abcdef";
            let streamed = Gph2Writer::encode(uuid, &docs).unwrap();
            let dense = encode_dense(uuid, &docs).unwrap();
            prop_assert_eq!(streamed, dense);
        }
    }

    #[test]
    fn stream_writer_matches_dense_encode_empty() {
        let uuid = "0123456789abcdef0123456789abcdef";
        let docs: Vec<Option<SentenceRecord>> = Vec::new();
        assert_eq!(
            Gph2Writer::encode(uuid, &docs).unwrap(),
            encode_dense(uuid, &docs).unwrap()
        );
    }

    #[test]
    fn stream_writer_matches_dense_encode_exact_block_boundary() {
        let uuid = "0123456789abcdef0123456789abcdef";
        let mut docs = Vec::new();
        for _ in 0..BLOCK_DOCS * 2 {
            let mut rec = SentenceRecord::new(3);
            rec.set_edges(vec![(1, 0, "nsubj".to_string())]);
            docs.push(Some(rec));
        }
        assert_eq!(
            Gph2Writer::encode(uuid, &docs).unwrap(),
            encode_dense(uuid, &docs).unwrap()
        );
    }
}
