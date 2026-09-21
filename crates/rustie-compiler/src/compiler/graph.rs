//! Graph traversal patterns → [`GraphCompiled`] (candidate filter + plan).

use crate::candidate::{CandidateFilter, FIELD_INCOMING_EDGES, FIELD_OUTGOING_EDGES};
use crate::compiler::doc_filter::DocFilter;
use crate::error::Result;
use crate::graph::nfa::{Dir, LabelPred};
use crate::graph::plan::{compile_plan, GraphPlanSpec};
use rustie_query::{Constraint, FlatPatternStep, Matcher, Pattern, Traversal};

/// Compiled graph query: Quickwit prefilter + in-memory [`GraphPlanSpec`].
#[derive(Debug, Clone)]
pub struct GraphCompiled {
    pub candidate: CandidateFilter,
    pub plan: GraphPlanSpec,
}

pub struct GraphCompiler;

/// What the hop next to an endpoint tells us about that endpoint's edges.
enum HopEdge {
    Labeled {
        outgoing: bool,
        matcher: EdgeMatcher,
    },
    TokenOnly,
}

#[derive(Debug, Clone)]
enum EdgeMatcher {
    Any,
    Exact(String),
    RegexPattern(String),
}

impl From<&Matcher> for EdgeMatcher {
    fn from(m: &Matcher) -> Self {
        match m {
            Matcher::String(s) => Self::Exact(s.clone()),
            Matcher::Regex { pattern, .. } => {
                let clean = pattern.trim_start_matches('/').trim_end_matches('/');
                Self::RegexPattern(clean.to_string())
            }
        }
    }
}

fn hop_edge(hop: &Traversal, is_first: bool) -> Option<HopEdge> {
    let labeled =
        |outgoing: bool, matcher: EdgeMatcher| Some(HopEdge::Labeled { outgoing, matcher });
    match hop {
        Traversal::Outgoing(m) => labeled(true, EdgeMatcher::from(m)),
        Traversal::Incoming(m) => labeled(false, EdgeMatcher::from(m)),
        Traversal::OutgoingWildcard => labeled(true, EdgeMatcher::Any),
        Traversal::IncomingWildcard => labeled(false, EdgeMatcher::Any),
        Traversal::Concatenated(steps) => {
            let step = if is_first {
                steps.first()
            } else {
                steps.last()
            }?;
            match step {
                Traversal::Outgoing(m) => labeled(true, EdgeMatcher::from(m)),
                Traversal::Incoming(m) => labeled(false, EdgeMatcher::from(m)),
                _ => None,
            }
        }
        Traversal::Optional(_) | Traversal::KleeneStar(_) => Some(HopEdge::TokenOnly),
        Traversal::Disjunctive(steps) => {
            let mut labels = Vec::new();
            let mut outgoing = None;
            for step in steps {
                let (dir, m) = match step {
                    Traversal::Outgoing(m) => (true, m),
                    Traversal::Incoming(m) => (false, m),
                    _ => return None,
                };
                if outgoing.is_some_and(|o| o != dir) {
                    return None;
                }
                outgoing = Some(dir);
                labels.push(match m {
                    Matcher::String(s) => regex::escape(s),
                    Matcher::Regex { pattern, .. } => pattern.clone(),
                });
            }
            let matcher = EdgeMatcher::RegexPattern(format!("({})", labels.join("|")));
            labeled(outgoing?, matcher)
        }
        _ => None,
    }
}

fn edge_field_name(outgoing: bool, is_first: bool) -> &'static str {
    // First token leaves by the hop's side; last token arrives on the opposite.
    if outgoing == is_first {
        FIELD_OUTGOING_EDGES
    } else {
        FIELD_INCOMING_EDGES
    }
}

fn flatten_graph_traversal_pattern(pattern: &Pattern, steps: &mut Vec<FlatPatternStep>) {
    match pattern {
        Pattern::GraphTraversal {
            src,
            traversal,
            dst,
        } => {
            flatten_graph_traversal_pattern(src, steps);
            flatten_traversal(traversal, steps);
            flatten_graph_traversal_pattern(dst, steps);
        }
        Pattern::Constraint(_)
        | Pattern::NamedCapture { .. }
        | Pattern::Repetition { .. }
        | Pattern::Assertion(_) => {
            steps.push(FlatPatternStep::Constraint(pattern.clone()));
        }
        Pattern::Concatenated(patterns) => {
            for p in patterns {
                flatten_graph_traversal_pattern(p, steps);
            }
        }
    }
}

