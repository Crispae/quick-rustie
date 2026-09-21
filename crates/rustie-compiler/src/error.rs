//! Compile errors for `rustie-compiler`.

use thiserror::Error;

pub type Result<T> = std::result::Result<T, CompileError>;

#[derive(Debug, Error)]
pub enum CompileError {
    #[error("compile error: {0}")]
    Compile(String),

    #[error("unsupported pattern: {0}")]
    Unsupported(String),

    #[error(transparent)]
    Query(#[from] rustie_query::QueryError),

    #[error("graph plan: {0}")]
    Plan(String),
}

impl CompileError {
    pub fn compile(msg: impl Into<String>) -> Self {
        Self::Compile(msg.into())
    }

    pub fn unsupported(msg: impl Into<String>) -> Self {
        Self::Unsupported(msg.into())
    }
}

impl From<crate::graph::plan::PlanError> for CompileError {
    fn from(e: crate::graph::plan::PlanError) -> Self {
        Self::Plan(e.to_string())
    }
}
