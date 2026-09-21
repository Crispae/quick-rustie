//! RustIE query language: Odinson-inspired patterns over annotated text.
//!
//! Extracted from [RustIE](https://github.com/) `src/query` — grammar (`query.pest`),
//! AST, and the pest-backed [`QueryParser`]. Compilers that bind patterns to Tantivy
//! live outside this crate (they need index / graph integration).
//!
//! # Example
//!
//! ```
//! use rustie_query::QueryParser;
//!
//! let parser = QueryParser::new();
//! let pattern = parser.parse_query("[word=John] >nsubj [pos=VBZ]").unwrap();
//! assert!(matches!(pattern, rustie_query::Pattern::GraphTraversal { .. }));
//! ```

pub mod ast;
pub mod error;
pub mod parser;
pub mod pest_parser;

pub use ast::{
    Assertion, Constraint, FlatPatternStep, Matcher, Pattern, QuantifierKind, Traversal,
};
pub use error::{QueryError, Result};
pub use parser::QueryParser;
