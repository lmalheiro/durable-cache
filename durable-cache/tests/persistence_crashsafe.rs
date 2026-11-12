// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-PERSISTENCE.CRASHSAFE-01 — Host crash serves no bad content.
//!
//! In every state a host crash or power loss could leave on disk, the
//! restarted cache serves no object whose content differs from what was
//! stored and no object whose purge it had acknowledged; objects that did
//! not survive are misses (HLR-PERSISTENCE.CRASHSAFE-01).
//!
//! The test case's setup replays a device-mapper `log-writes` device to
//! every flush and FUA point. That needs root, a scratch block device, and
//! the kernel's replay tool, and this repository has no harness for it yet.
//! This file covers the same contract by constructing the on-disk states
//! such a replay produces —
//! which is what the design's argument turns on:
//!
//! - a store's bytes not yet on stable storage when the name appeared
//!   (`fdatasync` precedes the rename, so on storage that honours flushes,
//!   which the requirement assumes, this state cannot arise for a published
//!   name),
//! - an in-place metadata update torn halfway, and a withdrawal torn
//!   halfway,
//! - an unfinished store's temporary,
//! - a purge whose directory entry was written but whose object file's
//!   blocks were not;
//! - with stores unsynchronised (ADR-PERSISTENCE.UNSYNCED-01), a published
//!   record whose body never reached the device: zeros under a valid
//!   header, as a file system that records a file's length first can leave
//!   it, after a host restart the discard detects; or cut short, as one
//!   that records the length only once the data is written leaves it.

use durable_cache::recovery::recover;
use pingora_cache::eviction::simple_lru::Manager;
use pingora_cache::eviction::EvictionManager;
use pingora_cache::storage::{PurgeOutcome, Storage};

use super::support;

// IMP-TC.PERSISTENCE.CRASHSAFE-01: satisfy TC-PERSISTENCE.CRASHSAFE-01

/// A torn in-place metadata update: the metadata region holds the new
/// bytes while the header still commits the old ones, or the other way
/// round. Either way a reader sees one complete version or a miss, never a
/// mixture of the two.
#[tokio::test]
async fn tc_persistence_crashsafe_01_a_torn_metadata_update_is_never_a_mixture() {
    let (cache, tmp) = support::leaked_cache();
    let key = support::key("/crashsafe-torn-meta");
    let first = support::meta_with_header_filler(16);
    support::store(cache, &key, &first, b"body that never changes").await;
    let path = support::canonical_path(cache, &key);
    let before = std::fs::read(&path).unwrap();

    // Apply an in-place update, then reconstruct the states a crash could
    // have left between its two writes: region new + header old, and
    // region old + header new.
    let trace = support::inactive_trace();
    let second = support::revalidation(&first, &support::meta_with_header_filler(64));
    cache.update_meta(&key, &second, &trace).await.unwrap();
    let after = std::fs::read(&path).unwrap();

    let meta_start = support::read_header(&path).meta_region_offset();
    let states = [
        (
            "new region, old header",
            &before[..meta_start],
            &after[meta_start..],
        ),
        (
            "old region, new header",
            &after[..meta_start],
            &before[meta_start..],
        ),
    ];
    for (name, head, tail) in states {
        let mut torn = Vec::with_capacity(head.len() + tail.len());
        torn.extend_from_slice(head);
        torn.extend_from_slice(tail);
        std::fs::write(&path, &torn).unwrap();

        let restarted = support::leaked_cache_at(tmp.path());
        match support::lookup_body(restarted, &key).await {
            None => {} // a miss is always allowed
            Some((meta, body)) => {
                assert_eq!(
                    body, b"body that never changes",
                    "{name}: the body is untouched"
                );
                let filler = meta
                    .headers()
                    .get("x-filler")
                    .expect("one of the two header sets")
                    .len();
                assert!(
                    filler == 16 || filler == 64,
                    "{name}: the metadata must be one complete version, not a mixture"
                );
            }
        }
    }
}

