// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-STORAGE.PURGE-01 — Purge unlinks the canonical name only.
//!
//! Purge resolves and unlinks only the canonical path derived from the
//! presented key's combined hash, is idempotent and a harmless no-op on an
//! absent key, ends a same-key store in flight when it invalidates and
//! leaves it alone when it evicts, disturbs no reader already streaming the
//! file, answers an invalidation only once the shard directory is
//! synchronised, and fails once the backend is read-only.

use std::time::Duration;

use bytes::Bytes;
use durable_cache::hooks::{FaultPoint, PausePoint};
use pingora_cache::storage::{PurgeOutcome, PurgeTarget, PurgeType, Storage};

use super::support;

// IMP-TC.STORAGE.PURGE-01: satisfy TC-STORAGE.PURGE-01

/// Purge as `HttpCache` issues an invalidation, reducing the outcome to
/// "was anything removed" for the steps that only care about that.
async fn purge(cache: &'static durable_cache::FsCache, key: &pingora_cache::CacheKey) -> bool {
    match support::purge_active(cache, key).await {
        PurgeOutcome::Purged(None) => true,
        PurgeOutcome::NotFound => false,
        other => panic!("this backend assigns no entry identifier, got {other:?}"),
    }
}

/// A store of `key` holding `body` in full, open and not yet finished.
async fn store_in_flight(
    cache: &'static durable_cache::FsCache,
    key: &pingora_cache::CacheKey,
    body: &'static [u8],
) -> pingora_cache::storage::MissHandler {
    let mut handler = cache
        .get_miss_handler(key, &support::meta(), &support::inactive_trace())
        .await
        .expect("a store opens");
    handler
        .write_body(Bytes::from_static(body), true)
        .await
        .expect("the body is written");
    handler
}

/// Steps 1 and 2: a primary purge never touches a `Vary` variant, and a
/// variant purge — which `HttpCache::spawn_async_purge` can present — never
/// touches its primary.
#[tokio::test]
async fn tc_storage_purge_01_removes_only_the_presented_keys_own_file() {
    let (cache, _tmp) = support::leaked_cache();
    let primary = support::key("/purge-slots");
    let variant = support::variant(&primary, [0x11; 16]);

    support::store(cache, &primary, &support::meta(), b"primary body").await;
    support::store(cache, &variant, &support::meta(), b"variant body").await;
    let primary_path = support::canonical_path(cache, &primary);
    let variant_path = support::canonical_path(cache, &variant);

    assert!(purge(cache, &primary).await, "the primary was removed");
    assert!(!primary_path.exists());
    assert!(
        variant_path.exists(),
        "a primary purge never touches a variant's file"
    );

    // Again, the other way round.
    support::store(cache, &primary, &support::meta(), b"primary body").await;
    assert!(purge(cache, &variant).await, "the variant was removed");
    assert!(!variant_path.exists());
    assert!(
        primary_path.exists(),
        "a variant purge never touches its primary"
    );
}

/// Step 3: purging twice, or purging a key that was never stored, reports
/// nothing removed, with no error and no side effect.
#[tokio::test]
async fn tc_storage_purge_01_absent_and_repeated_purges_are_harmless_no_ops() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/purge-twice");
    let bystander = support::key("/purge-bystander");
    support::store(cache, &key, &support::meta(), b"body").await;
    support::store(cache, &bystander, &support::meta(), b"untouched").await;

    assert!(purge(cache, &key).await);
    assert!(
        !purge(cache, &key).await,
        "the second purge removes nothing"
    );
    assert!(
        !purge(cache, &support::key("/never-stored")).await,
        "a key that was never stored removes nothing"
    );
    assert_eq!(
        support::lookup_body(cache, &bystander).await.unwrap().1,
        b"untouched",
        "no other object is affected"
    );
}

/// Step 4: the two target kinds `HttpCache` issues. An `Active`
/// target (an invalidation) and a key-only `Exact` target (an eviction
/// victim) resolve to the same file and report `Purged(None)` — `None`
/// because this backend assigns no entry identifier, which is what makes the
/// caller's `eviction.remove` address the entry it admitted
/// (ADR-COMPAT.ENTRYID-01).
#[tokio::test]
async fn tc_storage_purge_01_active_and_key_only_exact_targets_agree() {
    let (cache, _tmp) = support::leaked_cache();
    let trace = support::inactive_trace();

    let by_active = support::key("/purge-target-active");
    support::store(cache, &by_active, &support::meta(), b"body").await;
    assert_eq!(
        support::purge_active(cache, &by_active).await,
        PurgeOutcome::Purged(None)
    );
    assert!(!support::canonical_path(cache, &by_active).exists());

    let by_exact = support::key("/purge-target-exact");
    support::store(cache, &by_exact, &support::meta(), b"body").await;
    let entry = pingora_cache::eviction::CacheEntryKey::key_only(support::compact(&by_exact));
    assert_eq!(
        cache
            .purge(PurgeTarget::Exact(&entry), PurgeType::Eviction, &trace)
            .await
            .unwrap(),
        PurgeOutcome::Purged(None)
    );
    assert!(!support::canonical_path(cache, &by_exact).exists());
}

