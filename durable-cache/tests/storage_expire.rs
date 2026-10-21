// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-STORAGE.EXPIRE-01: an expiry stops the object being served as a fresh
//! hit while leaving its body byte-identical and readable for the
//! revalidation that follows, takes the in-place metadata path whatever
//! headroom the record has left, and reports the outcome each case calls
//! for.
//!
//! Driven directly through the `Storage` trait, with no HTTP and no
//! processes: what is under test is the backend's own contract
//! (FDS-STORAGE.EXPIRE-01), not how `pingora-cache` uses it.

use super::support;

// IMP-TC.STORAGE.EXPIRE-01: satisfy TC-STORAGE.EXPIRE-01

use durable_cache::format;
use durable_cache::hooks::PausePoint;
use pingora_cache::eviction::{CacheEntryId, CacheEntryKey};
use pingora_cache::storage::{PurgeOutcome, PurgeTarget, Storage};
use pingora_cache::CacheMeta;
use pingora_http::ResponseHeader;
use std::os::unix::fs::MetadataExt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Metadata that is fresh well into the future, so "no longer fresh" after
/// an expiry is unambiguous rather than a timing accident.
fn fresh_for(secs: u64) -> CacheMeta {
    let now = SystemTime::now();
    CacheMeta::new(
        now + Duration::from_secs(secs),
        now,
        0,
        0,
        ResponseHeader::build(200, None).unwrap(),
    )
}

/// The (device, inode) pair of a key's canonical file: what shows that an
/// expiry changed the record's file rather than replacing it.
fn identity_of(cache: &durable_cache::FsCache, key: &pingora_cache::CacheKey) -> (u64, u64) {
    let meta = std::fs::metadata(support::canonical_path(cache, key)).unwrap();
    (meta.dev(), meta.ino())
}

/// Step 1: the record survives, its body reads back byte for byte, and the
/// metadata the next lookup returns is no longer fresh.
#[tokio::test]
async fn tc_storage_expire_01_keeps_the_body_and_drops_the_freshness() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/expire-keeps-body");
    let body = b"the body a 304 will reuse";
    support::store(cache, &key, &fresh_for(300), body).await;

    let before = support::lookup_body(cache, &key).await.unwrap().0;
    assert!(
        before.is_fresh(SystemTime::now()),
        "precondition: the object starts fresh"
    );

    assert_eq!(
        support::expire_active(cache, &key).await,
        PurgeOutcome::Expired
    );

    let (meta, read_back) = support::lookup_body(cache, &key)
        .await
        .expect("the entry is still stored");
    assert_eq!(read_back, body, "the body is byte-identical");
    assert!(
        !meta.is_fresh(SystemTime::now() + Duration::from_secs(1)),
        "the object is no longer served as a fresh hit"
    );
}

/// Step 5: expiring must not close the serve-stale windows the response set
/// — those decide whether the retained body may be served while the
/// revalidation is in flight, which an expiry does not touch.
#[tokio::test]
async fn tc_storage_expire_01_leaves_the_serve_stale_windows_open() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/expire-serve-stale");
    let now = SystemTime::now();
    let meta = CacheMeta::new(
        now + Duration::from_secs(300),
        now,
        // stale-while-revalidate and stale-if-error windows.
        60,
        60,
        ResponseHeader::build(200, None).unwrap(),
    );
    support::store(cache, &key, &meta, b"stale-servable body").await;

    assert_eq!(
        support::expire_active(cache, &key).await,
        PurgeOutcome::Expired
    );

    let (after, _) = support::lookup_body(cache, &key).await.unwrap();
    assert!(
        after.serve_stale_while_revalidate(SystemTime::now()),
        "expiring must not close the serve-stale window the response set"
    );
}

/// Step 2: idempotent, and a key that was never stored reports not-found
/// rather than an error (FRE-COMPLIANCE.PURGEABSENT-01).
#[tokio::test]
async fn tc_storage_expire_01_is_idempotent_and_absent_keys_are_not_found() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/expire-twice");
    support::store(cache, &key, &fresh_for(300), b"body").await;

    assert_eq!(
        support::expire_active(cache, &key).await,
        PurgeOutcome::Expired
    );
    assert_eq!(
        support::expire_active(cache, &key).await,
        PurgeOutcome::Expired,
        "expiring an already-expired entry is a harmless no-op"
    );
    assert!(
        support::lookup_body(cache, &key).await.is_some(),
        "and leaves it stored"
    );

    let never = support::key("/expire-never-stored");
    assert_eq!(
        support::expire_active(cache, &never).await,
        PurgeOutcome::NotFound
    );
    assert!(!support::canonical_path(cache, &never).exists());
}

