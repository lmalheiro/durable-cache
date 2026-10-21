// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-LAYOUT.OBJECTFILE-01 — Damaged records treated as a miss.
//!
//! A reader accepts a published record only when its magic, version, header
//! and metadata CRCs, and stored key all validate and its length is exactly
//! the computed body offset plus `body_len`. Any single violation is a miss
//! that leaves the file in place; a corrupted body is read to a normal end,
//! and logged when the read-time body check is on; and a metadata read
//! racing an in-place update sees one complete version.
//!
//! Byte-level cases on the header block itself live as unit tests in
//! `src/record.rs`, close to the format they exercise; this file drives the
//! real `Storage` API.

use std::time::Duration;

use durable_cache::{format, FsCacheConfig};
use pingora_cache::storage::Storage;

use super::support;

// IMP-TC.LAYOUT.OBJECTFILE-01: satisfy TC-LAYOUT.OBJECTFILE-01

/// Steps 1, 2, and 5: a violated magic, version, header CRC, metadata CRC,
/// or length is a miss, and the damaged file is still there afterwards — a
/// lookup never unlinks a path whose content may have changed since it
/// opened it.
#[tokio::test]
async fn tc_layout_objectfile_01_each_violation_is_a_miss_that_leaves_the_file() {
    /// One way of damaging a published record.
    type Damage = Box<dyn Fn(&std::path::Path)>;

    let damage: Vec<(&str, Damage)> = vec![
        (
            "magic",
            Box::new(|p: &std::path::Path| support::flip_byte_at(p, format::MAGIC_OFFSET as u64)),
        ),
        (
            "version",
            Box::new(|p: &std::path::Path| support::flip_byte_at(p, format::VERSION_OFFSET as u64)),
        ),
        (
            "header CRC",
            Box::new(|p: &std::path::Path| {
                support::flip_byte_at(p, format::BODY_LEN_OFFSET as u64)
            }),
        ),
        (
            "metadata CRC",
            Box::new(|p: &std::path::Path| {
                let meta_start = support::read_header(p).meta_region_offset() as u64;
                support::flip_byte_at(p, meta_start)
            }),
        ),
        (
            "truncated",
            Box::new(|p: &std::path::Path| support::truncate_by(p, 1)),
        ),
        (
            "extended",
            Box::new(|p: &std::path::Path| support::extend_by(p, 1)),
        ),
    ];

    for (what, damage) in damage {
        let (cache, _tmp) = support::leaked_cache();
        let key = support::key("/damaged");
        support::store(cache, &key, &support::meta(), b"body bytes").await;
        let path = support::canonical_path(cache, &key);

        damage(&path);

        assert!(
            support::lookup_body(cache, &key).await.is_none(),
            "a record with a damaged {what} must be reported as a miss"
        );
        assert!(
            path.exists(),
            "the damaged record ({what}) must be left for the restart-time scan"
        );
    }
}

/// Step 3: a record of another format version is a miss even though every
/// checksum in it verifies: its version is the only foreign thing about it.
#[tokio::test]
async fn tc_layout_objectfile_01_another_format_version_is_a_miss() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/foreign");
    support::store(cache, &key, &support::meta(), b"body").await;
    let path = support::canonical_path(cache, &key);

    support::set_format_version(&path, durable_cache::format::FORMAT_VERSION + 1);

    assert!(support::lookup_body(cache, &key).await.is_none());
    assert!(path.exists());
}

/// Step 4: a record whose stored key differs from the looked-up key is a
/// miss — the check that catches a wrong-shard read.
#[tokio::test]
async fn tc_layout_objectfile_01_stored_key_must_match_the_looked_up_key() {
    let (cache, _tmp) = support::leaked_cache();
    let stored = support::key("/stored-key");
    let other = support::key("/other-key");
    support::store(cache, &stored, &support::meta(), b"body").await;

    // The very same record, byte for byte, at another key's canonical path.
    let from = support::canonical_path(cache, &stored);
    let to = support::canonical_path(cache, &other);
    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
    std::fs::copy(&from, &to).unwrap();

    assert!(
        support::lookup_body(cache, &other).await.is_none(),
        "a record stored under another key must never be served"
    );
    assert!(
        support::lookup_body(cache, &stored).await.is_some(),
        "the original is untouched"
    );
}

/// What the read-time body check logs on a mismatch.
const BODY_MISMATCH: &str = "does not match its stored checksum";