/// Step 5: an `Exact` target carrying an `Identified` entry key names a
/// stored generation this backend never issued, so it is reported not-found
/// and the file is left in place — never resolved to the key's canonical
/// path (ADR-COMPAT.ENTRYID-01).
#[tokio::test]
async fn tc_storage_purge_01_an_identified_exact_target_is_not_found() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/purge-target-identified");
    support::store(cache, &key, &support::meta(), b"body").await;
    let path = support::canonical_path(cache, &key);

    let entry = pingora_cache::eviction::CacheEntryKey::identified(
        support::compact(&key),
        pingora_cache::eviction::CacheEntryId::new(7),
    );
    let trace = support::inactive_trace();
    assert_eq!(
        cache
            .purge(PurgeTarget::Exact(&entry), PurgeType::Eviction, &trace)
            .await
            .unwrap(),
        PurgeOutcome::NotFound
    );
    assert!(path.exists(), "the record is left in place");
    assert!(support::lookup_body(cache, &key).await.is_some());
}

/// Step 6: an invalidation ends a same-key store in flight
/// (FRE-COMPLIANCE.PURGE-01). It leaves the writer's temporary to the
/// writer; an untagged lookup no longer finds the store, while the request
/// writing it still reads it back by its tag, and a reader already
/// streaming it gets the whole body. Finishing, the store publishes nothing
/// and reports the purge; a store begun after the purge publishes as usual.
#[tokio::test]
async fn tc_storage_purge_01_an_invalidation_ends_a_store_in_flight() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/purge-racing-writer");
    let meta = support::meta();
    let trace = support::inactive_trace();
    support::store(cache, &key, &meta, b"old").await;

    let mut handler = cache
        .get_miss_handler(&key, &meta, &trace)
        .await
        .expect("a store opens");
    handler
        .write_body(Bytes::from_static(b"new "), false)
        .await
        .expect("a first chunk is written");
    let tag = handler
        .streaming_write_tag()
        .expect("the store has a streaming write tag")
        .to_vec();
    let (_meta, mut reader) = support::lookup_hit(cache, &key).await;
    let temps_before = support::temp_entries(cache, &key);
    assert_eq!(temps_before.len(), 1);

    assert_eq!(
        support::purge_active(cache, &key).await,
        PurgeOutcome::Purged(None),
        "the published object is removed"
    );
    assert_eq!(
        support::temp_entries(cache, &key),
        temps_before,
        "the purge leaves the writer's temporary to the writer"
    );
    assert!(
        cache
            .lookup(&key, &trace)
            .await
            .expect("a lookup")
            .is_none(),
        "an untagged lookup no longer finds the store"
    );
    assert!(
        cache
            .lookup_streaming_write(&key, Some(&tag), &trace)
            .await
            .expect("a tagged lookup")
            .is_some(),
        "the request writing the store still reads it back"
    );

    handler
        .write_body(Bytes::from_static(b"body"), true)
        .await
        .expect("the last chunk is written");
    let err = handler
        .finish()
        .await
        .err()
        .expect("a store the purge ended publishes nothing");
    assert_eq!(
        err.etype,
        pingora_error::ErrorType::Custom("store_purged"),
        "unexpected error: {err}"
    );
    assert!(
        support::temp_entries(cache, &key).is_empty(),
        "the store removed its temporary"
    );
    assert!(support::lookup_body(cache, &key).await.is_none());
    assert_eq!(
        support::drain_body(&mut reader)
            .await
            .expect("a clean end of body"),
        b"new body",
        "a reader already streaming the store gets its whole body"
    );

    support::store(cache, &key, &meta, b"after").await;
    assert_eq!(
        support::lookup_body(cache, &key)
            .await
            .expect("a store begun after the purge publishes")
            .1,
        b"after"
    );
}

