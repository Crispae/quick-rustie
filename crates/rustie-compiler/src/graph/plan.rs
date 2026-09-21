//! Graph plan compiler: `C (H+ C)*` → [`GraphPlanSpec`].

use crate::graph::nfa::{Dir, HopNfa, LabelPred};
use crate::graph::require::{derive, HopRequirements};
pub(crate) use crate::matching::node_test::{anchor as anchored, constraint_to_test};
pub use crate::matching::node_test::{NodeMatcher, NodeTest};
use rustie_query::{Assertion, Constraint, Matcher, Pattern, Traversal};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Capture materialization mode (`(?<name:mode> …)`). Default is token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureMode {
    Token,
    Phrase,
    Pred,
}

impl CaptureMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "token" => Some(Self::Token),
            "phrase" => Some(Self::Phrase),
            "pred" => Some(Self::Pred),
            _ => None,
        }
    }
}

/// Split `name:mode` at the last colon. Unknown modes keep the full string as name.
pub fn split_capture_mode(raw: &str) -> (String, CaptureMode) {
    if let Some((name, mode)) = raw.rsplit_once(':') {
        if let Some(mode) = CaptureMode::parse(mode) {
            return (name.to_string(), mode);
        }
    }
    (raw.to_string(), CaptureMode::Token)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureSpec {
    pub name: String,
    /// Name as written, including any `:mode` suffix. Emitted on matches so
    /// OpenIE expansion (which splits the suffix) sees the requested mode.
    pub raw_name: String,
    pub mode: CaptureMode,
    pub node_index: usize,
    /// Covers the whole match span (capture wrapping an entire traversal).
    pub whole_match: bool,
}

/// One node of a `C (H+ C)*` chain: a single token, or a multi-token surface
/// span (Odinson endpoints are full surface patterns).
#[derive(Debug, Clone)]
pub enum Endpoint {
    Token(NodeTest),
    Span(SpanEndpoint),
}

/// A surface pattern used as a graph endpoint. Regex matchers are anchored so
/// span matching is full-match, consistent with token endpoints and postings.
#[derive(Debug, Clone)]
pub struct SpanEndpoint {
    pub pattern: Pattern,
    /// Named captures written inside the span (kept in output; the matcher's
    /// automatic `c{pos}` atom captures are dropped).
    pub user_captures: BTreeSet<String>,
    pub fields: BTreeSet<String>,
    /// True when the span has at least one mandatory token constraint that can
    /// narrow candidate documents.
    pub mandatory: bool,
}

impl Endpoint {
    pub fn field_names(&self, out: &mut BTreeSet<String>) {
        match self {
            Self::Token(t) => t.field_names(out),
            Self::Span(s) => out.extend(s.fields.iter().cloned()),
        }
    }
}

/// Compiled graph pattern: endpoints, hop NFAs, captures, hop requirements.
#[derive(Debug, Clone)]
pub struct GraphPlanSpec {
    pub nodes: Vec<Endpoint>,
    pub hops: Vec<HopNfa>,
    pub hop_reqs: Vec<HopRequirements>,
    pub captures: Vec<CaptureSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    UnsupportedP7(String),
    Invalid(String),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedP7(s) => write!(f, "not yet supported: {s}"),
            Self::Invalid(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for PlanError {}

/// Mandatory token constraints of a surface pattern: every match must contain
/// a token satisfying each. Used to pick candidate-narrowing terms for spans.
pub fn mandatory_constraints(p: &Pattern) -> Vec<&Constraint> {
    fn collect<'a>(p: &'a Pattern, out: &mut Vec<&'a Constraint>) {
        match p {
            Pattern::Constraint(Constraint::Wildcard) => {}
            Pattern::Constraint(c) => out.push(c),
            Pattern::NamedCapture { pattern, .. } => collect(pattern, out),
            Pattern::Concatenated(ps) => ps.iter().for_each(|x| collect(x, out)),
            Pattern::Repetition { pattern, min, .. } => {
                if *min >= 1 {
                    collect(pattern, out);
                }
            }
            Pattern::Assertion(_) | Pattern::GraphTraversal { .. } => {}
        }
    }
    let mut out = Vec::new();
    collect(p, &mut out);
    out
}

/// A mandatory token test at a fixed offset inside a run of fixed-length
/// elements. Tests with the same `group` sit at known offsets from each other.
#[derive(Debug, Clone, Copy)]
pub struct FixedTerm<'a> {
    pub group: usize,
    pub offset: u32,
    pub constraint: &'a Constraint,
}

/// Copies of a quantified single token whose position is still known.
const MAX_FIXED_COPIES: usize = 8;

fn single_constraint(p: &Pattern) -> Option<&Constraint> {
    match strip_captures(p) {
        Pattern::Constraint(c) => Some(c),
        _ => None,
    }
}

fn flatten_sequence<'a>(p: &'a Pattern, out: &mut Vec<&'a Pattern>) {
    match p {
        Pattern::Concatenated(ps) => ps.iter().for_each(|x| flatten_sequence(x, out)),
        Pattern::NamedCapture { pattern, .. } => flatten_sequence(pattern, out),
        other => out.push(other),
    }
}

