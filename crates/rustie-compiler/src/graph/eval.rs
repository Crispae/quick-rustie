//! In-memory graph evaluator over endpoint spans (Yannakakis-style
//! forward/backward reachability, then DFS enumeration).
//!
//! Edges come from [`SentenceGraph`] JSON (Quickwit stored field), not GPH2.

use crate::graph::nfa::HopNfa;
use crate::graph::plan::{Endpoint, GraphPlanSpec, NodeTest, SpanEndpoint};
use crate::matching::span_vm::{EndpointSpan, SpanProg, MAX_ENDPOINT_SPANS};
use rustie_schema::SentenceGraph;
use std::collections::{BTreeSet, HashMap};

pub use crate::matching::span_vm::EndpointSpan as EvalEndpointSpan;

/// Default cap on enumerated tuples per sentence.
pub const DEFAULT_SENTENCE_CAP: usize = 256;

/// One match: one span per endpoint, in plan order.
pub type SpanTuple = Vec<EndpointSpan>;

/// Token-field accessor used by node tests.
pub trait FieldAccess {
    fn get(&self, field: &str, tok: usize) -> &str;
}

impl FieldAccess for HashMap<String, Vec<String>> {
    fn get(&self, field: &str, tok: usize) -> &str {
        HashMap::get(self, field)
            .and_then(|v| v.get(tok))
            .map(|s| s.as_str())
            .unwrap_or("")
    }
}

impl FieldAccess for &HashMap<String, Vec<String>> {
    fn get(&self, field: &str, tok: usize) -> &str {
        FieldAccess::get(*self, field, tok)
    }
}

/// Start token of every endpoint span (the token tuple for token-only plans).
pub fn tuple_starts(tuples: &[SpanTuple]) -> BTreeSet<Vec<usize>> {
    tuples
        .iter()
        .map(|t| t.iter().map(|s| s.start).collect())
        .collect()
}

/// Evaluate `plan` on a [`SentenceGraph`] plus per-field token vectors.
pub fn evaluate_on_sentence(
    plan: &GraphPlanSpec,
    graph: &SentenceGraph,
    fields: &impl FieldAccess,
    cap: usize,
) -> Vec<SpanTuple> {
    let edge_owned: Vec<(u32, u32, String)> = graph
        .edges
        .iter()
        .map(|e| (e.from, e.to, e.rel.clone()))
        .collect();
    let n_tokens = infer_n_tokens(fields, &edge_owned);
    let edges: Vec<(u32, u32, &str)> = edge_owned
        .iter()
        .map(|(g, d, r)| (*g, *d, r.as_str()))
        .collect();
    evaluate_with_fields(plan, n_tokens, &edges, fields, cap)
}

fn infer_n_tokens(fields: &impl FieldAccess, edges: &[(u32, u32, String)]) -> usize {
    let mut n = 0usize;
    // Prefer `word` length by probing until empty fails consistently — callers
    // should pass consistent field lengths; we take max of edge endpoints + 1.
    for &(g, d, _) in edges {
        n = n.max(g as usize + 1).max(d as usize + 1);
    }
    // Extend from word field if present.
    for i in 0..4096 {
        if fields.get("word", i).is_empty() && i >= n {
            break;
        }
        if !fields.get("word", i).is_empty() {
            n = n.max(i + 1);
        }
    }
    n
}

/// Evaluate `plan` on an explicit edge list and token fields.
pub fn evaluate_with_fields(
    plan: &GraphPlanSpec,
    n_tokens: usize,
    edges: &[(u32, u32, &str)],
    fields: &impl FieldAccess,
    cap: usize,
) -> Vec<SpanTuple> {
    if n_tokens == 0 {
        return Vec::new();
    }
    let mut endpoint_spans: Vec<Vec<EndpointSpan>> = vec![Vec::new(); plan.nodes.len()];
    for (i, ep) in plan.nodes.iter().enumerate() {
        if let Endpoint::Token(test) = ep {
            endpoint_spans[i] = token_spans(test, n_tokens, fields);
            if endpoint_spans[i].is_empty() {
                return Vec::new();
            }
        }
    }
    for (i, ep) in plan.nodes.iter().enumerate() {
        if let Endpoint::Span(sp) = ep {
            endpoint_spans[i] = span_endpoint_spans(sp, n_tokens, fields);
            if endpoint_spans[i].is_empty() {
                return Vec::new();
            }
        }
    }
    let bounds: Vec<Vec<(usize, usize)>> = endpoint_spans
        .iter()
        .map(|v| v.iter().map(|s| (s.start, s.end)).collect())
        .collect();
    evaluate_spans(&bounds, &plan.hops, n_tokens, edges, cap)
        .into_iter()
        .map(|idx| {
            idx.iter()
                .enumerate()
                .map(|(i, &j)| endpoint_spans[i][j].clone())
                .collect()
        })
        .collect()
}

fn token_spans(node: &NodeTest, n: usize, fields: &impl FieldAccess) -> Vec<EndpointSpan> {
    (0..n)
        .filter(|&tok| node.matches_token(&|f, t| fields.get(f, t), tok))
        .map(|tok| EndpointSpan {
            start: tok,
            end: tok + 1,
            captures: Vec::new(),
        })
        .collect()
}

fn span_endpoint_spans(
    sp: &SpanEndpoint,
    n: usize,
    fields: &impl FieldAccess,
) -> Vec<EndpointSpan> {
    let Ok(prog) = SpanProg::compile(&sp.pattern) else {
        return Vec::new();
    };
    let mut spans = prog.spans_over_tokens(n, &|f, t| fields.get(f, t), false);
    // Keep only user-written captures when present.
    if !sp.user_captures.is_empty() {
        for s in &mut spans {
            s.captures.retain(|c| sp.user_captures.contains(&c.name));
        }
    }
    spans.truncate(MAX_ENDPOINT_SPANS);
    spans
}

