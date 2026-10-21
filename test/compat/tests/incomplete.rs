// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.INCOMPLETE-01 — an aborted, crashed, or truncated store is never served as if complete.
//!
//! The orderly-abort, damaged-record, truncated-record, and
//! follower-sees-an-error legs are covered directly against
//! `Storage::lookup`/`get_miss_handler` in
//! `durable-cache/tests/compat_incomplete.rs` (no HTTP needed).
//! What remained here -- a real `kill -9` mid-write plus an actual process
//! restart, which an in-process `drop` can't stand in for -- is covered
//! below, closing the TC (jointly with `compat_incomplete.rs`'s coverage
//! of every other leg).

use durable_cache_harness::fixture_proxy::{FixtureProxyHandle, FixtureProxyOptions};
use durable_cache_harness::process::free_port;
use durable_cache_mock_origin::script::{Chunk, Scenario};
use durable_cache_mock_origin::MockOrigin;
use futures_util::StreamExt;
use std::time::Duration;

// IMP-TC.COMPLIANCE.INCOMPLETE-01: satisfy TC-COMPLIANCE.INCOMPLETE-01

/// Walk the two-level shard tree for any file whose name ends in the
/// temporaries' extension (`durable_cache::test_support::TEMP_SUFFIX`), as
/// in `<hash>.<pid>-<nonce>.<counter>.tmp` -- a crash-orphaned per-writer
/// temporary.
fn find_temp_files(cache_dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let Ok(first_level) = std::fs::read_dir(cache_dir) else {
        return found;
    };
    for first in first_level.flatten() {
        let Ok(second_level) = std::fs::read_dir(first.path()) else {
            continue;
        };
        for second in second_level.flatten() {
            let Ok(entries) = std::fs::read_dir(second.path()) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.ends_with(&format!(".{}", durable_cache::test_support::TEMP_SUFFIX)) {
                    found.push(entry.path());
                }
            }
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_compliance_incomplete_01_killed_process_leaves_an_orphan_that_gets_reclaimed() {
    let origin = MockOrigin::spawn()
        .await
        .expect("failed to bind mock-origin");
    let scenario = Scenario {
        status: 200,
        headers: vec![("cache-control".into(), "max-age=60".into())],
        chunks: vec![
            Chunk {
                data: "first-half-".into(),
                delay_before: Duration::ZERO,
            },
            Chunk {
                // Long enough that the test can reliably kill the proxy in
                // the gap between the two chunks.
                data: "second-half".into(),
                delay_before: Duration::from_secs(5),
            },
        ],
        declare_length: false,
        ..Default::default()
    };
    origin.set_scenario("/incomplete-kill", scenario.clone());

    let cache_dir = tempfile::tempdir().unwrap();
    let listen_addr = free_port().unwrap();
    let mut proxy = FixtureProxyHandle::start(FixtureProxyOptions {
        cache_dir: cache_dir.path(),
        upstream_addr: origin.addr,
        listen_addr,
        extra_args: vec![],
    })
    .expect("failed to start fixture-proxy");

    let client = durable_cache_harness::http_client::client();
    let url = format!("{}/incomplete-kill", proxy.base_url());

    // Start the request and consume only the first chunk -- the proxy has
    // forwarded (and, keeping pace, written to its own miss handler) that
    // much, but the connection is still open, waiting on the origin's
    // delayed second chunk.
    let response = client.get(&url).send().await.unwrap();
    let mut stream = response.bytes_stream();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(&first[..], b"first-half-");

    // Crash: no abort path runs, unlike an orderly drop.
    proxy.proxy.kill_immediately().unwrap();
    proxy.proxy.wait(Duration::from_secs(5)).unwrap();

    let orphans = find_temp_files(cache_dir.path());
    assert_eq!(
        orphans.len(),
        1,
        "the crashed write's per-writer temporary must still be on disk, \
         never promoted to the canonical name: found {orphans:?}"
    );

    // Restart against the same cache directory; recovery-mode=block means
    // Eviction Recovery -- which reclaims a stopped writer's temporary
    // (FDS-STORAGE.RECLAIM-01) -- has already run by the time this
    // constructor returns.
    let restarted = FixtureProxyHandle::start(FixtureProxyOptions {
        cache_dir: cache_dir.path(),
        upstream_addr: origin.addr,
        listen_addr,
        extra_args: vec![],
    })
    .expect("failed to restart fixture-proxy");

    assert!(
        find_temp_files(cache_dir.path()).is_empty(),
        "the orphaned temporary must be reclaimed by the restart's recovery pass"
    );

    // The key was never durably stored (the crash preceded the publishing
    // rename), so this is a genuine miss: the proxy re-fetches the whole
    // object from the still-live origin, and this time it completes.
    let after_restart = client.get(&url).send().await.unwrap();
    assert_eq!(after_restart.status(), 200);
    assert_eq!(
        after_restart.text().await.unwrap(),
        "first-half-second-half",
        "a fresh, complete fetch must succeed -- the crashed attempt's \
         partial bytes must never surface as part of a served response"
    );

    drop(restarted);
}
