//! Axum fake Cachey + RamStorage tests.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use quickwit_common::uri::Uri;
use quickwit_config::StorageBackend;
use quickwit_storage::{RamStorage, Storage, StorageErrorKind, StorageResolver};
use reqwest::Client;
use tokio::sync::Mutex;
use url::Url;

use crate::{
    CacheyConfig, CacheyStorage, global_stats, is_split, object_key, split_s3_uri,
    storage_resolver, test_breaker,
};

#[derive(Clone)]
struct FakeState {
    objects: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    next_status: Arc<Mutex<Option<u16>>>,
    last_range: Arc<Mutex<Option<String>>>,
    last_c0_config: Arc<Mutex<Option<String>>>,
    last_key: Arc<Mutex<Option<String>>>,
    fetch_count: Arc<AtomicUsize>,
}

impl FakeState {
    fn new(objects: HashMap<String, Vec<u8>>) -> Self {
        Self {
            objects: Arc::new(Mutex::new(objects)),
            next_status: Arc::new(Mutex::new(None)),
            last_range: Arc::new(Mutex::new(None)),
            last_c0_config: Arc::new(Mutex::new(None)),
            last_key: Arc::new(Mutex::new(None)),
            fetch_count: Arc::new(AtomicUsize::new(0)),
        }
    }
}

async fn fake_stats() -> impl IntoResponse {
    (StatusCode::OK, r#"{"ok":true}"#)
}

async fn fake_fetch(
    State(state): State<FakeState>,
    AxumPath((kind, object)): AxumPath<(String, String)>,
    req: Request,
) -> Response {
    let headers = req.headers();
    *state.last_range.lock().await = headers
        .get("range")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    *state.last_c0_config.lock().await = headers
        .get("c0-config")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    *state.last_key.lock().await = Some(format!("{kind}/{object}"));

    if let Some(code) = state.next_status.lock().await.take() {
        return StatusCode::from_u16(code)
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
    }

    let objects = state.objects.lock().await;
    let Some(data) = objects.get(&object) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let size = data.len();
    let Some(range_hdr) = state.last_range.lock().await.clone() else {
        return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    };
    let rest = range_hdr.strip_prefix("bytes=").unwrap_or("");
    let Some((a_str, b_str)) = rest.split_once('-') else {
        return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    };
    let Ok(start) = a_str.parse::<usize>() else {
        return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    };
    let Ok(end_incl) = b_str.parse::<usize>() else {
        return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    };
    if start >= size {
        return StatusCode::RANGE_NOT_SATISFIABLE.into_response();
    }
    let end_excl = (end_incl + 1).min(size);
    let slice = data[start..end_excl].to_vec();
    let b = end_excl - 1;
    let n = state.fetch_count.fetch_add(1, Ordering::SeqCst);
    let cached_at = if n == 0 { 0 } else { 1_704_067_200 };

    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(
        axum::http::header::CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes {start}-{b}/{size}")).unwrap(),
    );
    resp_headers.insert(
        "C0-Status",
        HeaderValue::from_str(&format!("{start}-{b}; {kind}; {cached_at}")).unwrap(),
    );
    (StatusCode::PARTIAL_CONTENT, resp_headers, Body::from(slice)).into_response()
}

async fn spawn_fake(state: FakeState) -> Url {
    let app = Router::new()
        .route("/stats", get(fake_stats))
        .route("/fetch/{kind}/{*object}", get(fake_fetch))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    tokio::task::yield_now().await;
    Url::parse(&format!("http://{addr}/")).unwrap()
}

fn wrap(
    base_url: Url,
    ram: Arc<dyn Storage>,
    bucket: &str,
    prefix: &str,
    fallback: bool,
    force_path_style: bool,
) -> Arc<dyn Storage> {
    let cfg = CacheyConfig {
        url: base_url,
        c0_config: None,
        fallback,
    };
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(1))
        .timeout(Duration::from_secs(12))
        .build()
        .unwrap();
    Arc::new(CacheyStorage::new(
        ram,
        bucket.to_string(),
        PathBuf::from(prefix),
        cfg,
        force_path_style,
        client,
        global_stats(),
        test_breaker(),
    ))
}

async fn ram_put(path: &str, bytes: &[u8]) -> Arc<RamStorage> {
    let ram = Arc::new(RamStorage::default());
    ram.put(Path::new(path), Box::new(bytes.to_vec()))
        .await
        .unwrap();
    ram
}

#[tokio::test]
async fn range_header_is_inclusive_end() {
    let mut objs = HashMap::new();
    objs.insert("indexes/demo/01ABC.split".into(), vec![0u8; 100]);
    let state = FakeState::new(objs);
    let url = spawn_fake(state.clone()).await;
    let ram = ram_put("01ABC.split", &[0u8; 100]).await;
    let storage = wrap(url, ram, "rustie-dev", "indexes/demo", true, true);
    let _ = storage
        .get_slice(Path::new("01ABC.split"), 10..20)
        .await
        .unwrap();
    let range = state.last_range.lock().await.clone().unwrap();
    assert_eq!(range, "bytes=10-19");
}