/// Step 6, continued: a key that holds nothing but a store in flight is not
/// absent. An invalidation ends the store and reports the key purged.
#[tokio::test]
async fn tc_storage_purge_01_a_store_in_flight_alone_is_purged() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/purge-only-in-flight");
    let trace = support::inactive_trace();
    let mut handler = cache
        .get_miss_handler(&key, &support::meta(), &trace)
        .await
        .expect("a store opens");
    handler
        .write_body(Bytes::from_static(b"body"), true)
        .await
        .expect("the body is written");

    assert_eq!(
        support::purge_active(cache, &key).await,
        PurgeOutcome::Purged(None)
    );
    let err = handler
        .finish()
        .await
        .err()
        .expect("a store the purge ended publishes nothing");
    assert_eq!(err.etype, pingora_error::ErrorType::Custom("store_purged"));
    assert!(support::lookup_body(cache, &key).await.is_none());
}

/// Step 6, continued: a purge made to evict leaves a same-key store in
/// flight alone: lookups still find it, and it publishes when it finishes.
#[tokio::test]
async fn tc_storage_purge_01_an_eviction_leaves_a_store_in_flight_alone() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/purge-evicting-a-writer");
    let meta = support::meta();
    let trace = support::inactive_trace();
    support::store(cache, &key, &meta, b"old").await;

    let mut handler = cache
        .get_miss_handler(&key, &meta, &trace)
        .await
        .expect("a store opens");
    handler
        .write_body(Bytes::from_static(b"new body"), true)
        .await
        .expect("the body is written");
    let entry = pingora_cache::eviction::CacheEntryKey::key_only(support::compact(&key));
    assert_eq!(
        cache
            .purge(PurgeTarget::Exact(&entry), PurgeType::Eviction, &trace)
            .await
            .expect("an eviction"),
        PurgeOutcome::Purged(None)
    );
    assert!(
        cache
            .lookup(&key, &trace)
            .await
            .expect("a lookup")
            .is_some(),
        "a lookup still finds the store in flight"
    );

    handler.finish().await.expect("the store still publishes");
    assert_eq!(
        support::lookup_body(cache, &key)
            .await
            .expect("the store's object")
            .1,
        b"new body"
    );
}

/// Step 7: a reader already streaming an object runs to the end of the body
/// it opened, and a later lookup misses (FRE-COMPLIANCE.READERISOLATION-01).
#[tokio::test]
async fn tc_storage_purge_01_reader_mid_stream_completes_undisturbed() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/purge-reader");
    let body: Vec<u8> = (0..300_000u32).map(|i| i as u8).collect();
    support::store(cache, &key, &support::meta(), &body).await;

    let (_meta, mut hit) = support::lookup_hit(cache, &key).await;
    let first = hit.read_body().await.unwrap().expect("a first chunk");

    assert!(purge(cache, &key).await);

    let mut read = first.to_vec();
    read.extend_from_slice(
        &support::drain_body(&mut hit)
            .await
            .expect("a clean end of body"),
    );
    assert_eq!(
        read, body,
        "the reader receives exactly the object it opened"
    );
    assert!(
        support::lookup_body(cache, &key).await.is_none(),
        "a later lookup misses"
    );
}

/// Step 8: an invalidation racing a completing store leaves the key absent
/// in either order, as it ends a store still in flight and removes one that
/// has published; a purge made to evict and a store resolve to whichever
/// lands last. No partial state either way.
#[tokio::test]
async fn tc_storage_purge_01_a_race_with_a_store_resolves_by_purge_type() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/purge-race");
    let meta = support::meta();
    let trace = support::inactive_trace();
    let entry = pingora_cache::eviction::CacheEntryKey::key_only(support::compact(&key));
    let evict = || cache.purge(PurgeTarget::Exact(&entry), PurgeType::Eviction, &trace);
    support::store(cache, &key, &meta, b"old").await;

    // An invalidation, then the store's finish: the store publishes nothing.
    let handler = store_in_flight(cache, &key, b"newer").await;
    assert!(purge(cache, &key).await);
    handler
        .finish()
        .await
        .err()
        .expect("the invalidation ended the store");
    assert!(support::lookup_body(cache, &key).await.is_none());

    // The store's rename, then an invalidation: the invalidation removes it.
    let handler = store_in_flight(cache, &key, b"newest").await;
    handler.finish().await.expect("the store publishes");
    assert!(purge(cache, &key).await);
    assert!(support::lookup_body(cache, &key).await.is_none());

    // An eviction, then the store's rename: the store lands.
    support::store(cache, &key, &meta, b"old").await;
    let handler = store_in_flight(cache, &key, b"evicted, then stored").await;
    assert_eq!(
        evict().await.expect("an eviction"),
        PurgeOutcome::Purged(None)
    );
    handler.finish().await.expect("the store publishes");
    assert_eq!(
        support::lookup_body(cache, &key)
            .await
            .expect("the store's object")
            .1,
        b"evicted, then stored"
    );

    // The store's rename, then an eviction: the eviction removes it.
    let handler = store_in_flight(cache, &key, b"stored, then evicted").await;
    handler.finish().await.expect("the store publishes");
    assert_eq!(
        evict().await.expect("an eviction"),
        PurgeOutcome::Purged(None)
    );
    assert!(support::lookup_body(cache, &key).await.is_none());
}

