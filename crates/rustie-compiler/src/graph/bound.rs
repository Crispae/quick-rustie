//! Plans bound to one split: evaluated over dictionary ids and token bitsets, never strings.
//!
//! A [`GraphPlanSpec`] (or a surface pattern) is bound once per split, then evaluated per
//! sentence:
//! - tests on fields colocated in the split's GPH2 graph (tag / entity / chunk) become
//!   [`AttrSet`]s over the graph's dictionaries and are answered from the u16 ids already in the
//!   graph block;
//! - every other field test becomes an [`ExternalLeaf`], whose per-sentence token set is supplied
//!   by the caller through a [`LeafSource`] (in the search engine: postings positions);
//! - hops are [`BoundHop`]s (label tests are [`crate::graph::dict::LabelSet`]s) searched over
//!   the sentence CSR, forward for reach and reversed for pruning.
//!
//! Identical field tests share one leaf, so a caller resolves each one once per sentence.

use std::collections::{BTreeSet, HashMap};

use regex::Regex;
use rustie_graph_store::{Gph2Trailer, SentenceView};
use rustie_query::Pattern;

use crate::graph::dict::{full_match, AttrSet};
use crate::graph::eval::SpanTuple;
use crate::graph::nfa::{BoundHop, HopScratch};
use crate::graph::plan::{Endpoint, GraphPlanSpec, NodeMatcher, NodeTest};
use crate::matching::span_vm::{EndpointSpan, SpanProg, VmScratch};
use crate::matching::tokenset::TokenSet;
use crate::types::NamedCapture;

/// Boolean expression over deduplicated leaf token sets.
#[derive(Debug, Clone)]
enum Expr {
    True,
    Leaf(usize),
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

/// The value test of an [`ExternalLeaf`]. All tests are full-match on one token's value.
#[derive(Debug, Clone)]
pub enum LeafTest {
    Exact(String),
    /// Anchored regex (full match).
    Regex(Regex),
    /// Case-insensitive substring; the needle is lowercase.
    Fuzzy(String),
}

impl LeafTest {
    pub fn matches(&self, value: &str) -> bool {
        match self {
            Self::Exact(x) => value == x,
            Self::Regex(re) => full_match(re, value),
            Self::Fuzzy(needle) => value.to_lowercase().contains(needle.as_str()),
        }
    }
}

/// A token test the caller answers: which tokens of the sentence have a `field` value
/// satisfying `test`.
#[derive(Debug, Clone)]
pub struct ExternalLeaf {
    pub field: String,
    pub test: LeafTest,
}

/// Supplies the token sets of [`ExternalLeaf`]s for the sentence being evaluated.
pub trait LeafSource {
    /// Set in `out` (a cleared set of the sentence's length) every token satisfying external
    /// leaf `leaf`.
    fn fill(&mut self, leaf: usize, out: &mut TokenSet);
}

/// A [`LeafSource`] over token strings, for tests and for re-evaluating a stored sentence.
pub struct StringLeafSource<'a> {
    pub fields: &'a HashMap<String, Vec<String>>,
    pub leaves: &'a [ExternalLeaf],
}

impl LeafSource for StringLeafSource<'_> {
    fn fill(&mut self, leaf: usize, out: &mut TokenSet) {
        let leaf = &self.leaves[leaf];
        let Some(values) = self.fields.get(&leaf.field) else {
            return;
        };
        for (tok, value) in values.iter().enumerate().take(out.len()) {
            if leaf.test.matches(value) {
                out.set(tok);
            }
        }
    }
}

#[derive(Debug, Clone)]
enum Leaf {
    /// Colocated field: membership of the token's dictionary id.
    Attr { field_idx: usize, set: AttrSet },
    /// Answered by the caller (index into the plan's external leaves).
    External(usize),
}

#[derive(Debug, Clone)]
enum BoundEndpoint {
    Token(Expr),
    /// A span program plus its token tests.
    Span {
        prog: SpanProg,
        tests: Vec<Expr>,
    },
    /// A span pattern that failed to compile; matches nothing.
    Never,
}

/// Reusable per-evaluator buffers.
#[derive(Debug, Default)]
pub struct EvalScratch {
    hop: HopScratch,
    leaves: Vec<Option<TokenSet>>,
    vm: VmScratch,
}

/// Turns node tests into expressions over deduplicated leaves.
struct Binder<'a> {
    trailer: &'a Gph2Trailer,
    leaves: Vec<Leaf>,
    keys: HashMap<String, usize>,
    external: Vec<ExternalLeaf>,
}

