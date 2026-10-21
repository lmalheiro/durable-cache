// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPAT.WRITEFAIL-01 — Write failures fail the store.
//!
//! A failure writing or synchronising a store's data is returned from the
//! call during which it occurred, and nothing is published.
//!
//! The test case's setup names a device-mapper `flakey` target for the
//! failing writes. Injecting the failure at the backend's own write,
//! synchronisation, and rename points covers the same contract without
//! root, and a real out-of-space run over such a device is kept as the
//! privileged test `test/compat/tests/writefail_device.rs`. Steps 3 and 4
//! (a shard directory removed behind the backend's back) need no special
//! device and are exercised here directly.

use std::os::unix::fs::PermissionsExt;

use bytes::Bytes;
use durable_cache::hooks::FaultPoint;
use pingora_cache::storage::Storage;

use super::support;

// IMP-TC.COMPAT.WRITEFAIL-01: satisfy TC-COMPAT.WRITEFAIL-01

/// Step 1: a failing body write fails that `write_body` call, and the store
/// is aborted from then on — nothing is published, and the lookup misses.
#[tokio::test]
async fn tc_compat_writefail_01_failing_write_body_fails_the_store() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/writefail-body");
    let meta = support::meta();
    let trace = support::inactive_trace();

    let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
    handler
        .write_body(Bytes::from_static(b"first"), false)
        .await
        .unwrap();

    cache.hooks().inject_fault(FaultPoint::WriteTemp, 1);
    let err = handler
        .write_body(Bytes::from_static(b"second"), false)
        .await
        .expect_err("a failed write must fail its own call");
    assert!(
        err.to_string().contains("WriteTemp"),
        "unexpected error: {err}"
    );

    // The store is aborted from then on.
    assert!(handler
        .write_body(Bytes::from_static(b"third"), true)
        .await
        .is_err());
    assert!(handler.finish().await.is_err(), "and cannot be completed");

    assert!(support::lookup_body(cache, &key).await.is_none());
    assert!(support::temp_entries(cache, &key).is_empty());
}

/// Step 2: a failure at `finish` — synchronising or renaming — fails that
/// call, and nothing is published.
#[tokio::test]
async fn tc_compat_writefail_01_failing_finish_publishes_nothing() {
    for fault in [FaultPoint::SyncTemp, FaultPoint::Rename] {
        let (cache, _tmp) = support::leaked_cache();
        let key = support::key("/writefail-finish");
        let meta = support::meta();
        let trace = support::inactive_trace();

        let mut handler = cache.get_miss_handler(&key, &meta, &trace).await.unwrap();
        handler
            .write_body(Bytes::from_static(b"complete body"), true)
            .await
            .unwrap();

        cache.hooks().inject_fault(fault, 1);
        assert!(
            handler.finish().await.is_err(),
            "a failure at {fault:?} must fail finish()"
        );

        assert!(
            support::lookup_body(cache, &key).await.is_none(),
            "nothing may be published when {fault:?} fails"
        );
        assert!(support::temp_entries(cache, &key).is_empty());
    }
}

/// Step 3: a shard directory removed from outside the backend is recreated
/// once, and the store succeeds (FDS-LAYOUT.STARTUP-01).
#[tokio::test]
async fn tc_compat_writefail_01_missing_shard_directory_is_recreated_once() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/writefail-shard");
    // A first store creates the shard directory and records it in the cache.
    support::store(cache, &key, &support::meta(), b"first").await;

    let shard = support::shard_dir_of(cache, &key);
    std::fs::remove_file(support::canonical_path(cache, &key)).unwrap();
    std::fs::remove_dir(&shard).unwrap();

    support::store(cache, &key, &support::meta(), b"after the shard vanished").await;
    assert_eq!(
        support::lookup_body(cache, &key).await.unwrap().1,
        b"after the shard vanished"
    );
}

/// Step 4: when the directory cannot be recreated either, the store returns
/// an error and publishes nothing.
#[tokio::test]
async fn tc_compat_writefail_01_unrecreatable_shard_directory_fails_the_store() {
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/writefail-readonly");
    support::store(cache, &key, &support::meta(), b"first").await;

    let shard = support::shard_dir_of(cache, &key);
    let parent = shard.parent().unwrap().to_path_buf();
    std::fs::remove_file(support::canonical_path(cache, &key)).unwrap();
    std::fs::remove_dir(&shard).unwrap();
    let original = std::fs::metadata(&parent).unwrap().permissions();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();

    // Root ignores directory permissions, so this step's premise would not
    // hold there; check it rather than assume it.
    if std::fs::create_dir(parent.join("probe")).is_ok() {
        std::fs::remove_dir(parent.join("probe")).unwrap();
        std::fs::set_permissions(&parent, original).unwrap();
        eprintln!("skipped: this user can create directories in a read-only directory (root?)");
        return;
    }

    let meta = support::meta();
    let trace = support::inactive_trace();
    let result = cache.get_miss_handler(&key, &meta, &trace).await;

    // Restore before asserting, so a failure here cannot leave the
    // directory unwritable for the temp dir's own cleanup.
    std::fs::set_permissions(&parent, original).unwrap();

    assert!(
        result.is_err(),
        "a store into a shard that cannot be created must return an error"
    );
    assert!(support::lookup_body(cache, &key).await.is_none());
}
