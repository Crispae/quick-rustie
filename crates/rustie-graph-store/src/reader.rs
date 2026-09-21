//! GPH2 trailer and block cursors over a byte buffer.

use crate::format::{
    BLOCK_DOCS, BLOCK_HDR_LEN, BlockHeader, END_RECORD_LEN, Gph2Trailer, decode_head,
    trailer_from_file_bytes,
};
use crate::view::{SentenceScratch, SentenceView};
use bytes::Bytes as OwnedBytes;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct Gph2File<'a> {
    bytes: &'a [u8],
    pub trailer: Arc<Gph2Trailer>,
}

impl<'a> Gph2File<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Self, String> {
        let (trailer, _) = trailer_from_file_bytes(bytes)?;
        Ok(Self {
            bytes,
            trailer: Arc::new(trailer),
        })
    }

    pub fn block_bytes(&self, block_idx: usize) -> Result<&'a [u8], String> {
        let off = self.trailer.block_off.get(block_idx).copied();
        let end = self.trailer.block_off.get(block_idx + 1).copied();
        match (off, end) {
            (Some(s), Some(e)) if (e as usize) <= self.bytes.len() && s <= e => {
                Ok(&self.bytes[s as usize..e as usize])
            }
            _ => Err(format!("block {block_idx} out of range")),
        }
    }

    pub fn block(&self, block_idx: usize) -> Result<Gph2Block, String> {
        Gph2Block::parse(
            OwnedBytes::copy_from_slice(self.block_bytes(block_idx)?),
            &self.trailer,
        )
    }

    pub fn block_for_doc(&self, doc_id: u32) -> Result<Gph2Block, String> {
        if doc_id >= self.trailer.max_doc {
            return Err(format!("doc {doc_id} >= max_doc {}", self.trailer.max_doc));
        }
        self.block((doc_id as usize) / BLOCK_DOCS)
    }

    pub fn sentence<'s>(
        &self,
        doc_id: u32,
        scratch: &'s mut SentenceScratch,
    ) -> Result<SentenceView<'s>, String> {
        let block = self.block_for_doc(doc_id)?;
        let local = (doc_id as usize) % BLOCK_DOCS;
        block.sentence(local, scratch)
    }

    pub fn file_len(&self) -> usize {
        self.bytes.len()
    }

    pub fn trailer_range(&self) -> std::ops::Range<u64> {
        let end = self.bytes.len() as u64 - END_RECORD_LEN as u64;
        let start = end - self.trailer.encode().len() as u64;
        start..end
    }
}

/// One sentence's raw dict ids, undecoded against any particular dictionary.
/// See [`Gph2Block::raw_columns`].
#[derive(Debug, Clone)]
pub(crate) struct RawSentenceColumns {
    pub n_tokens: u32,
    /// Absolute, ROOT-aware head per token (already `decode_head`-resolved).
    pub head: Vec<u32>,
    /// Raw `rel_dict` ids, one per token, in this block's source dictionary.
    pub rel_ids: Vec<u16>,
    /// Raw `attr_dicts[k]` ids, one column per colocated attr field.
    pub attr_ids: Vec<Vec<u16>>,
    /// Overlay edges as (gov token idx, dep token idx, raw rel id).
    pub overlay: Vec<(u32, u32, u16)>,
}

/// One GPH2 block. Only the two start tables are decoded when the block is
/// opened; a sentence's columns are decoded straight from the raw bytes, so
/// visiting `k` of a block's sentences costs `k` sentence decodes, not 128.
#[derive(Debug, Clone)]
pub struct Gph2Block {
    bytes: OwnedBytes,
    pub hdr: BlockHeader,
    tok_start: Vec<u32>,
    ov_start: Vec<u32>,
    head_off: usize,
    rel_off: usize,
    attr_off: Vec<usize>,
    ov_gov_off: usize,
    ov_dep_off: usize,
    ov_rel_off: usize,
    trailer: Arc<Gph2Trailer>,
}

fn read_u32s(bytes: &[u8], off: &mut usize, count: usize, what: &str) -> Result<Vec<u32>, String> {
    let end = *off + count * 4;
    if end > bytes.len() {
        return Err(format!("{what} truncated"));
    }
    let v = bytes[*off..end]
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes(c.try_into().unwrap()))
        .collect();
    *off = end;
    Ok(v)
}

#[inline]
fn width_at(bytes: &[u8], base: usize, width: u8, i: usize) -> u32 {
    match width {
        1 => bytes[base + i] as u32,
        _ => {
            let o = base + i * 2;
            u16::from_le_bytes([bytes[o], bytes[o + 1]]) as u32
        }
    }
}

