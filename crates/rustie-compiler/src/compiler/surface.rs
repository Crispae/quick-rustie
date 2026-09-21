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
        }))
    }
}