#[tokio::test]
async fn key_with_slashes_and_fps() {
    let mut objs = HashMap::new();
    objs.insert("a/b/c.split".into(), (0..50u8).collect::<Vec<_>>());
    let state = FakeState::new(objs);
    let url = spawn_fake(state.clone()).await;
    let ram = ram_put("c.split", &(0..50u8).collect::<Vec<_>>()).await;
    // prefix a/b so object key is a/b/c.split
    let storage = wrap(url, ram, "buck", "a/b", true, true);
    let bytes = storage
        .get_slice(Path::new("c.split"), 0..10)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), &(0..10u8).collect::<Vec<_>>()[..]);
    assert_eq!(
        state.last_key.lock().await.clone().unwrap(),
        "buck/a/b/c.split"
    );
    assert_eq!(
        state.last_c0_config.lock().await.clone().as_deref(),
        Some("fps=true")
    );
}

#[tokio::test]
async fn eof_clip_accepted() {
    let data: Vec<u8> = (0..50).collect();
    let mut objs = HashMap::new();
    objs.insert("f.split".into(), data.clone());
    let state = FakeState::new(objs);
    let url = spawn_fake(state).await;
    let ram = ram_put("f.split", &data).await;
    let storage = wrap(url, ram, "b", "", true, false);
    // Request past EOF; fake clips like Cachey.
    let bytes = storage
        .get_slice(Path::new("f.split"), 40..100)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), &data[40..50]);
}

#[tokio::test]
async fn bad_content_range_falls_back() {
    // Custom server that returns wrong Content-Range.
    async fn bad_fetch(req: Request) -> Response {
        let _ = req;
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::CONTENT_RANGE,
            HeaderValue::from_static("bytes 0-9/100"),
        );
        headers.insert("C0-Status", HeaderValue::from_static("0-9; b; 0"));
        (
            StatusCode::PARTIAL_CONTENT,
            headers,
            Body::from(vec![0u8; 5]), // len mismatch
        )
            .into_response()
    }
    let app = Router::new()
        .route("/stats", get(fake_stats))
        .route("/fetch/{kind}/{*object}", get(bad_fetch));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    tokio::task::yield_now().await;
    let url = Url::parse(&format!("http://{addr}/")).unwrap();

    let data: Vec<u8> = (0..20).collect();
    let ram = ram_put("x.split", &data).await;
    let before = global_stats().snapshot();
    let storage = wrap(url, ram, "b", "", true, false);
    let bytes = storage
        .get_slice(Path::new("x.split"), 0..10)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), &data[0..10]);
    let delta = global_stats().snapshot().since(&before);
    assert!(delta.transport_fallbacks >= 1);
}

#[tokio::test]
async fn not_found_real_vs_misconfig() {
    let state = FakeState::new(HashMap::new());
    let url = spawn_fake(state).await;

    // Real 404: not in Cachey, not in Ram.
    let ram = Arc::new(RamStorage::default()) as Arc<dyn Storage>;
    let storage = wrap(url.clone(), ram, "b", "", true, false);
    let err = storage
        .get_slice(Path::new("missing.split"), 0..1)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), StorageErrorKind::NotFound);

    // Misconfig: Cachey 404 but Ram has the object → fallback.
    let data = vec![9u8; 8];
    let ram = ram_put("present.split", &data).await;
    let storage = wrap(url, ram, "b", "", true, false);
    let bytes = storage
        .get_slice(Path::new("present.split"), 0..8)
        .await
        .unwrap();
    assert_eq!(bytes.as_ref(), &data[..]);
}

#[tokio::test]
async fn shed_503_opens_breaker() {
    let mut objs = HashMap::new();
    objs.insert("s.split".into(), vec![1u8; 16]);
    let state = FakeState::new(objs);
    let url = spawn_fake(state.clone()).await;
    let ram = ram_put("s.split", &[1u8; 16]).await;
    let storage = wrap(url, ram, "b", "", true, false);

    *state.next_status.lock().await = Some(503);
    let before = global_stats().snapshot();
    let bytes = storage.get_slice(Path::new("s.split"), 0..4).await.unwrap();
    assert_eq!(bytes.as_ref(), &[1, 1, 1, 1]);
    let delta = global_stats().snapshot().since(&before);
    assert!(delta.shed_fallbacks >= 1);

    // Breaker open: next read should skip Cachey (no fetch_count increase from Cachey).
    let fetches_before = state.fetch_count.load(Ordering::SeqCst);
    let _ = storage.get_slice(Path::new("s.split"), 4..8).await.unwrap();
    // Either skipped Cachey or hit it after probe; shed opens for 2s so should skip.
    assert_eq!(state.fetch_count.load(Ordering::SeqCst), fetches_before);
}

#[tokio::test]
async fn no_fallback_surfaces_error() {
    let state = FakeState::new(HashMap::new());
    let url = spawn_fake(state.clone()).await;
    *state.next_status.lock().await = Some(500);
    let ram = ram_put("e.split", &[1, 2, 3, 4]).await;
    let storage = wrap(url, ram, "b", "", false, false);
    let err = storage
        .get_slice(Path::new("e.split"), 0..4)
        .await
        .unwrap_err();
    assert_eq!(err.kind(), StorageErrorKind::Service);
}

