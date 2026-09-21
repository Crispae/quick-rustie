//! Graph plan compilation and in-memory traversal (no GPH2).

pub mod eval;
pub mod nfa;
pub mod plan;
pub mod require;

pub use eval::{
    evaluate_on_sentence, evaluate_spans, evaluate_with_fields, FieldAccess, SpanTuple,
    DEFAULT_SENTENCE_CAP,
};
pub use nfa::{compile_hops, Dir, HopNfa, HopQuant, LabelPred};
pub use plan::{
    compile_plan, mandatory_constraints, CaptureMode, CaptureSpec, Endpoint, GraphPlanSpec,
    NodeMatcher, NodeTest, PlanError, SpanEndpoint,
};
