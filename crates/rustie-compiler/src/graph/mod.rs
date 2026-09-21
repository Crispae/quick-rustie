//! Graph plan compilation and traversal: the string-based reference evaluator ([`eval`]) and
//! the dictionary-bound evaluator over GPH2 sentence views ([`bound`]).

pub mod bound;
pub mod dict;
pub mod eval;
pub mod nfa;
pub mod plan;
pub mod require;

pub use bound::{
    BoundPlan, BoundSurface, EvalScratch, ExternalLeaf, LeafSource, LeafTest, StringLeafSource,
};
pub use dict::{AttrSet, LabelSet};
pub use eval::{
    evaluate_on_sentence, evaluate_spans, evaluate_with_fields, FieldAccess, SpanTuple,
    DEFAULT_SENTENCE_CAP,
};
pub use nfa::{compile_hops, BoundHop, Dir, HopNfa, HopQuant, HopScratch, LabelPred};
pub use plan::{
    compile_plan, mandatory_constraints, CaptureMode, CaptureSpec, Endpoint, GraphPlanSpec,
    NodeMatcher, NodeTest, PlanError, SpanEndpoint,
};