/// Step 3: an `Exact` target carrying an `Identified` entry key names a
/// stored generation this backend never issued (ADR-COMPAT.ENTRYID-01), so
/// it is reported not-found with the record left alone — never resolved to
/// the key's canonical path.
#[tokio::test]
async fn tc_storage_expire_01_an_identified_exact_target_is_not_found() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/expire-identified");
    support::store(cache, &key, &fresh_for(300), b"body").await;
    let trace = support::inactive_trace();

    let entry = CacheEntryKey::identified(support::compact(&key), CacheEntryId::new(7));
    assert_eq!(
        cache
            .expire(PurgeTarget::Exact(&entry), &trace)
            .await
            .unwrap(),
        PurgeOutcome::NotFound
    );

    let (meta, _) = support::lookup_body(cache, &key).await.unwrap();
    assert!(
        meta.is_fresh(SystemTime::now()),
        "the record is untouched: still stored and still fresh"
    );

    // A key-only Exact target, by contrast, resolves exactly as an Active
    // one does.
    let key_only = CacheEntryKey::key_only(support::compact(&key));
    assert_eq!(
        cache
            .expire(PurgeTarget::Exact(&key_only), &trace)
            .await
            .unwrap(),
        PurgeOutcome::Expired
    );
}

/// Step 4: with headroom configured the expiry writes the metadata in
/// place: same file, same length, and the body untouched.
#[tokio::test]
async fn tc_storage_expire_01_takes_the_in_place_path_when_headroom_allows() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/expire-in-place");
    let body = b"body that must not be copied";
    support::store(cache, &key, &fresh_for(300), body).await;

    let canonical = support::canonical_path(cache, &key);
    let length = std::fs::metadata(&canonical).expect("the record").len();
    let before = identity_of(cache, &key);
    assert_eq!(
        support::expire_active(cache, &key).await,
        PurgeOutcome::Expired
    );
    assert_eq!(
        identity_of(cache, &key),
        before,
        "the default headroom leaves the expiry in place: same device and inode"
    );
    assert_eq!(
        std::fs::metadata(&canonical).expect("the record").len(),
        length,
        "and the same length"
    );
    assert_eq!(
        support::lookup_body(cache, &key).await.unwrap().1,
        body,
        "and the body is untouched"
    );
}

/// A `fresh_until` landing exactly on a second boundary, so its
/// internal-metadata segment is as small as the encoding gets.
///
/// `expire_at` only ever rewrites `fresh_until`, so that one field's encoded
/// width is the whole difference between a record's stored and expired
/// metadata. MessagePack spends one byte on a sub-second component of 0 and
/// five on one at nanosecond scale, so a record stored on a whole second and
/// expired at an arbitrary instant would grow by four bytes, and with no
/// headroom it would outgrow its reserved region.
fn fresh_for_on_a_whole_second(secs: u64) -> CacheMeta {
    let now = SystemTime::now();
    let whole =
        UNIX_EPOCH + Duration::from_secs(now.duration_since(UNIX_EPOCH).unwrap().as_secs() + secs);
    CacheMeta::new(whole, now, 0, 0, ResponseHeader::build(200, None).unwrap())
}

