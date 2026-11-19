// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-STORAGE.INLINEWRITES-01 — Inline write slots.
//!
//! An unsynchronised store's operations run on the worker that polls them
//! when an inline slot is free, never more of them at once than the
//! runtime's workers less one, and on the blocking pool otherwise
//! (FDS-STORAGE.INLINEWRITES-01). The test hooks count the operations that
//! ran in a slot, and hold a body write in its slot.
//!
//! A test's own body runs on the thread that drives the runtime rather than
//! on one of its workers, but what decides is the same: the slots, and the
//! workers of the runtime that polls the operation.

use std::time::Duration;

use bytes::Bytes;
use durable_cache::hooks::PausePoint;
use durable_cache::{Durability, FsCache, FsCacheConfig};
use pingora_cache::storage::{MissFinishType, Storage};

use super::support;

// IMP-TC.STORAGE.INLINEWRITES-01: satisfy TC-STORAGE.INLINEWRITES-01

/// A cache with `config`'s durability and slots, rooted in a new directory.
fn cache_with(
    configure: impl FnOnce(FsCacheConfig) -> FsCacheConfig,
) -> (&'static FsCache, tempfile::TempDir) {
    let tmp = support::tempdir();
    let cache = support::leaked_cache_with(configure(FsCacheConfig::new(tmp.path(), None)));
    (cache, tmp)
}

/// A cache whose stores are unsynchronised, with the default inline slots.
fn unsynced_cache() -> (&'static FsCache, tempfile::TempDir) {
    cache_with(|config| config.with_durability(Durability::NoStoreSync))
}

/// Store a body of two chunks under `primary`, and read it back: how many
/// of the store's operations ran in an inline slot.
async fn store_counting(cache: &'static FsCache, primary: &str) -> u64 {
    let key = support::key(primary);
    let before = cache.hooks().inline_ops();
    support::store_chunks(cache, &key, &support::meta(), &[b"first ", b"and last"]).await;
    let ops = cache.hooks().inline_ops() - before;
    let (_meta, body) = support::lookup_body(cache, &key)
        .await
        .expect("the store is published");
    assert_eq!(body, b"first and last", "the body reads as stored");
    ops
}

/// Step 1: on four workers, an unsynchronised store opens, writes its first
/// chunk, and finishes, writing the last, each in an inline slot.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_storage_inlinewrites_01_an_unsynchronised_store_writes_on_the_worker() {
    let (cache, _tmp) = unsynced_cache();
    assert_eq!(
        store_counting(cache, "/inline-unsynced").await,
        3,
        "the opening, the first chunk and the finish run in a slot"
    );
}

/// Step 2: a synchronised store keeps every operation on the pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_storage_inlinewrites_01_a_synchronised_store_stays_on_the_pool() {
    let (cache, _tmp) = cache_with(|config| config.with_durability(Durability::Strict));
    assert_eq!(store_counting(cache, "/inline-strict").await, 0);
}

/// Step 3: a runtime of one worker, Pingora's default, runs nothing in a
/// slot, whatever the configuration asks.
#[tokio::test]
async fn tc_storage_inlinewrites_01_one_worker_writes_nothing_inline() {
    let (cache, _tmp) = unsynced_cache();
    assert_eq!(store_counting(cache, "/inline-one-worker").await, 0);
}

/// Step 4: no slots keep every operation on the pool.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_storage_inlinewrites_01_no_slots_keep_every_operation_on_the_pool() {
    let (cache, _tmp) = cache_with(|config| {
        config
            .with_durability(Durability::NoStoreSync)
            .with_inline_write_slots(0)
    });
    assert_eq!(store_counting(cache, "/inline-no-slots").await, 0);
}

/// Step 5: on two workers, three slots asked for give one. While a body
/// write holds it, another store's opening goes to the pool; once the write
/// is let go, the slot serves again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tc_storage_inlinewrites_01_the_workers_less_one_cap_the_slots() {
    let (cache, _tmp) = unsynced_cache();
    let trace = support::inactive_trace();
    let meta = support::meta();
    let mut held = cache
        .get_miss_handler(&support::key("/inline-held"), &meta, &trace)
        .await
        .expect("the store opens");

    let pause = cache.hooks().arm_pause(PausePoint::WritingBodyChunk, None);
    let before = cache.hooks().inline_ops();
    let write = tokio::spawn(async move {
        held.write_body(Bytes::from_static(b"held"), false)
            .await
            .expect("the held write completes once released");
        held
    });
    assert!(
        pause.wait_reached(Duration::from_secs(10)),
        "the write reaches its pause"
    );
    assert_eq!(
        cache.hooks().inline_ops() - before,
        1,
        "the held write runs in the one slot"
    );

    let other = cache
        .get_miss_handler(&support::key("/inline-other"), &meta, &trace)
        .await
        .expect("the other store opens");
    assert_eq!(
        cache.hooks().inline_ops() - before,
        1,
        "with the one slot taken, the other store opens on the pool"
    );

    pause.release();
    let held = write.await.expect("the held write's task ends");
    let after_release = cache.hooks().inline_ops();
    drop(other);
    let finished = held.finish().await.expect("the held store finishes");
    assert!(matches!(finished, MissFinishType::Created(_)));
    assert_eq!(
        cache.hooks().inline_ops() - after_release,
        1,
        "the slot is free again: the finish takes it"
    );
}

/// Step 6: with the discard after a host restart on, which may have to
/// write and synchronise the marker, a store finishes on the pool; its
/// opening and first chunk still run in a slot.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_storage_inlinewrites_01_with_the_discard_on_the_finish_stays_on_the_pool() {
    let (cache, _tmp) = cache_with(|config| {
        config
            .with_durability(Durability::NoStoreSync)
            .with_discard_after_host_restart(true)
    });
    assert_eq!(store_counting(cache, "/inline-discard").await, 2);
}

/// Step 7: a store that replaces a record, finished in a slot, leaves the
/// replaced record's close to the pool, since the close frees it: the store
/// completes while that close is held.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_storage_inlinewrites_01_a_replaced_record_is_freed_on_the_pool() {
    let (cache, _tmp) = unsynced_cache();
    let key = support::key("/inline-replace");
    support::store(cache, &key, &support::meta(), b"old").await;
    let canonical = support::canonical_path(cache, &key);

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::BeforeClosingRemovedFile, Some(&canonical));
    let before = cache.hooks().inline_ops();
    support::store(cache, &key, &support::meta(), b"new").await;
    assert_eq!(
        cache.hooks().inline_ops() - before,
        2,
        "the replacing store opens and finishes in a slot"
    );
    assert!(
        pause.wait_reached(Duration::from_secs(10)),
        "the replaced record is closed on the pool, which the store did not wait for"
    );
    pause.release();
    let (_meta, body) = support::lookup_body(cache, &key)
        .await
        .expect("the new record is published");
    assert_eq!(body, b"new");
}
