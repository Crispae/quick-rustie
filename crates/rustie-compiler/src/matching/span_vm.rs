//! Span endpoints as a Pike VM over token bitsets.
//!
//! A span pattern (concatenation of quantified token tests, named captures and
//! look-arounds) compiles into a small program. Each token test refers to an
//! [`NodeTest`] evaluated once per sentence into a [`TokenSet`], so the VM never
//! touches strings. There is no iteration cap: the cost is bounded by
//! `starts × program × tokens`, and results are bounded by `MAX_ENDPOINT_SPANS`.
//!
//! Selection matches the surface matcher this replaces (`surface::stored_match` +
//! `MatchSelector`, the test-only reference): per start position the highest-priority completed match
//! wins (greedy prefers more iterations, lazy fewer; leftmost quantifier
//! decides first), then greedy patterns drop overlapped shorter matches.

use crate::matching::node_test::{constraint_to_test, NodeTest};
use crate::matching::tokenset::TokenSet;
use crate::types::{NamedCapture, Span};
use rustie_query::{Assertion, Constraint, Pattern, QuantifierKind};
use std::collections::HashMap;

/// Most spans one endpoint may yield in a sentence.
pub const MAX_ENDPOINT_SPANS: usize = 1024;

/// A span matched by one endpoint of a plan.
#[derive(Debug, Clone)]
pub struct EndpointSpan {
    pub start: usize,
    pub end: usize,
    /// User-written named captures inside a span endpoint.
    pub captures: Vec<NamedCapture>,
}

/// Largest program a span pattern may compile to (guards `{1,100000}`).
pub const MAX_PROGRAM_SIZE: usize = 4096;

const NO_EVENT: u32 = u32::MAX;

#[derive(Debug, Clone, Copy)]
enum Inst {
    /// Consume one token if `tests[k]` holds at it.
    Test(usize),
    Split(usize, usize),
    Jmp(usize),
    /// Record a capture boundary (`2 * slot` opens, `2 * slot + 1` closes).
    Save(usize),
    /// Zero-width assertion, index into `looks`.
    Look(usize),
    Match,
}

#[derive(Debug, Clone)]
struct LookProg {
    ahead: bool,
    positive: bool,
    insts: Vec<Inst>,
}

#[derive(Debug, Clone)]
pub struct SpanProg {
    insts: Vec<Inst>,
    looks: Vec<LookProg>,
    /// Token tests, referenced by `Inst::Test`.
    pub tests: Vec<NodeTest>,
    /// Source constraint of `tests[k]`, so a caller can resolve the test its own way
    /// (postings positions, columns, ...).
    pub constraints: Vec<Constraint>,
    names: Vec<String>,
    greedy: bool,
}

struct Compiler {
    tests: Vec<NodeTest>,
    constraints: Vec<Constraint>,
    test_keys: HashMap<String, usize>,
    looks: Vec<LookProg>,
    names: Vec<String>,
}

impl Compiler {
    fn test_index(&mut self, e: NodeTest, source: &Constraint) -> usize {
        let key = format!("{e:?}");
        if let Some(&i) = self.test_keys.get(&key) {
            return i;
        }
        self.tests.push(e);
        self.constraints.push(source.clone());
        self.test_keys.insert(key, self.tests.len() - 1);
        self.tests.len() - 1
    }

    fn check_size(out: &[Inst]) -> Result<(), String> {
        if out.len() > MAX_PROGRAM_SIZE {
            return Err(format!(
                "span pattern is too large (more than {MAX_PROGRAM_SIZE} instructions after expanding quantifiers)"
            ));
        }
        Ok(())
    }

