//! Thompson NFA over directed, labeled graph hops.

use crate::graph::dict::LabelSet;
use crate::matching::node_test::full_match;
use crate::matching::tokenset::TokenSet;
use regex::Regex;
use rustie_graph_store::SentenceView;
use std::collections::{HashSet, VecDeque};

/// Edge direction relative to the current node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Dir {
    Out,
    In,
}

/// Label test on a single hop.
#[derive(Debug, Clone)]
pub enum LabelPred {
    Any,
    Exact(String),
    Alt(Vec<String>),
    Regex(Regex),
}

impl PartialEq for LabelPred {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Any, Self::Any) => true,
            (Self::Exact(a), Self::Exact(b)) => a == b,
            (Self::Alt(a), Self::Alt(b)) => a == b,
            (Self::Regex(a), Self::Regex(b)) => a.as_str() == b.as_str(),
            _ => false,
        }
    }
}

impl Eq for LabelPred {}

impl LabelPred {
    pub fn matches_str(&self, label: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(s) => s == label,
            Self::Alt(v) => v.iter().any(|s| s == label),
            Self::Regex(re) => full_match(re, label),
        }
    }

    pub fn is_any(&self) -> bool {
        matches!(self, Self::Any)
    }
}

#[derive(Debug, Clone)]
struct Consume {
    dir: Dir,
    pred: LabelPred,
    to: usize,
}

/// Thompson NFA: ε-closure plus consume-transitions `(dir, pred)`.
#[derive(Debug, Clone)]
pub struct HopNfa {
    n_states: usize,
    start: usize,
    accept: usize,
    eps: Vec<Vec<usize>>,
    consume: Vec<Vec<Consume>>,
    /// True if the empty path is accepted (nullable).
    pub nullable: bool,
}

impl HopNfa {
    pub fn n_states(&self) -> usize {
        self.n_states
    }

    pub fn start(&self) -> usize {
        self.start
    }

    pub fn accept(&self) -> usize {
        self.accept
    }

    fn new_empty() -> Self {
        Self {
            n_states: 1,
            start: 0,
            accept: 0,
            eps: vec![Vec::new()],
            consume: vec![Vec::new()],
            nullable: true,
        }
    }

    fn alloc(&mut self) -> usize {
        let id = self.n_states;
        self.n_states += 1;
        self.eps.push(Vec::new());
        self.consume.push(Vec::new());
        id
    }

    /// Single labeled (or wildcard) hop.
    pub fn atom(dir: Dir, pred: LabelPred) -> Self {
        let mut n = Self {
            n_states: 0,
            start: 0,
            accept: 0,
            eps: Vec::new(),
            consume: Vec::new(),
            nullable: false,
        };
        let s = n.alloc();
        let a = n.alloc();
        n.start = s;
        n.accept = a;
        n.consume[s].push(Consume { dir, pred, to: a });
        n
    }

    pub fn concat(mut self, other: Self) -> Self {
        let shift = self.n_states;
        self.n_states += other.n_states;
        self.eps.extend(
            other
                .eps
                .into_iter()
                .map(|v| v.into_iter().map(|x| x + shift).collect::<Vec<_>>()),
        );
        self.consume.extend(other.consume.into_iter().map(|v| {
            v.into_iter()
                .map(|c| Consume {
                    dir: c.dir,
                    pred: c.pred,
                    to: c.to + shift,
                })
                .collect::<Vec<_>>()
        }));
        self.eps[self.accept].push(other.start + shift);
        self.accept = other.accept + shift;
        self.nullable = self.nullable && other.nullable;
        self
    }

    pub fn optional(mut self) -> Self {
        let s = self.alloc();
        let a = self.alloc();
        self.eps[s].push(self.start);
        self.eps[s].push(a);
        self.eps[self.accept].push(a);
        self.start = s;
        self.accept = a;
        self.nullable = true;
        self
    }

    pub fn star(mut self) -> Self {
        let s = self.alloc();
        let a = self.alloc();
        self.eps[s].push(self.start);
        self.eps[s].push(a);
        self.eps[self.accept].push(self.start);
        self.eps[self.accept].push(a);
        self.start = s;
        self.accept = a;
        self.nullable = true;
        self
    }

    pub fn plus(self) -> Self {
        let base2 = self.clone().star();
        self.concat(base2)
    }

    /// Thompson union: `self | other`.
    pub fn union(mut self, other: Self) -> Self {
        let shift = self.n_states;
        self.n_states += other.n_states;
        self.eps.extend(
            other
                .eps
                .into_iter()
                .map(|v| v.into_iter().map(|x| x + shift).collect::<Vec<_>>()),
        );
        self.consume.extend(other.consume.into_iter().map(|v| {
            v.into_iter()
                .map(|c| Consume {
                    dir: c.dir,
                    pred: c.pred,
                    to: c.to + shift,
                })
                .collect::<Vec<_>>()
        }));
        let s = self.alloc();
        let a = self.alloc();
        self.eps[s].push(self.start);
        self.eps[s].push(other.start + shift);
        self.eps[self.accept].push(a);
        self.eps[other.accept + shift].push(a);
        self.start = s;
        self.accept = a;
        self.nullable = self.nullable || other.nullable;
        self
    }

