//! Pure matching core: token tests, token sets, and the span VM.

pub mod node_test;
pub mod span_vm;
pub mod tokenset;

pub use node_test::{NodeMatcher, NodeTest};
pub use span_vm::{EndpointSpan, SpanProg, MAX_ENDPOINT_SPANS};
pub use tokenset::TokenSet;