/// A body spanning several of the reader's 64 KiB chunks, so a check folds
/// more than one.
fn multi_chunk_body() -> Vec<u8> {
    (0..200_000u32).map(|i| (i % 251) as u8).collect()
}

/// A fresh cache, with the read-time body check switched on or off. A build
/// without `body-crc-verify` has no check to switch on.
fn cache_checking_bodies(check: bool) -> (&'static durable_cache::FsCache, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let config = FsCacheConfig::new(tmp.path(), None);
    #[cfg(feature = "body-crc-verify")]
    let config = config.with_body_crc_verification(check);
    #[cfg(not(feature = "body-crc-verify"))]
    assert!(
        !check,
        "the read-time body check needs the body-crc-verify feature"
    );
    (support::leaked_cache_with(config), tmp)
}

/// Step 6: a body corrupted without changing its length is served as it lies
/// on disk, and its read ends normally: serving a hit does not check the
/// body, whose integrity at rest is the storage layer's
/// (ADR-PERSISTENCE.DURABILITY-01). With the read-time body check switched
/// on (`body-crc-verify`), the whole read still ends normally and the
/// mismatch is logged as an error naming the file.
#[tokio::test]
async fn tc_layout_objectfile_01_a_corrupted_body_is_read_to_a_normal_end() {
    let log = support::LogCapture::install();
    #[cfg(not(feature = "body-crc-verify"))]
    let settings = [false];
    #[cfg(feature = "body-crc-verify")]
    let settings = [false, true];
    for check in settings {
        let (cache, _tmp) = cache_checking_bodies(check);
        let key = support::key("/corrupt-body");
        let stored = multi_chunk_body();
        support::store(cache, &key, &support::meta(), &stored).await;
        let path = support::canonical_path(cache, &key);
        support::flip_body_byte(&path);

        // The lookup succeeds: the header and metadata are intact.
        let (_meta, mut hit) = support::lookup_hit(cache, &key).await;
        let served = support::drain_body(&mut hit)
            .await
            .expect("a corrupted body's read still ends normally");
        assert_eq!(served.len(), stored.len());
        assert_ne!(served, stored, "served as it lies on disk");

        let logged: Vec<String> = log
            .matching(&path.display().to_string())
            .into_iter()
            .filter(|l| l.contains(BODY_MISMATCH))
            .collect();
        assert_eq!(
            logged.len(),
            usize::from(check),
            "check {}: {logged:?}",
            if check { "on" } else { "off" }
        );
        if check {
            assert!(logged[0].starts_with("ERROR"), "{logged:?}");
        }
    }
}

/// Step 6, with the read-time body check on: an intact body read whole, and
/// a corrupted one read from a sought position, log nothing. The stored
/// checksum covers a whole body only.
#[cfg(feature = "body-crc-verify")]
#[tokio::test]
async fn tc_layout_objectfile_01_the_body_check_logs_only_a_whole_mismatched_body() {
    let log = support::LogCapture::install();
    let (cache, _tmp) = cache_checking_bodies(true);
    let key = support::key("/checked-body");
    let stored = multi_chunk_body();
    support::store(cache, &key, &support::meta(), &stored).await;
    let path = support::canonical_path(cache, &key);
    let mismatches = || {
        log.matching(&path.display().to_string())
            .into_iter()
            .filter(|l| l.contains(BODY_MISMATCH))
            .count()
    };

    let (_meta, mut hit) = support::lookup_hit(cache, &key).await;
    assert_eq!(support::drain_body(&mut hit).await.unwrap(), stored);
    assert_eq!(mismatches(), 0, "an intact body logs nothing");

    support::flip_body_byte(&path);
    let (_meta, mut hit) = support::lookup_hit(cache, &key).await;
    hit.seek(0, Some(stored.len())).unwrap();
    assert_eq!(
        support::drain_body(&mut hit).await.unwrap().len(),
        stored.len()
    );
    assert_eq!(mismatches(), 0, "a sought read is not checked");
}

