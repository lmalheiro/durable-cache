// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-STORAGE.PUBLISH-01 — Atomic same-directory publication.
//!
//! The per-writer temporary and its rename publish atomically, keep
//! concurrent writers isolated, never reuse another writer's file,
//! synchronise before publishing, report the record size, and publish
//! nothing once the backend is read-only.
//!
//! Step 4's crash leg (a restart reclaims the abandoned temporary) is
//! covered by `storage_reclaim.rs`, and step 5's replacement leg (a reader
//! of the outgoing object completes undisturbed) by `compat_replace.rs`.
//! Step 7 is an inspection; its test here checks the part a test can.

use bytes::Bytes;
use durable_cache::hooks::FaultPoint;
use durable_cache::{writerid, Durability, FsCacheConfig};
use pingora_cache::key::CacheHashKey;
use pingora_cache::storage::Storage;

use super::support;

// IMP-TC.STORAGE.PUBLISH-02: satisfy TC-STORAGE.PUBLISH-01

/// Step 1: each writer writes its own temporary, named with this process's
/// writer identity and a distinct counter, and neither's bytes appear in
/// the other's file.
#[tokio::test]
async fn tc_storage_publish_01_concurrent_writers_use_their_own_temporaries() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/publish-concurrent");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut a = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    let mut b = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();

    // Interleave the writes across both handlers to provoke any shared
    // state before either completes.
    a.write_body(Bytes::from_static(b"AAAA"), false)
        .await
        .unwrap();
    b.write_body(Bytes::from_static(b"1111"), false)
        .await
        .unwrap();
    a.write_body(Bytes::from_static(b"AAAA"), true)
        .await
        .unwrap();
    b.write_body(Bytes::from_static(b"1111"), true)
        .await
        .unwrap();

    let temps = support::temp_entries(cache, &key);
    assert_eq!(
        temps.len(),
        2,
        "each writer needs its own temporary: {temps:?}"
    );
    let me = cache.lease.id();
    let mut counters: Vec<u64> = temps
        .iter()
        .map(|name| {
            let parsed = writerid::parse_temp_name(name)
                .unwrap_or_else(|| panic!("temporary {name} must carry a writer identity"));
            assert_eq!(parsed.object_name, cache.object_name(&key));
            assert_eq!(parsed.writer, me, "the name carries this instance's lease");
            parsed.counter
        })
        .collect();
    counters.sort();
    assert_ne!(counters[0], counters[1], "the counter keeps writers apart");

    a.finish().await.unwrap();
    b.finish().await.unwrap();

    let (_, body) = support::read_and_validate_record(&support::canonical_path(cache, &key));
    assert!(
        body == b"AAAAAAAA" || body == b"11111111",
        "the body must be one writer's bytes whole, never interleaved: {body:?}"
    );
}

/// Step 2: a file already at the name the next counter would produce is
/// left untouched, and the store takes the following counter value.
#[tokio::test]
async fn tc_storage_publish_01_name_collision_advances_the_counter() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/publish-collision");
    let combined = key.combined();
    let me = cache.lease.id();

    // Claim the names this instance would use next. The counter is the
    // instance's own and monotonic, so claiming a generous range guarantees
    // a collision without depending on its current value.
    let dir = support::shard_dir_of(cache, &key);
    std::fs::create_dir_all(&dir).unwrap();
    let squatted: Vec<_> = (0..4)
        .map(|i| {
            let counter = cache.lease.next_counter() + 1 + i;
            let path = dir.join(writerid::temp_name(&combined, &me, counter));
            std::fs::write(&path, b"another writer's bytes").unwrap();
            path
        })
        .collect();

    support::store(cache, &key, &support::meta(), b"mine").await;

    for path in &squatted {
        assert_eq!(
            std::fs::read(path).unwrap(),
            b"another writer's bytes",
            "a pre-existing temporary must never be reused or clobbered"
        );
    }
    let (_, body) = support::read_and_validate_record(&support::canonical_path(cache, &key));
    assert_eq!(body, b"mine");
}