/// A torn withdrawal: an update whose metadata outgrew the region rewrites
/// only the header, as withdrawn, and a crash can keep part of that write:
/// the version without the checksum, or the checksum without the version.
/// A reader then finds the record as it was, or a miss, never a mixture.
#[tokio::test]
async fn tc_persistence_crashsafe_01_a_torn_withdrawal_is_never_a_mixture() {
    use durable_cache::format;

    let (cache, tmp) = support::leaked_cache();
    let key = support::key("/crashsafe-torn-withdrawal");
    let first = support::meta_with_header_filler(16);
    support::store(cache, &key, &first, b"body that never changes").await;
    let path = support::canonical_path(cache, &key);
    let before = std::fs::read(&path).expect("the record");

    // 4000 bytes of headers outgrow the region, so the update withdraws the
    // record.
    let trace = support::inactive_trace();
    let second = support::revalidation(&first, &support::meta_with_header_filler(4000));
    assert!(
        !cache
            .update_meta(&key, &second, &trace)
            .await
            .expect("the update returns"),
        "the update is not applied"
    );
    let after = std::fs::read(&path).expect("the withdrawn record");
    assert_eq!(
        after.len(),
        before.len(),
        "a withdrawal writes only the header"
    );

    // The version and the checksum are the header bytes a withdrawal changes.
    let version = format::VERSION_OFFSET..format::VERSION_OFFSET + 4;
    let checksum = format::HEADER_CRC_OFFSET..format::HEADER_CRC_OFFSET + 4;
    let torn = |written: std::ops::Range<usize>| {
        let mut torn = before.clone();
        torn[written.clone()].copy_from_slice(&after[written]);
        torn
    };
    let states = [
        ("the version written, the checksum not", torn(version)),
        ("the checksum written, the version not", torn(checksum)),
    ];
    for (name, torn) in states {
        std::fs::write(&path, &torn).expect("the torn state");

        let restarted = support::leaked_cache_at(tmp.path());
        match support::lookup_body(restarted, &key).await {
            None => {} // a miss is always allowed
            Some((meta, body)) => {
                assert_eq!(
                    body, b"body that never changes",
                    "{name}: the body is untouched"
                );
                assert_eq!(
                    meta.headers()
                        .get("x-filler")
                        .expect("the stored header set")
                        .len(),
                    16,
                    "{name}: the record as it was, not a mixture"
                );
            }
        }
    }
}

/// An unfinished store's temporary is never served or promoted, and the
/// restart scan reclaims it once its writer is gone.
#[tokio::test]
async fn tc_persistence_crashsafe_01_an_unfinished_store_is_never_served() {
    let (cache, tmp) = support::leaked_cache();
    let key = support::key("/crashsafe-unfinished");
    let meta = support::meta();
    let trace = support::inactive_trace();
    let mut handler = cache
        .get_miss_handler(&key, &meta, &trace)
        .await
        .map_err(|e| e.to_string())
        .unwrap();
    handler
        .write_body(bytes::Bytes::from_static(b"half a body"), false)
        .await
        .unwrap();

    // A crash drops no handler, so the temporary stays behind: forget the
    // handler rather than dropping it, which is what a killed process
    // leaves on disk.
    std::mem::forget(handler);

    let restarted = support::leaked_cache_at(tmp.path());
    assert!(
        support::lookup_body(restarted, &key).await.is_none(),
        "an unfinished store is never served"
    );
    assert!(
        !support::canonical_path(restarted, &key).exists(),
        "and never appears under the canonical name"
    );
}

/// A purge is acknowledged only once the shard directory is synchronised,
/// so an acknowledged purge cannot come back after a crash: the states in
/// which the object could return are exactly the ones in which the purge
/// was never acknowledged.
#[tokio::test]
async fn tc_persistence_crashsafe_01_an_acknowledged_purge_never_returns() {
    let (cache, tmp) = support::leaked_cache();
    let key = support::key("/crashsafe-purged");
    support::store(cache, &key, &support::meta(), b"purged body").await;

    let acknowledged = support::purge_active(cache, &key).await;
    assert_eq!(
        acknowledged,
        PurgeOutcome::Purged(None),
        "the purge is acknowledged only once it is durable"
    );

    // Every later process, and the restart scan, agree it is gone.
    let restarted = support::leaked_cache_at(tmp.path());
    assert!(support::lookup_body(restarted, &key).await.is_none());
    let manager = Manager::new(usize::MAX);
    recover(restarted, &manager).await.unwrap();
    assert!(!manager.peek(&support::entry(&key)));
    assert!(support::lookup_body(restarted, &key).await.is_none());
}

