//! Errors for schema flatten / validation.

use thiserror::Error;

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum SchemaError {
    #[error("{0}")]
    Validate(String),

    #[error("JSON error: {0}")]
    Json(String),
}

impl SchemaError {
    pub fn validate(msg: impl Into<String>) -> Self {
        Self::Validate(msg.into())
    }
}

impl From<serde_json::Error> for SchemaError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err.to_string())
    }
}

pub type Result<T> = std::result::Result<T, SchemaError>;