/// Wait until the wall clock sits well inside a second, so that the instant
/// the backend samples a moment later has a sub-second component encoded in
/// five bytes: an expiry at that very instant would need four bytes more
/// than a whole-second deadline took. The window is ~0.9 s wide, so only a
/// stall of most of a second could leave it.
fn wait_for_a_wide_sub_second_instant() {
    loop {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        if (50_000_000..950_000_000).contains(&nanos) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Step 4, continued: a deadline on a whole second takes the fewest bytes a
/// deadline can, and with no headroom its record has none to spare. An
/// expiry at an instant well inside a second would lengthen it by four
/// bytes; the expiry starts the expired deadline at a whole second instead,
/// so it still writes in place: same file, same length
/// (FDS-STORAGE.EXPIRE-01).
#[tokio::test]
async fn tc_storage_expire_01_a_whole_second_deadline_expires_in_place_without_headroom() {
    let (cache, _tmp) = support::leaked_cache_with_headroom(0);
    let key = support::key("/expire-whole-second");
    support::store(cache, &key, &fresh_for_on_a_whole_second(300), b"body").await;

    let canonical = support::canonical_path(cache, &key);
    let header = support::read_header(&canonical);
    assert_eq!(
        header.meta_capacity(),
        header.internal_len() + header.header_len(),
        "the region must be exactly full, or any expiry would fit"
    );
    let length = std::fs::metadata(&canonical).expect("the record").len();
    let before = identity_of(cache, &key);

    wait_for_a_wide_sub_second_instant();
    assert_eq!(
        support::expire_active(cache, &key).await,
        PurgeOutcome::Expired
    );
    assert_eq!(
        identity_of(cache, &key),
        before,
        "written in place: same device and inode"
    );
    assert_eq!(
        std::fs::metadata(&canonical).expect("the record").len(),
        length,
        "and the same length"
    );
    let (meta, body) = support::lookup_body(cache, &key)
        .await
        .expect("the object is still stored");
    assert_eq!(body, b"body");
    assert!(
        !meta.is_fresh(SystemTime::now()),
        "and it is no longer fresh"
    );
}

/// Step 6: a same-key store already in flight is not cancelled by an expiry.
/// When it completes it publishes a fresh object over the canonical name,
/// which is the behaviour FRE-COMPAT.EXPIRE-01 states and the same race a
/// purge has.
#[tokio::test]
async fn tc_storage_expire_01_a_concurrent_store_still_replaces_the_record() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/expire-racing-store");
    support::store(cache, &key, &fresh_for(300), b"old body").await;

    // Open the replacing store and write its body, but do not finish it.
    let trace = support::inactive_trace();
    let mut handler = cache
        .get_miss_handler(&key, &fresh_for(300), &trace)
        .await
        .unwrap();
    handler
        .write_body(bytes::Bytes::from_static(b"new body"), true)
        .await
        .unwrap();

    // Expire the published record while that store is still open.
    assert_eq!(
        support::expire_active(cache, &key).await,
        PurgeOutcome::Expired
    );

    // The store completes and publishes over it.
    handler.finish().await.unwrap();

    let (meta, body) = support::lookup_body(cache, &key).await.unwrap();
    assert_eq!(
        body, b"new body",
        "the completing store replaced the record"
    );
    assert!(
        meta.is_fresh(SystemTime::now()),
        "and its object is fresh: the expiry applied to the record it replaced"
    );
}

/// Step 7: a reader already streaming the record keeps its open descriptor
/// and completes the body it opened, undisturbed by an expiry, which
/// rewrites only the metadata.
#[tokio::test]
async fn tc_storage_expire_01_a_streaming_reader_is_undisturbed() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/expire-racing-reader");
    let body: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
    support::store(cache, &key, &fresh_for(300), &body).await;

    let (_, mut hit) = support::lookup_hit(cache, &key).await;
    // Take the first chunk, so the read is genuinely in progress.
    let first = hit
        .read_body()
        .await
        .expect("the body reads")
        .expect("a first chunk");

    assert_eq!(
        support::expire_active(cache, &key).await,
        PurgeOutcome::Expired
    );

    let mut read_back = first.to_vec();
    read_back.extend(
        support::drain_body(&mut hit)
            .await
            .expect("the rest of the body reads"),
    );
    assert_eq!(
        read_back, body,
        "the reader runs to the end of the body it already opened"
    );
}

/// Step 8: the read, the derivation, and the write are one critical section
/// under the key's publication lock, so a metadata update racing an expiry
/// cannot interleave with it. The record ends carrying one complete
/// metadata set, never a mixture.
#[tokio::test]
async fn tc_storage_expire_01_serialises_against_a_concurrent_metadata_update() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/expire-vs-update");
    let stored = fresh_for(300);
    support::store(cache, &key, &stored, b"body").await;
    let canonical = support::canonical_path(cache, &key);

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::ExpireBeforeWrite, Some(&canonical));

    let expiring = {
        let key = key.clone();
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let compact = key.to_compact();
            rt.block_on(cache.expire(PurgeTarget::Active(&compact), &support::inactive_trace()))
        })
    };
    assert!(pause.wait_reached(Duration::from_secs(5)));

    // The expiry holds the publication lock while paused, so this update
    // cannot land until it is released.
    let updating = {
        let key = key.clone();
        let update = support::revalidation(&stored, &fresh_for(900));
        tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(cache.update_meta(&key, &update, &support::inactive_trace()))
        })
    };

    pause.release();
    assert_eq!(expiring.await.unwrap().unwrap(), PurgeOutcome::Expired);
    updating.await.unwrap().unwrap();

    // Whichever landed last, the record is readable and carries exactly one
    // of the two complete metadata sets — a mixture would fail the metadata
    // checksum and read as a miss.
    let (meta, body) = support::lookup_body(cache, &key)
        .await
        .expect("the record is readable, so its metadata is one complete set");
    assert_eq!(body, b"body");
    let _ = meta;
}