/// Split the mandatory token tests of a span into phrase groups.
///
/// Returns the tests whose offset from the group start is known (each element
/// before a variable-length one occupies a fixed number of tokens), and the
/// remaining mandatory tests, which only hold "somewhere in the span".
/// Offsets restart after any element of unknown length.
pub fn phrase_terms(p: &Pattern) -> (Vec<FixedTerm<'_>>, Vec<&Constraint>) {
    let mut elems = Vec::new();
    flatten_sequence(p, &mut elems);
    let mut terms: Vec<FixedTerm<'_>> = Vec::new();
    let (mut group, mut offset) = (0usize, 0u32);
    fn push<'a>(terms: &mut Vec<FixedTerm<'a>>, group: usize, offset: u32, c: &'a Constraint) {
        if !matches!(c, Constraint::Wildcard) {
            terms.push(FixedTerm {
                group,
                offset,
                constraint: c,
            });
        }
    }
    for el in elems {
        match el {
            Pattern::Constraint(c) => {
                push(&mut terms, group, offset, c);
                offset += 1;
            }
            // Zero-width: constrains the neighbourhood, occupies no token.
            Pattern::Assertion(_) => {}
            Pattern::Repetition {
                pattern, min, max, ..
            } => match single_constraint(pattern) {
                Some(c) if *min >= 1 => {
                    let known = (*min).min(MAX_FIXED_COPIES);
                    for i in 0..known {
                        push(&mut terms, group, offset + i as u32, c);
                    }
                    if *max == Some(*min) && *min <= MAX_FIXED_COPIES {
                        offset += *min as u32;
                    } else {
                        group += 1;
                        offset = 0;
                    }
                }
                _ => {
                    group += 1;
                    offset = 0;
                }
            },
            _ => {
                group += 1;
                offset = 0;
            }
        }
    }
    let extras = mandatory_constraints(p)
        .into_iter()
        .filter(|c| !terms.iter().any(|t| std::ptr::eq(t.constraint, *c)))
        .collect();
    (terms, extras)
}

fn strip_captures(mut p: &Pattern) -> &Pattern {
    while let Pattern::NamedCapture { pattern, .. } = p {
        p = pattern;
    }
    p
}

fn contains_traversal(p: &Pattern) -> bool {
    match p {
        Pattern::GraphTraversal { .. } => true,
        Pattern::Constraint(_) => false,
        Pattern::NamedCapture { pattern, .. } | Pattern::Repetition { pattern, .. } => {
            contains_traversal(pattern)
        }
        Pattern::Concatenated(ps) => ps.iter().any(contains_traversal),
        Pattern::Assertion(a) => contains_traversal(assertion_inner(a)),
    }
}

fn assertion_inner(a: &Assertion) -> &Pattern {
    match a {
        Assertion::PositiveLookahead(p)
        | Assertion::NegativeLookahead(p)
        | Assertion::PositiveLookbehind(p)
        | Assertion::NegativeLookbehind(p) => p,
    }
}

fn collect_user_captures(p: &Pattern, out: &mut BTreeSet<String>) {
    match p {
        Pattern::NamedCapture { name, pattern } => {
            out.insert(name.clone());
            collect_user_captures(pattern, out);
        }
        Pattern::Repetition { pattern, .. } => collect_user_captures(pattern, out),
        Pattern::Concatenated(ps) => ps.iter().for_each(|x| collect_user_captures(x, out)),
        Pattern::Assertion(a) => collect_user_captures(assertion_inner(a), out),
        Pattern::Constraint(_) | Pattern::GraphTraversal { .. } => {}
    }
}

