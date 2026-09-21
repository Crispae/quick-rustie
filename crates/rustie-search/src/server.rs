//! HTTP API over a [`Searcher`].
//!
//! | Route | |
//! | --- | --- |
//! | `GET /health` | liveness |
//! | `GET /v1/index` | published splits / docs from the metastore |
//! | `POST /v1/search` | JSON body [`SearchQuery`] |
//! | `GET /v1/search?q=…&limit=…&cursor=…&max_candidates=…&count=…` | same, for curl / browsers |
//!
//! There is no authentication: bind to loopback or put it behind a proxy.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;

use crate::error::SearchError;
use crate::model::SearchQuery;
use crate::searcher::Searcher;

#[derive(Clone)]
struct AppState {
    searcher: Arc<Searcher>,
    /// Bounds concurrent searches; each one fans out to Quickwit and holds decoded pages.
    permits: Arc<Semaphore>,
}

pub fn router(searcher: Arc<Searcher>, max_concurrent_searches: usize) -> Router {
    let state = AppState {
        searcher,
        permits: Arc::new(Semaphore::new(max_concurrent_searches.max(1))),
    };
    Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/v1/index", get(index_info))
        .route("/v1/search", get(search_get).post(search_post))
        .with_state(state)
}

impl IntoResponse for SearchError {
    fn into_response(self) -> Response {
        let status = match &self {
            SearchError::InvalidQuery(_) => StatusCode::BAD_REQUEST,
            SearchError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            SearchError::Backend(_) => StatusCode::BAD_GATEWAY,
        };
        (status, Json(json!({"error": self.to_string()}))).into_response()
    }
}

async fn index_info(State(state): State<AppState>) -> Result<Response, SearchError> {
    let summary = state.searcher.summary().await?;
    Ok(Json(json!({
        "index_id": state.searcher.options().index_id,
        "index_uid": summary.index_uid,
        "published_splits": summary.num_published_splits,
        "num_docs": summary.num_docs,
        "uncompressed_bytes": summary.uncompressed_bytes,
    }))
    .into_response())
}

async fn search_post(
    State(state): State<AppState>,
    Json(query): Json<SearchQuery>,
) -> Result<Response, SearchError> {
    run(state, query).await
}

#[derive(Deserialize)]
struct SearchParams {
    q: String,
    limit: Option<usize>,
    cursor: Option<String>,
    max_candidates: Option<usize>,
    #[serde(default)]
    count: bool,
}

async fn search_get(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> Result<Response, SearchError> {
    run(
        state,
        SearchQuery {
            query: params.q,
            limit: params.limit,
            cursor: params.cursor,
            max_candidates: params.max_candidates,
            count: params.count,
        },
    )
    .await
}

async fn run(state: AppState, query: SearchQuery) -> Result<Response, SearchError> {
    let _permit = state
        .permits
        .acquire()
        .await
        .map_err(|_| SearchError::Backend("server is shutting down".into()))?;
    Ok(Json(state.searcher.search(query).await?).into_response())
}