/// Core algorithm. `spans[i]` are the (start, end) spans of endpoint i; the
/// result holds tuples of span indices.
pub fn evaluate_spans(
    spans: &[Vec<(usize, usize)>],
    hops: &[HopNfa],
    n: usize,
    edges: &[(u32, u32, &str)],
    cap: usize,
) -> BTreeSet<Vec<usize>> {
    let k = spans.len();
    if k == 0 || spans.iter().any(|s| s.is_empty()) {
        return BTreeSet::new();
    }
    debug_assert_eq!(hops.len() + 1, k);

    let inv: Vec<Vec<Vec<usize>>> = spans
        .iter()
        .map(|ss| {
            let mut idx = vec![Vec::new(); n];
            for (j, &(a, b)) in ss.iter().enumerate() {
                for slot in idx.iter_mut().take(b.min(n)).skip(a) {
                    slot.push(j);
                }
            }
            idx
        })
        .collect();

    let mut tok_reach: Vec<Vec<Option<Vec<usize>>>> = vec![vec![None; n]; hops.len()];
    let mut span_reach: Vec<Vec<Option<Vec<usize>>>> = (0..hops.len())
        .map(|i| vec![None; spans[i].len()])
        .collect();
    let mut reach_of = |i: usize, j: usize, tok_reach: &mut Vec<Vec<Option<Vec<usize>>>>| {
        if span_reach[i][j].is_none() {
            let (a, b) = spans[i][j];
            let mut set = BTreeSet::new();
            for (t, slot) in tok_reach[i].iter_mut().enumerate().take(b.min(n)).skip(a) {
                let r = slot.get_or_insert_with(|| hops[i].reachable_nodes(n, edges, t));
                set.extend(r.iter().copied());
            }
            span_reach[i][j] = Some(set.into_iter().collect());
        }
        span_reach[i][j].clone().unwrap_or_default()
    };

    let mut live: Vec<Vec<bool>> = spans.iter().map(|s| vec![false; s.len()]).collect();
    live[0].iter_mut().for_each(|b| *b = true);
    for i in 0..hops.len() {
        for j in 0..spans[i].len() {
            if !live[i][j] {
                continue;
            }
            for v in reach_of(i, j, &mut tok_reach) {
                for &nj in &inv[i + 1][v] {
                    live[i + 1][nj] = true;
                }
            }
        }
    }

    let mut alive = live.clone();
    for i in (0..hops.len()).rev() {
        for j in 0..spans[i].len() {
            if !live[i][j] {
                continue;
            }
            alive[i][j] = reach_of(i, j, &mut tok_reach)
                .into_iter()
                .any(|v| inv[i + 1][v].iter().any(|&nj| alive[i + 1][nj]));
        }
    }

    let mut out = BTreeSet::new();
    let mut cur = Vec::with_capacity(k);
    for j in 0..spans[0].len() {
        if alive[0][j] {
            cur.push(j);
            dfs(
                &inv,
                &alive,
                &mut reach_of,
                &mut tok_reach,
                k,
                cap,
                &mut cur,
                &mut out,
            );
            cur.pop();
            if out.len() >= cap {
                break;
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn dfs(
    inv: &[Vec<Vec<usize>>],
    alive: &[Vec<bool>],
    reach_of: &mut impl FnMut(usize, usize, &mut Vec<Vec<Option<Vec<usize>>>>) -> Vec<usize>,
    tok_reach: &mut Vec<Vec<Option<Vec<usize>>>>,
    k: usize,
    cap: usize,
    cur: &mut Vec<usize>,
    out: &mut BTreeSet<Vec<usize>>,
) {
    if out.len() >= cap {
        return;
    }
    let i = cur.len();
    if i == k {
        out.insert(cur.clone());
        return;
    }
    let prev = cur[i - 1];
    let mut next = BTreeSet::new();
    for v in reach_of(i - 1, prev, tok_reach) {
        for &nj in &inv[i][v] {
            if alive[i][nj] {
                next.insert(nj);
            }
        }
    }
    for nj in next {
        cur.push(nj);
        dfs(inv, alive, reach_of, tok_reach, k, cap, cur, out);
        cur.pop();
        if out.len() >= cap {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::plan::compile_plan;
    use rustie_query::QueryParser;
    use rustie_schema::{DependencyEdge, SentenceGraph};

    fn fields_word(tokens: &[&str]) -> HashMap<String, Vec<String>> {
        let mut m = HashMap::new();
        m.insert(
            "word".into(),
            tokens.iter().map(|s| s.to_string()).collect(),
        );
        m
    }

    #[test]
    fn simple_nsubj_on_sentence_graph() {
        let plan = compile_plan(
            &QueryParser::new()
                .parse_query("[word=eats] >nsubj [word=John]")
                .unwrap(),
        )
        .unwrap();
        let graph = SentenceGraph::new(
            "dependencies",
            vec![DependencyEdge::new(1, 0, "nsubj")],
            vec![1],
        );
        let fields = fields_word(&["John", "eats"]);
        let got = tuple_starts(&evaluate_on_sentence(&plan, &graph, &fields, 32));
        assert!(got.contains(&vec![1, 0]));
    }

    #[test]
    fn star_nullable_keeps_same_node() {
        let plan = compile_plan(
            &QueryParser::new()
                .parse_query("[word=a] >>* [word=a]")
                .unwrap(),
        )
        .unwrap();
        let edges = vec![];
        let fields = fields_word(&["a"]);
        let got = tuple_starts(&evaluate_with_fields(&plan, 1, &edges, &fields, 32));
        assert!(got.contains(&vec![0, 0]));
    }
}
