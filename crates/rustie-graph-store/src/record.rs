//! Per-sentence graph payload stored in the indexing spool.

use std::collections::BTreeSet;
use std::io::{Cursor, Read};

/// One sentence's OpenIE graph plus optional colocated token attributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SentenceRecord {
    pub n_tokens: u32,
    /// Deduplicated (governor, dependent, label) triples. Governor may equal
    /// dependent (self-loop). `u32::MAX` is not used here; ROOT is a backbone
    /// concept only.
    pub edges: Vec<(u32, u32, String)>,
    /// Per-token basic-tree head (`u32::MAX` = ROOT) when a basic graph was
    /// supplied. Length equals `n_tokens`.
    pub basic_heads: Option<Vec<u32>>,
    /// Colocated fields in schema order. Each inner vec is per-token; empty
    /// string means missing (encoded as dictionary id 0 / NONE).
    pub attr_names: Vec<String>,
    pub attrs: Vec<Vec<String>>,
}

impl SentenceRecord {
    pub fn new(n_tokens: u32) -> Self {
        Self {
            n_tokens,
            edges: Vec::new(),
            basic_heads: None,
            attr_names: Vec::new(),
            attrs: Vec::new(),
        }
    }

    pub fn set_edges(&mut self, edges: Vec<(u32, u32, String)>) {
        let mut seen = BTreeSet::new();
        self.edges = edges
            .into_iter()
            .filter(|(g, d, r)| seen.insert((*g, *d, r.clone())))
            .collect();
    }

    pub fn set_basic_heads(&mut self, heads: Vec<u32>) {
        debug_assert_eq!(heads.len(), self.n_tokens as usize);
        self.basic_heads = Some(heads);
    }

    pub fn set_attr(&mut self, name: impl Into<String>, values: Vec<String>) {
        debug_assert_eq!(values.len(), self.n_tokens as usize);
        self.attr_names.push(name.into());
        self.attrs.push(values);
    }

    /// Compact spool encoding.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&self.n_tokens.to_le_bytes());
        out.extend_from_slice(&(self.edges.len() as u32).to_le_bytes());
        let n_basic = if self.basic_heads.is_some() {
            self.n_tokens
        } else {
            0
        };
        out.extend_from_slice(&n_basic.to_le_bytes());
        out.push(self.attr_names.len() as u8);
        for (gov, dep, rel) in &self.edges {
            out.extend_from_slice(&gov.to_le_bytes());
            out.extend_from_slice(&dep.to_le_bytes());
            write_str(&mut out, rel);
        }
        if let Some(heads) = &self.basic_heads {
            for h in heads {
                out.extend_from_slice(&h.to_le_bytes());
            }
        }
        for (name, values) in self.attr_names.iter().zip(self.attrs.iter()) {
            write_str(&mut out, name);
            for v in values {
                write_str(&mut out, v);
            }
        }
        out
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let mut cur = Cursor::new(bytes);
        let n_tokens = read_u32(&mut cur)?;
        let n_edges = read_u32(&mut cur)? as usize;
        let n_basic = read_u32(&mut cur)?;
        let n_attr = read_u8(&mut cur)? as usize;
        let mut edges = Vec::with_capacity(n_edges);
        for _ in 0..n_edges {
            let gov = read_u32(&mut cur)?;
            let dep = read_u32(&mut cur)?;
            let rel = read_str(&mut cur)?;
            edges.push((gov, dep, rel));
        }
        let basic_heads = if n_basic == 0 {
            None
        } else {
            if n_basic != n_tokens {
                return Err("basic head count != n_tokens".into());
            }
            let mut heads = Vec::with_capacity(n_tokens as usize);
            for _ in 0..n_tokens {
                heads.push(read_u32(&mut cur)?);
            }
            Some(heads)
        };
        let mut attr_names = Vec::with_capacity(n_attr);
        let mut attrs = Vec::with_capacity(n_attr);
        for _ in 0..n_attr {
            attr_names.push(read_str(&mut cur)?);
            let mut values = Vec::with_capacity(n_tokens as usize);
            for _ in 0..n_tokens {
                values.push(read_str(&mut cur)?);
            }
            attrs.push(values);
        }
        Ok(Self {
            n_tokens,
            edges,
            basic_heads,
            attr_names,
            attrs,
        })
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    let bytes = s.as_bytes();
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn read_u8(cur: &mut Cursor<&[u8]>) -> Result<u8, String> {
    let mut buf = [0u8; 1];
    cur.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf[0])
}

fn read_u32(cur: &mut Cursor<&[u8]>) -> Result<u32, String> {
    let mut buf = [0u8; 4];
    cur.read_exact(&mut buf).map_err(|e| e.to_string())?;
    Ok(u32::from_le_bytes(buf))
}

fn read_str(cur: &mut Cursor<&[u8]>) -> Result<String, String> {
    let mut len_buf = [0u8; 2];
    cur.read_exact(&mut len_buf).map_err(|e| e.to_string())?;
    let len = u16::from_le_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    cur.read_exact(&mut buf).map_err(|e| e.to_string())?;
    String::from_utf8(buf).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_empty() {
        let rec = SentenceRecord::new(0);
        let bytes = rec.to_bytes();
        assert_eq!(SentenceRecord::from_bytes(&bytes).unwrap(), rec);
    }

    #[test]
    fn roundtrip_with_attrs_and_basic() {
        let mut rec = SentenceRecord::new(3);
        rec.set_edges(vec![(1, 0, "nsubj".into()), (1, 2, "dobj".into())]);
        rec.set_basic_heads(vec![1, u32::MAX, 1]);
        rec.set_attr("tag", vec!["NN".into(), "VBZ".into(), "NN".into()]);
        let bytes = rec.to_bytes();
        assert_eq!(SentenceRecord::from_bytes(&bytes).unwrap(), rec);
    }

    #[test]
    fn dedups_edges() {
        let mut rec = SentenceRecord::new(2);
        rec.set_edges(vec![
            (0, 1, "amod".into()),
            (0, 1, "amod".into()),
            (1, 0, "nsubj".into()),
        ]);
        assert_eq!(rec.edges.len(), 2);
    }
}
