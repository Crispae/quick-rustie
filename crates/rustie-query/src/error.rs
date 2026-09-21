//! Parse / query-language errors for `rustie-query`.

use thiserror::Error;

/// Errors produced while parsing or validating a RustIE query string.
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum QueryError {
    /// Query string could not be parsed (syntax error).
    #[error("{0}")]
    Parse(String),

    /// Query uses a feature that is not supported by this crate.
    #[error("{0}")]
    Unsupported(String),
}

impl QueryError {
    pub fn parse(msg: impl Into<String>) -> Self {
        Self::Parse(msg.into())
    }

    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self::Unsupported(msg.into())
    }
}

impl From<pest::error::Error<crate::pest_parser::Rule>> for QueryError {
    fn from(err: pest::error::Error<crate::pest_parser::Rule>) -> Self {
        Self::Parse(format!("Parse error: {err}"))
    }
}

/// Result alias for query parsing.
pub type Result<T> = std::result::Result<T, QueryError>;
