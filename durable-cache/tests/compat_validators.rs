// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.VALIDATORS-01 — Validator fields preserved.
//!
//! Purely a `CacheMeta`/`ResponseHeader` round-trip property (`ETag` and
//! `Last-Modified` are ordinary response header fields to this backend, no
//! different from any other), so covered directly against
//! `Storage::get_miss_handler`/`lookup`/`update_meta` -- no HTTP needed.

// IMP-TC.COMPLIANCE.VALIDATORS-01: satisfy TC-COMPLIANCE.VALIDATORS-01
use super::support;

use bytes::Bytes;
use durable_cache::{FsCache, FsCacheConfig};
use pingora_cache::storage::Storage;
use pingora_cache::CacheMeta;
use pingora_http::ResponseHeader;
use std::time::{Duration, SystemTime};

fn meta_with_validators(etag: Option<&str>, last_modified: Option<&str>) -> CacheMeta {
    let mut header = ResponseHeader::build(200, None).unwrap();
    if let Some(etag) = etag {
        header.append_header("etag", etag).unwrap();
    }
    if let Some(lm) = last_modified {
        header.append_header("last-modified", lm).unwrap();
    }
    let now = SystemTime::now();
    CacheMeta::new(now + Duration::from_secs(60), now, 0, 0, header)
}

fn validators(meta: &CacheMeta) -> (Option<String>, Option<String>) {
    let h = meta.response_header();
    (
        h.headers
            .get("etag")
            .map(|v| v.to_str().unwrap().to_string()),
        h.headers
            .get("last-modified")
            .map(|v| v.to_str().unwrap().to_string()),
    )
}

async fn store_and_lookup(
    cache: &'static FsCache,
    name: &str,
    meta: &CacheMeta,
) -> (Option<String>, Option<String>) {
    let key = support::key(name);
    let trace = support::inactive_trace();
    let mut handler = cache.get_miss_handler(&key, meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"validated"), true)
        .await
        .unwrap();
    handler.finish().await.unwrap();

    let (found, _hit) = cache.lookup(&key, &trace).await.unwrap().unwrap();
    validators(&found)
}

#[tokio::test]
async fn tc_compliance_validators_01_strong_etag_round_trips() {
    let (cache, _tmp) = support::leaked_cache();
    let meta = meta_with_validators(Some("\"abc123\""), None);
    let (etag, lm) = store_and_lookup(cache, "/validators-strong-etag", &meta).await;
    assert_eq!(etag.as_deref(), Some("\"abc123\""));
    assert_eq!(lm, None);
}

#[tokio::test]
async fn tc_compliance_validators_01_weak_etag_marker_retained() {
    let (cache, _tmp) = support::leaked_cache();
    let meta = meta_with_validators(Some("W/\"abc123\""), None);
    let (etag, _lm) = store_and_lookup(cache, "/validators-weak-etag", &meta).await;
    assert_eq!(
        etag.as_deref(),
        Some("W/\"abc123\""),
        "the W/ weak indicator must survive byte-exactly"
    );
}

#[tokio::test]
async fn tc_compliance_validators_01_empty_etag_intact() {
    let (cache, _tmp) = support::leaked_cache();
    let meta = meta_with_validators(Some("\"\""), None);
    let (etag, _lm) = store_and_lookup(cache, "/validators-empty-etag", &meta).await;
    assert_eq!(etag.as_deref(), Some("\"\""));
}

#[tokio::test]
async fn tc_compliance_validators_01_last_modified_round_trips() {
    let (cache, _tmp) = support::leaked_cache();
    let meta = meta_with_validators(None, Some("Wed, 21 Oct 2015 07:28:00 GMT"));
    let (etag, lm) = store_and_lookup(cache, "/validators-last-modified", &meta).await;
    assert_eq!(etag, None);
    assert_eq!(lm.as_deref(), Some("Wed, 21 Oct 2015 07:28:00 GMT"));
}

#[tokio::test]
async fn tc_compliance_validators_01_both_validators_together() {
    let (cache, _tmp) = support::leaked_cache();
    let meta = meta_with_validators(Some("\"combo\""), Some("Wed, 21 Oct 2015 07:28:00 GMT"));
    let (etag, lm) = store_and_lookup(cache, "/validators-both", &meta).await;
    assert_eq!(etag.as_deref(), Some("\"combo\""));
    assert_eq!(lm.as_deref(), Some("Wed, 21 Oct 2015 07:28:00 GMT"));
}

#[tokio::test]
async fn tc_compliance_validators_01_survive_a_restart() {
    let (cache, tmp) = support::leaked_cache();
    let meta = meta_with_validators(Some("\"restart\""), Some("Wed, 21 Oct 2015 07:28:00 GMT"));
    let key = support::key("/validators-restart");
    let trace = support::inactive_trace();
    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"x"), true)
        .await
        .unwrap();
    handler.finish().await.unwrap();

    let restarted = FsCache::new(FsCacheConfig::new(tmp.path(), None)).unwrap();
    let restarted: &'static FsCache = Box::leak(Box::new(restarted));
    let (found, _hit) = restarted.lookup(&key, &trace).await.unwrap().unwrap();
    let (etag, lm) = validators(&found);
    assert_eq!(etag.as_deref(), Some("\"restart\""));
    assert_eq!(lm.as_deref(), Some("Wed, 21 Oct 2015 07:28:00 GMT"));
}

#[tokio::test]
async fn tc_compliance_validators_01_metadata_update_changes_validators_for_every_lookup() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/validators-update");
    let trace = support::inactive_trace();

    let stored = meta_with_validators(Some("\"v1\""), Some("Wed, 21 Oct 2015 07:28:00 GMT"));
    let mut handler = cache.get_miss_handler(&key, &stored, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"body"), true)
        .await
        .unwrap();
    handler.finish().await.unwrap();

    cache
        .update_meta(
            &key,
            &support::revalidation(
                &stored,
                &meta_with_validators(Some("\"v2\""), Some("Thu, 22 Oct 2015 07:28:00 GMT")),
            ),
            &trace,
        )
        .await
        .unwrap();

    // Every subsequent lookup returns the updated validators, never the
    // superseded set.
    for _ in 0..2 {
        let (found, _hit) = cache.lookup(&key, &trace).await.unwrap().unwrap();
        let (etag, lm) = validators(&found);
        assert_eq!(etag.as_deref(), Some("\"v2\""));
        assert_eq!(lm.as_deref(), Some("Thu, 22 Oct 2015 07:28:00 GMT"));
    }
}
