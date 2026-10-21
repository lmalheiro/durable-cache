// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.REPLACE-01 — Stores replace atomically.
//!
//! `tc_compliance_replace_01_reader_of_old_object_undisturbed_during_replacement`
//! below also closes TC-STORAGE.PUBLISH-01's step 5 (the identical
//! scenario under a different TC ID); that TC's own IMP marker is in
//! `storage_publish.rs`.

use super::support;

use bytes::Bytes;
use pingora_cache::storage::Storage;

// IMP-TC.COMPLIANCE.REPLACE-01: satisfy TC-COMPLIANCE.REPLACE-01

#[tokio::test]
async fn tc_compliance_replace_01_reader_of_old_object_undisturbed_during_replacement() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/replace-basic");
    let meta = support::meta();
    let trace = support::inactive_trace();

    // Store and publish object A.
    let mut a = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    a.write_body(Bytes::from_static(b"AAAAAAAAAA"), true)
        .await
        .unwrap();
    a.finish().await.unwrap();

    // Attach a reader to A while it's the only object under this key.
    let (_meta, mut reader) = cache.lookup(&key, &trace).await.unwrap().unwrap();
    let first_read = reader.read_body().await.unwrap().unwrap();
    assert_eq!(&first_read[..], b"AAAAAAAAAA");

    // Store B under the same key, replacing A. While B is still open, a
    // fresh lookup must prefer B's in-flight write over A's now-stale
    // canonical object (FRE-COMPAT.STREAMING-01's priority rule doubles as
    // this TC's atomicity guarantee: no lookup ever observes a mixture).
    let mut b = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    b.write_body(Bytes::from_static(b"BB"), false)
        .await
        .unwrap();

    let (_meta, mut mid_flight) = cache.lookup(&key, &trace).await.unwrap().unwrap();
    let mid_chunk = mid_flight.read_body().await.unwrap().unwrap();
    assert_eq!(
        &mid_chunk[..],
        b"BB",
        "a lookup while B is in flight must prefer B's partial hit over A's stale canonical object"
    );

    b.write_body(Bytes::from_static(b"BBBBBBBBBBBBBBBBBB"), true)
        .await
        .unwrap();
    b.finish().await.unwrap();

    // The reader opened on A before the replacement continues to read
    // exactly A's bytes to a clean end of body -- never B's.
    assert_eq!(
        reader.read_body().await.unwrap(),
        None,
        "A's reader must see a clean end of body, never cross into B's bytes"
    );

    // A fresh lookup now sees B whole, never a mixture of the two.
    let (_meta, mut hit) = cache.lookup(&key, &trace).await.unwrap().unwrap();
    let mut body = Vec::new();
    while let Some(chunk) = hit.read_body().await.unwrap() {
        body.extend_from_slice(&chunk);
    }
    assert_eq!(body, b"BBBBBBBBBBBBBBBBBBBB");
}

#[tokio::test]
async fn tc_compliance_replace_01_racing_stores_converge_on_one_intact_winner() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/replace-race");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut c = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    let mut d = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();

    c.write_body(Bytes::from_static(b"CCCC"), false)
        .await
        .unwrap();
    d.write_body(Bytes::from_static(b"dddd"), false)
        .await
        .unwrap();
    c.write_body(Bytes::from_static(b"CCCC"), true)
        .await
        .unwrap();
    d.write_body(Bytes::from_static(b"dddd"), true)
        .await
        .unwrap();

    c.finish().await.unwrap();
    d.finish().await.unwrap();

    let (_meta, mut hit) = cache.lookup(&key, &trace).await.unwrap().unwrap();
    let mut body = Vec::new();
    while let Some(chunk) = hit.read_body().await.unwrap() {
        body.extend_from_slice(&chunk);
    }
    assert!(
        body == b"CCCCCCCC" || body == b"dddddddd",
        "exactly one racing writer's bytes, whole and intact, never interleaved: {body:?}"
    );

    // Restart: a fresh FsCache over the same root sees the identical
    // winner, whole and unchanged -- the replacement is durable, not just
    // an in-memory artifact of this process.
    let restarted =
        durable_cache::FsCache::new(durable_cache::FsCacheConfig::new(cache.root(), None)).unwrap();
    let restarted: &'static durable_cache::FsCache = Box::leak(Box::new(restarted));
    let (_meta, mut hit) = restarted.lookup(&key, &trace).await.unwrap().unwrap();
    let mut body_after_restart = Vec::new();
    while let Some(chunk) = hit.read_body().await.unwrap() {
        body_after_restart.extend_from_slice(&chunk);
    }
    assert_eq!(
        body_after_restart, body,
        "the same winner must survive a restart unchanged"
    );
}

/// Step 2: while B's store is in flight, an in-process lookup is served a
/// partial hit of B (FRE-COMPAT.STREAMING-01), and once it completes every
/// lookup returns B whole — also after a restart.
#[tokio::test]
async fn tc_compliance_replace_01_lookups_during_the_new_store_see_the_new_object() {
    let (cache, tmp) = support::leaked_cache();
    let key = support::key("/replace-during-store");
    support::store(cache, &key, &support::meta(), b"object A").await;

    let meta = support::meta();
    let trace = support::inactive_trace();
    let mut storing = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    storing
        .write_body(Bytes::from_static(b"object B, part one;"), false)
        .await
        .unwrap();

    // In-process, an in-flight store takes precedence over the published
    // object, so this is a partial hit of B.
    let (_meta, mut hit) = support::lookup_hit(cache, &key).await;
    let chunk = hit.read_body().await.unwrap().unwrap();
    assert_eq!(&chunk[..], b"object B, part one;");
    drop(hit);

    storing
        .write_body(Bytes::from_static(b" part two"), true)
        .await
        .unwrap();
    storing.finish().await.unwrap();

    assert_eq!(
        support::lookup_body(cache, &key).await.unwrap().1,
        b"object B, part one; part two",
        "after the store completes every lookup returns B in its entirety"
    );
    let restarted = support::leaked_cache_at(tmp.path());
    assert_eq!(
        support::lookup_body(restarted, &key).await.unwrap().1,
        b"object B, part one; part two",
        "and so does a lookup after a restart"
    );
}