impl<'a> Binder<'a> {
    fn new(trailer: &'a Gph2Trailer) -> Self {
        Self {
            trailer,
            leaves: Vec::new(),
            keys: HashMap::new(),
            external: Vec::new(),
        }
    }

    fn leaf(&mut self, key: String, make: impl FnOnce(&mut Self) -> Leaf) -> Expr {
        if let Some(&i) = self.keys.get(&key) {
            return Expr::Leaf(i);
        }
        let leaf = make(self);
        self.leaves.push(leaf);
        let i = self.leaves.len() - 1;
        self.keys.insert(key, i);
        Expr::Leaf(i)
    }

    fn external(&mut self, field: &str, test: LeafTest) -> Leaf {
        self.external.push(ExternalLeaf {
            field: field.to_string(),
            test,
        });
        Leaf::External(self.external.len() - 1)
    }

    fn compile(&mut self, t: &NodeTest) -> Expr {
        match t {
            NodeTest::True => Expr::True,
            NodeTest::Field { name, matcher } => {
                let key = match matcher {
                    NodeMatcher::Exact(s) => format!("{name}\0e\0{s}"),
                    NodeMatcher::Regex(re) => format!("{name}\0r\0{}", re.as_str()),
                };
                let colocated = self.trailer.colocated.iter().position(|c| c == name);
                self.leaf(key, |b| match colocated {
                    Some(k) => {
                        let dict = &b.trailer.attr_dicts[k];
                        let set = match matcher {
                            NodeMatcher::Exact(s) => AttrSet::from_exact(dict, s),
                            NodeMatcher::Regex(re) => AttrSet::from_regex(dict, re),
                        };
                        Leaf::Attr { field_idx: k, set }
                    }
                    None => b.external(
                        name,
                        match matcher {
                            NodeMatcher::Exact(s) => LeafTest::Exact(s.clone()),
                            NodeMatcher::Regex(re) => LeafTest::Regex(re.clone()),
                        },
                    ),
                })
            }
            NodeTest::Fuzzy { name, needle } => {
                let key = format!("{name}\0f\0{needle}");
                let needle = needle.to_lowercase();
                let colocated = self.trailer.colocated.iter().position(|c| c == name);
                self.leaf(key, |b| match colocated {
                    Some(k) => {
                        let dict = &b.trailer.attr_dicts[k];
                        let mut set = AttrSet::empty(dict.len());
                        for (id, s) in dict.iter().enumerate() {
                            if s.to_lowercase().contains(&needle) {
                                set.insert(id);
                            }
                        }
                        Leaf::Attr { field_idx: k, set }
                    }
                    None => b.external(name, LeafTest::Fuzzy(needle)),
                })
            }
            NodeTest::And(xs) => Expr::And(xs.iter().map(|x| self.compile(x)).collect()),
            NodeTest::Or(xs) => Expr::Or(xs.iter().map(|x| self.compile(x)).collect()),
            NodeTest::Not(x) => Expr::Not(Box::new(self.compile(x))),
        }
    }
}

/// Leaves shared by bound surface and graph plans.
#[derive(Debug, Clone)]
struct Leaves {
    leaves: Vec<Leaf>,
    external: Vec<ExternalLeaf>,
}

impl Leaves {
    fn prepare(&self, sc: &mut EvalScratch) {
        sc.leaves.clear();
        sc.leaves.resize(self.leaves.len(), None);
    }

    fn leaf_tokens(
        &self,
        li: usize,
        n: usize,
        view: Option<&SentenceView<'_>>,
        src: &mut dyn LeafSource,
        sc: &mut EvalScratch,
    ) -> TokenSet {
        if let Some(Some(s)) = sc.leaves.get(li) {
            return s.clone();
        }
        let mut set = TokenSet::new(n);
        match &self.leaves[li] {
            Leaf::Attr {
                field_idx,
                set: ids,
            } => {
                if let Some(view) = view {
                    for t in 0..n {
                        if ids.contains(view.attr_id(*field_idx, t) as usize) {
                            set.set(t);
                        }
                    }
                }
            }
            Leaf::External(e) => src.fill(*e, &mut set),
        }
        sc.leaves[li] = Some(set.clone());
        set
    }

