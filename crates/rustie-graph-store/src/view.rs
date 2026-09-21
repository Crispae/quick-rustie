//! Borrowed per-sentence view with reusable CSR scratch.

use crate::backbone::ROOT;
use crate::format::Gph2Trailer;
use std::sync::Arc;

/// Reusable buffers for one sentence's CSR (backbone ∪ overlay). The segment
/// dictionaries are shared through an `Arc`, never copied per sentence.
#[derive(Debug)]
pub struct SentenceScratch {
    pub(crate) n_tokens: usize,
    pub(crate) head: Vec<u32>,
    pub(crate) rel: Vec<u16>,
    pub(crate) attr_ids: Vec<Vec<u16>>,
    /// Overlay edges of the current sentence, as (gov, dep, rel id).
    pub(crate) ov: Vec<(u32, u32, u16)>,
    dicts: Arc<Gph2Trailer>,
    edges: Vec<(u32, u32, u16)>,
    out_off: Vec<u32>,
    out_to: Vec<u32>,
    out_rel: Vec<u16>,
    in_off: Vec<u32>,
    in_from: Vec<u32>,
    in_rel: Vec<u16>,
    out_cur: Vec<u32>,
    in_cur: Vec<u32>,
}

impl Default for SentenceScratch {
    fn default() -> Self {
        Self {
            n_tokens: 0,
            head: Vec::new(),
            rel: Vec::new(),
            attr_ids: Vec::new(),
            ov: Vec::new(),
            dicts: Arc::new(Gph2Trailer::empty()),
            edges: Vec::new(),
            out_off: Vec::new(),
            out_to: Vec::new(),
            out_rel: Vec::new(),
            in_off: Vec::new(),
            in_from: Vec::new(),
            in_rel: Vec::new(),
            out_cur: Vec::new(),
            in_cur: Vec::new(),
        }
    }
}

impl SentenceScratch {
    /// Point the scratch at a segment's dictionaries (refcount bump only).
    pub(crate) fn set_dicts(&mut self, dicts: &Arc<Gph2Trailer>) {
        if !Arc::ptr_eq(&self.dicts, dicts) {
            self.dicts = dicts.clone();
        }
    }

    /// Prepare the per-token columns for a sentence of `n` tokens.
    pub(crate) fn begin(&mut self, n: usize, n_attr_cols: usize) {
        self.n_tokens = n;
        self.head.clear();
        self.rel.clear();
        self.ov.clear();
        if self.attr_ids.len() < n_attr_cols {
            self.attr_ids.resize_with(n_attr_cols, Vec::new);
        }
        self.attr_ids.truncate(n_attr_cols);
        for c in &mut self.attr_ids {
            c.clear();
        }
    }

    /// Build a scratch from explicit columns and dictionaries (tests, tools).
    #[allow(clippy::too_many_arguments)]
    pub fn rebuild(
        &mut self,
        n: usize,
        head: &[u32],
        rel: &[u16],
        rel_dict: &[String],
        attr_cols: &[&[u16]],
        attr_dicts: &[Vec<String>],
        colocated: &[String],
        ov_gov: &[u32],
        ov_dep: &[u32],
        ov_rel: &[u16],
    ) {
        let mut dicts = Gph2Trailer::empty();
        dicts.rel_dict = rel_dict.to_vec();
        dicts.attr_dicts = attr_dicts.to_vec();
        dicts.colocated = colocated.to_vec();
        self.dicts = Arc::new(dicts);
        self.begin(n, attr_cols.len());
        self.head.extend_from_slice(head);
        self.rel.extend_from_slice(rel);
        for (dst, col) in self.attr_ids.iter_mut().zip(attr_cols) {
            dst.extend_from_slice(col);
        }
        for i in 0..ov_gov.len() {
            self.ov
                .push((ov_gov[i], ov_dep[i], ov_rel.get(i).copied().unwrap_or(0)));
        }
        self.build_csr();
    }

    /// Build the CSR of E = backbone ∪ overlay from `head`, `rel` and `ov`.
    pub(crate) fn build_csr(&mut self) {
        let n = self.n_tokens;
        self.edges.clear();
        for d in 0..n {
            let g = self.head.get(d).copied().unwrap_or(ROOT);
            if g != ROOT && (g as usize) < n {
                self.edges
                    .push((g, d as u32, self.rel.get(d).copied().unwrap_or(0)));
            }
        }
        self.edges.extend_from_slice(&self.ov);

        self.out_off.clear();
        self.out_off.resize(n + 1, 0);
        self.in_off.clear();
        self.in_off.resize(n + 1, 0);
        for &(g, d, _) in &self.edges {
            if (g as usize) < n {
                self.out_off[g as usize + 1] += 1;
            }
            if (d as usize) < n {
                self.in_off[d as usize + 1] += 1;
            }
        }
        for i in 0..n {
            self.out_off[i + 1] += self.out_off[i];
            self.in_off[i + 1] += self.in_off[i];
        }
        let n_out = self.out_off[n] as usize;
        let n_in = self.in_off[n] as usize;
        self.out_to.clear();
        self.out_to.resize(n_out, 0);
        self.out_rel.clear();
        self.out_rel.resize(n_out, 0);
        self.in_from.clear();
        self.in_from.resize(n_in, 0);
        self.in_rel.clear();
        self.in_rel.resize(n_in, 0);
        self.out_cur.clear();
        self.out_cur.extend_from_slice(&self.out_off[..n]);
        self.in_cur.clear();
        self.in_cur.extend_from_slice(&self.in_off[..n]);
        for &(g, d, r) in &self.edges {
            if (g as usize) < n {
                let i = self.out_cur[g as usize] as usize;
                self.out_to[i] = d;
                self.out_rel[i] = r;
                self.out_cur[g as usize] += 1;
            }
            if (d as usize) < n {
                let i = self.in_cur[d as usize] as usize;
                self.in_from[i] = g;
                self.in_rel[i] = r;
                self.in_cur[d as usize] += 1;
            }
        }
    }