    pub fn epsilon_closure(&self, states: &[usize]) -> Vec<usize> {
        let mut seen = vec![false; self.n_states];
        let mut stack: Vec<usize> = states.to_vec();
        let mut out = Vec::new();
        while let Some(s) = stack.pop() {
            if s >= self.n_states || seen[s] {
                continue;
            }
            seen[s] = true;
            out.push(s);
            stack.extend_from_slice(&self.eps[s]);
        }
        out.sort_unstable();
        out
    }

    /// First-set of `(dir, pred)` on paths that consume at least one edge.
    pub fn first_preds(&self) -> Vec<(Dir, LabelPred)> {
        self.frontier_preds(self.start, false)
    }

    /// Last-set: preds that can be the last consume into an accept-reachable state.
    pub fn last_preds(&self) -> Vec<(Dir, LabelPred)> {
        // Reverse: from states that ε-reach accept, collect incoming consume.
        let mut reaches_accept = vec![false; self.n_states];
        for s in 0..self.n_states {
            let clos = self.epsilon_closure(&[s]);
            if clos.contains(&self.accept) {
                reaches_accept[s] = true;
            }
        }
        let mut out = Vec::new();
        let mut seen: HashSet<(Dir, String)> = HashSet::new();
        for from in 0..self.n_states {
            for c in &self.consume[from] {
                if reaches_accept[c.to] {
                    let key = (c.dir, format!("{:?}", c.pred));
                    if seen.insert(key) {
                        out.push((c.dir, c.pred.clone()));
                    }
                }
            }
        }
        out
    }

    fn frontier_preds(&self, start: usize, _unused: bool) -> Vec<(Dir, LabelPred)> {
        let clos = self.epsilon_closure(&[start]);
        let mut out = Vec::new();
        let mut seen: HashSet<(Dir, String)> = HashSet::new();
        for s in clos {
            for c in &self.consume[s] {
                let key = (c.dir, format!("{:?}", c.pred));
                if seen.insert(key) {
                    out.push((c.dir, c.pred.clone()));
                }
            }
        }
        out
    }

    /// BFS over (node × NFA state) on an explicit edge list.
    ///
    /// `edges` are (gov, dep, label). Out from `u` walks gov==u; In walks dep==u.
    pub fn reachable_nodes(
        &self,
        n_tokens: usize,
        edges: &[(u32, u32, &str)],
        start: usize,
    ) -> Vec<usize> {
        let mut reached = vec![false; n_tokens];
        let mut seen = vec![false; n_tokens * self.n_states];
        let mut q: VecDeque<(usize, usize)> = VecDeque::new();
        for s in self.epsilon_closure(&[self.start]) {
            let key = start * self.n_states + s;
            if !seen[key] {
                seen[key] = true;
                q.push_back((start, s));
            }
        }
        while let Some((node, state)) = q.pop_front() {
            if state == self.accept || self.epsilon_closure(&[state]).contains(&self.accept) {
                reached[node] = true;
            }
            for c in &self.consume[state] {
                for (gov, dep, lab) in edges {
                    let (from, to) = match c.dir {
                        Dir::Out => (*gov as usize, *dep as usize),
                        Dir::In => (*dep as usize, *gov as usize),
                    };
                    if from != node || !c.pred.matches_str(lab) {
                        continue;
                    }
                    if to >= n_tokens {
                        continue;
                    }
                    for ns in self.epsilon_closure(&[c.to]) {
                        let key = to * self.n_states + ns;
                        if !seen[key] {
                            seen[key] = true;
                            q.push_back((to, ns));
                        }
                    }
                }
            }
        }
        reached
            .iter()
            .enumerate()
            .filter_map(|(i, ok)| ok.then_some(i))
            .collect()
    }

    /// NFA accepting exactly the reversed paths (edge directions flipped).
    pub fn reversed(&self) -> HopNfa {
        let mut eps = vec![Vec::new(); self.n_states];
        let mut consume: Vec<Vec<Consume>> = vec![Vec::new(); self.n_states];
        for s in 0..self.n_states {
            for &t in &self.eps[s] {
                eps[t].push(s);
            }
            for c in &self.consume[s] {
                let dir = match c.dir {
                    Dir::Out => Dir::In,
                    Dir::In => Dir::Out,
                };
                consume[c.to].push(Consume {
                    dir,
                    pred: c.pred.clone(),
                    to: s,
                });
            }
        }
        HopNfa {
            n_states: self.n_states,
            start: self.accept,
            accept: self.start,
            eps,
            consume,
            nullable: self.nullable,
        }
    }

