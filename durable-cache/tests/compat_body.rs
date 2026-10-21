// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.BODY-01 — Body round-trips byte for byte.
//!
//! Steps 1-2 (full-read fidelity at boundary sizes, including a restart)
//! and step 3 (seeked reads, which a published record supports per
//! SDS-LAYOUT.OBJECTFILE-01) are covered directly against
//! `Storage::get_miss_handler`/`lookup` -- no HTTP needed.

use super::support;

use bytes::Bytes;
use durable_cache::{FsCache, FsCacheConfig};
use pingora_cache::storage::Storage;

// IMP-TC.COMPLIANCE.BODY-01: satisfy TC-COMPLIANCE.BODY-01

async fn store_and_read_back(cache: &'static FsCache, name: &str, body: &[u8]) -> Vec<u8> {
    let key = support::key(name);
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    // Split into a couple of writes to also exercise the growing-write
    // path, not just a single one-shot body.
    if body.len() > 1 {
        let mid = body.len() / 2;
        handler
            .write_body(Bytes::copy_from_slice(&body[..mid]), false)
            .await
            .unwrap();
        handler
            .write_body(Bytes::copy_from_slice(&body[mid..]), true)
            .await
            .unwrap();
    } else {
        handler
            .write_body(Bytes::copy_from_slice(body), true)
            .await
            .unwrap();
    }
    handler.finish().await.unwrap();

    let (_meta, mut hit) = cache.lookup(&key, &trace).await.unwrap().unwrap();
    let mut read = Vec::new();
    while let Some(chunk) = hit.read_body().await.unwrap() {
        read.extend_from_slice(&chunk);
    }
    read
}

fn random_body(len: usize, seed: u64) -> Vec<u8> {
    // A small xorshift PRNG -- deterministic, no extra dev-dependency, and
    // exercises genuinely non-repeating binary content rather than a
    // pattern that could mask an off-by-one against itself.
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state & 0xFF) as u8
        })
        .collect()
}

#[tokio::test]
async fn tc_compliance_body_01_boundary_sizes_round_trip() {
    let (cache, _tmp) = support::leaked_cache();

    // Empty, one byte, either side of a 4 KiB filesystem block boundary,
    // either side of the backend's internal 64 KiB read-chunk boundary, and
    // a large multi-chunk body.
    let sizes = [
        0usize,
        1,
        4095,
        4096,
        4097,
        64 * 1024 - 1,
        64 * 1024,
        64 * 1024 + 1,
        3 * 64 * 1024 + 12345,
    ];

    for (i, &size) in sizes.iter().enumerate() {
        let body = random_body(size, 0xC0FFEE ^ i as u64);
        let read = store_and_read_back(cache, &format!("/body-boundary-{i}"), &body).await;
        assert_eq!(
            read, body,
            "size {size} must round-trip byte for byte and in order"
        );
    }
}

#[tokio::test]
async fn tc_compliance_body_01_survives_a_restart() {
    let (cache, tmp) = support::leaked_cache();
    let body = random_body(200_000, 42);
    let key = support::key("/body-restart");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from(body.clone()), true)
        .await
        .unwrap();
    handler.finish().await.unwrap();

    let restarted = FsCache::new(FsCacheConfig::new(tmp.path(), None)).unwrap();
    let restarted: &'static FsCache = Box::leak(Box::new(restarted));
    let (_meta, mut hit) = restarted.lookup(&key, &trace).await.unwrap().unwrap();
    let mut read = Vec::new();
    while let Some(chunk) = hit.read_body().await.unwrap() {
        read.extend_from_slice(&chunk);
    }
    assert_eq!(read, body);
}

/// Step 3: a published record advertises seek support, and each seeked
/// read delivers exactly the requested slice, so range responses built from
/// the reader carry precisely the stored bytes.
#[tokio::test]
async fn tc_compliance_body_01_seeked_reads_deliver_exactly_the_requested_slice() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/body-seek");
    let body = random_body(200_000, 7);
    support::store(cache, &key, &support::meta(), &body).await;

    // The start, an interior slice spanning the backend's 64 KiB read-chunk
    // boundary, the tail, a single byte, and an end past the last byte.
    let ranges: [(usize, Option<usize>); 5] = [
        (0, Some(1000)),
        (64 * 1024 - 10, Some(64 * 1024 + 10)),
        (150_000, None),
        (4096, Some(4097)),
        (199_000, Some(500_000)),
    ];
    for (start, end) in ranges {
        let (_meta, mut hit) = support::lookup_hit(cache, &key).await;
        assert!(hit.can_seek(), "a published record supports seeking");
        hit.seek(start, end).unwrap();
        let read = support::drain_body(&mut hit).await.unwrap();
        let expected = &body[start..end.unwrap_or(body.len()).min(body.len())];
        assert_eq!(
            read, expected,
            "seek({start}, {end:?}) must deliver exactly that slice"
        );
    }
}

/// A seek whose start is not inside the body is an error, as in `MemCache`,
/// the in-memory backend of `pingora-cache`, rather than an empty read: past
/// the end of a body, and anywhere in an empty one.
#[tokio::test]
async fn tc_compliance_body_01_seek_past_the_end_is_an_error() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/body-seek-past-end");
    support::store(cache, &key, &support::meta(), b"twelve bytes").await;
    let (_meta, mut hit) = support::lookup_hit(cache, &key).await;
    assert!(hit.seek(12, None).is_err(), "a start at the end");
    assert!(hit.seek(11, None).is_ok(), "the last byte");

    let empty = support::key("/body-seek-empty");
    support::store(cache, &empty, &support::meta(), b"").await;
    let (_meta, mut hit) = support::lookup_hit(cache, &empty).await;
    assert!(hit.seek(0, None).is_err(), "an empty body");
}