/// Step 3: the object becomes visible under its canonical name only at the
/// rename, and `finish` reports the published file's full length.
#[tokio::test]
async fn tc_storage_publish_01_visible_only_at_the_rename_and_reports_record_size() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/publish-visibility");
    let meta = support::meta();
    let canonical = support::canonical_path(cache, &key);
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"hello "), false)
        .await
        .unwrap();
    handler
        .write_body(Bytes::from_static(b"world"), true)
        .await
        .unwrap();
    assert!(!canonical.exists(), "not visible before finish()");

    let reported = match handler.finish().await.unwrap() {
        pingora_cache::storage::MissFinishType::Created(size) => size,
        pingora_cache::storage::MissFinishType::Appended(..) => panic!("expected Created"),
    };

    assert!(canonical.exists());
    let on_disk = std::fs::metadata(&canonical).unwrap().len();
    assert_eq!(
        reported as u64, on_disk,
        "the size reported is the record size: the published file's full length, \
         metadata and body together (FRE-COMPAT.EVICTION-01)"
    );
    let (header, body) = support::read_and_validate_record(&canonical);
    assert_eq!(body, b"hello world");
    assert_eq!(header.body_len(), 11);
    assert!(
        on_disk > header.body_len(),
        "the record size is more than the body"
    );
}

/// Step 3, continued: the temporary is synchronised *before* the rename, so
/// a failing synchronisation means nothing is ever published.
#[tokio::test]
async fn tc_storage_publish_01_synchronises_before_renaming() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/publish-sync-order");
    let meta = support::meta();
    let trace = support::inactive_trace();

    cache.hooks().inject_fault(FaultPoint::SyncTemp, 1);
    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"body"), true)
        .await
        .unwrap();
    let err = match handler.finish().await {
        Err(e) => e,
        Ok(_) => panic!("a failed synchronisation must fail the store"),
    };
    assert!(
        err.to_string().contains("SyncTemp"),
        "unexpected error: {err}"
    );

    assert!(
        !support::canonical_path(cache, &key).exists(),
        "nothing may be published when the synchronisation that precedes the rename fails"
    );
    assert!(
        support::temp_entries(cache, &key).is_empty(),
        "the failed store's temporary is removed with its handler"
    );
}

/// With `Durability::NoStoreSync`, a store is renamed into place without
/// being synchronised: a fault injected at the synchronisation is never
/// reached, and the record is published whole.
#[tokio::test]
async fn tc_storage_publish_01_no_store_sync_publishes_without_synchronising() {
    let tmp = tempfile::tempdir().expect("a temporary cache root");
    let cache = support::leaked_cache_with(
        FsCacheConfig::new(tmp.path(), None).with_durability(Durability::NoStoreSync),
    );
    let key = support::key("/publish-no-store-sync");
    let meta = support::meta();
    let trace = support::inactive_trace();

    cache.hooks().inject_fault(FaultPoint::SyncTemp, 1);
    let mut handler = cache
        .get_miss_handler(&key, &meta, &trace)
        .await
        .expect("the store opens");
    handler
        .write_body(Bytes::from_static(b"body"), true)
        .await
        .expect("the body is written");
    handler
        .finish()
        .await
        .expect("a store that skips the synchronisation never meets its fault");

    let (_, body) = support::read_and_validate_record(&support::canonical_path(cache, &key));
    assert_eq!(body, b"body", "the record is published whole");
}