/// Step 9: the shard directory is synchronised after the unlink and before
/// the purge reports a removal, so a purge whose synchronisation fails
/// reports an error rather than a removal it cannot guarantee
/// (FRE-COMPLIANCE.PURGEDURABLE-01).
#[tokio::test]
async fn tc_storage_purge_01_failed_directory_sync_fails_the_purge() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/purge-sync-fails");
    support::store(cache, &key, &support::meta(), b"body").await;

    cache.hooks().inject_fault(FaultPoint::SyncDir, 1);
    let trace = support::inactive_trace();
    let compact = support::compact(&key);
    let err = cache
        .purge(
            PurgeTarget::Active(&compact),
            PurgeType::Invalidation,
            &trace,
        )
        .await
        .expect_err("a purge that cannot be made durable must not be acknowledged");
    assert!(
        err.to_string().contains("SyncDir"),
        "unexpected error: {err}"
    );

    // A purge whose directory sync failed still removed the entry: the
    // object is gone, and only its durability is in doubt, which is exactly
    // what the error reports.
    assert!(support::lookup_body(cache, &key).await.is_none());
}

/// Step 9, continued: only an invalidation synchronises the shard directory
/// (FRE-COMPLIANCE.PURGEDURABLE-01). With the synchronisation made to fail,
/// a purge made to evict still removes its record, since it does not
/// synchronise; an invalidation of the evicted key finds nothing and still
/// synchronises, so that a crash cannot undo the eviction after the
/// invalidation's answer, and fails; and an invalidation of a key whose
/// shard directory does not exist has nothing to synchronise.
#[tokio::test]
async fn tc_storage_purge_01_only_an_invalidation_synchronises() {
    let (cache, _tmp) = support::leaked_cache();
    let trace = support::inactive_trace();
    let key = support::key("/purge-evicted");
    support::store(cache, &key, &support::meta(), b"body").await;
    let compact = support::compact(&key);

    cache.hooks().inject_fault(FaultPoint::SyncDir, 1);
    let entry = pingora_cache::eviction::CacheEntryKey::key_only(compact.clone());
    assert_eq!(
        cache
            .purge(PurgeTarget::Exact(&entry), PurgeType::Eviction, &trace)
            .await
            .expect("an eviction does not synchronise, so the fault is not reached"),
        PurgeOutcome::Purged(None)
    );
    assert!(!support::canonical_path(cache, &key).exists());

    let err = cache
        .purge(
            PurgeTarget::Active(&compact),
            PurgeType::Invalidation,
            &trace,
        )
        .await
        .expect_err("an invalidation that finds nothing still synchronises");
    assert!(
        err.to_string().contains("SyncDir"),
        "unexpected error: {err}"
    );

    let elsewhere = support::key("/purge-in-a-shard-never-created");
    let shard = support::canonical_path(cache, &elsewhere);
    assert!(
        !shard.parent().expect("a shard directory").exists(),
        "the key's shard directory must not exist for this check"
    );
    cache.hooks().inject_fault(FaultPoint::SyncDir, 1);
    assert_eq!(
        support::purge_active(cache, &elsewhere).await,
        PurgeOutcome::NotFound,
        "a shard directory that does not exist has nothing to synchronise"
    );
}

/// Step 10: once the backend is read-only, a purge fails with an error and
/// leaves the file in place (FRE-COMPAT.HOTUPGRADE-01).
#[tokio::test]
async fn tc_storage_purge_01_once_read_only_fails_and_changes_nothing() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/purge-read-only");
    support::store(cache, &key, &support::meta(), b"body").await;
    let path = support::canonical_path(cache, &key);

    cache.set_read_only();

    let trace = support::inactive_trace();
    let compact = support::compact(&key);
    let err = cache
        .purge(
            PurgeTarget::Active(&compact),
            PurgeType::Invalidation,
            &trace,
        )
        .await
        .expect_err("a read-only backend refuses purges");
    assert_eq!(
        err.etype,
        pingora_error::ErrorType::Custom("cache_read_only"),
        "unexpected error: {err}"
    );
    assert!(path.exists(), "the file is left in place");
    assert!(
        support::lookup_body(cache, &key).await.is_some(),
        "and reads keep working while read-only"
    );
}

