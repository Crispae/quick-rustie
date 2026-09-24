//! Read-through Cachey wrapper for Quickwit S3 `.split` range reads.
//!
//! Searchers register [`storage_resolver`] over an existing
//! [`StorageResolver`](quickwit_storage::StorageResolver). Only
//! [`Storage::get_slice`](quickwit_storage::Storage::get_slice) on paths whose
//! extension is `split` goes through Cachey; writes, whole-file downloads, and
//! metastore objects stay on the inner S3 client.
//!
//! `get_slice_stream` on splits is a buffered `get_slice` wrapped in a cursor —
//! not real streaming (search does not use it for splits today).

mod client;
mod factory;
mod storage;

use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use quickwit_common::uri::Uri;
use quickwit_config::{S3StorageConfig, StorageBackend};
use quickwit_storage::{Storage, StorageResolver};
use reqwest::Client;
use tracing::{error, warn};
use url::Url;

use crate::factory::{CacheyS3StorageFactory, DelegatingFactory};

pub use client::{object_key, split_s3_uri};
pub use storage::CacheyStorage;

/// Configuration for the Cachey read-through layer (single URL, v1).
#[derive(Debug, Clone)]
pub struct CacheyConfig {
    pub url: Url,
    /// Extra space-separated `C0-Config` overrides appended after `fps=…`.
    pub c0_config: Option<String>,
    /// When true (default), transport / shed / misconfig failures fall back to
    /// direct S3. When false, those failures surface as errors.
    pub fallback: bool,
}

impl CacheyConfig {
    pub fn new(url: Url) -> Self {
        Self {
            url,
            c0_config: None,
            fallback: true,
        }
    }
}

