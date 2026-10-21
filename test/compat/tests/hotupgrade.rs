// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPAT.HOTUPGRADE-01 — the outgoing process makes no further durable change once draining begins; the incoming process never corrupts what it's still reading.
//!
//! Both processes access the very same on-disk cache during the hot-upgrade
//! overlap window, so the guarantee under test is fundamentally
//! cross-process: a reader already streaming an object through the
//! outgoing process must complete with that object's original,
//! uncorrupted bytes even while the incoming process concurrently purges
//! or replaces it -- exactly the same POSIX atomic-rename/open-fd-survives-
//! unlink guarantees the crate-level tests already prove within one
//! process, but only a real two-process harness can confirm nothing in
//! this backend's design silently assumed same-process coordination (e.g.
//! the in-memory in-flight-writer registry, which is deliberately *not*
//! shared across processes -- FRE-COMPAT.HOTUPGRADE-01 itself scopes
//! streaming partial hits out for exactly this reason).
//!
//! Linux-only: the fd-transfer mechanism of `pingora-core`
//! (`server/transfer_fd/mod.rs`) is implemented only for
//! `target_os = "linux"` -- on any other platform the incoming process's
//! bootstrap simply fails to receive a socket at all, a `pingora-core`
//! limitation this crate has no way around. No root needed, but
//! `#[ignore]`d as a heavier, slower, multi-process test --
//! `compliance.yml` runs it explicitly.

#![cfg(target_os = "linux")]