impl Gph2Block {
    pub fn parse(bytes: OwnedBytes, trailer: &Arc<Gph2Trailer>) -> Result<Self, String> {
        let raw = bytes.as_ref();
        let hdr = BlockHeader::decode(raw)?;
        for w in std::iter::once(hdr.head_w)
            .chain(std::iter::once(trailer.label_w))
            .chain(trailer.attr_w.iter().copied())
        {
            if w != 1 && w != 2 {
                return Err(format!("unsupported width {w}"));
            }
        }
        let n_docs = hdr.n_docs as usize;
        let n_tok = hdr.n_tok as usize;
        let n_ov = hdr.n_ov as usize;
        let mut off = BLOCK_HDR_LEN;
        let tok_start = read_u32s(raw, &mut off, n_docs + 1, "tok_start")?;
        let ov_start = read_u32s(raw, &mut off, n_docs + 1, "ov_start")?;
        let head_off = off;
        off += n_tok * hdr.head_w as usize;
        let rel_off = off;
        off += n_tok * trailer.label_w as usize;
        let mut attr_off = Vec::with_capacity(trailer.attr_w.len());
        for &w in &trailer.attr_w {
            attr_off.push(off);
            off += n_tok * w as usize;
        }
        let ov_gov_off = off;
        off += n_ov * hdr.head_w as usize;
        let ov_dep_off = off;
        off += n_ov * hdr.head_w as usize;
        let ov_rel_off = off;
        off += n_ov * trailer.label_w as usize;
        if off > raw.len() {
            return Err("block columns truncated".into());
        }
        if tok_start.last().copied().unwrap_or(0) as usize > n_tok
            || ov_start.last().copied().unwrap_or(0) as usize > n_ov
        {
            return Err("block start table exceeds column length".into());
        }
        Ok(Self {
            bytes,
            hdr,
            tok_start,
            ov_start,
            head_off,
            rel_off,
            attr_off,
            ov_gov_off,
            ov_dep_off,
            ov_rel_off,
            trailer: trailer.clone(),
        })
    }

    /// Decode sentence `local_doc`'s raw columns without resolving dict ids to
    /// strings or building a CSR view.
    ///
    /// Used by segment-merge column transcode (`crate::merge`), which
    /// must remap ids into a union dictionary — the normal `sentence` path
    /// resolves ids to strings eagerly via [`SentenceScratch`], which throws
    /// away exactly the information a remap needs.
    pub(crate) fn raw_columns(&self, local_doc: usize) -> Result<RawSentenceColumns, String> {
        if local_doc + 1 >= self.tok_start.len() {
            return Err("local doc out of block".into());
        }
        let ts = self.tok_start[local_doc] as usize;
        let te = self.tok_start[local_doc + 1] as usize;
        let os = self.ov_start[local_doc] as usize;
        let oe = self.ov_start[local_doc + 1] as usize;
        if te < ts || oe < os {
            return Err("non-monotonic block start table".into());
        }
        let raw = self.bytes.as_ref();
        let (hw, lw) = (self.hdr.head_w, self.trailer.label_w);
        let mut head = Vec::with_capacity(te - ts);
        let mut rel_ids = Vec::with_capacity(te - ts);
        for i in ts..te {
            head.push(decode_head(width_at(raw, self.head_off, hw, i), hw));
            rel_ids.push(width_at(raw, self.rel_off, lw, i) as u16);
        }
        let mut attr_ids: Vec<Vec<u16>> = Vec::with_capacity(self.attr_off.len());
        for (k, &base) in self.attr_off.iter().enumerate() {
            let w = self.trailer.attr_w[k];
            attr_ids.push((ts..te).map(|i| width_at(raw, base, w, i) as u16).collect());
        }
        // Overlay gov/dep are plain token indices, not ROOT-aware heads
        // (mirrors `sentence`, which reads them with plain `width_at` too).
        let mut overlay = Vec::with_capacity(oe - os);
        for i in os..oe {
            overlay.push((
                width_at(raw, self.ov_gov_off, hw, i),
                width_at(raw, self.ov_dep_off, hw, i),
                width_at(raw, self.ov_rel_off, lw, i) as u16,
            ));
        }
        Ok(RawSentenceColumns {
            n_tokens: (te - ts) as u32,
            head,
            rel_ids,
            attr_ids,
            overlay,
        })
    }

    /// Decode sentence `local_doc` into `scratch` and return its view.
    pub fn sentence<'s>(
        &self,
        local_doc: usize,
        scratch: &'s mut SentenceScratch,
    ) -> Result<SentenceView<'s>, String> {
        if local_doc + 1 >= self.tok_start.len() {
            return Err("local doc out of block".into());
        }
        let ts = self.tok_start[local_doc] as usize;
        let te = self.tok_start[local_doc + 1] as usize;
        let os = self.ov_start[local_doc] as usize;
        let oe = self.ov_start[local_doc + 1] as usize;
        if te < ts || oe < os {
            return Err("non-monotonic block start table".into());
        }
        let raw = self.bytes.as_ref();
        let (hw, lw) = (self.hdr.head_w, self.trailer.label_w);
        scratch.set_dicts(&self.trailer);
        scratch.begin(te - ts, self.attr_off.len());
        for i in ts..te {
            scratch
                .head
                .push(decode_head(width_at(raw, self.head_off, hw, i), hw));
            scratch.rel.push(width_at(raw, self.rel_off, lw, i) as u16);
        }
        for (k, col) in scratch.attr_ids.iter_mut().enumerate() {
            let (base, w) = (self.attr_off[k], self.trailer.attr_w[k]);
            col.extend((ts..te).map(|i| width_at(raw, base, w, i) as u16));
        }
        for i in os..oe {
            scratch.ov.push((
                width_at(raw, self.ov_gov_off, hw, i),
                width_at(raw, self.ov_dep_off, hw, i),
                width_at(raw, self.ov_rel_off, lw, i) as u16,
            ));
        }
        scratch.build_csr();
        Ok(scratch.view())
    }
}
