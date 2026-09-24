//! HTTP client for Cachey's `/fetch` API.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Context, bail};
use bytes::Bytes;
use quickwit_common::uri::Uri;
use regex::Regex;
use reqwest::Client;
use reqwest::header::{HeaderMap, HeaderValue, RANGE};
use url::Url;

use crate::CacheyStats;

/// Same regex as Quickwit's `parse_s3_uri` (not re-exported from `quickwit_storage`).
pub fn split_s3_uri(uri: &Uri) -> Option<(String, PathBuf)> {
    static S3_URI_PTN: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"s3(\+[^:]+)?://(?P<bucket>[^/]+)(/(?P<prefix>.+))?").expect("s3 uri regex")
    });
    let captures = S3_URI_PTN.captures(uri.as_str())?;
    let bucket = captures.name("bucket")?.as_str().to_string();
    let prefix = captures
        .name("prefix")
        .map(|m| PathBuf::from(m.as_str()))
        .unwrap_or_default();
    Some((bucket, prefix))
}

/// Object key matching `S3CompatibleObjectStorage::key`: `prefix.join(path)`.
pub fn object_key(prefix: &Path, path: &Path) -> String {
    prefix.join(path).to_string_lossy().into_owned()
}

pub(crate) fn build_c0_config(force_path_style: bool, extra: Option<&str>) -> Option<String> {
    let mut parts = Vec::new();
    if force_path_style {
        parts.push("fps=true".to_string());
    }
    if let Some(extra) = extra {
        let trimmed = extra.trim();
        if !trimmed.is_empty() {
            parts.push(trimmed.to_string());
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

pub(crate) fn fetch_url(base: &Url, bucket: &str, key: &str) -> anyhow::Result<Url> {
    let existing: Vec<String> = base
        .path()
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    let mut url = base.clone();
    {
        let mut segs = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("cachey base url cannot be a base"))?;
        segs.clear();
        for s in &existing {
            segs.push(s);
        }
        segs.push("fetch");
        segs.push(bucket);
        for part in key.split('/') {
            if !part.is_empty() {
                segs.push(part);
            }
        }
    }
    Ok(url)
}

#[derive(Debug)]
pub(crate) enum FetchOutcome {
    Ok {
        body: Bytes,
        #[allow(dead_code)]
        first_page_cached_at: Option<u64>,
    },
    NotFound,
    RangeNotSatisfiable,
    Shed,
    /// Transport / 5xx / 409 / protocol / short body — candidates for fallback.
    Retryable(anyhow::Error),
}

pub(crate) async fn fetch_range(
    client: &Client,
    base: &Url,
    bucket: &str,
    key: &str,
    range: Range<usize>,
    c0_config: Option<&str>,
    stats: &CacheyStats,
) -> FetchOutcome {
    if range.start >= range.end {
        return FetchOutcome::Ok {
            body: Bytes::new(),
            first_page_cached_at: None,
        };
    }
    let url = match fetch_url(base, bucket, key) {
        Ok(u) => u,
        Err(err) => return FetchOutcome::Retryable(err),
    };
    let inclusive_end = range.end - 1;
    let range_header = format!("bytes={}-{}", range.start, inclusive_end);

    let mut headers = HeaderMap::new();
    headers.insert(RANGE, HeaderValue::from_str(&range_header).unwrap());
    if let Some(cfg) = c0_config
        && let Ok(v) = HeaderValue::from_str(cfg)
    {
        headers.insert("C0-Config", v);
    }

    stats
        .requests
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let resp = match client.get(url).headers(headers).send().await {
        Ok(r) => r,
        Err(err) => return FetchOutcome::Retryable(err.into()),
    };

    let status = resp.status().as_u16();
    match status {
        404 => return FetchOutcome::NotFound,
        416 => return FetchOutcome::RangeNotSatisfiable,
        503 => return FetchOutcome::Shed,
        206 => {}
        409 | 500 | 504 => {
            return FetchOutcome::Retryable(anyhow::anyhow!("cachey HTTP {status}"));
        }
        other => {
            return FetchOutcome::Retryable(anyhow::anyhow!("unexpected cachey HTTP {other}"));
        }
    }

    let content_range = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let c0_status = resp
        .headers()
        .get("C0-Status")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let body = match resp.bytes().await {
        Ok(b) => b,
        Err(err) => return FetchOutcome::Retryable(err.into()),
    };

    if let Err(err) =
        validate_content_range(content_range.as_deref(), range.start, range.end, body.len())
    {
        return FetchOutcome::Retryable(err);
    }

    let cached_at = parse_c0_status_cached_at(c0_status.as_deref());
    if let Some(at) = cached_at {
        if at == 0 {
            stats
                .first_page_misses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        } else {
            stats
                .first_page_hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
    stats
        .bytes
        .fetch_add(body.len() as u64, std::sync::atomic::Ordering::Relaxed);

    FetchOutcome::Ok {
        body,
        first_page_cached_at: cached_at,
    }
}

/// `Content-Range: bytes a-b/size` must satisfy `a == start`, `b == min(end, size)-1`,
/// and `body.len() == b - a + 1`.
fn validate_content_range(
    header: Option<&str>,
    start: usize,
    end: usize,
    body_len: usize,
) -> anyhow::Result<()> {
    let header = header.context("missing Content-Range")?;
    let rest = header
        .strip_prefix("bytes ")
        .context("Content-Range missing 'bytes '")?;
    let (range_part, size_part) = rest
        .split_once('/')
        .context("Content-Range missing /size")?;
    let (a_str, b_str) = range_part
        .split_once('-')
        .context("Content-Range missing a-b")?;
    let a: usize = a_str.parse().context("Content-Range a")?;
    let b: usize = b_str.parse().context("Content-Range b")?;
    let size: usize = size_part.parse().context("Content-Range size")?;
    if a != start {
        bail!("Content-Range start {a} != requested {start}");
    }
    let expected_b = end.min(size).saturating_sub(1);
    // When start >= size, Cachey should have returned 416; treat bad shape as protocol error.
    if start >= size {
        bail!("Content-Range for start past EOF");
    }
    if b != expected_b {
        bail!("Content-Range end {b} != expected {expected_b} (end={end}, size={size})");
    }
    let expected_len = b - a + 1;
    if body_len != expected_len {
        bail!("body len {body_len} != Content-Range len {expected_len}");
    }
    Ok(())
}

/// `C0-Status: {first}-{last}; {bucket}; {cached_at}`
fn parse_c0_status_cached_at(header: Option<&str>) -> Option<u64> {
    let header = header?;
    let parts: Vec<&str> = header.split(';').map(str::trim).collect();
    if parts.len() < 3 {
        return None;
    }
    parts[2].parse().ok()
}

pub(crate) async fn get_stats(client: &Client, base: &Url) -> anyhow::Result<()> {
    let url = base.join("/stats").context("join /stats")?;
    let resp = client.get(url).send().await.context("GET /stats")?;
    if !resp.status().is_success() {
        bail!("GET /stats HTTP {}", resp.status());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_s3_uri_matches_quickwit_shapes() {
        let (b, p) = split_s3_uri(&Uri::from_str("s3://bucket/path/to/object").unwrap()).unwrap();
        assert_eq!(b, "bucket");
        assert_eq!(p, PathBuf::from("path/to/object"));

        let (b, p) = split_s3_uri(&Uri::from_str("s3://bucket/path").unwrap()).unwrap();
        assert_eq!(b, "bucket");
        assert_eq!(p, PathBuf::from("path"));

        let (b, p) = split_s3_uri(&Uri::from_str("s3://bucket/").unwrap()).unwrap();
        assert_eq!(b, "bucket");
        assert!(p.as_os_str().is_empty());

        let (b, p) = split_s3_uri(&Uri::from_str("s3://bucket").unwrap()).unwrap();
        assert_eq!(b, "bucket");
        assert!(p.as_os_str().is_empty());
    }

    #[test]
    fn object_key_joins_like_quickwit() {
        assert_eq!(
            object_key(Path::new("indexes/demo"), Path::new("01ABC.split")),
            "indexes/demo/01ABC.split"
        );
        assert_eq!(
            object_key(Path::new(""), Path::new("01ABC.split")),
            "01ABC.split"
        );
        // Trailing slash in prefix string becomes a path component join.
        assert_eq!(
            object_key(Path::new("indexes/demo/"), Path::new("01ABC.split")),
            Path::new("indexes/demo/")
                .join("01ABC.split")
                .to_string_lossy()
        );
    }

    #[test]
    fn fetch_url_encodes_segments() {
        let base = Url::parse("http://127.0.0.1:9020/").unwrap();
        let url = fetch_url(&base, "rustie-dev", "indexes/demo/01ABC.split").unwrap();
        assert_eq!(
            url.as_str(),
            "http://127.0.0.1:9020/fetch/rustie-dev/indexes/demo/01ABC.split"
        );
        let url = fetch_url(&base, "b", "a b/c").unwrap();
        assert!(url.path().contains("a%20b") || url.path().contains("a b"));
    }

    #[test]
    fn validate_eof_clip() {
        // Request 0..100, file size 50 → bytes 0-49/50, body 50.
        validate_content_range(Some("bytes 0-49/50"), 0, 100, 50).unwrap();
        validate_content_range(Some("bytes 0-99/100"), 0, 100, 100).unwrap();
        assert!(validate_content_range(Some("bytes 1-49/50"), 0, 100, 50).is_err());
        assert!(validate_content_range(Some("bytes 0-49/50"), 0, 100, 49).is_err());
    }

    #[test]
    fn c0_config_fps() {
        assert_eq!(build_c0_config(true, None).as_deref(), Some("fps=true"));
        assert_eq!(
            build_c0_config(true, Some("oat=1500")).as_deref(),
            Some("fps=true oat=1500")
        );
        assert_eq!(build_c0_config(false, None), None);
    }

    #[test]
    fn parse_c0_status() {
        assert_eq!(
            parse_c0_status_cached_at(Some("0-16777215; rustie-dev; 0")),
            Some(0)
        );
        assert_eq!(
            parse_c0_status_cached_at(Some("0-16777215; rustie-dev; 1704067200")),
            Some(1704067200)
        );
    }

    use std::str::FromStr;
}
