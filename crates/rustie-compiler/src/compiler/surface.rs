//! Non-traversal patterns → [`SurfacePlan`].

use crate::candidate::CandidateFilter;
use crate::compiler::doc_filter::DocFilter;
use crate::compiler::CompiledQuery;
use crate::error::Result;
use rustie_query::Pattern;

/// Surface (non-graph) plan: Quickwit candidate filter + pattern for position match.
#[derive(Debug, Clone)]
pub struct SurfacePlan {
    pub candidate: CandidateFilter,
    pub pattern: Pattern,
    /// The candidate filter admits exactly the matching sentences: executors may skip the
    /// per-sentence check (see [`DocFilter::decides_match`]).
    pub exact: bool,
}

pub struct SurfaceCompiler;

impl SurfaceCompiler {
    pub fn compile_pattern(&self, pattern: &Pattern) -> Result<CompiledQuery> {
        if matches!(pattern, Pattern::GraphTraversal { .. }) {
            return Err(crate::error::CompileError::compile(
                "Graph traversal patterns should be handled by GraphCompiler",
            ));
        }
        let candidate = DocFilter::for_pattern(pattern)?
            .unwrap_or(CandidateFilter::All)
            .normalize();
        Ok(CompiledQuery::Surface(SurfacePlan {
            candidate,
            pattern: pattern.clone(),
            exact: DocFilter::decides_match(pattern),
        }))
    }
}
