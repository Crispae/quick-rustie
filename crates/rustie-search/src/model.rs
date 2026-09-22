//! Request / response types of the search API (also the JSON schema of the HTTP API).

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A RustIE / Odinson pattern plus paging controls.
#[derive(Debug, Clone, Deserialize, ToSchema)]
pub struct SearchQuery {
    /// Pattern, e.g. `[word=John] >nsubj [pos=VBZ]`.
    pub query: String,
    /// Maximum sentences to return (default 20, capped by the server).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Opaque `next_cursor` of a previous response to the *same* query: resumes right after
    /// its last hit.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Make `total_hits` exact. Every split then evaluates all of its candidates; otherwise the
    /// search may stop once the page is filled and `total_hits` is a lower bound.
    #[serde(default)]
    pub count: bool,
}

impl SearchQuery {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            limit: None,
            cursor: None,
            count: false,
        }
    }

    pub fn limit(mut self, limit: usize) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }

    pub fn count(mut self, count: bool) -> Self {
        self.count = count;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum QueryKind {
    /// Token pattern only.
    Surface,
    /// Contains a dependency-graph traversal.
    Graph,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct CaptureOut {
    pub name: String,
    pub start: usize,
    /// Exclusive.
    pub end: usize,
    pub text: String,
}

/// A matched token span (`end` exclusive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SpanOut {
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub captures: Vec<CaptureOut>,
}

/// One match inside a sentence: a single span for surface patterns, one span per
/// traversal endpoint (in pattern order) for graph patterns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct MatchOut {
    pub spans: Vec<SpanOut>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct SentenceHit {
    pub doc_id: String,
    pub sentence_id: String,
    pub sentence_length: u64,
    pub words: Vec<String>,
    pub matches: Vec<MatchOut>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SearchResults {
    pub query: String,
    pub kind: QueryKind,
    pub hits: Vec<SentenceHit>,
    /// Matching sentences in the index: exact when `total_is_exact`, else a lower bound.
    pub total_hits: u64,
    pub total_is_exact: bool,
    /// `true` when there is nothing after `hits`.
    pub exhausted: bool,
    /// Pass as `cursor` to get the next page. `None` once `exhausted`.
    pub next_cursor: Option<String>,
    pub took_ms: u64,
    pub timing: Timing,
}

/// Wall-clock split of one search, in microseconds.
#[derive(Debug, Clone, Default, Serialize, ToSchema)]
pub struct Timing {
    /// Quickwit search: matching inside the splits and fetching the page's documents.
    pub backend_us: u64,
    /// Rendering the matched spans of the page.
    pub render_us: u64,
}

/// Shape of every non-2xx JSON response.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ErrorBody {
    pub error: String,
}