/// Step 8: a header whose lengths don't fit is a miss that leaves the file in
/// place, even though its CRC verifies: metadata segments that exceed
/// `meta_capacity`, and a `key_len` that puts the key and the body past the
/// end of the file. Both are caught before the reader sizes any read by a
/// length from the header.
#[tokio::test]
async fn tc_layout_objectfile_01_header_lengths_that_do_not_fit_are_a_miss() {
    let (cache, _tmp) = support::leaked_cache();
    let cases = [
        (
            "segments past the capacity",
            format::META_CAPACITY_OFFSET,
            0,
        ),
        (
            "a key past the file's end",
            format::KEY_LEN_OFFSET,
            u32::MAX,
        ),
    ];
    for (case, offset, value) in cases {
        let key = support::key(&format!("/lengths-{offset}"));
        support::store(cache, &key, &support::meta(), b"a body").await;
        let path = support::canonical_path(cache, &key);
        support::set_header_u32(&path, offset, value);

        assert!(
            support::lookup_body(cache, &key).await.is_none(),
            "{case}: a miss"
        );
        assert!(path.exists(), "{case}: the file is left in place");
    }
}

/// Step 7, without the race: a lookup that finds an in-place update stopped
/// between its two writes is not a miss. Its re-reads all fail the
/// checksum, so it reads once more holding the key's publication lock,
/// which the update holds until its header is written, and then finds the
/// updated metadata.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tc_layout_objectfile_01_a_lookup_waits_out_an_update_stopped_between_its_writes() {
    use durable_cache::hooks::PausePoint;

    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/in-place-paused");
    let stored = support::meta_with_header_filler(8);
    support::store(cache, &key, &stored, b"body that never changes").await;
    let canonical = support::canonical_path(cache, &key);

    let pause = cache
        .hooks()
        .arm_pause(PausePoint::InPlaceBetweenWrites, Some(&canonical));
    let updating = {
        let key = key.clone();
        let update = support::revalidation(&stored, &support::meta_with_header_filler(200));
        tokio::spawn(async move {
            cache
                .update_meta(&key, &update, &support::inactive_trace())
                .await
        })
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the update stops between its writes"
    );

    let looking = {
        let key = key.clone();
        tokio::spawn(async move { support::lookup_body(cache, &key).await })
    };
    // Were the lookup kept off the processor for the whole sleep, the test
    // would pass this check without testing it, but it can never fail
    // falsely.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !looking.is_finished(),
        "the lookup waits for the update, rather than reporting a miss"
    );

    pause.release();
    assert!(
        updating
            .await
            .expect("the updating task completes")
            .expect("the update succeeds"),
        "the update is applied"
    );
    let (meta, body) = looking
        .await
        .expect("the lookup task completes")
        .expect("a hit, not a miss");
    assert_eq!(body, b"body that never changes");
    assert_eq!(
        meta.headers()
            .get("x-filler")
            .expect("the updated header set")
            .len(),
        200,
        "the lookup finds the updated metadata"
    );
}

/// Step 7: lookups racing repeated in-place metadata updates see the
/// metadata before or after an update, never a mixture and never a miss.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_layout_objectfile_01_in_place_updates_are_never_seen_half_applied() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/in-place-race");
    let stored = support::meta_with_header_filler(8);
    support::store(cache, &key, &stored, b"body that never changes").await;
    // Revalidations of the stored response with two header sets of different
    // sizes, both fitting the reserved region, so every update rewrites a
    // different number of bytes in place.
    let small = support::revalidation(&stored, &support::meta_with_header_filler(8));
    let large = support::revalidation(&stored, &support::meta_with_header_filler(200));

    let updating = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let updater = {
        let key = key.clone();
        let updating = updating.clone();
        tokio::spawn(async move {
            let trace = support::inactive_trace();
            for i in 0..200 {
                let meta = if i % 2 == 0 { &large } else { &small };
                cache.update_meta(&key, meta, &trace).await.unwrap();
                tokio::task::yield_now().await;
            }
            updating.store(false, std::sync::atomic::Ordering::SeqCst);
        })
    };

    let reader = {
        let key = key.clone();
        tokio::spawn(async move {
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            let mut lookups = 0;
            while updating.load(std::sync::atomic::Ordering::SeqCst)
                && std::time::Instant::now() < deadline
            {
                let (meta, body) = support::lookup_body(cache, &key)
                    .await
                    .expect("an object being updated in place is never a miss");
                assert_eq!(body, b"body that never changes");
                // Whichever set is returned, it is one of the two whole:
                // a mixture would not survive the metadata checksum.
                let filler = meta
                    .headers()
                    .get("x-filler")
                    .expect("the header set must be one of the two written")
                    .len();
                assert!(filler == 8 || filler == 200, "half-applied header set");
                lookups += 1;
            }
            lookups
        })
    };

    updater.await.unwrap();
    let lookups = reader.await.unwrap();
    assert!(lookups > 0, "the reader must have observed the object");
}
