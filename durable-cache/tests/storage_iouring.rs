// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-STORAGE.IOURING-01 — Store writes on the ring.
//!
//! With the ring on, the default on Linux, a store writes the body chunks
//! that end past its first 64 KiB on the ring, whatever its durability, and
//! every other write as without the ring: an unsynchronised store's in its
//! inline slots, a synchronised store's on the pool (FDS-STORAGE.IOURING-01).
//! The completions of its writes outlive the runtime that collected them
//! first. The test hooks count the writes submitted to the ring and the
//! operations run in a slot.

use std::time::Duration;

use bytes::Bytes;
use durable_cache::hooks::{FaultPoint, HookConfig};
use durable_cache::{Durability, FsCache, FsCacheConfig};
use pingora_cache::storage::Storage;

use super::support;

// IMP-TC.STORAGE.IOURING-01: satisfy TC-STORAGE.IOURING-01

const KIB: usize = 1024;

/// A cache configured by `configure`, rooted in a new directory.
fn cache_with(
    configure: impl FnOnce(FsCacheConfig) -> FsCacheConfig,
) -> (&'static FsCache, tempfile::TempDir) {
    let tmp = support::tempdir();
    let cache = support::leaked_cache_with(configure(FsCacheConfig::new(tmp.path(), None)));
    (cache, tmp)
}

/// `count` chunks of `len` bytes, each different.
fn chunks(count: u64, len: usize) -> Vec<String> {
    (0..count)
        .map(|seed| support::incompressible(len, seed))
        .collect()
}

/// Store `chunks` under `primary` and read the body back: how many of the
/// store's operations went on the ring, and how many ran in a slot.
async fn store_counting(cache: &'static FsCache, primary: &str, chunks: &[String]) -> (u64, u64) {
    let key = support::key(primary);
    let chunks: Vec<&[u8]> = chunks.iter().map(|c| c.as_bytes()).collect();
    let ring_before = cache.hooks().ring_ops();
    let inline_before = cache.hooks().inline_ops();
    support::store_chunks(cache, &key, &support::meta(), &chunks).await;
    let counts = (
        cache.hooks().ring_ops() - ring_before,
        cache.hooks().inline_ops() - inline_before,
    );
    let (_meta, body) = support::lookup_body(cache, &key)
        .await
        .expect("the store is published");
    assert_eq!(body, chunks.concat(), "the body reads as stored");
    counts
}

/// Step 1: on four workers, an unsynchronised store of eight 16 KiB chunks
/// writes the four that end within its first 64 KiB in a slot, and the four
/// after them, the last included, on the ring.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_storage_iouring_01_an_unsynchronised_store_writes_past_64_kib_on_the_ring() {
    let (cache, _tmp) = cache_with(|config| config.with_durability(Durability::NoStoreSync));
    let (ring, inline) = store_counting(cache, "/ring-unsynced", &chunks(8, 16 * KIB)).await;
    assert_eq!(ring, 4, "the chunks past the first 64 KiB go on the ring");
    assert_eq!(
        inline, 6,
        "the opening, the first four chunks and the finish run in a slot"
    );
}

/// Step 2: a synchronised store of the same chunks writes the same four on
/// the ring, and takes no slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_storage_iouring_01_a_synchronised_store_writes_past_64_kib_on_the_ring() {
    let (cache, _tmp) = cache_with(|config| config.with_durability(Durability::Strict));
    let (ring, inline) = store_counting(cache, "/ring-strict", &chunks(8, 16 * KIB)).await;
    assert_eq!(ring, 4, "the chunks past the first 64 KiB go on the ring");
    assert_eq!(inline, 0);
}

/// Step 3: a store of 64 KiB, of either durability, writes as without the
/// ring: unsynchronised, its opening, its first three chunks and its finish,
/// which writes the last, each in a slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_storage_iouring_01_a_store_of_64_kib_writes_as_without_the_ring() {
    for (durability, primary, slot_ops) in [
        (Durability::NoStoreSync, "/ring-small-unsynced", 5),
        (Durability::Strict, "/ring-small-strict", 0),
    ] {
        let (cache, _tmp) = cache_with(|config| config.with_durability(durability));
        let (ring, inline) = store_counting(cache, primary, &chunks(4, 16 * KIB)).await;
        assert_eq!(ring, 0, "{primary}: nothing on the ring");
        assert_eq!(inline, slot_ops, "{primary}: the slots as without the ring");
    }
}

