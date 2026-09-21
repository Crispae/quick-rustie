//! Request / response types of the search API (also the JSON schema of the HTTP API).

use serde::{Deserialize, Serialize};

/// A RustIE / Odinson pattern plus paging controls.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchQuery {
    /// Pattern, e.g. `[word=John] >nsubj [pos=VBZ]`.
    pub query: String,
    /// Maximum sentences to return (default 20, capped by the server).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Opaque `next_cursor` of a previous response to the *same* query: resumes right after
    /// the last candidate that response examined (no re-scan).
    #[serde(default)]
    pub cursor: Option<String>,
    /// Upper bound on candidate sentences examined in this call (capped by the server).
    #[serde(default)]
    pub max_candidates: Option<usize>,
    /// Also count every candidate in the index (`candidates_total`). Costs a full pass of the
    /// prefilter, so it is off by default.
    #[serde(default)]
    pub count: bool,
}

impl SearchQuery {
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            limit: None,
            cursor: None,
            max_candidates: None,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum QueryKind {
    /// Token pattern only.
    Surface,
    /// Contains a dependency-graph traversal.
    Graph,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaptureOut {
    pub name: String,
    pub start: usize,
    /// Exclusive.
    pub end: usize,
    pub text: String,
}

/// A matched token span (`end` exclusive).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SpanOut {
    pub start: usize,
    pub end: usize,
    pub text: String,
    pub captures: Vec<CaptureOut>,
}

/// One match inside a sentence: a single span for surface patterns, one span per
/// traversal endpoint (in pattern order) for graph patterns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MatchOut {
    pub spans: Vec<SpanOut>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SentenceHit {
    pub doc_id: String,
    pub sentence_id: String,
    pub sentence_length: u64,
    pub words: Vec<String>,
    pub matches: Vec<MatchOut>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SearchResults {
    pub query: String,
    pub kind: QueryKind,
    /// Quickwit prefilter that was executed (a superset of the matching sentences).
    pub candidate_query: serde_json::Value,
    pub hits: Vec<SentenceHit>,
    /// Sentences the prefilter matches in the whole index; only when `count` was requested.
    pub candidates_total: Option<u64>,
    /// Sentences fetched and evaluated in memory by this call.
    pub candidates_scanned: usize,
    /// `true` when every candidate has been examined: there is nothing after `hits`.
    pub exhausted: bool,
    /// `true` when this call stopped at `max_candidates` before filling the page; pass
    /// `next_cursor` to keep going.
    pub truncated: bool,
    /// Pass as `cursor` to get the next page. `None` once `exhausted`.
    pub next_cursor: Option<String>,
    /// Regex clauses dropped from the prefilter because the index engine cannot run them
    /// (results stay exact; more candidates are scanned).
    pub prefilter_relaxed_clauses: usize,
    pub took_ms: u64,
    /// Where `took_ms` went: Quickwit (prefilter + fetching stored candidates) versus decoding
    /// and exactly matching them in this process.
    pub timing: Timing,
}

/// Wall-clock split of one search, in microseconds.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Timing {
    pub backend_us: u64,
    pub match_us: u64,
}