    fn emit(&mut self, p: &Pattern, out: &mut Vec<Inst>) -> Result<(), String> {
        Self::check_size(out)?;
        match p {
            Pattern::Constraint(c) => {
                let e = constraint_to_test(c).to_nnf();
                let t = self.test_index(e, c);
                out.push(Inst::Test(t));
            }
            Pattern::Concatenated(ps) => {
                for x in ps {
                    self.emit(x, out)?;
                }
            }
            Pattern::NamedCapture { name, pattern } => {
                let slot = self.names.len();
                self.names.push(name.clone());
                out.push(Inst::Save(2 * slot));
                self.emit(pattern, out)?;
                out.push(Inst::Save(2 * slot + 1));
            }
            Pattern::Repetition {
                pattern,
                min,
                max,
                kind,
            } => {
                let greedy = *kind == QuantifierKind::Greedy;
                for _ in 0..*min {
                    self.emit(pattern, out)?;
                }
                match max {
                    None => {
                        let l1 = out.len();
                        out.push(Inst::Jmp(0)); // patched to the Split below
                        self.emit(pattern, out)?;
                        out.push(Inst::Jmp(l1));
                        let exit = out.len();
                        out[l1] = if greedy {
                            Inst::Split(l1 + 1, exit)
                        } else {
                            Inst::Split(exit, l1 + 1)
                        };
                    }
                    Some(mx) => {
                        let mut splits = Vec::new();
                        for _ in *min..*mx {
                            splits.push(out.len());
                            out.push(Inst::Jmp(0));
                            self.emit(pattern, out)?;
                        }
                        let exit = out.len();
                        for s in splits {
                            out[s] = if greedy {
                                Inst::Split(s + 1, exit)
                            } else {
                                Inst::Split(exit, s + 1)
                            };
                        }
                    }
                }
            }
            Pattern::Assertion(a) => {
                let (child, ahead, positive) = match a {
                    Assertion::PositiveLookahead(c) => (c, true, true),
                    Assertion::NegativeLookahead(c) => (c, true, false),
                    Assertion::PositiveLookbehind(c) => (c, false, true),
                    Assertion::NegativeLookbehind(c) => (c, false, false),
                };
                let mut insts = Vec::new();
                self.emit(child, &mut insts)?;
                insts.push(Inst::Match);
                let k = self.looks.len();
                self.looks.push(LookProg {
                    ahead,
                    positive,
                    insts,
                });
                out.push(Inst::Look(k));
            }
            Pattern::GraphTraversal { .. } => {
                return Err("nested graph traversal inside a span endpoint".into());
            }
        }
        Self::check_size(out)
    }
}

/// Static greedy/lazy class of a pattern, as the surface matcher defines it:
/// a greedy repetition reachable through sequences and captures.
fn is_greedy(p: &Pattern) -> bool {
    match p {
        Pattern::Repetition { kind, .. } => *kind == QuantifierKind::Greedy,
        Pattern::Concatenated(ps) => ps.iter().any(is_greedy),
        Pattern::NamedCapture { pattern, .. } => is_greedy(pattern),
        _ => false,
    }
}

/// Instructions `p` compiles to (saturating), without compiling it.
fn program_size(p: &Pattern) -> usize {
    match p {
        Pattern::Constraint(_) => 1,
        Pattern::Concatenated(ps) => ps.iter().map(program_size).fold(0, usize::saturating_add),
        Pattern::NamedCapture { pattern, .. } => program_size(pattern).saturating_add(2),
        Pattern::Repetition {
            pattern, min, max, ..
        } => {
            let inner = program_size(pattern);
            let fixed = inner.saturating_mul(*min);
            match max {
                None => fixed.saturating_add(inner).saturating_add(2),
                Some(mx) => fixed.saturating_add(
                    inner
                        .saturating_add(1)
                        .saturating_mul(mx.saturating_sub(*min)),
                ),
            }
        }
        // The child is a separate program; it is bounded on its own.
        Pattern::Assertion(_) => 1,
        Pattern::GraphTraversal { .. } => 1,
    }
}

fn assertion_children<'a>(p: &'a Pattern, out: &mut Vec<&'a Pattern>) {
    match p {
        Pattern::Assertion(a) => {
            let child = match a {
                Assertion::PositiveLookahead(c)
                | Assertion::NegativeLookahead(c)
                | Assertion::PositiveLookbehind(c)
                | Assertion::NegativeLookbehind(c) => c,
            };
            out.push(child);
            assertion_children(child, out);
        }
        Pattern::Concatenated(ps) => ps.iter().for_each(|x| assertion_children(x, out)),
        Pattern::NamedCapture { pattern, .. } | Pattern::Repetition { pattern, .. } => {
            assertion_children(pattern, out)
        }
        _ => {}
    }
}