/// Process-global counters. First-page hit/miss counts are approximate: over
/// HTTP/1.1 Cachey reports `C0-Status` for the first page of a multi-page read
/// only. Prefer Cachey's `/metrics` for authoritative S3 GET counts.
#[derive(Debug, Default)]
pub struct CacheyStats {
    pub requests: AtomicU64,
    pub bytes: AtomicU64,
    pub first_page_hits: AtomicU64,
    pub first_page_misses: AtomicU64,
    pub transport_fallbacks: AtomicU64,
    pub shed_fallbacks: AtomicU64,
    pub not_found: AtomicU64,
    pub misconfig: AtomicU64,
    pub errors: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheyStatsSnapshot {
    pub requests: u64,
    pub bytes: u64,
    pub first_page_hits: u64,
    pub first_page_misses: u64,
    pub transport_fallbacks: u64,
    pub shed_fallbacks: u64,
    pub not_found: u64,
    pub misconfig: u64,
    pub errors: u64,
}

impl CacheyStats {
    pub fn snapshot(&self) -> CacheyStatsSnapshot {
        CacheyStatsSnapshot {
            requests: self.requests.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            first_page_hits: self.first_page_hits.load(Ordering::Relaxed),
            first_page_misses: self.first_page_misses.load(Ordering::Relaxed),
            transport_fallbacks: self.transport_fallbacks.load(Ordering::Relaxed),
            shed_fallbacks: self.shed_fallbacks.load(Ordering::Relaxed),
            not_found: self.not_found.load(Ordering::Relaxed),
            misconfig: self.misconfig.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

impl CacheyStatsSnapshot {
    /// `self - earlier` (saturating).
    pub fn since(&self, earlier: &CacheyStatsSnapshot) -> CacheyStatsSnapshot {
        CacheyStatsSnapshot {
            requests: self.requests.saturating_sub(earlier.requests),
            bytes: self.bytes.saturating_sub(earlier.bytes),
            first_page_hits: self.first_page_hits.saturating_sub(earlier.first_page_hits),
            first_page_misses: self
                .first_page_misses
                .saturating_sub(earlier.first_page_misses),
            transport_fallbacks: self
                .transport_fallbacks
                .saturating_sub(earlier.transport_fallbacks),
            shed_fallbacks: self.shed_fallbacks.saturating_sub(earlier.shed_fallbacks),
            not_found: self.not_found.saturating_sub(earlier.not_found),
            misconfig: self.misconfig.saturating_sub(earlier.misconfig),
            errors: self.errors.saturating_sub(earlier.errors),
        }
    }
}

pub fn global_stats() -> &'static CacheyStats {
    static STATS: OnceLock<CacheyStats> = OnceLock::new();
    STATS.get_or_init(CacheyStats::default)
}

/// Breaker state shared by all `CacheyStorage` instances in the process.
#[derive(Debug)]
pub(crate) struct Breaker {
    inner: Mutex<BreakerInner>,
}

#[derive(Debug)]
struct BreakerInner {
    consecutive_transport_failures: u32,
    open_until: Option<Instant>,
    /// Set while the single half-open probe request is in flight.
    probe_started: Option<Instant>,
    last_warn: Option<Instant>,
    reason: &'static str,
}

/// A half-open probe that never reports back (e.g. its future was dropped by a query
/// timeout) is considered lost after this long, and another request may probe.
const PROBE_STALE_AFTER: Duration = Duration::from_secs(15);
/// Consecutive transport failures that open the breaker.
const TRANSPORT_FAILURE_THRESHOLD: u32 = 5;

pub(crate) enum BreakerKind {
    Transport,
    Shed,
    Misconfig,
}

impl Breaker {
    pub(crate) fn new() -> Self {
        Self {
            inner: Mutex::new(BreakerInner {
                consecutive_transport_failures: 0,
                open_until: None,
                probe_started: None,
                last_warn: None,
                reason: "",
            }),
        }
    }

    /// Returns true when Cachey should be skipped (straight to inner).
    ///
    /// Once the cooldown has elapsed the breaker is half-open: exactly one caller (the probe)
    /// gets `false`, everyone else keeps skipping Cachey until the probe reports through
    /// [`Breaker::record_success`] (closes) or [`Breaker::trip`] (reopens).
    pub(crate) fn is_open(&self) -> bool {
        let mut g = self.inner.lock().unwrap();
        let Some(until) = g.open_until else {
            return false;
        };
        let now = Instant::now();
        if now < until {
            return true;
        }
        match g.probe_started {
            Some(started) if now.duration_since(started) < PROBE_STALE_AFTER => true,
            _ => {
                g.probe_started = Some(now);
                self.warn_locked(&mut g, "cachey breaker half-open; probing with one request");
                false
            }
        }
    }

    pub(crate) fn record_success(&self) {
        let mut g = self.inner.lock().unwrap();
        g.consecutive_transport_failures = 0;
        g.open_until = None;
        g.probe_started = None;
    }

    pub(crate) fn trip(&self, kind: BreakerKind) {
        let mut g = self.inner.lock().unwrap();
        let cooldown = match kind {
            BreakerKind::Shed => {
                g.consecutive_transport_failures = 0;
                Duration::from_secs(2)
            }
            BreakerKind::Misconfig => {
                g.consecutive_transport_failures = 0;
                Duration::from_secs(60)
            }
            BreakerKind::Transport => {
                if g.probe_started.is_some() {
                    // A failed half-open probe reopens immediately.
                    g.consecutive_transport_failures = TRANSPORT_FAILURE_THRESHOLD;
                } else {
                    g.consecutive_transport_failures =
                        g.consecutive_transport_failures.saturating_add(1);
                }
                if g.consecutive_transport_failures < TRANSPORT_FAILURE_THRESHOLD {
                    return;
                }
                Duration::from_secs(10)
            }
        };
        g.probe_started = None;
        let reason = match kind {
            BreakerKind::Shed => "cachey 503 shed",
            BreakerKind::Misconfig => "cachey misconfig (404 but object exists on S3)",
            BreakerKind::Transport => "cachey transport failures",
        };
        g.reason = reason;
        g.open_until = Some(Instant::now() + cooldown);
        self.warn_locked(
            &mut g,
            &format!("cachey breaker open for {cooldown:?} ({reason})"),
        );
    }

    fn warn_locked(&self, g: &mut BreakerInner, msg: &str) {
        let now = Instant::now();
        let should = g
            .last_warn
            .map(|t| now.duration_since(t) >= Duration::from_secs(10))
            .unwrap_or(true);
        if should {
            g.last_warn = Some(now);
            warn!(target: "rustie_cachey", "{msg}");
        }
    }
}

pub(crate) fn shared_breaker() -> &'static Arc<Breaker> {
    static BREAKER: OnceLock<Arc<Breaker>> = OnceLock::new();
    BREAKER.get_or_init(|| Arc::new(Breaker::new()))
}

#[cfg(test)]
pub(crate) fn test_breaker() -> Arc<Breaker> {
    Arc::new(Breaker::new())
}

fn shared_client() -> &'static Client {
    static CLIENT: OnceLock<Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        Client::builder()
            .connect_timeout(Duration::from_secs(1))
            .timeout(Duration::from_secs(12))
            .pool_max_idle_per_host(32)
            .build()
            .expect("reqwest client")
    })
}