fn collect_constraint_fields(c: &Constraint, out: &mut BTreeSet<String>) {
    match c {
        Constraint::Wildcard => {}
        Constraint::Field { name, .. } | Constraint::Fuzzy { name, .. } => {
            out.insert(name.clone());
        }
        Constraint::Negated(x) => collect_constraint_fields(x, out),
        Constraint::Conjunctive(xs) | Constraint::Disjunctive(xs) => {
            xs.iter().for_each(|x| collect_constraint_fields(x, out))
        }
    }
}

fn collect_pattern_fields(p: &Pattern, out: &mut BTreeSet<String>) {
    match p {
        Pattern::Constraint(c) => collect_constraint_fields(c, out),
        Pattern::NamedCapture { pattern, .. } | Pattern::Repetition { pattern, .. } => {
            collect_pattern_fields(pattern, out)
        }
        Pattern::Concatenated(ps) => ps.iter().for_each(|x| collect_pattern_fields(x, out)),
        Pattern::Assertion(a) => collect_pattern_fields(assertion_inner(a), out),
        Pattern::GraphTraversal { .. } => {}
    }
}

/// Anchor a regex so that `is_match` means the whole string matches. A plain
/// `find` would return only the leftmost-first alternative (`a|ab` on "ab").
fn anchor_matcher(m: &Matcher) -> Matcher {
    match m {
        Matcher::String(_) => m.clone(),
        Matcher::Regex { pattern, regex } => Matcher::Regex {
            pattern: pattern.clone(),
            regex: Arc::new(anchored(regex)),
        },
    }
}

fn anchor_constraint(c: &Constraint) -> Constraint {
    match c {
        Constraint::Wildcard | Constraint::Fuzzy { .. } => c.clone(),
        Constraint::Field { name, matcher } => Constraint::Field {
            name: name.clone(),
            matcher: anchor_matcher(matcher),
        },
        Constraint::Negated(x) => Constraint::Negated(Box::new(anchor_constraint(x))),
        Constraint::Conjunctive(xs) => {
            Constraint::Conjunctive(xs.iter().map(anchor_constraint).collect())
        }
        Constraint::Disjunctive(xs) => {
            Constraint::Disjunctive(xs.iter().map(anchor_constraint).collect())
        }
    }
}

fn anchor_pattern(p: &Pattern) -> Pattern {
    match p {
        Pattern::Constraint(c) => Pattern::Constraint(anchor_constraint(c)),
        Pattern::NamedCapture { name, pattern } => Pattern::NamedCapture {
            name: name.clone(),
            pattern: Box::new(anchor_pattern(pattern)),
        },
        Pattern::Repetition {
            pattern,
            min,
            max,
            kind,
        } => Pattern::Repetition {
            pattern: Box::new(anchor_pattern(pattern)),
            min: *min,
            max: *max,
            kind: *kind,
        },
        Pattern::Concatenated(ps) => Pattern::Concatenated(ps.iter().map(anchor_pattern).collect()),
        Pattern::Assertion(a) => Pattern::Assertion(match a {
            Assertion::PositiveLookahead(x) => {
                Assertion::PositiveLookahead(Box::new(anchor_pattern(x)))
            }
            Assertion::NegativeLookahead(x) => {
                Assertion::NegativeLookahead(Box::new(anchor_pattern(x)))
            }
            Assertion::PositiveLookbehind(x) => {
                Assertion::PositiveLookbehind(Box::new(anchor_pattern(x)))
            }
            Assertion::NegativeLookbehind(x) => {
                Assertion::NegativeLookbehind(Box::new(anchor_pattern(x)))
            }
        }),
        Pattern::GraphTraversal { .. } => p.clone(),
    }
}

/// Compile a graph-traversal AST into a segment-chain plan.
pub fn compile_plan(pattern: &Pattern) -> Result<GraphPlanSpec, PlanError> {
    // A capture wrapping a whole traversal covers the whole match span.
    let mut whole = Vec::new();
    let mut cur = pattern;
    if matches!(strip_captures(pattern), Pattern::GraphTraversal { .. }) {
        while let Pattern::NamedCapture {
            name,
            pattern: inner,
        } = cur
        {
            let (base, mode) = split_capture_mode(name);
            whole.push(CaptureSpec {
                name: base,
                raw_name: name.clone(),
                mode,
                node_index: 0,
                whole_match: true,
            });
            cur = inner;
        }
    }
    let mut b = PlanBuilder::default();
    b.chain(cur)?;
    b.finish(whole)
}

