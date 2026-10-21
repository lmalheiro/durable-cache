// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.INCOMPLETE-01 — Incomplete responses never served.
//!
//! Also exercises the basic complete-hit round trip (`Storage::lookup`
//! returning a whole, correctly-decoded object) as groundwork, since every
//! other scenario here is a variation on "this must NOT look like that".
//!
//! Covers every leg of this TC's spec except a genuine `kill -9` mid-write
//! plus process restart (an in-process `drop` can simulate an orderly
//! abort's Drop impl running, but not the complete absence of one) --
//! that leg is `test/compat/tests/incomplete.rs`'s, which is why the IMP
//! marker for this TC lives there rather than here: neither file alone is
//! a comprehensive closure, but the two together are.

use super::support;

use bytes::Bytes;
use durable_cache::sharding::shard_path;
use pingora_cache::key::CacheHashKey;
use pingora_cache::storage::{HandleHit, Storage};

async fn drain(mut handler: Box<dyn HandleHit + Send + Sync>) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(chunk) = handler.read_body().await.unwrap() {
        out.extend_from_slice(&chunk);
    }
    out
}

#[tokio::test]
async fn tc_compliance_incomplete_01_complete_hit_round_trips() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/incomplete-roundtrip");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"hello world"), true)
        .await
        .unwrap();
    handler.finish().await.unwrap();

    let (found_meta, hit) = cache
        .lookup(&key, &trace)
        .await
        .unwrap()
        .expect("must be a hit");
    assert_eq!(
        found_meta.response_header().status,
        meta.response_header().status
    );
    let body = drain(hit).await;
    assert_eq!(body, b"hello world");
}

#[tokio::test]
async fn tc_compliance_incomplete_01_lookup_on_absent_key_is_a_miss() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/incomplete-absent");
    let trace = support::inactive_trace();

    assert!(cache.lookup(&key, &trace).await.unwrap().is_none());
}

#[tokio::test]
async fn tc_compliance_incomplete_01_aborted_store_is_never_served() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/incomplete-aborted");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"partial"), false)
        .await
        .unwrap();
    // Abort: dropped without finish(), simulating an upstream disconnect
    // (or, for a real process, a crash -- see storage_publish.rs's
    // dropped_without_finish test for that guarantee in isolation).
    drop(handler);

    assert!(
        cache.lookup(&key, &trace).await.unwrap().is_none(),
        "an abandoned store must never be looked up as a hit, complete or partial"
    );
}

/// A damaged record is a miss, and the lookup leaves it where it is: a
/// later store of the same key replaces it, and the restart-time scan
/// reclaims it, so a lookup never unlinks a path whose content may have
/// changed since it opened it (SDS-LAYOUT.OBJECTFILE-01).
#[tokio::test]
async fn tc_compliance_incomplete_01_damaged_record_is_a_miss_and_left_for_the_scan() {
    let (cache, tmp) = support::leaked_cache();
    let key = support::key("/incomplete-damaged");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"whole object"), true)
        .await
        .unwrap();
    handler.finish().await.unwrap();

    let canonical = shard_path(cache.root(), &key.combined()).unwrap();
    assert!(canonical.exists());
    support::flip_magic_byte(&canonical);

    assert!(
        cache.lookup(&key, &trace).await.unwrap().is_none(),
        "a damaged record must be a miss, never a corrupt read"
    );
    assert!(
        canonical.exists(),
        "the lookup leaves the file for the restart-time scan"
    );

    // The scan is what reclaims it.
    let restarted = support::leaked_cache_at(tmp.path());
    let manager = pingora_cache::eviction::simple_lru::Manager::new(usize::MAX);
    let report = durable_cache::recover(restarted, &manager).await.unwrap();
    assert_eq!(report.reclaimed_damaged, 1);
    assert!(!canonical.exists());
}

/// A store aborted after an orderly abort takes its temporary with it, so
/// the space it held is released no later than when its miss handler is
/// dropped (FRE-COMPAT.ABORTSPACE-01).
#[tokio::test]
async fn tc_compliance_incomplete_01_aborted_store_leaves_no_temporary() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/incomplete-abort-space");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"partial body"), false)
        .await
        .unwrap();
    assert_eq!(support::temp_entries(cache, &key).len(), 1);

    drop(handler);

    assert!(
        support::temp_entries(cache, &key).is_empty(),
        "the temporary is gone by the time the handler is dropped"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tc_compliance_incomplete_01_follower_of_an_aborted_store_gets_an_error() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/incomplete-follower-abort");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"first chunk"), false)
        .await
        .unwrap();

    let (_meta, mut follower) = cache
        .lookup(&key, &trace)
        .await
        .unwrap()
        .expect("a partial hit while the writer is open");
    let first = follower.read_body().await.unwrap();
    assert_eq!(first, Some(Bytes::from_static(b"first chunk")));

    let read_task = tokio::spawn(async move { follower.read_body().await });

    // Give the follower a moment to catch up and start waiting on the next
    // chunk before aborting -- not required for correctness (the wakeup
    // can't be missed either way, see lookup.rs), but it makes this test
    // actually exercise the wakeup-on-abort path instead of a pre-abort read.
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    drop(handler); // abort: no finish() ever called

    let result = read_task.await.unwrap();
    assert!(
        result.is_err(),
        "the follower's in-flight stream must terminate with an error, never a clean end of body"
    );
}

#[tokio::test]
async fn tc_compliance_incomplete_01_truncated_record_is_a_miss() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/incomplete-truncated");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"whole object body"), true)
        .await
        .unwrap();
    handler.finish().await.unwrap();

    let canonical = shard_path(cache.root(), &key.combined()).unwrap();
    let len = std::fs::metadata(&canonical).unwrap().len();
    // Simulate a host crash that left the record's content not durably
    // complete, out of band -- truncate short of the recorded extent.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&canonical)
        .unwrap();
    file.set_len(len - 1).unwrap();

    assert!(cache.lookup(&key, &trace).await.unwrap().is_none());
}