/// Wrap `base` so S3 `.split` `get_slice` reads go through Cachey.
///
/// File / Ram / Azure / Google resolve via `base` unchanged. S3 resolves via
/// `base` then wraps the result in [`CacheyStorage`].
pub fn storage_resolver(
    base: StorageResolver,
    s3: S3StorageConfig,
    cfg: CacheyConfig,
) -> StorageResolver {
    let client = shared_client().clone();
    let stats = global_stats();
    let breaker = Arc::clone(shared_breaker());
    StorageResolver::builder()
        .register(DelegatingFactory::new(StorageBackend::File, base.clone()))
        .register(DelegatingFactory::new(StorageBackend::Ram, base.clone()))
        .register(DelegatingFactory::new(StorageBackend::Azure, base.clone()))
        .register(DelegatingFactory::new(StorageBackend::Google, base.clone()))
        .register(CacheyS3StorageFactory::new(
            base, s3, cfg, client, stats, breaker,
        ))
        .build()
        .expect("storage factory backends should match")
}

/// Startup check against Cachey.
///
/// 1. `GET {url}/stats` — fatal when `!cfg.fallback`; otherwise warn and skip the probe.
/// 2. Optional footer probe: fetch `range` of `path` from Cachey **directly** (no fallback, no
///    breaker) and compare it with `direct`'s bytes. Any non-206 answer or a byte mismatch is
///    fatal regardless of `cfg.fallback`, with the kind and key in the message — a silent
///    fallback here would hide a Cachey pointed at the wrong S3 endpoint.
///
/// `direct` must be the unwrapped S3 storage for `index_uri`; `s3` is the S3 config the
/// wrapped resolver was built with (for `fps=true`).
pub async fn check(
    cfg: &CacheyConfig,
    s3: &S3StorageConfig,
    index_uri: &Uri,
    direct: Arc<dyn Storage>,
    probe: Option<(&Path, Range<usize>)>,
) -> anyhow::Result<()> {
    if let Err(err) = client::get_stats(shared_client(), &cfg.url).await {
        let msg = format!("cachey /stats unreachable at {}: {err}", cfg.url);
        if !cfg.fallback {
            bail!("{msg}");
        }
        // Cachey is down and fallback is on: searches will use S3 directly, nothing to probe.
        warn!(target: "rustie_cachey", "{msg}; skipping footer probe");
        return Ok(());
    }

    let Some((path, range)) = probe else {
        return Ok(());
    };

    let (bucket, prefix) = split_s3_uri(index_uri)
        .with_context(|| format!("cachey probe: cannot parse s3 uri {index_uri}"))?;
    let key = object_key(&prefix, path);
    let c0_config = client::build_c0_config(s3.force_path_style_access, cfg.c0_config.as_deref());
    let hint = "Check AWS_ENDPOINT_URL / credentials on the Cachey process and that C0-Config \
                includes fps=true for MinIO.";

    // Private stats: the probe must not skew the process-global counters.
    let stats = CacheyStats::default();
    let outcome = client::fetch_range(
        shared_client(),
        &cfg.url,
        &bucket,
        &key,
        range.clone(),
        c0_config.as_deref(),
        &stats,
    )
    .await;
    let via_cachey = match outcome {
        client::FetchOutcome::Ok { body, .. } => body,
        other => {
            error!(target: "rustie_cachey", kind = %bucket, key = %key, ?other, "cachey footer probe failed");
            bail!("cachey footer probe failed for kind={bucket} key={key}: {other:?}. {hint}");
        }
    };
    let via_direct = direct
        .get_slice(path, range)
        .await
        .with_context(|| format!("direct S3 footer probe failed for key={key}"))?;
    if via_cachey.as_ref() != via_direct.as_ref() {
        bail!(
            "cachey footer probe byte mismatch for kind={bucket} key={key} \
             (cachey {} bytes, s3 {} bytes). {hint}",
            via_cachey.len(),
            via_direct.len()
        );
    }
    Ok(())
}

