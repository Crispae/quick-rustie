//! Compile RustIE patterns into Quickwit candidate filters and in-memory plans.
//!
//! # Example
//!
//! ```
//! use rustie_compiler::QueryCompiler;
//!
//! let compiled = QueryCompiler::new()
//!     .compile("[word=John] >nsubj [pos=VBZ]")
//!     .unwrap();
//! assert!(matches!(compiled, rustie_compiler::CompiledQuery::Graph(_)));
//! ```

pub mod candidate;
pub mod compiler;
pub mod error;
pub mod graph;
pub mod matching;
pub mod types;

pub use candidate::CandidateFilter;
pub use compiler::{CompiledQuery, GraphCompiled, QueryCompiler, SurfacePlan};
pub use error::{CompileError, Result};
pub use graph::{
    compile_plan, evaluate_on_sentence, evaluate_with_fields, BoundPlan, BoundSurface, EvalScratch,
    ExternalLeaf, GraphPlanSpec, LeafSource, LeafTest, StringLeafSource, DEFAULT_SENTENCE_CAP,
};
pub use matching::{EndpointSpan, NodeTest, TokenSet};
pub use types::{NamedCapture, Span};