#[derive(Default)]
struct PlanBuilder {
    nodes: Vec<Endpoint>,
    hops: Vec<HopNfa>,
    captures: Vec<CaptureSpec>,
    pending: Option<HopNfa>,
}

impl PlanBuilder {
    fn chain(&mut self, p: &Pattern) -> Result<(), PlanError> {
        match p {
            Pattern::GraphTraversal {
                src,
                traversal,
                dst,
            } => {
                self.chain(src)?;
                self.pending = Some(traversal_to_nfa(traversal)?);
                self.chain(dst)
            }
            other => self.endpoint(other),
        }
    }

    fn endpoint(&mut self, p: &Pattern) -> Result<(), PlanError> {
        if !self.nodes.is_empty() {
            match self.pending.take() {
                Some(h) => self.hops.push(h),
                None => {
                    return Err(PlanError::Invalid(
                        "adjacent endpoints without a hop".into(),
                    ))
                }
            }
        }
        let idx = self.nodes.len();
        let mut cur = p;
        while let Pattern::NamedCapture { name, pattern } = cur {
            if matches!(strip_captures(pattern), Pattern::GraphTraversal { .. }) {
                return Err(PlanError::UnsupportedP7(
                    "capture wrapping a nested graph traversal".into(),
                ));
            }
            let (base, mode) = split_capture_mode(name);
            self.captures.push(CaptureSpec {
                name: base,
                raw_name: name.clone(),
                mode,
                node_index: idx,
                whole_match: false,
            });
            cur = pattern;
        }
        let endpoint = match cur {
            Pattern::Constraint(c) => Endpoint::Token(constraint_to_test(c).to_nnf()),
            Pattern::Assertion(_) => {
                return Err(PlanError::Invalid(
                    "an assertion matches no tokens and cannot be a graph endpoint".into(),
                ))
            }
            Pattern::GraphTraversal { .. } | Pattern::NamedCapture { .. } => {
                return Err(PlanError::UnsupportedP7(
                    "nested graph traversal as an endpoint".into(),
                ))
            }
            Pattern::Concatenated(_) | Pattern::Repetition { .. } => {
                if contains_traversal(cur) {
                    return Err(PlanError::UnsupportedP7(
                        "graph traversal nested inside a span endpoint".into(),
                    ));
                }
                crate::matching::span_vm::check_pattern_size(cur).map_err(PlanError::Invalid)?;
                let mut user_captures = BTreeSet::new();
                collect_user_captures(cur, &mut user_captures);
                let mut fields = BTreeSet::new();
                collect_pattern_fields(cur, &mut fields);
                Endpoint::Span(SpanEndpoint {
                    pattern: anchor_pattern(cur),
                    user_captures,
                    fields,
                    mandatory: !mandatory_constraints(cur).is_empty(),
                })
            }
        };
        self.nodes.push(endpoint);
        Ok(())
    }

    fn finish(mut self, whole: Vec<CaptureSpec>) -> Result<GraphPlanSpec, PlanError> {
        if self.nodes.is_empty() {
            return Err(PlanError::Invalid("empty graph pattern".into()));
        }
        if self.pending.is_some() {
            return Err(PlanError::Invalid(
                "graph pattern must end with a node test".into(),
            ));
        }
        if self.hops.is_empty() || self.hops.len() + 1 != self.nodes.len() {
            return Err(PlanError::Invalid(format!(
                "expected C (H+ C)* shape, got {} nodes and {} hops",
                self.nodes.len(),
                self.hops.len()
            )));
        }
        let hop_reqs = self.hops.iter().map(derive).collect();
        let mut captures = whole;
        captures.append(&mut self.captures);
        Ok(GraphPlanSpec {
            nodes: self.nodes,
            hops: self.hops,
            hop_reqs,
            captures,
        })
    }
}

fn matcher_to_pred(m: &Matcher) -> LabelPred {
    match m {
        Matcher::String(s) => LabelPred::Exact(s.clone()),
        Matcher::Regex { regex, .. } => LabelPred::Regex(anchored(regex)),
    }
}