    fn eval(
        &self,
        e: &Expr,
        n: usize,
        view: Option<&SentenceView<'_>>,
        src: &mut dyn LeafSource,
        sc: &mut EvalScratch,
    ) -> TokenSet {
        match e {
            Expr::True => TokenSet::full(n),
            Expr::Leaf(i) => self.leaf_tokens(*i, n, view, src, sc),
            Expr::Not(x) => {
                let mut s = self.eval(x, n, view, src, sc);
                s.negate();
                s
            }
            Expr::And(xs) => {
                let mut acc = TokenSet::full(n);
                for x in xs {
                    let s = self.eval(x, n, view, src, sc);
                    acc.and_assign(&s);
                    if !acc.any() {
                        break;
                    }
                }
                acc
            }
            Expr::Or(xs) => {
                let mut acc = TokenSet::new(n);
                for x in xs {
                    let s = self.eval(x, n, view, src, sc);
                    acc.or_assign(&s);
                }
                acc
            }
        }
    }
}

/// A surface (non-graph) pattern bound for evaluation over leaf token sets.
#[derive(Debug, Clone)]
pub struct BoundSurface {
    prog: SpanProg,
    tests: Vec<Expr>,
    leaves: Leaves,
}

impl BoundSurface {
    pub fn bind(pattern: &Pattern) -> Result<Self, String> {
        let prog = SpanProg::compile(pattern)?;
        let empty = Gph2Trailer::empty();
        let mut b = Binder::new(&empty);
        let tests = prog.tests.iter().map(|t| b.compile(t)).collect();
        Ok(Self {
            prog,
            tests,
            leaves: Leaves {
                leaves: b.leaves,
                external: b.external,
            },
        })
    }

    /// The token tests the caller must answer, indexed as in [`LeafSource::fill`].
    pub fn external_leaves(&self) -> &[ExternalLeaf] {
        &self.leaves.external
    }

    /// Spans of the pattern in a sentence of `n` tokens. Zero-width spans (a bare look-around)
    /// are kept: for a plain token query they are hits.
    pub fn spans(
        &self,
        n: usize,
        src: &mut dyn LeafSource,
        sc: &mut EvalScratch,
    ) -> Vec<EndpointSpan> {
        if n == 0 {
            return Vec::new();
        }
        self.leaves.prepare(sc);
        let sets: Vec<TokenSet> = self
            .tests
            .iter()
            .map(|e| self.leaves.eval(e, n, None, src, sc))
            .collect();
        self.prog.spans_with(n, &sets, &mut sc.vm, true)
    }
}

/// A graph plan bound to one split's GPH2 dictionaries.
#[derive(Debug, Clone)]
pub struct BoundPlan {
    leaves: Leaves,
    endpoints: Vec<BoundEndpoint>,
    fwd: Vec<BoundHop>,
    rev: Vec<BoundHop>,
}

impl BoundPlan {
    pub fn bind(plan: &GraphPlanSpec, trailer: &Gph2Trailer) -> Self {
        let mut b = Binder::new(trailer);
        let mut endpoints = Vec::with_capacity(plan.nodes.len());
        for ep in plan.nodes.iter() {
            endpoints.push(match ep {
                Endpoint::Token(t) => BoundEndpoint::Token(b.compile(t)),
                Endpoint::Span(sp) => match SpanProg::compile(&sp.pattern) {
                    Ok(prog) => {
                        let tests = prog.tests.iter().map(|t| b.compile(t)).collect();
                        BoundEndpoint::Span { prog, tests }
                    }
                    Err(_) => BoundEndpoint::Never,
                },
            });
        }
        let fwd = plan
            .hops
            .iter()
            .map(|h| h.bind(&trailer.rel_dict))
            .collect();
        let rev = plan
            .hops
            .iter()
            .map(|h| h.reversed().bind(&trailer.rel_dict))
            .collect();
        Self {
            leaves: Leaves {
                leaves: b.leaves,
                external: b.external,
            },
            endpoints,
            fwd,
            rev,
        }
    }

    /// The token tests the caller must answer, indexed as in [`LeafSource::fill`].
    pub fn external_leaves(&self) -> &[ExternalLeaf] {
        &self.leaves.external
    }

