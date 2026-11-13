// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-STORAGE.PAGECACHE-01 — Page-cache reads on the worker.
//!
//! On Linux, the lookup and the body reads of a record the page cache holds
//! complete on the worker, without a handoff to the blocking pool; each
//! falls back to the pool when a call would wait, and the answer is the
//! same either way (FDS-STORAGE.PAGECACHE-01). The test hooks count the
//! lookups and body reads handed to the pool, and make a chosen call on the
//! worker report that it would wait. The records were just written, so the
//! page cache holds them.

use bytes::Bytes;
use durable_cache::hooks::{PausePoint, WorkerCall};
use durable_cache::{FsCache, FsCacheConfig};
use pingora_cache::storage::Storage;
use pingora_cache::CacheKey;

use super::support;

// IMP-TC.STORAGE.PAGECACHE-01: satisfy TC-STORAGE.PAGECACHE-01

/// The bodies of step 1: 1 KiB, which the lookup's first read holds whole,
/// and 200 KiB, which takes two chunk reads after it.
fn bodies() -> [(CacheKey, Vec<u8>); 2] {
    [
        (
            support::key("/pagecache-small"),
            support::incompressible(1024, 1).into_bytes(),
        ),
        (
            support::key("/pagecache-large"),
            support::incompressible(200 * 1024, 2).into_bytes(),
        ),
    ]
}

/// Store every body of [`bodies`] on `cache`.
async fn store_all(cache: &'static FsCache) -> [(CacheKey, Vec<u8>); 2] {
    let stored = bodies();
    for (key, body) in &stored {
        support::store(cache, key, &support::meta(), body).await;
    }
    stored
}

/// Look `key` up and read its body to the end, checking it is `body`: how
/// many operations that handed to the pool.
async fn read_counting(cache: &'static FsCache, key: &CacheKey, body: &[u8], step: &str) -> u64 {
    // Read just before, so that the page cache holds the record even on a
    // host short of memory.
    std::fs::read(support::canonical_path(cache, key)).expect("the record reads");
    let before = cache.hooks().handoffs();
    let (_meta, got) = support::lookup_body(cache, key)
        .await
        .unwrap_or_else(|| panic!("{step}: a hit"));
    assert_eq!(got, body, "{step}: served as stored");
    cache.hooks().handoffs() - before
}

/// Step 1: with nothing waiting, no lookup and no body read leaves the
/// worker.
#[tokio::test]
async fn tc_storage_pagecache_01_cached_hits_stay_on_the_worker() {
    let (cache, _tmp) = support::leaked_cache();
    for (key, body) in &store_all(cache).await {
        assert_eq!(read_counting(cache, key, body, "step 1").await, 0);
    }
}

/// Step 2: a worker open that would wait sends that lookup to the pool,
/// once; its body reads still run on the worker.
#[tokio::test]
async fn tc_storage_pagecache_01_an_open_that_would_wait_goes_to_the_pool() {
    let (cache, _tmp) = support::leaked_cache();
    for (key, body) in &store_all(cache).await {
        let path = support::canonical_path(cache, key);
        cache.hooks().arm_worker_wait(WorkerCall::Open, &path, 0);
        assert_eq!(read_counting(cache, key, body, "step 2").await, 1);
    }
}

/// Step 3: a body read that would wait sends that chunk to the pool, once.
#[tokio::test]
async fn tc_storage_pagecache_01_a_read_that_would_wait_goes_to_the_pool() {
    let (cache, _tmp) = support::leaked_cache();
    let [_, (key, body)] = store_all(cache).await;
    let path = support::canonical_path(cache, &key);
    cache
        .hooks()
        .arm_worker_wait(WorkerCall::BodyRead, &path, 1);
    assert_eq!(read_counting(cache, &key, &body, "step 3").await, 1);
}

/// Step 4: a key never stored is a miss, and so is a record whose metadata
/// checksum fails, reached through the pool's locked re-read.
#[tokio::test]
async fn tc_storage_pagecache_01_misses_are_the_same_on_either_path() {
    let (cache, _tmp) = support::leaked_cache();
    assert!(
        support::lookup_body(cache, &support::key("/pagecache-never-stored"))
            .await
            .is_none(),
        "step 4: a key never stored is a miss"
    );

    let [(key, body), _] = bodies();
    support::store(cache, &key, &support::meta(), &body).await;
    let path = support::canonical_path(cache, &key);
    let meta_start = support::read_header(&path).meta_region_offset() as u64;
    support::flip_byte_at(&path, meta_start + 1);
    // The pool's re-read of a record whose checksums keep failing takes
    // the key's publication lock: held here, the lookup waits for it.
    let name = cache.object_name(&key);
    let (locked, is_locked) = std::sync::mpsc::channel();
    let (release, on_release) = std::sync::mpsc::channel::<()>();
    let holder = std::thread::spawn(move || {
        let _lock = cache.locks.lock_key(&name);
        locked.send(()).expect("the test waits for the lock");
        let _ = on_release.recv();
    });
    is_locked.recv().expect("the lock is taken");
    let before = cache.hooks().handoffs();
    let lookup = tokio::spawn(async move { support::lookup_body(cache, &key).await });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(
        !lookup.is_finished(),
        "step 4: the lookup waits for the key's lock"
    );
    release.send(()).expect("the holder waits");
    holder.join().expect("the holder ends");
    assert!(
        lookup.await.expect("the lookup completes").is_none(),
        "step 4: a damaged record is a miss"
    );
    assert_eq!(
        cache.hooks().handoffs() - before,
        1,
        "step 4: the damaged record's lookup went to the pool"
    );
}