    pub fn view(&self) -> SentenceView<'_> {
        SentenceView {
            n_tokens: self.n_tokens,
            head: &self.head,
            rel: &self.rel,
            rel_dict: &self.dicts.rel_dict,
            attr_ids: &self.attr_ids,
            attr_dicts: &self.dicts.attr_dicts,
            colocated: &self.dicts.colocated,
            out_off: &self.out_off,
            out_to: &self.out_to,
            out_rel: &self.out_rel,
            in_off: &self.in_off,
            in_from: &self.in_from,
            in_rel: &self.in_rel,
        }
    }
}

/// Borrowed sentence graph: backbone columns + CSR of E = backbone ∪ overlay.
#[derive(Debug, Clone, Copy)]
pub struct SentenceView<'a> {
    pub n_tokens: usize,
    pub head: &'a [u32],
    pub rel: &'a [u16],
    pub rel_dict: &'a [String],
    pub attr_ids: &'a [Vec<u16>],
    pub attr_dicts: &'a [Vec<String>],
    pub colocated: &'a [String],
    pub out_off: &'a [u32],
    pub out_to: &'a [u32],
    pub out_rel: &'a [u16],
    pub in_off: &'a [u32],
    pub in_from: &'a [u32],
    pub in_rel: &'a [u16],
}

impl<'a> SentenceView<'a> {
    pub fn label(&self, id: u16) -> &str {
        self.rel_dict
            .get(id as usize)
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    pub fn attr(&self, field: &str, tok: usize) -> &str {
        let Some(k) = self.colocated.iter().position(|n| n == field) else {
            return "";
        };
        let id = self
            .attr_ids
            .get(k)
            .and_then(|c| c.get(tok))
            .copied()
            .unwrap_or(0);
        self.attr_dicts
            .get(k)
            .and_then(|d| d.get(id as usize))
            .map(|s| s.as_str())
            .unwrap_or("")
    }

    pub fn attr_id(&self, field_idx: usize, tok: usize) -> u16 {
        self.attr_ids
            .get(field_idx)
            .and_then(|c| c.get(tok))
            .copied()
            .unwrap_or(0)
    }

    pub fn outgoing(&self, u: usize) -> impl Iterator<Item = (u32, u16)> + '_ {
        let s = *self.out_off.get(u).unwrap_or(&0) as usize;
        let e = *self.out_off.get(u + 1).unwrap_or(&(s as u32)) as usize;
        self.out_to[s..e]
            .iter()
            .zip(self.out_rel[s..e].iter())
            .map(|(t, r)| (*t, *r))
    }

    pub fn incoming(&self, u: usize) -> impl Iterator<Item = (u32, u16)> + '_ {
        let s = *self.in_off.get(u).unwrap_or(&0) as usize;
        let e = *self.in_off.get(u + 1).unwrap_or(&(s as u32)) as usize;
        self.in_from[s..e]
            .iter()
            .zip(self.in_rel[s..e].iter())
            .map(|(t, r)| (*t, *r))
    }

    /// All E edges as (gov, dep, label).
    pub fn edges(&self) -> Vec<(u32, u32, &str)> {
        let mut out = Vec::new();
        for u in 0..self.n_tokens {
            for (to, rid) in self.outgoing(u) {
                out.push((u as u32, to, self.label(rid)));
            }
        }
        out
    }

    /// Backbone children of `u` (outgoing along `head[d] == u`).
    pub fn backbone_children(&self, u: u32) -> impl Iterator<Item = usize> + '_ {
        (0..self.n_tokens).filter(move |&d| self.head.get(d).copied() == Some(u))
    }

    /// O(n) DFS over the backbone subtree rooted at `root`.
    pub fn subtree(&self, root: usize) -> Vec<usize> {
        let mut out = Vec::new();
        let mut stack = vec![root];
        let mut seen = vec![false; self.n_tokens];
        while let Some(u) = stack.pop() {
            if u >= self.n_tokens || seen[u] {
                continue;
            }
            seen[u] = true;
            out.push(u);
            for c in self.backbone_children(u as u32) {
                stack.push(c);
            }
        }
        out.sort_unstable();
        out
    }

    pub fn backbone_roots(&self) -> Vec<usize> {
        (0..self.n_tokens)
            .filter(|&i| self.head.get(i).copied().unwrap_or(ROOT) == ROOT)
            .collect()
    }
}