    /// Evaluate the plan on one sentence. Results equal the string-based reference
    /// ([`crate::graph::eval::evaluate_with_fields`]): same tuples, same order, same cap prefix.
    pub fn evaluate(
        &self,
        view: &SentenceView<'_>,
        src: &mut dyn LeafSource,
        sc: &mut EvalScratch,
        cap: usize,
    ) -> Vec<SpanTuple> {
        let n = view.n_tokens;
        let k = self.endpoints.len();
        if n == 0 || k == 0 {
            return Vec::new();
        }
        self.leaves.prepare(sc);

        let mut bounds: Vec<Vec<(usize, usize)>> = vec![Vec::new(); k];
        let mut caps: Vec<Vec<Vec<NamedCapture>>> = vec![Vec::new(); k];
        // Cheap token endpoints first: an empty one makes span matching unnecessary.
        for (i, ep) in self.endpoints.iter().enumerate() {
            if let BoundEndpoint::Token(e) = ep {
                let toks = self.leaves.eval(e, n, Some(view), src, sc);
                bounds[i] = toks.iter_ones().map(|t| (t, t + 1)).collect();
                if bounds[i].is_empty() {
                    return Vec::new();
                }
            }
        }
        for (i, ep) in self.endpoints.iter().enumerate() {
            match ep {
                BoundEndpoint::Token(_) => {}
                BoundEndpoint::Never => return Vec::new(),
                BoundEndpoint::Span { prog, tests } => {
                    let sets: Vec<TokenSet> = tests
                        .iter()
                        .map(|e| self.leaves.eval(e, n, Some(view), src, sc))
                        .collect();
                    let spans = prog.spans(n, &sets, &mut sc.vm);
                    if spans.is_empty() {
                        return Vec::new();
                    }
                    bounds[i] = spans.iter().map(|s| (s.start, s.end)).collect();
                    caps[i] = spans.into_iter().map(|s| s.captures).collect();
                }
            }
        }

        let idx_tuples = self.chain(view, &bounds, sc, cap);
        idx_tuples
            .into_iter()
            .map(|idx| {
                idx.iter()
                    .enumerate()
                    .map(|(i, &j)| EndpointSpan {
                        start: bounds[i][j].0,
                        end: bounds[i][j].1,
                        captures: caps[i].get(j).cloned().unwrap_or_default(),
                    })
                    .collect()
            })
            .collect()
    }

    /// Forward/backward passes and enumeration over per-endpoint spans.
    fn chain(
        &self,
        view: &SentenceView<'_>,
        bounds: &[Vec<(usize, usize)>],
        sc: &mut EvalScratch,
        cap: usize,
    ) -> BTreeSet<Vec<usize>> {
        let n = view.n_tokens;
        let k = bounds.len();
        let mut live: Vec<Vec<bool>> = bounds.iter().map(|b| vec![false; b.len()]).collect();
        live[0].iter_mut().for_each(|b| *b = true);
        let mut from = TokenSet::new(n);
        let mut reached = TokenSet::new(n);

        for i in 0..k - 1 {
            from.clear();
            for (j, &(a, b)) in bounds[i].iter().enumerate() {
                if live[i][j] {
                    (a..b.min(n)).for_each(|t| from.set(t));
                }
            }
            reached.clear();
            self.fwd[i].reach(view, from.iter_ones(), &mut reached, &mut sc.hop);
            let mut any = false;
            for (j, &(a, b)) in bounds[i + 1].iter().enumerate() {
                if reached.any_in(a, b) {
                    live[i + 1][j] = true;
                    any = true;
                }
            }
            if !any {
                return BTreeSet::new();
            }
        }

        let mut alive = live.clone();
        for i in (0..k - 1).rev() {
            from.clear();
            for (j, &(a, b)) in bounds[i + 1].iter().enumerate() {
                if alive[i + 1][j] {
                    (a..b.min(n)).for_each(|t| from.set(t));
                }
            }
            reached.clear();
            self.rev[i].reach(view, from.iter_ones(), &mut reached, &mut sc.hop);
            for (j, &(a, b)) in bounds[i].iter().enumerate() {
                if live[i][j] {
                    alive[i][j] = reached.any_in(a, b);
                }
            }
        }

        let mut e = Enumerator {
            bp: self,
            view,
            bounds,
            alive: &alive,
            reach: bounds.iter().map(|b| vec![None; b.len()]).collect(),
            sc,
            cap,
            out: BTreeSet::new(),
        };
        let mut cur = Vec::with_capacity(k);
        for (j, &ok) in alive[0].iter().enumerate() {
            if ok {
                cur.push(j);
                e.dfs(&mut cur);
                cur.pop();
                if e.out.len() >= cap {
                    break;
                }
            }
        }
        e.out
    }
}