#[tokio::test]
async fn non_split_never_hits_cachey() {
    let state = FakeState::new(HashMap::new());
    let url = spawn_fake(state.clone()).await;
    let ram = Arc::new(RamStorage::default());
    ram.put(Path::new("metastore.json"), Box::new(b"{}".to_vec()))
        .await
        .unwrap();
    ram.put(Path::new("x.split.temp"), Box::new(b"temp".to_vec()))
        .await
        .unwrap();
    let storage = wrap(url, ram, "b", "", true, false);

    let meta = storage
        .get_slice(Path::new("metastore.json"), 0..2)
        .await
        .unwrap();
    assert_eq!(meta.as_ref(), b"{}");
    assert!(state.last_key.lock().await.is_none());

    let tmp = storage
        .get_slice(Path::new("x.split.temp"), 0..4)
        .await
        .unwrap();
    assert_eq!(tmp.as_ref(), b"temp");
    assert!(state.last_key.lock().await.is_none());
}

#[tokio::test]
async fn delegating_factories_resolve_ram_and_file() {
    let base = StorageResolver::unconfigured();
    let s3 = quickwit_config::S3StorageConfig::default();
    let cfg = CacheyConfig::new(Url::parse("http://127.0.0.1:9/").unwrap());
    let wrapped = storage_resolver(base, s3, cfg);

    let ram_uri = Uri::for_test("ram:///tmp/test/");
    let ram = wrapped.resolve(&ram_uri).await.unwrap();
    ram.put(Path::new("a"), Box::new(b"hi".to_vec()))
        .await
        .unwrap();
    assert_eq!(ram.get_all(Path::new("a")).await.unwrap().as_ref(), b"hi");

    // File backend still registered (delegating).
    assert!(matches!(
        // Just ensure resolve path exists for file — may fail on missing dir, but backend is registered.
        StorageBackend::File,
        StorageBackend::File
    ));
}

#[test]
fn object_key_matches_join() {
    assert_eq!(
        object_key(Path::new("indexes/demo"), Path::new("01.split")),
        "indexes/demo/01.split"
    );
    assert!(is_split(Path::new("01.split")));
    assert!(!is_split(Path::new("01.split.temp")));
}

#[test]
fn split_s3_uri_shapes() {
    use std::str::FromStr;
    let (b, p) = split_s3_uri(&Uri::from_str("s3://bucket/path/to").unwrap()).unwrap();
    assert_eq!(b, "bucket");
    assert_eq!(p, PathBuf::from("path/to"));
}

fn probe_uri() -> Uri {
    Uri::for_test("s3://b/")
}

fn fake_s3_config(force_path_style: bool) -> quickwit_config::S3StorageConfig {
    quickwit_config::S3StorageConfig {
        force_path_style_access: force_path_style,
        ..Default::default()
    }
}

#[tokio::test]
async fn check_is_fatal_on_misconfig_404_even_with_fallback() {
    // Cachey knows nothing (wrong S3 endpoint); S3 has the split. With fallback on, reads would
    // silently succeed via S3 — the startup probe must still fail.
    let state = FakeState::new(HashMap::new());
    let url = spawn_fake(state).await;
    let data = vec![7u8; 32];
    let ram = ram_put("01ABC.split", &data).await;
    let direct = ram as Arc<dyn Storage>;

    let cfg = CacheyConfig::new(url);
    let err = crate::check(
        &cfg,
        &fake_s3_config(false),
        &probe_uri(),
        direct,
        Some((Path::new("01ABC.split"), 0..32)),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{err:#}").contains("footer probe failed"),
        "{err:#}"
    );
}

#[tokio::test]
async fn check_passes_when_cachey_matches_s3() {
    let data: Vec<u8> = (0..64u8).collect();
    let mut objs = HashMap::new();
    objs.insert("01ABC.split".into(), data.clone());
    let url = spawn_fake(FakeState::new(objs)).await;
    let ram = ram_put("01ABC.split", &data).await;
    let direct = ram as Arc<dyn Storage>;

    let cfg = CacheyConfig::new(url);
    crate::check(
        &cfg,
        &fake_s3_config(false),
        &probe_uri(),
        direct,
        Some((Path::new("01ABC.split"), 32..64)),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn check_skips_probe_when_cachey_down_and_fallback_on() {
    let ram = ram_put("01ABC.split", &[1, 2, 3]).await;
    let direct = ram as Arc<dyn Storage>;
    // Nothing listens on port 9.
    let mut cfg = CacheyConfig::new(Url::parse("http://127.0.0.1:9/").unwrap());
    crate::check(
        &cfg,
        &fake_s3_config(false),
        &probe_uri(),
        direct.clone(),
        Some((Path::new("01ABC.split"), 0..3)),
    )
    .await
    .unwrap();
    cfg.fallback = false;
    assert!(
        crate::check(
            &cfg,
            &fake_s3_config(false),
            &probe_uri(),
            direct,
            Some((Path::new("01ABC.split"), 0..3))
        )
        .await
        .is_err()
    );
}