/// Step 9: once the backend is read-only, an expiry fails rather than being
/// reported as applied, and changes nothing on disk
/// (FRE-COMPAT.HOTUPGRADE-01).
#[tokio::test]
async fn tc_storage_expire_01_once_read_only_fails_and_changes_nothing() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/expire-read-only");
    support::store(cache, &key, &fresh_for(300), b"body").await;
    let before = std::fs::read(support::canonical_path(cache, &key)).unwrap();

    cache.set_read_only();

    let compact = support::compact(&key);
    let err = cache
        .expire(PurgeTarget::Active(&compact), &support::inactive_trace())
        .await
        .expect_err("a read-only backend refuses expiries");
    assert_eq!(
        err.etype,
        pingora_error::ErrorType::Custom("cache_read_only"),
        "unexpected error: {err}"
    );
    assert_eq!(
        std::fs::read(support::canonical_path(cache, &key)).unwrap(),
        before,
        "the record is byte-for-byte unchanged"
    );
}

/// Step 10: a record that fails the read-time validation reads as absent,
/// as it does to a lookup, so the expiry reports `NotFound` and leaves the
/// record as it is, for the restart scan (FDS-STORAGE.RECLAIM-01).
#[tokio::test]
async fn tc_storage_expire_01_an_unreadable_record_is_not_found_and_left_alone() {
    /// One way of making a published record unreadable.
    type Damage = Box<dyn Fn(&std::path::Path)>;

    let damage: Vec<(&str, Damage)> = vec![
        (
            "a damaged record",
            Box::new(|p: &std::path::Path| {
                let meta_start = support::read_header(p).meta_region_offset() as u64;
                support::flip_byte_at(p, meta_start)
            }),
        ),
        (
            "a truncated record",
            Box::new(|p: &std::path::Path| support::truncate_by(p, 1)),
        ),
        (
            "a record of another format version",
            Box::new(|p: &std::path::Path| {
                support::set_format_version(p, format::FORMAT_VERSION + 1)
            }),
        ),
        (
            "a withdrawn record",
            Box::new(|p: &std::path::Path| {
                support::set_format_version(p, format::WITHDRAWN_VERSION)
            }),
        ),
    ];

    for (what, damage) in damage {
        let (cache, _tmp) = support::leaked_cache();
        let key = support::key("/expire-unreadable");
        support::store(cache, &key, &fresh_for(300), b"body").await;
        let path = support::canonical_path(cache, &key);
        damage(&path);
        let before = std::fs::read(&path).expect("the record is there to expire");

        assert_eq!(
            support::expire_active(cache, &key).await,
            PurgeOutcome::NotFound,
            "{what} reads as absent"
        );
        assert_eq!(
            std::fs::read(&path).expect("the record is left in place"),
            before,
            "the expiry leaves {what} as it is"
        );
        assert!(
            support::lookup_body(cache, &key).await.is_none(),
            "a lookup finds {what} absent too"
        );
    }
}

/// An expiry still waiting for the key's lock when a store replaces the
/// record expires the stored record. It opens the record only once it holds
/// the lock, so it works on the record the key holds then. Had it opened the
/// record first, it would have expired the replaced file, and the object
/// would have stayed fresh while the expiry reported success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expiry_paused_before_its_lock_expires_the_record_a_store_left() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/expire-replaced-before-the-lock");
    support::store(cache, &key, &fresh_for(300), b"old body").await;
    let canonical = support::canonical_path(cache, &key);

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::ExpireBeforeLock, Some(&canonical));
    let expiring = {
        let key = key.clone();
        tokio::spawn(async move { support::expire_active(cache, &key).await })
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the expiry must reach its pause point"
    );

    support::store(cache, &key, &fresh_for(300), b"new body").await;
    pause.release();

    assert_eq!(
        expiring.await.expect("the expiring task completes"),
        PurgeOutcome::Expired
    );
    let (meta, body) = support::lookup_body(cache, &key)
        .await
        .expect("the object is still stored");
    assert_eq!(body, b"new body", "the store's record is the one stored");
    assert!(
        !meta.is_fresh(SystemTime::now()),
        "and the expiry applied to it"
    );
}

/// An expiry still waiting for the key's lock when a purge removes the
/// record reports `NotFound`, the absent-key outcome
/// (FRE-COMPLIANCE.PURGEABSENT-01).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expiry_paused_before_its_lock_finds_a_purged_key_absent() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/expire-purged-before-the-lock");
    support::store(cache, &key, &fresh_for(300), b"body").await;
    let canonical = support::canonical_path(cache, &key);

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::ExpireBeforeLock, Some(&canonical));
    let expiring = {
        let key = key.clone();
        tokio::spawn(async move { support::expire_active(cache, &key).await })
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the expiry must reach its pause point"
    );

    assert_eq!(
        support::purge_active(cache, &key).await,
        PurgeOutcome::Purged(None),
        "the purge removes the record"
    );
    pause.release();

    assert_eq!(
        expiring.await.expect("the expiring task completes"),
        PurgeOutcome::NotFound
    );
    assert!(
        support::lookup_body(cache, &key).await.is_none(),
        "the key still holds no object"
    );
}