/// Reject span patterns whose expanded program would exceed
/// [`MAX_PROGRAM_SIZE`] (e.g. `[]{1,100000}`), before any sentence is read.
pub fn check_pattern_size(p: &Pattern) -> Result<(), String> {
    let mut all = vec![p];
    assertion_children(p, &mut all);
    for q in all {
        if program_size(q) > MAX_PROGRAM_SIZE {
            return Err(format!(
                "span pattern is too large (more than {MAX_PROGRAM_SIZE} instructions after expanding quantifiers)"
            ));
        }
    }
    Ok(())
}

impl SpanProg {
    pub fn compile(pattern: &Pattern) -> Result<Self, String> {
        let mut c = Compiler {
            tests: Vec::new(),
            constraints: Vec::new(),
            test_keys: HashMap::new(),
            looks: Vec::new(),
            names: Vec::new(),
        };
        let mut insts = Vec::new();
        c.emit(pattern, &mut insts)?;
        insts.push(Inst::Match);
        Ok(Self {
            insts,
            looks: c.looks,
            tests: c.tests,
            constraints: c.constraints,
            names: c.names,
            greedy: is_greedy(pattern),
        })
    }

    /// Spans of this pattern in a sentence of `n` tokens. `sets[k]` is the
    /// token set of `tests[k]`.
    pub fn spans(&self, n: usize, sets: &[TokenSet], sc: &mut VmScratch) -> Vec<EndpointSpan> {
        self.spans_with(n, sets, sc, false)
    }

    /// Spans over plain token strings: each test is evaluated by `NodeTest::matches_token`
    /// with `get(field, token)`. No index involved, so callers holding a sentence's
    /// tokens (result enrichment, tests) need no postings or columns.
    pub fn spans_over_tokens<'a>(
        &self,
        n: usize,
        get: &dyn Fn(&str, usize) -> &'a str,
        keep_empty: bool,
    ) -> Vec<EndpointSpan> {
        let sets: Vec<TokenSet> = self
            .tests
            .iter()
            .map(|t| {
                let mut set = TokenSet::new(n);
                for i in 0..n {
                    if t.matches_token(get, i) {
                        set.set(i);
                    }
                }
                set
            })
            .collect();
        self.spans_with(n, &sets, &mut VmScratch::default(), keep_empty)
    }

    /// Like [`Self::spans`]; `keep_empty` also returns zero-width matches (a bare
    /// look-around is one), which a graph endpoint cannot use but a plain
    /// token query reports as a hit.
    pub fn spans_with(
        &self,
        n: usize,
        sets: &[TokenSet],
        sc: &mut VmScratch,
        keep_empty: bool,
    ) -> Vec<EndpointSpan> {
        let mut ctx = Ctx {
            prog: self,
            sets,
            n,
            memo: HashMap::new(),
        };
        let mut cands: Vec<Cand> = Vec::new();
        for s in 0..n {
            if let Some((end, ev)) = ctx.best_from(&self.insts, s, sc) {
                let caps = self.captures(&sc.events, ev);
                cands.push(Cand {
                    start: s,
                    end,
                    caps,
                });
            }
        }
        let mut out: Vec<EndpointSpan> = select(cands, self.greedy)
            .into_iter()
            // A zero-width span has an empty token interval: it can neither
            // start nor receive a traversal.
            .filter(|c| keep_empty || c.end > c.start)
            .map(|c| EndpointSpan {
                start: c.start,
                end: c.end,
                captures: c.caps,
            })
            .collect();
        out.sort_by_key(|s| (s.start, s.end));
        out.truncate(MAX_ENDPOINT_SPANS);
        out
    }

    /// Captures of the winning thread, in completion order.
    fn captures(&self, events: &[Ev], mut ev: u32) -> Vec<NamedCapture> {
        let mut chain = Vec::new();
        while ev != NO_EVENT {
            let e = &events[ev as usize];
            chain.push((e.slot, e.pos));
            ev = e.prev;
        }
        chain.reverse();
        let mut open: Vec<Option<usize>> = vec![None; self.names.len()];
        let mut caps = Vec::new();
        for (slot, pos) in chain {
            let k = slot / 2;
            if slot % 2 == 0 {
                open[k] = Some(pos);
            } else if let Some(start) = open[k].take() {
                caps.push(NamedCapture::new(
                    self.names[k].clone(),
                    Span::new(start, pos),
                ));
            }
        }
        caps
    }
}

struct Cand {
    start: usize,
    end: usize,
    caps: Vec<NamedCapture>,
}