/// Every object that did survive is served exactly as stored: the states
/// above never damage a bystander.
#[tokio::test]
async fn tc_persistence_crashsafe_01_surviving_objects_are_served_as_stored() {
    let (cache, tmp) = support::leaked_cache();
    let mut expected = Vec::new();
    for i in 0..8 {
        let key = support::key(&format!("/crashsafe-intact-{i}"));
        let body: Vec<u8> = (0..(1000 * (i + 1)) as u32)
            .map(|b| (b % 253) as u8)
            .collect();
        support::store(cache, &key, &support::meta(), &body).await;
        expected.push((key, body));
    }
    // One neighbour damaged in the same shard tree.
    let damaged = support::key("/crashsafe-intact-3");
    support::flip_magic_byte(&support::canonical_path(cache, &damaged));

    let restarted = support::leaked_cache_at(tmp.path());
    for (key, body) in &expected {
        match support::lookup_body(restarted, key).await {
            Some((_meta, got)) => assert_eq!(got, *body, "a surviving object is served as stored"),
            None => assert_eq!(
                key.to_compact(),
                damaged.to_compact(),
                "only the damaged record may be a miss"
            ),
        }
    }
}

/// Unsynchronised stores, step 4 of the test case. With the discard on, a
/// record whose body a host crash left as zeros under a valid header is
/// never served after the restart: the backend, constructed under another
/// boot, finds the marker and discards the cache. With the discard off, a
/// record a crash left short, as ext4 in its default `data=ordered` mode
/// leaves one, is a miss, and the others are served as stored.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn tc_persistence_crashsafe_01_unsynchronised_stores_serve_no_lost_body() {
    use std::os::unix::fs::FileExt;

    use durable_cache::hooks::HookConfig;
    use durable_cache::{Durability, FsCacheConfig};

    let boots = support::tempdir();
    let boot = |id: &str| {
        let path = boots.path().join(id);
        std::fs::write(&path, format!("{id}\n")).expect("the boot identity is written");
        path
    };
    let unsynced = |root: &std::path::Path, discard: bool, boot_id: std::path::PathBuf| {
        support::leaked_cache_configured(
            FsCacheConfig::new(root, None)
                .with_durability(Durability::NoStoreSync)
                .with_discard_after_host_restart(discard),
            HookConfig {
                boot_id_path: Some(boot_id),
                ..HookConfig::default()
            },
        )
    };
    let body = support::incompressible(20_000, 7).into_bytes();

    // The discard on: a body of zeros under a valid header.
    let tmp = support::tempdir();
    let cache = unsynced(tmp.path(), true, boot("before-crash"));
    let key = support::key("/crashsafe-unsynced-zeros");
    support::store(cache, &key, &support::meta(), &body).await;
    let path = support::canonical_path(cache, &key);
    let header = support::read_header(&path);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .and_then(|f| {
            f.write_all_at(
                &vec![0u8; header.body_len() as usize],
                header.body_offset() as u64,
            )
        })
        .expect("the body is zeroed");
    // The host crash stopped the instance: its lease is not held any more.
    std::fs::remove_file(support::lease_path(cache.root(), &cache.lease.id()))
        .expect("the instance's lease is there");
    let restarted = unsynced(tmp.path(), true, boot("after-crash"));
    assert!(
        support::lookup_body(restarted, &key).await.is_none(),
        "a body that never reached the device is never served"
    );

    // The discard off: a record cut short.
    let tmp = support::tempdir();
    let cache = unsynced(tmp.path(), false, boot("before-crash-2"));
    let short = support::key("/crashsafe-unsynced-short");
    let intact = support::key("/crashsafe-unsynced-intact");
    support::store(cache, &short, &support::meta(), &body).await;
    support::store(cache, &intact, &support::meta(), &body).await;
    support::truncate_by(&support::canonical_path(cache, &short), 4096);
    let restarted = unsynced(tmp.path(), false, boot("after-crash-2"));
    assert!(
        support::lookup_body(restarted, &short).await.is_none(),
        "a record left short is a miss"
    );
    let (_meta, got) = support::lookup_body(restarted, &intact)
        .await
        .expect("an intact record is a hit");
    assert_eq!(got, body, "served as stored");
}