    /// Compile label tests against a segment's relation dictionary and fold
    /// ε-closures into the transitions, so a search never touches strings.
    pub fn bind(&self, rel_dict: &[String]) -> BoundHop {
        let closure = |s: usize| -> Vec<u16> {
            self.epsilon_closure(&[s])
                .into_iter()
                .map(|x| x as u16)
                .collect()
        };
        let trans = (0..self.n_states)
            .map(|s| {
                self.consume[s]
                    .iter()
                    .filter_map(|c| {
                        let labels = LabelSet::compile(rel_dict, &c.pred);
                        // A label that never occurs in this segment can't be walked.
                        (!labels.is_empty()).then(|| BoundTrans {
                            dir: c.dir,
                            labels,
                            to: closure(c.to),
                        })
                    })
                    .collect()
            })
            .collect();
        BoundHop {
            n_states: self.n_states,
            start: closure(self.start),
            accept: self.accept as u16,
            trans,
        }
    }

    pub fn accepts_path(
        &self,
        n_tokens: usize,
        edges: &[(u32, u32, &str)],
        src: usize,
        dst: usize,
    ) -> bool {
        self.reachable_nodes(n_tokens, edges, src).contains(&dst)
    }
}

#[derive(Debug, Clone)]
struct BoundTrans {
    dir: Dir,
    labels: LabelSet,
    /// ε-closure of the target state.
    to: Vec<u16>,
}

/// A [`HopNfa`] bound to one segment's relation dictionary.
#[derive(Debug, Clone)]
pub struct BoundHop {
    n_states: usize,
    start: Vec<u16>,
    accept: u16,
    trans: Vec<Vec<BoundTrans>>,
}

/// Reusable buffers for [`BoundHop::reach`].
#[derive(Debug, Default)]
pub struct HopScratch {
    visited: Vec<u64>,
    queue: Vec<(u32, u16)>,
}

impl BoundHop {
    /// Add to `out` every token reachable from any of `sources` along an
    /// accepted path, walking the sentence's CSR in (token × state) space.
    pub fn reach(
        &self,
        view: &SentenceView<'_>,
        sources: impl Iterator<Item = usize>,
        out: &mut TokenSet,
        sc: &mut HopScratch,
    ) {
        let n = view.n_tokens;
        let ns = self.n_states;
        sc.visited.clear();
        sc.visited.resize((n * ns).div_ceil(64).max(1), 0);
        sc.queue.clear();
        let visit = |visited: &mut Vec<u64>, queue: &mut Vec<(u32, u16)>, node: usize, st: u16| {
            let k = node * ns + st as usize;
            let (w, b) = (k / 64, 1u64 << (k % 64));
            if visited[w] & b == 0 {
                visited[w] |= b;
                queue.push((node as u32, st));
            }
        };
        for u in sources {
            if u >= n {
                continue;
            }
            for &s in &self.start {
                visit(&mut sc.visited, &mut sc.queue, u, s);
            }
        }
        while let Some((node, st)) = sc.queue.pop() {
            let node = node as usize;
            if st == self.accept {
                out.set(node);
            }
            for t in &self.trans[st as usize] {
                let (off, to, rel) = match t.dir {
                    Dir::Out => (view.out_off, view.out_to, view.out_rel),
                    Dir::In => (view.in_off, view.in_from, view.in_rel),
                };
                let (a, b) = (off[node] as usize, off[node + 1] as usize);
                for e in a..b {
                    if !t.labels.contains(rel[e] as usize) {
                        continue;
                    }
                    let next = to[e] as usize;
                    if next >= n {
                        continue;
                    }
                    for &ns2 in &t.to {
                        visit(&mut sc.visited, &mut sc.queue, next, ns2);
                    }
                }
            }
        }
    }
}

impl Default for HopNfa {
    fn default() -> Self {
        Self::new_empty()
    }
}

/// Compile a chain of quantified hops. `+` = base · base*.
pub fn compile_hops(hops: &[(Dir, LabelPred, HopQuant)]) -> HopNfa {
    if hops.is_empty() {
        return HopNfa::default();
    }
    let mut acc: Option<HopNfa> = None;
    for (dir, pred, q) in hops {
        let atom = HopNfa::atom(*dir, pred.clone());
        let piece = match q {
            HopQuant::One => atom,
            HopQuant::Optional => atom.optional(),
            HopQuant::Star => atom.star(),
            HopQuant::Plus => atom.plus(),
        };
        acc = Some(match acc {
            None => piece,
            Some(prev) => prev.concat(piece),
        });
    }
    acc.unwrap_or_default()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HopQuant {
    One,
    Optional,
    Star,
    Plus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_accepts_zero_and_one() {
        let nfa = HopNfa::atom(Dir::Out, LabelPred::Exact("nsubj".into())).optional();
        let edges = vec![(1u32, 0u32, "nsubj")];
        assert!(nfa.accepts_path(2, &edges, 1, 1));
        assert!(nfa.accepts_path(2, &edges, 1, 0));
    }

    #[test]
    fn star_walks() {
        let nfa = HopNfa::atom(Dir::Out, LabelPred::Any).star();
        let edges = vec![(0u32, 1u32, "a"), (1u32, 2u32, "b")];
        let reach = nfa.reachable_nodes(3, &edges, 0);
        assert!(reach.contains(&0));
        assert!(reach.contains(&1));
        assert!(reach.contains(&2));
    }
}