/// Cross-start selection: overlapping greedy matches keep the longer one
/// (same rule as `MatchSelector::pick_matches`, on spans only). Candidates
/// come one per start, in start order.
fn select(cands: Vec<Cand>, greedy: bool) -> Vec<Cand> {
    let len = |c: &Cand| c.end - c.start;
    let mut kept: Vec<Cand> = Vec::new();
    for cand in cands {
        let mut dominated = false;
        let mut remove = Vec::new();
        for (idx, ex) in kept.iter().enumerate() {
            let overlaps = !(cand.end <= ex.start || cand.start >= ex.end);
            if overlaps {
                if greedy {
                    if len(ex) > len(&cand) {
                        dominated = true;
                        break;
                    } else if len(&cand) > len(ex) {
                        remove.push(idx);
                    }
                }
            } else if greedy {
                if ex.start <= cand.start && cand.end <= ex.end && len(ex) > len(&cand) {
                    dominated = true;
                    break;
                }
                if cand.start <= ex.start && ex.end <= cand.end && len(&cand) > len(ex) {
                    remove.push(idx);
                }
            }
        }
        for &idx in remove.iter().rev() {
            kept.remove(idx);
        }
        if !dominated {
            kept.push(cand);
        }
    }
    kept
}

#[derive(Debug, Clone, Copy)]
struct Ev {
    prev: u32,
    slot: usize,
    pos: usize,
}

#[derive(Debug, Default)]
struct Threads {
    dense: Vec<(u32, u32)>,
    seen: Vec<u32>,
    gen: u32,
}

impl Threads {
    fn reset(&mut self, prog_len: usize) {
        if self.seen.len() < prog_len {
            self.seen.resize(prog_len, 0);
        }
        self.gen = self.gen.wrapping_add(1);
        if self.gen == 0 {
            self.seen.iter_mut().for_each(|s| *s = 0);
            self.gen = 1;
        }
        self.dense.clear();
    }
}

/// Reusable VM buffers.
#[derive(Debug, Default)]
pub struct VmScratch {
    clist: Threads,
    nlist: Threads,
    stack: Vec<(u32, u32)>,
    events: Vec<Ev>,
}

struct Ctx<'a> {
    prog: &'a SpanProg,
    sets: &'a [TokenSet],
    n: usize,
    memo: HashMap<(usize, usize), bool>,
}

impl Ctx<'_> {
    /// Follow ε-transitions from `pc0`, adding consuming/matching threads to
    /// `list` in priority order.
    #[allow(clippy::too_many_arguments)]
    fn add(
        &mut self,
        insts: &[Inst],
        list: &mut Threads,
        stack: &mut Vec<(u32, u32)>,
        events: &mut Vec<Ev>,
        pc0: usize,
        pos: usize,
        ev0: u32,
    ) {
        stack.clear();
        stack.push((pc0 as u32, ev0));
        while let Some((pc, ev)) = stack.pop() {
            let pc = pc as usize;
            if list.seen[pc] == list.gen {
                continue;
            }
            list.seen[pc] = list.gen;
            match insts[pc] {
                Inst::Jmp(t) => stack.push((t as u32, ev)),
                Inst::Split(a, b) => {
                    stack.push((b as u32, ev));
                    stack.push((a as u32, ev));
                }
                Inst::Save(slot) => {
                    events.push(Ev {
                        prev: ev,
                        slot,
                        pos,
                    });
                    stack.push((pc as u32 + 1, events.len() as u32 - 1));
                }
                Inst::Look(k) => {
                    if self.look_holds(k, pos) {
                        stack.push((pc as u32 + 1, ev));
                    }
                }
                Inst::Test(_) | Inst::Match => list.dense.push((pc as u32, ev)),
            }
        }
    }

    fn look_holds(&mut self, k: usize, pos: usize) -> bool {
        if let Some(&v) = self.memo.get(&(k, pos)) {
            return v;
        }
        let look = &self.prog.looks[k];
        // Same rules as the surface matcher: lookahead tries the child at
        // `pos`; lookbehind tries it at `pos - 1` (any match, any end).
        let start = if look.ahead {
            Some(pos)
        } else {
            pos.checked_sub(1)
        };
        let found = match start {
            Some(s) => {
                let mut sc = VmScratch::default();
                let prog = self.prog;
                self.best_from(&prog.looks[k].insts, s, &mut sc).is_some()
            }
            None => false,
        };
        let v = if look.positive {
            found
        } else {
            // A negative lookbehind at position 0 holds; other cases invert.
            start.is_none() || !found
        };
        self.memo.insert((k, pos), v);
        v
    }

    /// Highest-priority match starting at `s`: (end, capture events).
    fn best_from(&mut self, insts: &[Inst], s: usize, sc: &mut VmScratch) -> Option<(usize, u32)> {
        sc.events.clear();
        let VmScratch {
            clist,
            nlist,
            stack,
            events,
        } = sc;
        clist.reset(insts.len());
        self.add(insts, clist, stack, events, 0, s, NO_EVENT);
        let mut best = None;
        let mut pos = s;
        while !clist.dense.is_empty() && pos <= self.n {
            nlist.reset(insts.len());
            let threads = std::mem::take(&mut clist.dense);
            for &(pc, ev) in &threads {
                match insts[pc as usize] {
                    Inst::Test(t) => {
                        if pos < self.n && self.sets[t].get(pos) {
                            self.add(insts, nlist, stack, events, pc as usize + 1, pos + 1, ev);
                        }
                    }
                    Inst::Match => {
                        best = Some((pos, ev));
                        // Lower-priority threads can no longer win.
                        break;
                    }
                    _ => {}
                }
            }
            clist.dense = threads;
            std::mem::swap(clist, nlist);
            pos += 1;
        }
        best
    }
}

