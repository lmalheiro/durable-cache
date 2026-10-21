// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPAT.STREAMING-01 — concurrent followers stream the most-recently-opened writer's bytes as they land.
//!
//! Steps 1-6 (partial-hit resolution to the most recently opened writer,
//! prompt per-chunk delivery, validity across the concluding rename) are
//! covered directly against `Storage::lookup`/`get_miss_handler` in
//! `durable-cache/tests/compat_streaming.rs` -- no HTTP needed for
//! any of that. Step 7 (counting upstream fetches) is the one leg that
//! genuinely needs a real upstream to count against; that's this file's
//! job, closing the TC jointly with the crate-level file.

use durable_cache_harness::fixture_proxy::{FixtureProxyHandle, FixtureProxyOptions};
use durable_cache_harness::process::free_port;
use durable_cache_mock_origin::script::{Chunk, Scenario};
use durable_cache_mock_origin::MockOrigin;
use futures_util::StreamExt;
use std::time::Duration;

// IMP-TC.COMPAT.STREAMING-01: satisfy TC-COMPAT.STREAMING-01

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_compat_streaming_01_followers_never_cause_a_second_upstream_fetch() {
    let origin = MockOrigin::spawn()
        .await
        .expect("failed to bind mock-origin");
    let path = "/streaming-fetch-count";
    origin.set_scenario(
        path,
        Scenario {
            status: 200,
            headers: vec![("cache-control".into(), "max-age=60".into())],
            chunks: vec![
                Chunk {
                    data: "leader-chunk-1-".into(),
                    delay_before: Duration::ZERO,
                },
                Chunk {
                    data: "leader-chunk-2-".into(),
                    delay_before: Duration::from_millis(300),
                },
                Chunk {
                    data: "leader-chunk-3".into(),
                    delay_before: Duration::from_millis(300),
                },
            ],
            declare_length: false,
            ..Default::default()
        },
    );

    let cache_dir = tempfile::tempdir().unwrap();
    let listen_addr = free_port().unwrap();
    let proxy = FixtureProxyHandle::start(FixtureProxyOptions {
        cache_dir: cache_dir.path(),
        upstream_addr: origin.addr,
        listen_addr,
        extra_args: vec![],
    })
    .expect("failed to start fixture-proxy");

    let client = durable_cache_harness::http_client::client();
    let url = format!("{}{path}", proxy.base_url());
    let expected = "leader-chunk-1-leader-chunk-2-leader-chunk-3";

    // The leader: read its first chunk so the writer is definitely
    // registered (and the origin definitely fetched once) before any
    // follower attaches, then leave it mid-stream.
    let leader_response = client.get(&url).send().await.unwrap();
    let mut leader_stream = leader_response.bytes_stream();
    let leader_first = leader_stream.next().await.unwrap().unwrap();
    assert_eq!(&leader_first[..], b"leader-chunk-1-");
    assert_eq!(
        origin.fetch_count(path),
        1,
        "the leader's own miss must fetch the origin exactly once"
    );

    // Three followers, attaching concurrently while the leader is still
    // streaming its remaining (delayed) chunks.
    let follower_urls = [url.clone(), url.clone(), url.clone()];
    let followers = futures_util::future::join_all(follower_urls.iter().map(|u| {
        let client = client.clone();
        async move {
            let resp = client.get(u).send().await.unwrap();
            assert_eq!(resp.status(), 200);
            resp.text().await.unwrap()
        }
    }))
    .await;

    for body in &followers {
        assert_eq!(
            body, expected,
            "every follower's body must be byte-identical to the leader's upstream response"
        );
    }

    // Drain the leader's own remaining stream too.
    let mut leader_body = leader_first.to_vec();
    while let Some(chunk) = leader_stream.next().await {
        leader_body.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(leader_body, expected.as_bytes());

    assert_eq!(
        origin.fetch_count(path),
        1,
        "three followers attaching to the same in-flight writer must never \
         cause a second upstream fetch"
    );

    drop(proxy);
}
