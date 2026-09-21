//! Query compilation: [`Pattern`] → [`CompiledQuery`].

mod doc_filter;
mod graph;
mod surface;

pub use graph::GraphCompiled;
pub use surface::SurfacePlan;

use crate::error::Result;
use rustie_query::{Pattern, QueryParser};

pub use graph::GraphCompiler;
pub use surface::SurfaceCompiler;

/// Typed result of compiling a [`Pattern`].
#[derive(Debug, Clone)]
pub enum CompiledQuery {
    Surface(SurfacePlan),
    Graph(GraphCompiled),
}

impl CompiledQuery {
    pub fn candidate(&self) -> &crate::candidate::CandidateFilter {
        match self {
            Self::Surface(s) => &s.candidate,
            Self::Graph(g) => &g.candidate,
        }
    }
}

fn wraps_traversal(mut p: &Pattern) -> bool {
    while let Pattern::NamedCapture { pattern, .. } = p {
        p = pattern;
    }
    matches!(p, Pattern::GraphTraversal { .. })
}

/// Unified compiler: routes each pattern shape to surface or graph execution.
pub struct QueryCompiler {
    surface: SurfaceCompiler,
    graph: GraphCompiler,
}

impl Default for QueryCompiler {
    fn default() -> Self {
        Self::new()
    }
}

impl QueryCompiler {
    pub fn new() -> Self {
        Self {
            surface: SurfaceCompiler,
            graph: GraphCompiler,
        }
    }

    /// Compile a pre-parsed [`Pattern`].
    pub fn compile_pattern(&self, pattern: &Pattern) -> Result<CompiledQuery> {
        match pattern {
            Pattern::GraphTraversal { .. } => {
                let g = self.graph.compile_graph_traversal(pattern)?;
                Ok(CompiledQuery::Graph(g))
            }
            Pattern::NamedCapture { .. } if wraps_traversal(pattern) => {
                let g = self.graph.compile_graph_traversal(pattern)?;
                Ok(CompiledQuery::Graph(g))
            }
            _ => self.surface.compile_pattern(pattern),
        }
    }

    /// Parse and compile a query string.
    pub fn compile(&self, query: &str) -> Result<CompiledQuery> {
        let pattern = QueryParser::new().parse_query(query)?;
        self.compile_pattern(&pattern)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::candidate::CandidateFilter;

    #[test]
    fn surface_term() {
        let c = QueryCompiler::new().compile("[word=hello]").unwrap();
        match c {
            CompiledQuery::Surface(s) => {
                assert!(matches!(
                    s.candidate,
                    CandidateFilter::Term { ref field, ref value }
                        if field == "word" && value == "hello"
                ));
            }
            _ => panic!("expected Surface"),
        }
    }

    #[test]
    fn graph_traversal() {
        let c = QueryCompiler::new()
            .compile("[word=John] >nsubj [pos=VBZ]")
            .unwrap();
        assert!(matches!(c, CompiledQuery::Graph(_)));
        let q = c.candidate().to_quickwit_query();
        assert_ne!(q, "*");
    }
}