pub fn traversal_to_nfa(t: &Traversal) -> Result<HopNfa, PlanError> {
    Ok(match t {
        Traversal::NoTraversal => HopNfa::default(),
        Traversal::OutgoingWildcard => HopNfa::atom(Dir::Out, LabelPred::Any),
        Traversal::IncomingWildcard => HopNfa::atom(Dir::In, LabelPred::Any),
        Traversal::Outgoing(m) => HopNfa::atom(Dir::Out, matcher_to_pred(m)),
        Traversal::Incoming(m) => HopNfa::atom(Dir::In, matcher_to_pred(m)),
        Traversal::Optional(inner) => traversal_to_nfa(inner)?.optional(),
        Traversal::KleeneStar(inner) => traversal_to_nfa(inner)?.star(),
        Traversal::Concatenated(hops) => {
            let mut acc: Option<HopNfa> = None;
            for h in hops {
                let n = traversal_to_nfa(h)?;
                acc = Some(match acc {
                    None => n,
                    Some(prev) => prev.concat(n),
                });
            }
            acc.unwrap_or_default()
        }
        Traversal::Disjunctive(hops) => {
            let mut acc: Option<HopNfa> = None;
            for h in hops {
                let n = traversal_to_nfa(h)?;
                acc = Some(match acc {
                    None => n,
                    Some(prev) => prev.union(n),
                });
            }
            acc.unwrap_or_default()
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustie_query::QueryParser;

    fn parse(q: &str) -> Pattern {
        QueryParser::new().parse_query(q).expect("parse")
    }

    #[test]
    fn compiles_simple_hop() {
        let plan = compile_plan(&parse("[word=eat] >nsubj []")).unwrap();
        assert_eq!(plan.nodes.len(), 2);
        assert_eq!(plan.hops.len(), 1);
        assert!(!plan.hops[0].nullable);
    }

    #[test]
    fn compiles_quantified_and_disjunctive() {
        let plan = compile_plan(&parse("[word=a] >nsubj|dobj* [word=b]")).unwrap();
        assert_eq!(plan.nodes.len(), 2);
        assert!(plan.hops[0].nullable);
    }

    #[test]
    fn concatenates_adjacent_hops() {
        let plan = compile_plan(&parse(
            "[entity=/[BI]-PER/] </nsubj(pass)?/ [word=born] >/nmod_.*/* >nmod_in [word=/[0-9]{4}/]",
        ));
        // Parser: `>/nmod_.*/* >nmod_in` is two hops with no node between them → one H+.
        let plan = plan.unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(plan.nodes.len(), 3, "PER, born, year");
        assert_eq!(plan.hops.len(), 2, "incoming + concat(kleene, nmod_in)");
    }

    #[test]
    fn capture_records_node_index() {
        let plan = compile_plan(&parse("(?<arg1> [word=John]) >nsubj [word=eats]")).unwrap();
        assert_eq!(plan.captures.len(), 1);
        assert_eq!(plan.captures[0].name, "arg1");
        assert_eq!(plan.captures[0].mode, CaptureMode::Token);
        assert_eq!(plan.captures[0].node_index, 0);
    }

    #[test]
    fn split_mode_phrase_and_pred() {
        assert_eq!(
            split_capture_mode("arg1:phrase"),
            ("arg1".into(), CaptureMode::Phrase)
        );
        assert_eq!(
            split_capture_mode("rel:pred"),
            ("rel".into(), CaptureMode::Pred)
        );
        assert_eq!(
            split_capture_mode("arg1"),
            ("arg1".into(), CaptureMode::Token)
        );
    }

    fn kinds(plan: &GraphPlanSpec) -> Vec<&'static str> {
        plan.nodes
            .iter()
            .map(|n| match n {
                Endpoint::Token(_) => "token",
                Endpoint::Span(_) => "span",
            })
            .collect()
    }

    #[test]
    fn quantified_source_is_a_span_endpoint() {
        let plan = compile_plan(&parse("[tag=/NN.*/]+ >nsubj [word=eats]")).unwrap();
        assert_eq!(kinds(&plan), ["span", "token"]);
    }

    #[test]
    fn captured_multi_token_endpoint_is_a_span() {
        let plan = compile_plan(&parse("(?<x> [word=a][word=b]) >nsubj [word=c]")).unwrap();
        assert_eq!(kinds(&plan), ["span", "token"]);
        assert_eq!(plan.captures.len(), 1);
        assert_eq!(plan.captures[0].name, "x");
        assert_eq!(plan.captures[0].node_index, 0);
    }

    #[test]
    fn trailing_atoms_extend_the_destination_span() {
        let plan = compile_plan(&parse("[word=a] >x [word=b] [word=c]")).unwrap();
        assert_eq!(kinds(&plan), ["token", "span"]);
        let plan = compile_plan(&parse("[word=a] >x [word=b] [word=c] >y [word=d]")).unwrap();
        assert_eq!(kinds(&plan), ["token", "span", "token"]);
    }

    #[test]
    fn leading_atoms_form_a_source_span() {
        let plan = compile_plan(&parse("[word=a] [word=b] >x [word=c]")).unwrap();
        assert_eq!(kinds(&plan), ["span", "token"]);
    }

    #[test]
    fn both_endpoints_may_be_spans() {
        let plan = compile_plan(&parse("[word=a] [word=b] >x [word=c] [word=d]")).unwrap();
        assert_eq!(kinds(&plan), ["span", "span"]);
    }

    #[test]
    fn optional_only_source_is_still_a_span() {
        let plan = compile_plan(&parse("[word=a]* >x [word=b]")).unwrap();
        assert_eq!(kinds(&plan), ["span", "token"]);
    }

    #[test]
    fn regex_in_span_is_anchored() {
        let plan = compile_plan(&parse("[word=/a|ab/]+ >x [word=b]")).unwrap();
        let Endpoint::Span(sp) = &plan.nodes[0] else {
            panic!("expected span");
        };
        let mut found = false;
        fn walk(p: &Pattern, found: &mut bool) {
            match p {
                Pattern::Repetition { pattern, .. } => walk(pattern, found),
                Pattern::Constraint(Constraint::Field {
                    matcher: Matcher::Regex { regex, .. },
                    ..
                }) => {
                    *found = true;
                    assert!(regex.is_match("ab"));
                    assert!(!regex.is_match("xab"));
                    assert!(!regex.is_match("abx"));
                }
                _ => {}
            }
        }
        walk(&sp.pattern, &mut found);
        assert!(found);
    }

    #[test]
    fn whole_traversal_capture_covers_match() {
        let plan = compile_plan(&parse("(?<rel> [word=a] >x [word=b])")).unwrap();
        assert_eq!(plan.captures.len(), 1);
        assert!(plan.captures[0].whole_match);
        assert_eq!(plan.captures[0].name, "rel");
    }

    #[test]
    fn assertion_endpoint_rejected() {
        let err = compile_plan(&parse("(?=[word=a]) >x [word=b]")).unwrap_err();
        assert!(matches!(err, PlanError::Invalid(_)));
    }
}

#[cfg(test)]
mod phrase_tests {
    use super::*;
    use rustie_query::QueryParser;

    fn span(q: &str) -> SpanEndpoint {
        let plan = compile_plan(
            &QueryParser::new()
                .parse_query(&format!("{q} >x []"))
                .unwrap(),
        )
        .unwrap();
        match plan.nodes.into_iter().next().unwrap() {
            Endpoint::Span(sp) => sp,
            _ => panic!("not a span"),
        }
    }

    fn shape(q: &str) -> (Vec<(usize, u32)>, usize) {
        let sp = span(q);
        let (terms, extras) = phrase_terms(&sp.pattern);
        (
            terms.iter().map(|t| (t.group, t.offset)).collect(),
            extras.len(),
        )
    }

    #[test]
    fn adjacent_tokens_form_one_phrase() {
        assert_eq!(
            shape("[word=a] [word=b] [word=c]"),
            (vec![(0, 0), (0, 1), (0, 2)], 0)
        );
    }

    #[test]
    fn wildcard_keeps_the_offset_but_adds_no_term() {
        assert_eq!(shape("[word=a] [] [word=c]"), (vec![(0, 0), (0, 2)], 0));
    }

    #[test]
    fn variable_length_element_starts_a_new_group() {
        // `[b]*` has unknown length, so [c] is not at a known offset from [a].
        assert_eq!(
            shape("[word=a] [word=b]* [word=c]"),
            (vec![(0, 0), (1, 0)], 0)
        );
    }

    #[test]
    fn plus_keeps_its_first_copy_then_breaks_the_run() {
        assert_eq!(
            shape("[word=a] [word=b]+ [word=c]"),
            (vec![(0, 0), (0, 1), (1, 0)], 0)
        );
    }

    #[test]
    fn fixed_count_repetition_advances_the_offset() {
        assert_eq!(
            shape("[word=a]{2,2} [word=b]"),
            (vec![(0, 0), (0, 1), (0, 2)], 0)
        );
    }

    #[test]
    fn assertions_occupy_no_token() {
        assert_eq!(
            shape("[word=a] (?=[word=x]) [word=b]"),
            (vec![(0, 0), (0, 1)], 0)
        );
    }
}