struct Enumerator<'a> {
    bp: &'a BoundPlan,
    view: &'a SentenceView<'a>,
    bounds: &'a [Vec<(usize, usize)>],
    alive: &'a [Vec<bool>],
    reach: Vec<Vec<Option<TokenSet>>>,
    sc: &'a mut EvalScratch,
    cap: usize,
    out: BTreeSet<Vec<usize>>,
}

impl Enumerator<'_> {
    fn span_reach(&mut self, i: usize, j: usize) -> TokenSet {
        if let Some(s) = &self.reach[i][j] {
            return s.clone();
        }
        let (a, b) = self.bounds[i][j];
        let mut set = TokenSet::new(self.view.n_tokens);
        self.bp.fwd[i].reach(
            self.view,
            a..b.min(self.view.n_tokens),
            &mut set,
            &mut self.sc.hop,
        );
        self.reach[i][j] = Some(set.clone());
        set
    }

    fn dfs(&mut self, cur: &mut Vec<usize>) {
        if self.out.len() >= self.cap {
            return;
        }
        let i = cur.len();
        if i == self.bounds.len() {
            self.out.insert(cur.clone());
            return;
        }
        let reach = self.span_reach(i - 1, cur[i - 1]);
        for j in 0..self.bounds[i].len() {
            let (a, b) = self.bounds[i][j];
            if self.alive[i][j] && reach.any_in(a, b) {
                cur.push(j);
                self.dfs(cur);
                cur.pop();
                if self.out.len() >= self.cap {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::eval::{evaluate_with_fields, DEFAULT_SENTENCE_CAP};
    use crate::graph::plan::compile_plan;
    use proptest::prelude::*;
    use rustie_graph_store::{SentenceScratch, ROOT};
    use rustie_query::QueryParser;

    const LABELS: [&str; 4] = ["nsubj", "dobj", "amod", "nmod_in"];
    const TAGS: [&str; 3] = ["NN", "VB", "DT"];
    const WORDS: [&str; 3] = ["a", "b", "c"];
    const QUERIES: [&str; 15] = [
        "[tag=NN] >nsubj [word=a]",
        "[] >nsubj []",
        "[] >>* []",
        "[tag=NN] >> [tag=VB]",
        "[tag=/NN|VB/] <nsubj|dobj* [word=/a|b/]",
        "[!tag=NN] >amod? [tag=DT]",
        "[word=a] >nsubj >dobj [tag=VB]",
        "[tag=NN] >nsubj [word=a] >dobj [tag=VB]",
        "[tag=NN & word=a] >/nsubj|dobj/ [word=b~]",
        "[tag=NN]+ >nsubj [word=a]",
        "[word=a] [word=b] >dobj []",
        "[tag=NN] >nsubj [word=a] [word=b]",
        "[tag=DT]+ >>+ [tag=NN]+",
        "[tag=NN] >nsubj [] [tag=VB] >dobj [word=c]",
        "[word=A~] >nsubj [!word=b]",
    ];

    fn spans(t: &[EndpointSpan]) -> Vec<(usize, usize)> {
        t.iter().map(|s| (s.start, s.end)).collect()
    }

    fn check(query: &str, tags: &[usize], words: &[usize], edges: &[(u32, u32, usize)]) {
        let n = tags.len();
        let plan = compile_plan(&QueryParser::new().parse_query(query).unwrap()).unwrap();

        let label_dict: Vec<String> = LABELS.iter().map(|s| s.to_string()).collect();
        let tag_dict: Vec<String> = std::iter::once(String::new())
            .chain(TAGS.iter().map(|s| s.to_string()))
            .collect();
        let tag_ids: Vec<u16> = tags.iter().map(|&t| t as u16 + 1).collect();
        let (gov, dep): (Vec<u32>, Vec<u32>) = edges.iter().map(|&(g, d, _)| (g, d)).unzip();
        let rel: Vec<u16> = edges.iter().map(|&(_, _, l)| l as u16).collect();
        let mut scratch = SentenceScratch::default();
        scratch.rebuild(
            n,
            &vec![ROOT; n],
            &vec![0; n],
            &label_dict,
            &[&tag_ids],
            &[tag_dict.clone()],
            &["tag".to_string()],
            &gov,
            &dep,
            &rel,
        );
        let view = scratch.view();

        // `word` is answered by the caller (as postings would); `tag` by the graph block.
        let mut words_only: HashMap<String, Vec<String>> = HashMap::new();
        words_only.insert(
            "word".into(),
            words.iter().map(|&w| WORDS[w].to_string()).collect(),
        );
        let mut all = words_only.clone();
        all.insert(
            "tag".into(),
            tags.iter().map(|&t| TAGS[t].to_string()).collect(),
        );

        let edge_refs: Vec<(u32, u32, &str)> =
            edges.iter().map(|&(g, d, l)| (g, d, LABELS[l])).collect();
        let want: Vec<Vec<(usize, usize)>> =
            evaluate_with_fields(&plan, n, &edge_refs, &all, DEFAULT_SENTENCE_CAP)
                .iter()
                .map(|t| spans(t))
                .collect();

        let trailer = {
            let mut t = Gph2Trailer::empty();
            t.rel_dict = label_dict.clone();
            t.colocated = vec!["tag".to_string()];
            t.attr_dicts = vec![tag_dict];
            t
        };
        let bound = BoundPlan::bind(&plan, &trailer);
        assert!(
            bound.external_leaves().iter().all(|l| l.field == "word"),
            "colocated fields never reach the caller"
        );
        let mut src = StringLeafSource {
            fields: &words_only,
            leaves: bound.external_leaves(),
        };
        let mut sc = EvalScratch::default();
        let got: Vec<Vec<(usize, usize)>> = bound
            .evaluate(&view, &mut src, &mut sc, DEFAULT_SENTENCE_CAP)
            .iter()
            .map(|t| spans(t))
            .collect();
        assert_eq!(
            got, want,
            "query {query:?} tags {tags:?} words {words:?} edges {edges:?}"
        );
        let again: Vec<Vec<(usize, usize)>> = bound
            .evaluate(&view, &mut src, &mut sc, DEFAULT_SENTENCE_CAP)
            .iter()
            .map(|t| spans(t))
            .collect();
        assert_eq!(again, want, "scratch reuse");
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(3000))]

        #[test]
        fn bound_evaluator_matches_reference(
            q in 0usize..QUERIES.len(),
            n in 1usize..9,
            tags in prop::collection::vec(0usize..3, 9),
            words in prop::collection::vec(0usize..3, 9),
            edges in prop::collection::vec((0u32..9, 0u32..9, 0usize..4), 0..14),
        ) {
            let edges: Vec<(u32, u32, usize)> = edges
                .into_iter()
                .map(|(g, d, l)| (g % n as u32, d % n as u32, l))
                .collect();
            check(QUERIES[q], &tags[..n], &words[..n], &edges);
        }

        #[test]
        fn bound_surface_matches_string_span_vm(
            q in 0usize..6,
            words in prop::collection::vec(0usize..3, 1..10),
        ) {
            const SURFACE: [&str; 6] = [
                "[word=a]",
                "[word=a] [word=b]",
                "[word=a] []{0,2} [word=c]",
                "(?<x> [word=/a|b/]+) [!word=c]",
                "[word=A~]",
                "[word=a] (?= [word=b])",
            ];
            let pattern = QueryParser::new().parse_query(SURFACE[q]).unwrap();
            let fields: HashMap<String, Vec<String>> = HashMap::from([(
                "word".to_string(),
                words.iter().map(|&w| WORDS[w].to_string()).collect(),
            )]);
            let n = words.len();
            let prog = SpanProg::compile(&pattern).unwrap();
            let want: Vec<(usize, usize)> = prog
                .spans_over_tokens(n, &|f, t| fields.get(f).and_then(|v| v.get(t)).map_or("", |s| s.as_str()), true)
                .iter()
                .map(|s| (s.start, s.end))
                .collect();
            let bound = BoundSurface::bind(&pattern).unwrap();
            let mut src = StringLeafSource { fields: &fields, leaves: bound.external_leaves() };
            let got: Vec<(usize, usize)> = bound
                .spans(n, &mut src, &mut EvalScratch::default())
                .iter()
                .map(|s| (s.start, s.end))
                .collect();
            prop_assert_eq!(got, want);
        }
    }

    #[test]
    fn shared_tests_share_one_leaf() {
        let pattern = QueryParser::new()
            .parse_query("[word=a] [word=a] [word=/a|b/] [word=a]")
            .unwrap();
        let bound = BoundSurface::bind(&pattern).unwrap();
        assert_eq!(bound.external_leaves().len(), 2);
    }
}