#[cfg(test)]
mod tests {
    //! Pure VM tests: hand-built sentences, no index and no GPH2.
    use super::*;
    use rustie_query::QueryParser;

    /// `(start, end)` of every span of `pattern` over `tags`, matching `tag=` tests.
    fn spans_of(pattern: &str, tags: &[&str]) -> Vec<(usize, usize)> {
        let ast = QueryParser::new().parse_query(pattern).expect("parse");
        let prog = SpanProg::compile(&ast).expect("compile");
        prog.spans_over_tokens(tags.len(), &|_field, tok| tags[tok], false)
            .into_iter()
            .map(|s| (s.start, s.end))
            .collect()
    }

    #[test]
    fn plus_is_greedy_and_takes_the_maximal_run() {
        assert_eq!(
            spans_of("[tag=A]+", &["A", "A", "B", "A"]),
            vec![(0, 2), (3, 4)]
        );
    }

    #[test]
    fn lazy_plus_takes_single_tokens() {
        assert_eq!(
            spans_of("[tag=A]+?", &["A", "A", "B"]),
            vec![(0, 1), (1, 2)]
        );
    }

    #[test]
    fn bounded_repetition_respects_min_and_max() {
        // (2,4)/(3,5)-style shorter overlaps are dropped; equal-length ones survive.
        assert_eq!(
            spans_of("[tag=A]{2,3}", &["A", "B", "A", "A", "A", "A"]),
            vec![(2, 5), (3, 6)]
        );
    }

    #[test]
    fn optional_token_matches_with_and_without() {
        let got = spans_of("[tag=D] [tag=J]? [tag=N]", &["D", "J", "N", "D", "N"]);
        assert_eq!(got, vec![(0, 3), (3, 5)]);
    }

    #[test]
    fn positive_lookahead_is_zero_width() {
        assert_eq!(
            spans_of("[tag=A] (?=[tag=B])", &["A", "B", "A", "C"]),
            vec![(0, 1)]
        );
    }

    #[test]
    fn negative_lookahead_rejects() {
        assert_eq!(
            spans_of("[tag=A] (?![tag=B])", &["A", "B", "A", "C"]),
            vec![(2, 3)]
        );
    }

    #[test]
    fn lookbehind_checks_the_previous_token() {
        assert_eq!(
            spans_of("(?<=[tag=D]) [tag=N]", &["D", "N", "N"]),
            vec![(1, 2)]
        );
    }

    #[test]
    fn nothing_matches_an_absent_tag() {
        assert!(spans_of("[tag=Z]+", &["A", "B"]).is_empty());
    }

    #[test]
    fn oversized_pattern_is_rejected_before_any_sentence() {
        let ast = QueryParser::new()
            .parse_query("[]{1,100000}")
            .expect("parse");
        assert!(check_pattern_size(&ast).is_err());
    }
}