fn flatten_traversal(traversal: &Traversal, steps: &mut Vec<FlatPatternStep>) {
    steps.push(FlatPatternStep::Traversal(traversal.clone()));
}

fn constraint_to_clauses(constraint: &Constraint) -> Option<Vec<(String, EdgeMatcher)>> {
    match constraint {
        Constraint::Field { name, matcher } => {
            Some(vec![(name.clone(), EdgeMatcher::from(matcher))])
        }
        Constraint::Conjunctive(constraints) => {
            let mut clauses = Vec::new();
            for c in constraints {
                clauses.extend(constraint_to_clauses(c)?);
            }
            Some(clauses)
        }
        Constraint::Disjunctive(constraints) => {
            let mut by_field: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            for c in constraints {
                match c {
                    Constraint::Field {
                        name,
                        matcher: Matcher::String(s),
                    } => {
                        by_field
                            .entry(name.clone())
                            .or_default()
                            .push(regex::escape(s));
                    }
                    Constraint::Field {
                        name,
                        matcher: Matcher::Regex { pattern, .. },
                    } => {
                        by_field
                            .entry(name.clone())
                            .or_default()
                            .push(pattern.clone());
                    }
                    _ => return None,
                }
            }
            Some(
                by_field
                    .into_iter()
                    .map(|(name, labels)| {
                        let matcher = if labels.len() == 1 {
                            let p = &labels[0];
                            if p.chars()
                                .all(|c| c.is_alphanumeric() || c == '_' || c == '-')
                            {
                                EdgeMatcher::Exact(p.clone())
                            } else {
                                EdgeMatcher::RegexPattern(p.clone())
                            }
                        } else {
                            EdgeMatcher::RegexPattern(format!("({})", labels.join("|")))
                        };
                        (name, matcher)
                    })
                    .collect(),
            )
        }
        _ => None,
    }
}

fn matcher_to_filter(field: &str, matcher: &EdgeMatcher) -> Option<CandidateFilter> {
    match matcher {
        EdgeMatcher::Any => None,
        EdgeMatcher::Exact(s) => Some(CandidateFilter::term(field, s)),
        EdgeMatcher::RegexPattern(p) => Some(CandidateFilter::regex(field, p)),
    }
}

impl GraphCompiler {
    pub fn compile_graph_traversal(&self, pattern: &Pattern) -> Result<GraphCompiled> {
        let plan = compile_plan(pattern)?;
        let candidate = self.build_candidate(pattern, &plan)?.normalize();
        Ok(GraphCompiled { candidate, plan })
    }

    fn build_candidate(&self, pattern: &Pattern, plan: &GraphPlanSpec) -> Result<CandidateFilter> {
        let mut flat_steps = Vec::new();
        let mut cur = pattern;
        while let Pattern::NamedCapture { pattern: inner, .. } = cur {
            cur = inner;
        }
        flatten_graph_traversal_pattern(cur, &mut flat_steps);

        let mut parts = Vec::new();
        if let Some(f) = self.endpoint_candidate(&flat_steps, true) {
            parts.push(f);
        }
        if let Some(f) = self.endpoint_candidate(&flat_steps, false) {
            parts.push(f);
        }

        // Span endpoints are full surface patterns: reuse the surface prefilter so their
        // adjacent-token runs become phrases too.
        for ep in &plan.nodes {
            if let crate::graph::plan::Endpoint::Span(sp) = ep {
                if let Some(f) = DocFilter::for_pattern(&sp.pattern)? {
                    parts.push(f);
                }
            }
        }

        // Hop first/last strict labels as edge postings.
        for req in &plan.hop_reqs {
            for (dir, pred) in req.first.iter().chain(req.last.iter()) {
                if let Some(f) = label_pred_filter(*dir, pred) {
                    parts.push(f);
                }
            }
        }

        Ok(match parts.len() {
            0 => CandidateFilter::All,
            1 => parts.pop().unwrap(),
            _ => CandidateFilter::And(parts),
        })
    }