/// Step 11: a purge whose unlink fails for any reason but the record's
/// absence reports an error and leaves the path as it was, rather than
/// telling the client there was nothing to purge. A directory where the
/// key's record would be makes the unlink fail (`EISDIR`), whatever the
/// test's privileges.
#[tokio::test]
async fn tc_storage_purge_01_a_failed_unlink_fails_the_purge() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/purge-unlink-fails");
    let path = support::canonical_path(cache, &key);
    std::fs::create_dir_all(&path).expect("a directory where the record would be");

    let compact = support::compact(&key);
    let err = cache
        .purge(
            PurgeTarget::Active(&compact),
            PurgeType::Invalidation,
            &support::inactive_trace(),
        )
        .await
        .expect_err("a purge that cannot unlink must not report the key absent");
    assert!(
        err.to_string().contains("unlinking"),
        "unexpected error: {err}"
    );
    assert!(path.is_dir(), "the path is left as it was");
}

/// A purge frees the record only after releasing the key's publication
/// lock. Paused just before the record is closed, the purge has unlinked it,
/// a store of the key goes through, so the lock is free, and the record is
/// still open, so the kernel has not freed it. Once the purge completes, the
/// record is closed.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_purge_frees_the_record_after_releasing_the_lock() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/purge-then-free");
    support::store(cache, &key, &support::meta(), b"record").await;
    let path = support::canonical_path(cache, &key);
    let old = support::file_id(&path);

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::BeforeFreeingOldRecord, Some(&path));
    let purging = {
        let key = key.clone();
        tokio::spawn(async move { support::purge_active(cache, &key).await })
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the purge must reach its pause point"
    );

    assert!(!path.exists(), "the record is unlinked");
    assert!(
        support::open_files().contains(&old),
        "the record is still open"
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        support::store(cache, &key, &support::meta(), b"stored again"),
    )
    .await
    .expect("the key's lock is free while the record waits to be closed");

    pause.release();
    assert_eq!(
        purging.await.expect("the purge completes"),
        PurgeOutcome::Purged(None)
    );
    assert!(
        !support::open_files().contains(&old),
        "the record is closed"
    );
}

/// A hit can hold the last descriptor of a record purged while it was read,
/// and closing that descriptor frees the record. The hit's handler, dropped
/// on an async worker under Pingora, makes that close on the blocking pool
/// (FDS-STORAGE.EXECUTOR-01). Held just before the close, the drop has
/// already returned and the record is still open; released, it is closed.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hit_on_a_purged_record_closes_it_off_the_dropping_thread() {
    use std::time::Instant;

    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/purge-hit-closed-off-thread");
    support::store(cache, &key, &support::meta(), b"read while purged").await;
    let path = support::canonical_path(cache, &key);
    let record = support::file_id(&path);
    let (_meta, hit) = support::lookup_hit(cache, &key).await;
    assert!(purge(cache, &key).await, "the record is purged");

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::BeforeClosingRemovedFile, Some(&path));
    // Dropped in a task: were the close made on the dropping thread, the
    // pause would hold the drop, and the timeout would fail the test.
    let dropping = tokio::spawn(async move { drop(hit) });
    tokio::time::timeout(Duration::from_secs(5), dropping)
        .await
        .expect("the drop returns without waiting for the close")
        .expect("the dropping task completes");
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the close is made on the blocking pool"
    );
    assert!(
        support::open_files().contains(&record),
        "the record is still open until then"
    );

    pause.release();
    let deadline = Instant::now() + Duration::from_secs(5);
    while support::open_files().contains(&record) {
        assert!(
            Instant::now() < deadline,
            "the record is closed once released"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// A hit on a record that keeps its name closes it where the handler is
/// dropped, with no handoff: that close frees nothing.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hit_on_a_record_that_keeps_its_name_closes_it_in_place() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/purge-hit-closed-in-place");
    support::store(cache, &key, &support::meta(), b"read, never purged").await;
    let path = support::canonical_path(cache, &key);
    let record = support::file_id(&path);
    let (_meta, hit) = support::lookup_hit(cache, &key).await;

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::BeforeClosingRemovedFile, Some(&path));
    drop(hit);
    assert!(
        !support::open_files().contains(&record),
        "the record is closed as the drop returns"
    );
    assert!(
        !pause.wait_reached(Duration::from_millis(200)),
        "the close is never handed to the blocking pool"
    );
}