/// Step 6: a store completing once the backend is read-only reports
/// success, publishes nothing, and leaves no temporary behind
/// (FDS-COMPAT.READONLY-01).
#[tokio::test]
async fn tc_storage_publish_01_store_completing_once_read_only_publishes_nothing() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/publish-read-only");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"stored while read-only"), true)
        .await
        .unwrap();

    cache.set_read_only();

    let reported = match handler
        .finish()
        .await
        .expect("the request must complete normally")
    {
        pingora_cache::storage::MissFinishType::Created(size) => size,
        pingora_cache::storage::MissFinishType::Appended(..) => panic!("expected Created"),
    };
    assert!(
        reported > 0,
        "the store is still reported created, with its record size"
    );
    assert!(
        !support::canonical_path(cache, &key).exists(),
        "a read-only backend publishes nothing"
    );
    assert!(
        support::temp_entries(cache, &key).is_empty(),
        "and leaves no temporary"
    );
    assert!(support::lookup_body(cache, &key).await.is_none());
}

/// Step 7 (inspection), the part a test can check: a store's temporary is
/// created in the directory of its canonical name, so the publishing
/// rename is atomic by construction. That no path publishes by copying is
/// left to inspection.
#[tokio::test]
async fn tc_storage_publish_01_temporary_shares_the_canonical_directory() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/publish-same-dir");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"body"), false)
        .await
        .unwrap();

    let canonical = support::canonical_path(cache, &key);
    let temps = support::temp_entries(cache, &key);
    assert_eq!(temps.len(), 1);
    let temp = support::shard_dir_of(cache, &key).join(&temps[0]);
    assert_eq!(
        temp.parent(),
        canonical.parent(),
        "rename must be atomic by construction, not by deployment convention"
    );
    drop(handler);
}

/// A store that replaces a record frees the old one only after releasing
/// the key's publication lock. Paused just before the old record is closed,
/// the store has published, a purge of the key goes through, so the lock is
/// free, and the old record is still open, so the kernel has not freed it.
/// Once the store completes, the old record is closed.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replacing_store_frees_the_old_record_after_releasing_the_lock() {
    use std::time::Duration;

    use durable_cache::hooks::PausePoint;
    use pingora_cache::storage::PurgeOutcome;

    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/replace-then-free");
    support::store(cache, &key, &support::meta(), b"old record").await;
    let path = support::canonical_path(cache, &key);
    let old = support::file_id(&path);

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::BeforeFreeingOldRecord, Some(&path));
    let storing = {
        let key = key.clone();
        tokio::spawn(
            async move { support::store(cache, &key, &support::meta(), b"new record").await },
        )
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the store must reach its pause point"
    );

    assert_ne!(support::file_id(&path), old, "the new record is published");
    assert!(
        support::open_files().contains(&old),
        "the old record is still open"
    );
    let purged = tokio::time::timeout(Duration::from_secs(5), support::purge_active(cache, &key))
        .await
        .expect("the key's lock is free while the old record waits to be closed");
    assert_eq!(purged, PurgeOutcome::Purged(None));

    pause.release();
    storing.await.expect("the store completes");
    assert!(
        !support::open_files().contains(&old),
        "the old record is closed"
    );
}

/// A store that has published drops its handler on the blocking pool, as
/// part of `finish`, not on the async worker that awaited it. Closing the
/// handler's handle on the record frees the record if another store has
/// replaced it, or a purge removed it, in the meantime, which must not
/// stall a worker. Held as its handler is dropped, the store leaves the
/// runtime's only worker free to run other tasks.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_store_drops_its_handler_off_the_worker() {
    use std::time::Duration;

    use durable_cache::hooks::PausePoint;

    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/publish-dropped-off-worker");
    let mut store = cache
        .get_miss_handler(&key, &support::meta(), &support::inactive_trace())
        .await
        .expect("a store opens");
    store
        .write_body(Bytes::from_static(b"published, then dropped"), true)
        .await
        .expect("the body is written");
    let path = support::canonical_path(cache, &key);
    let pause = cache
        .hooks()
        .arm_pause(PausePoint::StoreDropping, Some(&path));
    let finishing = tokio::spawn(async move { store.finish().await });
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the handler is dropped"
    );

    // Were the handler dropped on the only worker, the pause would hold that
    // worker, and this task could not run. The wait is the operating
    // system's, not the runtime's, whose timers a held worker would stop.
    let (ran, waiting) = std::sync::mpsc::channel();
    tokio::spawn(async move { ran.send(()).expect("the test waits for this") });
    assert!(
        waiting.recv_timeout(Duration::from_secs(5)).is_ok(),
        "the worker runs other tasks while the handler's drop is held"
    );

    pause.release();
    finishing
        .await
        .expect("the finishing task completes")
        .expect("the store finishes");
}

