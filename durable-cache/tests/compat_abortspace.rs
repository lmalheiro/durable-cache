// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPAT.ABORTSPACE-01 — Aborted stores release space.
//!
//! A store that ends without being published loses its temporary no later
//! than when its miss handler is dropped, and the storage it consumed is
//! freed without a restart, however it ended: an upstream failure or
//! cancellation (the handler simply dropped), a write failure, or a backend
//! made read-only.

use super::support;

use bytes::Bytes;
use durable_cache::hooks::FaultPoint;
use pingora_cache::storage::Storage;

// IMP-TC.COMPAT.ABORTSPACE-01: satisfy TC-COMPAT.ABORTSPACE-01

const BODY: &[u8] = &[0x5A; 256 * 1024];

/// Step 1: part of a large body streamed, then the handler dropped without
/// finishing — as `HttpCache` does after an upstream failure or a
/// cancellation.
#[tokio::test]
async fn tc_compat_abortspace_01_dropped_handler_releases_its_space() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/abortspace-dropped");
    let meta = support::meta();
    let trace = support::inactive_trace();
    let before = support::tree_file_usage(cache.root());

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(BODY), false)
        .await
        .unwrap();
    assert!(
        support::tree_file_usage(cache.root()) > before,
        "the store must actually occupy space before it is abandoned"
    );

    drop(handler);

    assert!(
        support::temp_entries(cache, &key).is_empty(),
        "the temporary is gone"
    );
    assert_eq!(
        support::tree_file_usage(cache.root()),
        before,
        "and the space it held is available again"
    );
    assert!(support::lookup_body(cache, &key).await.is_none());
}

/// Step 2: with a partial-hit reader attached, the reader's stream ends
/// with an error and the space is still released
/// (FRE-COMPLIANCE.PARTIALABORT-01).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tc_compat_abortspace_01_releases_space_with_a_reader_attached() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/abortspace-reader");
    let meta = support::meta();
    let trace = support::inactive_trace();
    let before = support::tree_file_usage(cache.root());

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(BODY), false)
        .await
        .unwrap();

    let (_meta, mut reader) = support::lookup_hit(cache, &key).await;
    reader
        .read_body()
        .await
        .unwrap()
        .expect("the bytes written so far");

    drop(handler);

    let err = support::drain_body(&mut reader)
        .await
        .expect_err("an aborted store's reader must not end cleanly");
    assert!(
        err.to_string().contains("aborted"),
        "unexpected error: {err}"
    );
    drop(reader);

    assert!(support::temp_entries(cache, &key).is_empty());
    assert_eq!(support::tree_file_usage(cache.root()), before);
}

/// Step 3: a store ended by a write failure releases its space too.
#[tokio::test]
async fn tc_compat_abortspace_01_store_ended_by_a_write_failure_releases_its_space() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/abortspace-writefail");
    let meta = support::meta();
    let trace = support::inactive_trace();
    let before = support::tree_file_usage(cache.root());

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(BODY), false)
        .await
        .unwrap();
    cache.hooks().inject_fault(FaultPoint::WriteTemp, 1);
    assert!(handler
        .write_body(Bytes::from_static(BODY), false)
        .await
        .is_err());
    drop(handler);

    assert!(support::temp_entries(cache, &key).is_empty());
    assert_eq!(support::tree_file_usage(cache.root()), before);
}

/// Step 4: a thousand aborts in a row leave the cache directory's disk
/// usage where it started — space is released per store, not at some later
/// sweep.
#[tokio::test]
async fn tc_compat_abortspace_01_a_thousand_aborts_leak_nothing() {
    let (cache, _tmp) = support::leaked_cache();
    let meta = support::meta();
    let trace = support::inactive_trace();
    // One store first, so the shard directories the loop uses already
    // exist and their own blocks are part of the baseline.
    let warmup = support::key("/abortspace-loop-0");
    let mut handler = cache
        .get_miss_handler(&warmup, &meta, &trace)
        .await
        .unwrap();
    handler
        .write_body(Bytes::from_static(BODY), false)
        .await
        .unwrap();
    drop(handler);
    let before = support::tree_file_usage(cache.root());

    for i in 0..1000 {
        let key = support::key(&format!("/abortspace-loop-{i}"));
        let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
        handler
            .write_body(Bytes::copy_from_slice(&BODY[..4096]), false)
            .await
            .unwrap();
        drop(handler);
    }

    assert_eq!(
        support::tree_file_usage(cache.root()),
        before,
        "a thousand aborted stores must leave no storage behind"
    );
}

/// A store that fails *while opening* — after its temporary was created but
/// before any handler owns it — leaves nothing behind either: no handler is
/// ever dropped for it, and the restart scan leaves a live writer's
/// temporaries alone, so it would otherwise sit there for the process's
/// whole life.
#[tokio::test]
async fn tc_compat_abortspace_01_a_store_that_fails_while_opening_leaves_nothing() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/abortspace-open-fails");
    let meta = support::meta();
    let trace = support::inactive_trace();
    let before = support::tree_file_usage(cache.root());

    cache.hooks().inject_fault(FaultPoint::WriteTemp, 1);
    assert!(
        cache.get_miss_handler(&key, &meta, &trace).await.is_err(),
        "a failure writing the record's header and metadata must fail the store"
    );

    assert!(
        support::temp_entries(cache, &key).is_empty(),
        "and must leave no temporary behind: {:?}",
        support::temp_entries(cache, &key)
    );
    assert_eq!(support::tree_file_usage(cache.root()), before);

    // The backend is unharmed: the next store of the same key succeeds.
    support::store(cache, &key, &meta, b"stored afterwards").await;
    assert_eq!(
        support::lookup_body(cache, &key).await.unwrap().1,
        b"stored afterwards"
    );
}

/// An aborted store's temporary loses its name as its handler is dropped,
/// and the handler's handle on it, whose close frees the storage, is closed
/// on the blocking pool rather than on the thread that dropped it, an async
/// worker under Pingora. Held just before that close, the drop has already
/// returned, the name is gone, and the file is still open; released, the
/// file is closed.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_aborted_store_frees_its_temporary_off_the_dropping_thread() {
    use durable_cache::hooks::PausePoint;
    use std::time::{Duration, Instant};

    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/abortspace-closed-off-thread");
    let mut handler = cache
        .get_miss_handler(&key, &support::meta(), &support::inactive_trace())
        .await
        .expect("a store opens");
    handler
        .write_body(Bytes::from_static(b"part of a body"), false)
        .await
        .expect("a chunk is written");
    let temps = support::temp_entries(cache, &key);
    assert_eq!(temps.len(), 1, "one temporary: {temps:?}");
    let temp = support::shard_dir_of(cache, &key).join(&temps[0]);
    let temp_file = support::file_id(&temp);

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::BeforeClosingRemovedFile, Some(&temp));
    // Dropped in a task: were the close made on the dropping thread, the
    // pause would hold the drop, and the timeout would fail the test.
    let dropping = tokio::spawn(async move { drop(handler) });
    tokio::time::timeout(Duration::from_secs(5), dropping)
        .await
        .expect("the drop returns without waiting for the close")
        .expect("the dropping task completes");
    assert!(
        support::temp_entries(cache, &key).is_empty(),
        "the name is gone as the drop returns"
    );
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the close is made on the blocking pool"
    );
    assert!(
        support::open_files().contains(&temp_file),
        "the file is still open until then"
    );

    pause.release();
    let deadline = Instant::now() + Duration::from_secs(5);
    while support::open_files().contains(&temp_file) {
        assert!(
            Instant::now() < deadline,
            "the file is closed once released"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}
