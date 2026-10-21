// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.HEADERS-01 — Header fields round-trip faithfully.
//!
//! Purely a `ResponseHeader` round-trip property, covered directly against
//! `Storage::get_miss_handler`/`lookup` -- no HTTP needed. `case_header_iter`
//! is used for comparison rather than `headers` directly: it's the one
//! accessor pingora_http itself documents as preserving both original case
//! and, for repeated names, insertion order -- exactly the two properties
//! this TC requires and a case-insensitive/unordered map could silently
//! paper over.

use std::time::{Duration, SystemTime};

use bytes::Bytes;
use durable_cache::{FsCache, FsCacheConfig};
use pingora_cache::storage::Storage;
use pingora_cache::CacheMeta;
use pingora_http::ResponseHeader;

use super::support;

// IMP-TC.COMPLIANCE.HEADERS-01: satisfy TC-COMPLIANCE.HEADERS-01

/// Every field shape TC-COMPLIANCE.HEADERS-01 names: repeated names in a
/// deliberate order, an unrecognised field, an empty value, an obs-text
/// (opaque non-ASCII octet) value, and a `Set-Cookie`-style field repeated
/// across lines (must never be list-combined).
fn exercising_header(status: u16) -> ResponseHeader {
    let mut header = ResponseHeader::build(status, None).unwrap();
    header.append_header("x-repeated", "first").unwrap();
    header.append_header("x-repeated", "second").unwrap();
    header.append_header("x-repeated", "third").unwrap();
    header
        .append_header("x-custom-unrecognised", "opaque-to-any-parser")
        .unwrap();
    header.append_header("x-empty", "").unwrap();
    // Opaque non-ASCII octets (obs-text): valid per HeaderValue::from_bytes
    // (any byte 0x21..=0xFF, plus interior space/tab), invalid UTF-8.
    header.append_header("x-obs-text", &b"caf\xE9"[..]).unwrap();
    header.append_header("set-cookie", "a=1; Path=/").unwrap();
    header.append_header("set-cookie", "b=2; Path=/x").unwrap();
    header
}

fn wrap(header: ResponseHeader) -> CacheMeta {
    let now = SystemTime::now();
    CacheMeta::new(now + Duration::from_secs(60), now, 0, 0, header)
}

/// `(case-preserved name bytes, value bytes)` pairs in the exact order
/// `case_header_iter` visits them -- the strongest comparison pingora_http
/// exposes, per its own doc: case retained, same-name entries in insertion
/// order.
fn header_fingerprint(meta: &CacheMeta) -> Vec<(Vec<u8>, Vec<u8>)> {
    meta.response_header()
        .case_header_iter()
        .map(|(name, value)| (name.as_slice().to_vec(), value.as_bytes().to_vec()))
        .collect()
}

async fn store_and_lookup(cache: &'static FsCache, name: &str, meta: &CacheMeta) -> CacheMeta {
    let key = support::key(name);
    let trace = support::inactive_trace();
    let mut handler = cache.get_miss_handler(&key, meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"headers-under-test"), true)
        .await
        .unwrap();
    handler.finish().await.unwrap();

    cache.lookup(&key, &trace).await.unwrap().unwrap().0
}

#[tokio::test]
async fn tc_compliance_headers_01_every_field_shape_round_trips() {
    let (cache, _tmp) = support::leaked_cache();
    let meta = wrap(exercising_header(451));
    let expected_fingerprint = header_fingerprint(&meta);
    let expected_status = meta.response_header().status;

    let found = store_and_lookup(cache, "/headers-shapes", &meta).await;

    assert_eq!(found.response_header().status, expected_status);
    assert_eq!(
        found.response_header().version,
        meta.response_header().version
    );
    assert_eq!(
        header_fingerprint(&found),
        expected_fingerprint,
        "every field's name (case), value (bytes), and relative order among \
         same-named fields must survive unchanged"
    );
}

#[tokio::test]
async fn tc_compliance_headers_01_survives_a_restart() {
    let (cache, tmp) = support::leaked_cache();
    let meta = wrap(exercising_header(451));
    let expected_fingerprint = header_fingerprint(&meta);
    let key = support::key("/headers-restart");
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

    assert_eq!(header_fingerprint(&found), expected_fingerprint);
}

#[tokio::test]
async fn tc_compliance_headers_01_non_200_status_preserved() {
    let (cache, _tmp) = support::leaked_cache();
    for status in [204u16, 206, 404, 500] {
        let meta = wrap(exercising_header(status));
        let found = store_and_lookup(cache, &format!("/headers-status-{status}"), &meta).await;
        assert_eq!(found.response_header().status.as_u16(), status);
    }
}

#[tokio::test]
async fn tc_compliance_headers_01_oversized_header_set_round_trips() {
    let (cache, _tmp) = support::leaked_cache();
    let mut header = ResponseHeader::build(200, None).unwrap();
    // Many times the default 512-byte headroom, forcing a metadata region
    // far larger than the smallest one.
    for i in 0..200 {
        header
            .append_header(
                format!("x-generated-{i}"),
                format!("value-{i}-{}", "z".repeat(20)),
            )
            .unwrap();
    }
    let meta = wrap(header);
    let expected_fingerprint = header_fingerprint(&meta);

    let found = store_and_lookup(cache, "/headers-oversized", &meta).await;

    assert_eq!(header_fingerprint(&found), expected_fingerprint);
}

/// Metadata regions left exactly full: with no headroom, a region's capacity
/// is its segments' own length, so its last byte is the one before the
/// body. Header sets of eight consecutive serialised sizes put that boundary
/// at every offset modulo 8, where an off-by-one in the region's capacity
/// or in the body's offset would show (SDS-LAYOUT.OBJECTFILE-01).
#[tokio::test]
async fn tc_compliance_headers_01_exactly_full_metadata_regions_round_trip() {
    let (cache, _tmp) = support::leaked_cache_with_headroom(0);
    let filler_meta = |filler: usize| {
        let mut header = ResponseHeader::build(200, None).unwrap();
        header
            .append_header("x-filler", support::incompressible(filler, 9))
            .unwrap();
        wrap(header)
    };

    // Grow a filler byte at a time, keeping the first filler of each
    // serialised size, until eight consecutive sizes are found.
    let mut by_size = std::collections::BTreeMap::new();
    for filler in 1..400usize {
        let (internal, header_bytes) = filler_meta(filler).serialize().unwrap();
        by_size
            .entry(internal.len() + header_bytes.len())
            .or_insert(filler);
    }
    let sizes: Vec<usize> = by_size.keys().copied().collect();
    let run = sizes
        .windows(8)
        .find(|w| w[7] - w[0] == 7)
        .expect("eight consecutive serialised sizes");

    for segments in run {
        let filler = by_size[segments];
        let meta = filler_meta(filler);
        let expected = header_fingerprint(&meta);
        let name = format!("/headers-full-{filler:03}");
        let found = store_and_lookup(cache, &name, &meta).await;
        assert_eq!(
            header_fingerprint(&found),
            expected,
            "a header set of {segments} metadata bytes must round-trip"
        );

        let key = support::key(&name);
        let header = support::read_header(&support::canonical_path(cache, &key));
        assert_eq!(
            header.meta_capacity() as usize,
            *segments,
            "with no headroom the region is exactly full"
        );
        let (_meta, body) = support::lookup_body(cache, &key).await.unwrap();
        assert_eq!(
            body, b"headers-under-test",
            "the body after a full region is intact"
        );
    }
}