    fn endpoint_candidate(
        &self,
        flat_steps: &[FlatPatternStep],
        is_first: bool,
    ) -> Option<CandidateFilter> {
        let constraint_indices: Vec<usize> = flat_steps
            .iter()
            .enumerate()
            .filter(|(_, s)| matches!(s, FlatPatternStep::Constraint(_)))
            .map(|(i, _)| i)
            .collect();
        let (constraint_step_idx, _) = if is_first {
            (*constraint_indices.first()?, 0)
        } else {
            (*constraint_indices.last()?, constraint_indices.len() - 1)
        };
        let traversal_step_idx = if is_first {
            constraint_step_idx + 1
        } else {
            constraint_step_idx.checked_sub(1)?
        };

        let clauses = self.endpoint_clauses(flat_steps.get(constraint_step_idx)?)?;
        let FlatPatternStep::Traversal(hop) = flat_steps.get(traversal_step_idx)? else {
            return None;
        };

        let mut filters = Vec::new();
        for (field, matcher) in &clauses {
            if let Some(f) = matcher_to_filter(field, matcher) {
                filters.push(f);
            }
        }

        match hop_edge(hop, is_first)? {
            HopEdge::TokenOnly => {
                if filters.is_empty() {
                    None
                } else if filters.len() == 1 {
                    filters.pop()
                } else {
                    Some(CandidateFilter::And(filters))
                }
            }
            HopEdge::Labeled { outgoing, matcher } => {
                if filters.is_empty() && matches!(matcher, EdgeMatcher::Any) {
                    return None;
                }
                let edge_field = edge_field_name(outgoing, is_first);
                if let Some(f) = matcher_to_filter(edge_field, &matcher) {
                    filters.push(f);
                }
                match filters.len() {
                    0 => None,
                    1 => filters.pop(),
                    _ => Some(CandidateFilter::And(filters)),
                }
            }
        }
    }

    fn endpoint_clauses(&self, step: &FlatPatternStep) -> Option<Vec<(String, EdgeMatcher)>> {
        let FlatPatternStep::Constraint(pat) = step else {
            return None;
        };
        Some(
            pat.as_token()
                .and_then(|(c, _)| constraint_to_clauses(c))
                .unwrap_or_default(),
        )
    }
}

fn label_pred_filter(dir: Dir, pred: &LabelPred) -> Option<CandidateFilter> {
    let field = match dir {
        Dir::Out => FIELD_OUTGOING_EDGES,
        Dir::In => FIELD_INCOMING_EDGES,
    };
    match pred {
        LabelPred::Any => None,
        LabelPred::Exact(s) => Some(CandidateFilter::term(field, s)),
        LabelPred::Alt(labels) => {
            if labels.is_empty() {
                None
            } else if labels.len() == 1 {
                Some(CandidateFilter::term(field, &labels[0]))
            } else {
                let alts: Vec<_> = labels
                    .iter()
                    .map(|l| CandidateFilter::term(field, l))
                    .collect();
                Some(CandidateFilter::Or(alts))
            }
        }
        LabelPred::Regex(re) => Some(CandidateFilter::regex(field, re.as_str())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustie_query::QueryParser;

    #[test]
    fn nsubj_builds_edge_and_token_candidate() {
        let pattern = QueryParser::new()
            .parse_query("[word=John] >nsubj [pos=VBZ]")
            .unwrap();
        let compiled = GraphCompiler.compile_graph_traversal(&pattern).unwrap();
        let q = compiled.candidate.to_quickwit_query();
        assert!(q.contains("word:John") || q.contains("John"), "{q}");
        assert!(
            q.contains("outgoing_edges") || q.contains("incoming_edges") || q.contains("nsubj"),
            "{q}"
        );
        assert_eq!(compiled.plan.nodes.len(), 2);
        assert_eq!(compiled.plan.hops.len(), 1);
    }

    #[test]
    fn hop_edge_sides() {
        let out = Traversal::Outgoing(Matcher::String("a".into()));
        match hop_edge(&out, true).unwrap() {
            HopEdge::Labeled { outgoing, .. } => assert!(outgoing),
            _ => panic!(),
        }
        assert_eq!(edge_field_name(true, true), FIELD_OUTGOING_EDGES);
        assert_eq!(edge_field_name(true, false), FIELD_INCOMING_EDGES);
    }
}