/// Step 4: with the ring switched off, no store submits to it, and
/// construction does not set it up: a fault armed there is never reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_storage_iouring_01_switched_off_no_store_uses_the_ring() {
    let tmp = support::tempdir();
    let cache = support::leaked_cache_configured(
        FsCacheConfig::new(tmp.path(), None).with_io_uring(false),
        HookConfig {
            faults: vec![(FaultPoint::RingSetup, 1)],
            ..HookConfig::default()
        },
    );
    let (ring, _inline) = store_counting(cache, "/ring-off", &chunks(8, 16 * KIB)).await;
    assert_eq!(ring, 0);
}

/// Step 5: where the ring cannot be set up, construction fails, naming the
/// option.
#[test]
fn tc_storage_iouring_01_construction_fails_without_the_ring() {
    let tmp = support::tempdir();
    let built = FsCache::with_test_hooks(
        FsCacheConfig::new(tmp.path(), None),
        HookConfig {
            faults: vec![(FaultPoint::RingSetup, 1)],
            ..HookConfig::default()
        },
    );
    let Err(e) = built else {
        panic!("construction succeeds without the ring the configuration asks for");
    };
    assert!(
        e.to_string().contains("FsCacheConfig::with_io_uring"),
        "the error names the option: {e}"
    );
}

/// Step 6: a write that fails on the ring fails the store, which publishes
/// nothing and leaves no temporary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_storage_iouring_01_a_failed_ring_write_fails_the_store() {
    let (cache, _tmp) = cache_with(|config| config.with_durability(Durability::Strict));
    let key = support::key("/ring-fails");
    let trace = support::inactive_trace();
    let mut handler = cache
        .get_miss_handler(&key, &support::meta(), &trace)
        .await
        .expect("the store opens");
    handler
        .write_body(Bytes::from(support::incompressible(64 * KIB, 3)), false)
        .await
        .expect("the first 64 KiB are written as without the ring");
    let ring_before = cache.hooks().ring_ops();
    cache.hooks().inject_fault(FaultPoint::RingWrite, 1);
    assert!(
        handler
            .write_body(Bytes::from_static(b"refused by the kernel"), false)
            .await
            .is_err(),
        "the write the kernel failed fails the call"
    );
    assert_eq!(
        cache.hooks().ring_ops() - ring_before,
        1,
        "the write went to the ring"
    );
    assert!(handler.finish().await.is_err(), "the store stays failed");
    assert!(support::temp_entries(cache, &key).is_empty());
    assert!(support::lookup_body(cache, &key).await.is_none());
}

/// Step 7: a store dropped while its write is in flight on the ring leaves
/// no temporary, and the next store of the key publishes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_storage_iouring_01_a_store_dropped_mid_write_leaves_nothing() {
    let (cache, _tmp) = cache_with(|config| config.with_durability(Durability::Strict));
    let key = support::key("/ring-dropped");
    let trace = support::inactive_trace();
    let mut handler = cache
        .get_miss_handler(&key, &support::meta(), &trace)
        .await
        .expect("the store opens");
    let chunk = Bytes::from(support::incompressible(1024 * KIB, 7));
    // A deadline already past: the write is polled once, which submits it,
    // and then dropped, most likely before the kernel completes it.
    let _ = tokio::time::timeout(Duration::ZERO, handler.write_body(chunk, false)).await;
    drop(handler);
    assert!(
        support::temp_entries(cache, &key).is_empty(),
        "the dropped store's temporary is gone"
    );
    support::store(cache, &key, &support::meta(), b"after").await;
    let (_meta, body) = support::lookup_body(cache, &key)
        .await
        .expect("the next store is published");
    assert_eq!(body, b"after");
}

/// Step 8: once the runtime whose task collected the ring's completions has
/// shut down, a store on another runtime still writes on the ring, and
/// completes.
#[test]
fn tc_storage_iouring_01_completions_outlive_the_runtime_that_collected_them() {
    let (cache, _tmp) = cache_with(|config| config.with_durability(Durability::NoStoreSync));
    let runtime = || {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("a runtime builds")
    };
    let bounded = |primary: &'static str| async move {
        tokio::time::timeout(
            Duration::from_secs(10),
            store_counting(cache, primary, &chunks(8, 16 * KIB)),
        )
        .await
        .expect("the store completes")
    };
    let first = runtime();
    let (ring, _) = first.block_on(bounded("/ring-first-runtime"));
    assert_eq!(ring, 4, "the first runtime's store writes on the ring");
    // Shuts the runtime down, and its completion task with it.
    drop(first);
    let second = runtime();
    let (ring, _) = second.block_on(bounded("/ring-second-runtime"));
    assert_eq!(ring, 4, "the second runtime's store writes on the ring");
}