/// A panic during a change, while its key's publication lock is held, is
/// reported once, as an error: the next change on the key logs it and clears
/// the poison, so the change after that logs nothing.
#[test]
fn a_poisoned_publication_lock_is_reported_once() {
    use durable_cache::locks::ChangeLocks;

    let logs = support::LogCapture::install();
    let locks = ChangeLocks::default();
    // Every test in this binary shares the capture, so the name is one no
    // other test uses.
    let name = "f00dface".repeat(4);
    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _change = locks.begin(&name);
        panic!("a bug during a change");
    }));
    assert!(panicked.is_err(), "the closure panicked");
    drop(locks.begin(&name));
    drop(locks.begin(&name));
    let reports = logs.matching(&name);
    assert_eq!(reports.len(), 1, "reported once: {reports:?}");
    assert!(reports[0].starts_with("ERROR"), "as an error: {reports:?}");
}

/// Discarding a temporary that is already gone is not an error and logs
/// nothing. One that cannot be removed is logged as a warning naming it,
/// and stays for the restart-time scan (FDS-STORAGE.RECLAIM-01).
#[test]
fn discarding_a_temporary_warns_only_when_it_cannot_be_removed() {
    use durable_cache::publish::discard_temp;

    let logs = support::LogCapture::install();
    // Every test in this binary shares the capture; these paths are this
    // test's own.
    let tmp = tempfile::tempdir().expect("a temporary directory");
    let gone = tmp.path().join("gone.tmp");
    discard_temp(&gone);
    assert!(
        logs.matching(&gone.display().to_string()).is_empty(),
        "a temporary already gone logs nothing"
    );

    // A directory cannot be removed as a file, even by root.
    let stuck = tmp.path().join("stuck.tmp");
    std::fs::create_dir(&stuck).expect("a directory where the temporary would be");
    discard_temp(&stuck);
    let reports = logs.matching(&stuck.display().to_string());
    assert_eq!(reports.len(), 1, "reported once: {reports:?}");
    assert!(reports[0].starts_with("WARN"), "as a warning: {reports:?}");
    assert!(stuck.exists(), "and left for the restart-time scan");
}

/// Holding open a record that is not there is not an error and logs
/// nothing: a key's first store has nothing to replace. A record that is
/// there is held. One that cannot be opened is logged as a warning naming
/// it, and nothing is held, so the rename or unlink frees it under the
/// lock.
#[test]
fn holding_a_record_open_warns_only_when_it_cannot_be_opened() {
    use durable_cache::publish::hold_open;

    let logs = support::LogCapture::install();
    let tmp = tempfile::tempdir().expect("a temporary directory");
    let record = tmp.path().join("record");
    assert!(hold_open(&record).is_none(), "no record, nothing to hold");
    assert!(
        logs.matching(&record.display().to_string()).is_empty(),
        "a record that is not there logs nothing"
    );
    std::fs::write(&record, b"a record").expect("the record is written");
    assert!(
        hold_open(&record).is_some(),
        "a record that is there is held"
    );

    // A path through a regular file cannot be opened, even by root, and the
    // error is not that nothing is there.
    let file = tmp.path().join("a-file");
    std::fs::write(&file, b"").expect("a regular file");
    let unopenable = file.join("record");
    assert!(hold_open(&unopenable).is_none(), "nothing is held");
    let reports = logs.matching(&unopenable.display().to_string());
    assert_eq!(reports.len(), 1, "reported once: {reports:?}");
    assert!(reports[0].starts_with("WARN"), "as a warning: {reports:?}");
}