/// Step 5: with page-cache reads off, every lookup goes to the pool, and so
/// does every chunk the lookup's first read did not hold.
#[tokio::test]
async fn tc_storage_pagecache_01_switched_off_every_read_goes_to_the_pool() {
    let tmp = support::tempdir();
    let cache = support::leaked_cache_with(
        FsCacheConfig::new(tmp.path(), None).with_page_cache_reads(false),
    );
    let [(small, small_body), (large, large_body)] = store_all(cache).await;
    assert_eq!(
        read_counting(cache, &small, &small_body, "step 5").await,
        1,
        "step 5: the lookup; its first read held the whole body"
    );
    assert_eq!(
        read_counting(cache, &large, &large_body, "step 5").await,
        3,
        "step 5: the lookup and two chunks"
    );
}

/// Step 6: a partial hit, on a store still in flight, opens and reads what
/// the store has written on the worker.
#[tokio::test]
async fn tc_storage_pagecache_01_partial_hits_stay_on_the_worker() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/pagecache-partial");
    let trace = support::inactive_trace();
    let mut store = cache
        .get_miss_handler(&key, &support::meta(), &trace)
        .await
        .expect("a store opens");
    let chunk = support::incompressible(30_000, 3).into_bytes();
    store
        .write_body(Bytes::from(chunk.clone()), false)
        .await
        .expect("a chunk is written");
    let tag = store
        .streaming_write_tag()
        .map(<[u8]>::to_vec)
        .expect("a write tag");

    let before = cache.hooks().handoffs();
    let (_meta, mut follower) = cache
        .lookup_streaming_write(&key, Some(&tag), &trace)
        .await
        .expect("the lookup succeeds")
        .expect("a partial hit");
    let read = follower
        .read_body()
        .await
        .expect("the read succeeds")
        .expect("what the store wrote");
    assert_eq!(&read[..], &chunk[..], "step 6: served as written");
    assert_eq!(
        cache.hooks().handoffs() - before,
        0,
        "step 6: neither the open nor the read left the worker"
    );

    drop(follower);
    store
        .write_body(Bytes::new(), true)
        .await
        .expect("the store ends");
    store.finish().await.expect("the store publishes");
}

/// Whether `e`, the error of an operation whose call on the worker the
/// system lacks, names the file and the option that asked for the call.
fn names_file_and_option(e: &pingora_error::Error, path: &std::path::Path) -> bool {
    let text = e.to_string();
    text.contains(&path.display().to_string()) && text.contains("with_page_cache_reads")
}

/// A call the system lacks, at run time, fails the operation with an error
/// naming the file and the option, and the pool does not do it instead: the
/// open, the record's first read, which also hands the open file to the pool
/// to close, and a body read (FDS-STORAGE.PAGECACHE-01, "Calls the system
/// lacks").
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tc_storage_pagecache_01_a_call_the_system_lacks_fails_the_operation() {
    let (cache, _tmp) = support::leaked_cache();
    let [(small, _), (large, _)] = store_all(cache).await;
    let trace = support::inactive_trace();

    let path = support::canonical_path(cache, &small);
    cache
        .hooks()
        .arm_worker_unsupported(WorkerCall::Open, &path, 0);
    let before = cache.hooks().handoffs();
    match cache.lookup(&small, &trace).await {
        Err(e) => assert!(names_file_and_option(&e, &path), "the open: {e}"),
        Ok(_) => panic!("an open the system lacks fails the lookup"),
    }
    assert_eq!(cache.hooks().handoffs(), before, "no pool did the lookup");

    cache
        .hooks()
        .arm_worker_unsupported(WorkerCall::RecordRead, &path, 0);
    let closing = cache
        .hooks()
        .arm_pause(PausePoint::BeforeClosingRemovedFile, Some(&path));
    match cache.lookup(&small, &trace).await {
        Err(e) => assert!(names_file_and_option(&e, &path), "the first read: {e}"),
        Ok(_) => panic!("a read the system lacks fails the lookup"),
    }
    assert!(
        closing.wait_reached(std::time::Duration::from_secs(5)),
        "the worker hands the open file to the pool to close"
    );
    closing.release();

    let path = support::canonical_path(cache, &large);
    cache
        .hooks()
        .arm_worker_unsupported(WorkerCall::BodyRead, &path, 0);
    let (_meta, mut hit) = support::lookup_hit(cache, &large).await;
    let e = support::drain_body(&mut hit)
        .await
        .expect_err("a body read the system lacks fails");
    assert!(names_file_and_option(&e, &path), "the body read: {e}");
}

/// The same for a partial hit's open and first read of a store's temporary.
#[tokio::test]
async fn tc_storage_pagecache_01_a_partial_hits_call_the_system_lacks_fails() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/pagecache-partial-unsupported");
    let trace = support::inactive_trace();
    let mut store = cache
        .get_miss_handler(&key, &support::meta(), &trace)
        .await
        .expect("a store opens");
    store
        .write_body(Bytes::from_static(b"in flight"), false)
        .await
        .expect("a chunk is written");
    let tag = store
        .streaming_write_tag()
        .map(<[u8]>::to_vec)
        .expect("a write tag");
    let [temp] = support::temp_entries(cache, &key)
        .try_into()
        .expect("one temporary");
    let temp = support::shard_dir_of(cache, &key).join(temp);

    for call in [WorkerCall::Open, WorkerCall::RecordRead] {
        cache.hooks().arm_worker_unsupported(call, &temp, 0);
        match cache.lookup_streaming_write(&key, Some(&tag), &trace).await {
            Err(e) => assert!(names_file_and_option(&e, &temp), "{call:?}: {e}"),
            Ok(_) => panic!("{call:?}: a call the system lacks fails the partial hit"),
        }
    }
    drop(store);
}
