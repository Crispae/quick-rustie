//! HTTP API over a [`Searcher`].
//!
//! | Route | |
//! | --- | --- |
//! | `GET /health` | liveness |
//! | `GET /v1/index` | published splits / docs from the metastore |
//! | `POST /v1/search` | JSON body [`SearchQuery`] |
//! | `GET /v1/search?q=…&limit=…&cursor=…&count=…` | same, for curl / browsers |
//! | `GET /swagger-ui` | interactive API docs (OpenAPI JSON at `/api-docs/openapi.json`) |
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
use utoipa::OpenApi;
use utoipa_swagger_ui::SwaggerUi;

use crate::error::SearchError;
use crate::model::{ErrorBody, SearchQuery, SearchResults};
use crate::searcher::Searcher;

#[derive(Clone)]
struct AppState {
    searcher: Arc<Searcher>,
    /// Bounds concurrent searches; each one fans out to Quickwit and holds decoded pages.
    permits: Arc<Semaphore>,
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "RustIE search API",
        description = "Run RustIE / Odinson patterns against the IE postings index on Quickwit."
    ),
    paths(health, index_info, search_get, search_post),
    components(schemas(SearchQuery, SearchResults, ErrorBody))
)]
struct ApiDoc;

pub fn router(searcher: Arc<Searcher>, max_concurrent_searches: usize) -> Router {
    let state = AppState {
        searcher,
        permits: Arc::new(Semaphore::new(max_concurrent_searches.max(1))),
    };
    Router::new()
        .route("/health", get(health))
        .route("/v1/index", get(index_info))
        .route("/v1/search", get(search_get).post(search_post))
        .merge(SwaggerUi::new("/swagger-ui").url("/api-docs/openapi.json", ApiDoc::openapi()))
        .with_state(state)
}

#[utoipa::path(
    get,
    path = "/health",
    tag = "meta",
    responses((status = 200, description = "Server is up", body = serde_json::Value))
)]
async fn health() -> Json<serde_json::Value> {
    Json(json!({"status": "ok"}))
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

#[utoipa::path(
    get,
    path = "/v1/index",
    tag = "index",
    responses(
        (status = 200, description = "Metastore summary of the index", body = serde_json::Value),
        (status = 502, description = "Metastore or storage error", body = ErrorBody),
    )
)]
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

#[utoipa::path(
    post,
    path = "/v1/search",
    tag = "search",
    request_body = SearchQuery,
    responses(
        (status = 200, description = "Search results", body = SearchResults),
        (status = 400, description = "Invalid query", body = ErrorBody),
        (status = 504, description = "Search timed out", body = ErrorBody),
        (status = 502, description = "Search backend error", body = ErrorBody),
    )
)]
async fn search_post(
    State(state): State<AppState>,
    Json(query): Json<SearchQuery>,
) -> Result<Response, SearchError> {
    run(state, query).await
}

#[derive(Deserialize, utoipa::IntoParams)]
struct SearchParams {
    /// RustIE / Odinson pattern, e.g. `[word=John] >nsubj [pos=VBZ]`.
    q: String,
    /// Maximum sentences to return (default 20, capped by the server).
    limit: Option<usize>,
    /// Opaque `next_cursor` of a previous response to the *same* query.
    cursor: Option<String>,
    /// Make `total_hits` exact.
    #[serde(default)]
    count: bool,
}

#[utoipa::path(
    get,
    path = "/v1/search",
    tag = "search",
    params(SearchParams),
    responses(
        (status = 200, description = "Search results", body = SearchResults),
        (status = 400, description = "Invalid query", body = ErrorBody),
        (status = 504, description = "Search timed out", body = ErrorBody),
        (status = 502, description = "Search backend error", body = ErrorBody),
    )
)]
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