/// Convenience: only the `/stats` health check (used by `rustie-node`).
pub async fn check_stats(cfg: &CacheyConfig) -> anyhow::Result<()> {
    match client::get_stats(shared_client(), &cfg.url).await {
        Ok(()) => Ok(()),
        Err(err) => {
            let msg = format!("cachey /stats unreachable at {}: {err}", cfg.url);
            if cfg.fallback {
                warn!(target: "rustie_cachey", "{msg}");
                Ok(())
            } else {
                bail!("{msg}")
            }
        }
    }
}

/// Whether `path` is an immutable Quickwit split file (not `.split.temp`).
pub fn is_split(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("split")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_split_rejects_temp_and_metastore() {
        assert!(is_split(Path::new("01ABC.split")));
        assert!(is_split(Path::new("dir/01ABC.split")));
        assert!(!is_split(Path::new("01ABC.split.temp")));
        assert!(!is_split(Path::new("metastore.json")));
        assert!(!is_split(Path::new("foo.split.bak")));
    }

    #[test]
    fn breaker_half_open_admits_one_probe() {
        let past = || {
            Instant::now()
                .checked_sub(Duration::from_millis(1))
                .unwrap()
        };
        let b = Breaker::new();
        b.trip(BreakerKind::Shed);
        assert!(b.is_open());

        // Cooldown over: exactly one caller probes, the rest keep skipping Cachey.
        b.inner.lock().unwrap().open_until = Some(past());
        assert!(!b.is_open());
        assert!(b.is_open());
        assert!(b.is_open());

        // A failed probe reopens immediately (no 5-failure grace).
        b.trip(BreakerKind::Transport);
        assert!(b.is_open());

        // A successful probe closes it for everyone.
        b.inner.lock().unwrap().open_until = Some(past());
        assert!(!b.is_open());
        b.record_success();
        assert!(!b.is_open());
        assert!(!b.is_open());
    }

    #[test]
    fn breaker_transport_needs_five_failures() {
        let b = Breaker::new();
        for _ in 0..4 {
            b.trip(BreakerKind::Transport);
            assert!(!b.is_open());
        }
        b.trip(BreakerKind::Transport);
        assert!(b.is_open());
    }

    #[test]
    fn stats_since_saturates() {
        let a = CacheyStatsSnapshot {
            requests: 10,
            bytes: 100,
            ..Default::default()
        };
        let b = CacheyStatsSnapshot {
            requests: 3,
            bytes: 200,
            ..Default::default()
        };
        let d = a.since(&b);
        assert_eq!(d.requests, 7);
        assert_eq!(d.bytes, 0);
    }
}

#[cfg(test)]
mod fake_cachey_tests;