// IMP-TC.COMPAT.HOTUPGRADE-01: satisfy TC-COMPAT.HOTUPGRADE-01
use durable_cache_harness::hotupgrade::HotUpgradePair;
use durable_cache_harness::process::free_port;
use durable_cache_mock_origin::script::{Chunk, Scenario};
use durable_cache_mock_origin::MockOrigin;
use futures_util::StreamExt;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Linux (the pingora-core fd transfer is Linux-only) and two real processes -- see compliance.yml"]
async fn tc_compat_hotupgrade_01_cache_consistent_across_hot_upgrade() {
    let origin = MockOrigin::spawn()
        .await
        .expect("failed to bind mock-origin");
    // Comfortably larger than this backend's 64 KiB internal read chunk,
    // so the client's stream is guaranteed to see more than one item --
    // real slack for a mid-stream pause regardless of exact framing.
    let body_a: Vec<u8> = (0..200_000u32).map(|i| (i % 251) as u8).collect();
    origin.set_scenario(
        "/hotupgrade-a",
        Scenario::fixed(
            200,
            vec![("cache-control".into(), "max-age=60".into())],
            body_a.clone(),
        ),
    );
    origin.set_scenario(
        "/hotupgrade-b",
        Scenario::fixed(
            200,
            vec![("cache-control".into(), "max-age=60".into())],
            "object-b-body",
        ),
    );

    let cache_dir = tempfile::tempdir().unwrap();
    let listen_addr = free_port().unwrap();
    let client = durable_cache_harness::http_client::client();
    let url_a = format!("http://{listen_addr}/hotupgrade-a");
    let url_b = format!("http://{listen_addr}/hotupgrade-b");

    let mut pair = HotUpgradePair::start(cache_dir.path(), origin.addr, listen_addr)
        .expect("failed to start the hot-upgrade pair");

    // Durably publish A through the outgoing process, before the upgrade
    // begins.
    let seed = client.get(&url_a).send().await.unwrap();
    assert_eq!(seed.status(), 200);
    assert_eq!(seed.bytes().await.unwrap().to_vec(), body_a);

    // Start a fresh read of A -- still through the outgoing process, since
    // the incoming one isn't accepting connections yet -- and stop after
    // the first chunk, leaving the read genuinely in flight.
    let reader_response = client.get(&url_a).send().await.unwrap();
    assert_eq!(reader_response.status(), 200);
    let mut reader_stream = reader_response.bytes_stream();
    let mut reader_body = reader_stream.next().await.unwrap().unwrap().to_vec();
    assert!(
        reader_body.len() < body_a.len(),
        "the first stream item must not already be the whole body, or the \
         pause below wouldn't actually span an in-flight read"
    );

    // Drain signal: the outgoing process must keep serving this
    // already-accepted connection to completion, but hands off the
    // listening socket and stops accepting new ones.
    pair.outgoing.proxy.quit().unwrap();

    // The incoming process takes over `listen_addr` once the handoff
    // completes.
    durable_cache_harness::process::ManagedChild::wait_until_listening(
        listen_addr,
        Duration::from_secs(10),
    )
    .expect("incoming process never took over the listening socket");

    // Through the incoming process: purge A -- the very object the
    // outgoing process is still mid-read on -- and store a new object B.
    let purge_client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    // During the close timeout both processes still accept, so a purge may
    // reach the draining one, which refuses it — "a purge refused by the
    // draining process surfaces as a failed purge request, which its client
    // can retry against the incoming process" (FRE-COMPAT.HOTUPGRADE-01).
    // Retry until the incoming process answers it, as such a client would.
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut refusals = 0;
    let purged = loop {
        let purge_request = purge_client
            .request(reqwest::Method::from_bytes(b"PURGE").unwrap(), &url_a)
            .build()
            .unwrap();
        let status = purge_client.execute(purge_request).await.unwrap().status();
        if status == 200 {
            break true;
        }
        refusals += 1;
        assert!(
            std::time::Instant::now() < deadline,
            "PURGE never succeeded against the incoming process (last status {status}, \
             {refusals} refusal(s))"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    assert!(purged);

    let store_b = client.get(&url_b).send().await.unwrap();
    assert_eq!(store_b.status(), 200);
    assert_eq!(store_b.text().await.unwrap(), "object-b-body");

    // The outgoing process's already-open read of A must be completely
    // unaffected by the incoming process's purge: it finishes with A's
    // full, original, uncorrupted bytes.
    while let Some(chunk) = reader_stream.next().await {
        reader_body.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(
        reader_body, body_a,
        "the draining process's in-flight read must complete with the \
         original object, never truncated, corrupted, or another object's bytes"
    );

    // The outgoing process finishes draining (this was its last accepted
    // connection) and exits on its own -- after the fixed 5s Pingora close
    // timeout plus the shortened grace period and runtime shutdown the
    // harness configures (see `hotupgrade::short_graceful_exit_args`).
    let status = pair
        .outgoing
        .proxy
        .wait(Duration::from_secs(30))
        .unwrap()
        .expect("outgoing process did not exit after draining");
    assert!(
        status.success(),
        "outgoing process must exit cleanly: {status:?}"
    );

    // Post-upgrade, only the incoming process remains: A is durably gone
    // (the purge), B is durably present (the concurrent store) -- the
    // overlap window left the shared cache in a fully consistent state.
    // Re-script A's origin content so that only a genuine re-fetch (never
    // a stale cache hit on the purged copy) could produce this new body --
    // direct proof the purge was durable, not just that PURGE returned 200.
    origin.set_scenario(
        "/hotupgrade-a",
        Scenario::fixed(
            200,
            vec![("cache-control".into(), "max-age=60".into())],
            "a-after-purge",
        ),
    );
    let after_a = client.get(&url_a).send().await.unwrap();
    assert_eq!(after_a.status(), 200);
    assert_eq!(
        after_a.text().await.unwrap(),
        "a-after-purge",
        "must be a genuine re-fetch -- the purged copy must never resurface"
    );

    let after_b = client.get(&url_b).send().await.unwrap();
    assert_eq!(after_b.status(), 200);
    assert_eq!(after_b.text().await.unwrap(), "object-b-body");

    drop(pair.incoming);
}

/// The read-only half of the same TC: a store that completes in the outgoing
/// process *after* it received the upgrade signal is not durably published
/// (FDS-COMPAT.READONLY-01), so the incoming process never sees it and the
/// object is fetched again on the next request.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs Linux (the pingora-core fd transfer is Linux-only) and two real processes -- see compliance.yml"]
async fn tc_compat_hotupgrade_01_a_store_completing_after_the_signal_is_not_published() {
    let origin = MockOrigin::spawn()
        .await
        .expect("failed to bind mock-origin");
    let path = "/hotupgrade-draining-store";
    // A body whose last chunk lands well after the upgrade signal, so the
    // store completes inside the draining process.
    origin.set_scenario(
        path,
        Scenario {
            status: 200,
            headers: vec![("cache-control".into(), "max-age=60".into())],
            chunks: vec![
                Chunk {
                    data: "first-half-".into(),
                    delay_before: Duration::ZERO,
                },
                Chunk {
                    data: "second-half".into(),
                    delay_before: Duration::from_secs(2),
                },
            ],
            declare_length: false,
            etag: None,
            revalidated_headers: Vec::new(),
        },
    );

    let cache_dir = tempfile::tempdir().unwrap();
    let listen_addr = free_port().unwrap();
    let client = durable_cache_harness::http_client::client();
    let url = format!("http://{listen_addr}{path}");

    let mut pair = HotUpgradePair::start(cache_dir.path(), origin.addr, listen_addr)
        .expect("failed to start the hot-upgrade pair");

    // Start the slow store through the outgoing process, then signal the
    // upgrade while its body is still streaming.
    let slow_request = tokio::spawn({
        let client = client.clone();
        let url = url.clone();
        async move { client.get(&url).send().await.unwrap().text().await.unwrap() }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    pair.outgoing.proxy.quit().unwrap();

    // The request completes normally: a read-only backend must not fail the
    // client's response, only stop publishing it.
    assert_eq!(slow_request.await.unwrap(), "first-half-second-half");

    durable_cache_harness::process::ManagedChild::wait_until_listening(
        listen_addr,
        Duration::from_secs(10),
    )
    .expect("incoming process never took over the listening socket");
    pair.outgoing
        .proxy
        .wait(Duration::from_secs(30))
        .unwrap()
        .expect("outgoing process did not exit after draining");

    // Nothing was published: no canonical record, and no temporary left
    // behind either. The two processes' leases are not counted.
    let files = count_files(cache_dir.path());
    assert_eq!(
        files, 0,
        "a store completing after the upgrade signal must publish nothing and \
         leave no temporary; found {files} file(s) under the cache directory"
    );

    // And the incoming process therefore fetches the object again.
    let fetches_before = origin.fetch_count(path);
    let after = client.get(&url).send().await.unwrap();
    assert_eq!(after.status(), 200);
    assert_eq!(after.text().await.unwrap(), "first-half-second-half");
    assert_eq!(
        origin.fetch_count(path),
        fetches_before + 1,
        "the object was never cached, so the incoming process must re-fetch it"
    );

    drop(pair.incoming);
}

/// Every record and temporary under a cache directory, ignoring the empty
/// shard tree, the writers' leases, and the pid/upgrade-socket files
/// `fixture-proxy` keeps beside it.
fn count_files(root: &std::path::Path) -> usize {
    fn walk(dir: &std::path::Path, count: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                if entry.file_name() != durable_cache::test_support::LEASE_DIR {
                    walk(&entry.path(), count);
                }
            } else if !entry
                .file_name()
                .to_string_lossy()
                .starts_with("fixture-proxy")
            {
                *count += 1;
            }
        }
    }
    let mut count = 0;
    walk(root, &mut count);
    count
}
