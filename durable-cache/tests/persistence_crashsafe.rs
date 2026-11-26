// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-PERSISTENCE.CRASHSAFE-03 — Host crash serves no bad content.
//!
//! In every state a host crash or power loss could leave on disk, the
//! restarted cache serves no object whose content differs from what was
//! stored and no object whose purge it had acknowledged; objects that did
//! not survive are misses (HLR-PERSISTENCE.CRASHSAFE-02).
//!
//! The test case's setup replays a device-mapper `log-writes` device a write
//! at a time, and a block at a time within a longer write, as a power
//! failure may leave one torn. That needs root and scratch block devices:
//! `cargo xtask crash-replay` does it, running the two ignored tests at the
//! end of this file, the scenario on the logged device and the check on
//! each replayed state. The other tests here cover the same contract
//! without root, by constructing the on-disk states such a replay produces
//! — which is what the design's argument turns on:
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
//!   record whose body never reached the device: zeros under a valid header,
//!   as a file system that records a file's length first can leave it, after
//!   a host restart the discard detects; or cut short, as one that records
//!   the length only once the data is written leaves it.

use durable_cache::recovery::recover;
use pingora_cache::eviction::simple_lru::Manager;
use pingora_cache::eviction::EvictionManager;
use pingora_cache::storage::{PurgeOutcome, Storage};

use super::support;

// IMP-TC.PERSISTENCE.CRASHSAFE-03: satisfy TC-PERSISTENCE.CRASHSAFE-03

/// A torn in-place metadata update: the metadata region holds the new bytes
/// while the header still commits the old ones, or the other way round.
/// Either way a reader sees one complete version or a miss, never a mixture
/// of the two.
#[tokio::test]
async fn tc_persistence_crashsafe_03_a_torn_metadata_update_is_never_a_mixture() {
    let (cache, tmp) = support::leaked_cache();
    let key = support::key("/crashsafe-torn-meta");
    let first = support::meta_with_header_filler(16);
    support::store(cache, &key, &first, b"body that never changes").await;
    let path = support::canonical_path(cache, &key);
    let before = std::fs::read(&path).unwrap();

    // Apply an in-place update, then reconstruct the states a crash could
    // have left between its two writes: region new + header old, and region
    // old + header new.
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
/// the version without the checksum, or the checksum without the version. A
/// reader then finds the record as it was, or a miss, never a mixture.
#[tokio::test]
async fn tc_persistence_crashsafe_03_a_torn_withdrawal_is_never_a_mixture() {
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

    // The version and the checksum are the header bytes a withdrawal
    // changes.
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
async fn tc_persistence_crashsafe_03_an_unfinished_store_is_never_served() {
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
    // handler rather than dropping it, which is what a killed process leaves
    // on disk.
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

/// A purge is acknowledged only once the shard directory is synchronised, so
/// an acknowledged purge cannot come back after a crash: the states in which
/// the object could return are exactly the ones in which the purge was never
/// acknowledged.
#[tokio::test]
async fn tc_persistence_crashsafe_03_an_acknowledged_purge_never_returns() {
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
async fn tc_persistence_crashsafe_03_surviving_objects_are_served_as_stored() {
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
async fn tc_persistence_crashsafe_03_unsynchronised_stores_serve_no_lost_body() {
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

// --- The log-writes replay ------------------------------------------------
//
// The test case's own setup. `cargo xtask crash-replay` mounts a file system
// on a device-mapper `log-writes` device and runs the scenario there, which
// marks the log after each operation the backend acknowledges; it then
// replays the log to each flush and FUA point in turn and runs the check on
// each state. Both read their setting from the environment it sets, and
// fail without it.

/// The directory of the replay's cache.
const REPLAY_DIR: &str = "DURABLE_CACHE_REPLAY_DIR";
/// The `log-writes` device the scenario marks, by its device-mapper name.
const REPLAY_DEVICE: &str = "DURABLE_CACHE_REPLAY_DEVICE";
/// How stores are made: `strict`, `unsynced-discard` or `unsynced-keep`.
const REPLAY_MODE: &str = "DURABLE_CACHE_REPLAY_MODE";
/// The file holding the identity of the boot the backend runs under.
const REPLAY_BOOT: &str = "DURABLE_CACHE_REPLAY_BOOT";
/// The marks the replay has passed, separated by commas.
const REPLAY_MARKS: &str = "DURABLE_CACHE_REPLAY_MARKS";

/// The objects the scenario stores: a key and a body length each, from one
/// byte to bodies spanning many file-system blocks.
const OBJECTS: [(&str, usize); 8] = [
    ("/replay-0", 1),
    ("/replay-1", 4096),
    ("/replay-2", 65_537),
    ("/replay-3", 300_000),
    ("/replay-4", 5_000),
    ("/replay-5", 5_000),
    ("/replay-6", 70_000),
    ("/replay-7", 10),
];

/// The objects the scenario purges once all are stored.
const PURGED: [usize; 2] = [0, 1];

/// The object whose metadata the scenario updates in place.
const UPDATED: usize = 4;

/// The object whose metadata update outgrows its region, which withdraws
/// the record.
const WITHDRAWN: usize = 5;

/// The key of the store the scenario leaves unfinished.
const UNFINISHED: &str = "/replay-unfinished";

/// The header filler of each object's metadata as stored, as updated in
/// place, and as the update that outgrows the region would have it.
const STORED_FILLER: usize = 16;
const UPDATED_FILLER: usize = 64;
const OUTGROWN_FILLER: usize = 4000;

fn replay_env(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} is unset: cargo xtask crash-replay runs this test"))
}

fn replay_body(i: usize) -> Vec<u8> {
    support::incompressible(OBJECTS[i].1, 100 + i as u64).into_bytes()
}

/// The backend over the replay's directory, made as `REPLAY_MODE` says,
/// under the boot `REPLAY_BOOT` names.
#[cfg(target_os = "linux")]
fn replay_cache() -> &'static durable_cache::FsCache {
    use durable_cache::hooks::HookConfig;
    use durable_cache::{Durability, FsCacheConfig};

    let config = FsCacheConfig::new(replay_env(REPLAY_DIR), None);
    let config = match replay_env(REPLAY_MODE).as_str() {
        "strict" => config,
        "unsynced-discard" => config
            .with_durability(Durability::NoStoreSync)
            .with_discard_after_host_restart(true),
        "unsynced-keep" => config.with_durability(Durability::NoStoreSync),
        mode => panic!("{REPLAY_MODE} is {mode}, not strict, unsynced-discard or unsynced-keep"),
    };
    support::leaked_cache_configured(
        config,
        HookConfig {
            boot_id_path: Some(replay_env(REPLAY_BOOT).into()),
            ..HookConfig::default()
        },
    )
}

/// The scenario, step 1 and 2 of the test case: store every object, purge
/// some, update one's metadata in place and outgrow another's, and leave a
/// store unfinished, marking the log after each operation the backend
/// acknowledges.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "cargo xtask crash-replay runs it, on a log-writes device"]
async fn tc_persistence_crashsafe_03_replay_scenario() {
    let cache = replay_cache();
    let device = replay_env(REPLAY_DEVICE);
    let mark = |name: String| {
        let status = std::process::Command::new("sudo")
            .args(["-n", "dmsetup", "message", &device, "0", "mark", &name])
            .status()
            .expect("dmsetup runs");
        assert!(status.success(), "the log is not marked {name}");
    };

    let stored: Vec<_> = (0..OBJECTS.len())
        .map(|_| support::meta_with_header_filler(STORED_FILLER))
        .collect();
    for (i, meta) in stored.iter().enumerate() {
        support::store(cache, &support::key(OBJECTS[i].0), meta, &replay_body(i)).await;
        mark(format!("stored-{i}"));
    }
    for i in PURGED {
        let outcome = support::purge_active(cache, &support::key(OBJECTS[i].0)).await;
        assert_eq!(outcome, PurgeOutcome::Purged(None), "object {i} is purged");
        mark(format!("purged-{i}"));
    }
    let trace = support::inactive_trace();
    let updated = support::revalidation(
        &stored[UPDATED],
        &support::meta_with_header_filler(UPDATED_FILLER),
    );
    assert!(
        cache
            .update_meta(&support::key(OBJECTS[UPDATED].0), &updated, &trace)
            .await
            .expect("the update returns"),
        "the update is written in place"
    );
    mark(format!("updated-{UPDATED}"));
    let outgrown = support::revalidation(
        &stored[WITHDRAWN],
        &support::meta_with_header_filler(OUTGROWN_FILLER),
    );
    assert!(
        !cache
            .update_meta(&support::key(OBJECTS[WITHDRAWN].0), &outgrown, &trace)
            .await
            .expect("the update returns"),
        "the update outgrows the region, and withdraws the record"
    );
    mark(format!("withdrawn-{WITHDRAWN}"));

    let mut unfinished = cache
        .get_miss_handler(&support::key(UNFINISHED), &support::meta(), &trace)
        .await
        .expect("the unfinished store opens");
    unfinished
        .write_body(
            bytes::Bytes::from(support::incompressible(50_000, 99)),
            false,
        )
        .await
        .expect("half the body is written");
    mark("unfinished".to_owned());
    // A crash drops no handler: the temporary stays.
    std::mem::forget(unfinished);
}

/// The check of one replayed state, step 3 of the test case: construct the
/// backend, let its restart procedure complete, look up every key and read
/// every hit's body in full, against the expected result for the marks the
/// replay has passed.
#[cfg(target_os = "linux")]
#[tokio::test]
#[ignore = "cargo xtask crash-replay runs it, on each replayed state"]
async fn tc_persistence_crashsafe_03_replay_check() {
    let marks: std::collections::HashSet<String> = replay_env(REPLAY_MARKS)
        .split(',')
        .filter(|mark| !mark.is_empty())
        .map(str::to_owned)
        .collect();
    let cache = replay_cache();
    let manager = Manager::new(usize::MAX);
    recover(cache, &manager)
        .await
        .expect("the restart procedure completes");

    let filler = |n: usize| support::incompressible(n, 1);
    for (i, (path, _)) in OBJECTS.iter().enumerate() {
        // A miss is always allowed: a host crash may lose any object.
        let Some((meta, body)) = support::lookup_body(cache, &support::key(path)).await else {
            continue;
        };
        assert!(
            !(PURGED.contains(&i) && marks.contains(&format!("purged-{i}"))),
            "object {i}, whose purge was acknowledged, is served"
        );
        assert!(
            body == replay_body(i),
            "object {i} is served with a body it was never stored with"
        );
        let header = meta
            .headers()
            .get("x-filler")
            .map(|value| value.as_bytes().to_vec())
            .unwrap_or_default();
        let versions: &[usize] = if i == UPDATED {
            &[STORED_FILLER, UPDATED_FILLER]
        } else {
            &[STORED_FILLER]
        };
        assert!(
            versions.iter().any(|&n| header == filler(n).as_bytes()),
            "object {i} is served with metadata it was never stored with"
        );
    }
    assert!(
        support::lookup_body(cache, &support::key(UNFINISHED))
            .await
            .is_none(),
        "the unfinished store is served"
    );
}
