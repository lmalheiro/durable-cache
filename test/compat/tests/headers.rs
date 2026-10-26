// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.HEADERS-01 — repeated/unrecognised/empty/obs-text/non-200/oversized header sets survive a store+restart round trip byte for byte.
//!
//! Fully closed at the crate level in
//! `durable-cache/tests/compat_headers.rs` (no HTTP needed for a
//! `ResponseHeader` round-trip property). This file is the end-to-end
//! check through `fixture-proxy`: a real HTTP round trip (miss, then hit)
//! plus a process restart with the origin unreachable, proving the wiring
//! itself (request_cache_filter/cache_key_callback/response_cache_filter)
//! actually stores and serves through `FsCache`, not just that the crate
//! does.

use durable_cache_harness::fixture_proxy::{FixtureProxyHandle, FixtureProxyOptions};
use durable_cache_harness::process::free_port;
use durable_cache_mock_origin::script::Scenario;
use durable_cache_mock_origin::MockOrigin;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_compliance_headers_01_header_fields_round_trip() {
    let origin = MockOrigin::spawn()
        .await
        .expect("failed to bind mock-origin");
    origin.set_scenario(
        "/headers-e2e",
        Scenario::fixed(
            200,
            vec![
                ("cache-control".into(), "max-age=60".into()),
                ("x-repeated".into(), "first".into()),
                ("x-empty".into(), "".into()),
                ("x-custom-unrecognised".into(), "opaque".into()),
            ],
            "headers-e2e body",
        ),
    );

    let cache_dir = durable_cache_harness::cache_dir();
    let listen_addr = free_port().unwrap();
    let mut proxy = FixtureProxyHandle::start(FixtureProxyOptions {
        cache_dir: cache_dir.path(),
        upstream_addr: origin.addr,
        listen_addr,
        extra_args: vec![],
    })
    .expect("failed to start fixture-proxy");

    let client = durable_cache_harness::http_client::client();
    let url = format!("{}/headers-e2e", proxy.base_url());

    // First request: a miss, fetched from the origin and stored.
    let first = client.get(&url).send().await.unwrap();
    assert_eq!(first.status(), 200);
    assert_eq!(
        first.headers().get("x-custom-unrecognised").unwrap(),
        "opaque"
    );
    assert_eq!(first.headers().get("x-empty").unwrap(), "");
    let first_body = first.text().await.unwrap();
    assert_eq!(first_body, "headers-e2e body");

    // Second request: a hit, served from FsCache without the origin being
    // consulted again for content (headers/body must be identical).
    let second = client.get(&url).send().await.unwrap();
    assert_eq!(second.status(), 200);
    assert_eq!(
        second.headers().get("x-custom-unrecognised").unwrap(),
        "opaque"
    );
    assert_eq!(second.headers().get("x-empty").unwrap(), "");
    assert_eq!(second.text().await.unwrap(), "headers-e2e body");

    // Restart fixture-proxy against the same cache directory but pointed
    // at a dead upstream -- the only way the next request can still
    // succeed is if it's served entirely from the durable, on-disk cache.
    proxy.proxy.kill_immediately().unwrap();
    proxy.proxy.wait(std::time::Duration::from_secs(5)).unwrap();
    let dead_upstream = free_port().unwrap(); // nothing listens here

    let restarted = FixtureProxyHandle::start(FixtureProxyOptions {
        cache_dir: cache_dir.path(),
        upstream_addr: dead_upstream,
        listen_addr,
        extra_args: vec![],
    })
    .expect("failed to restart fixture-proxy");

    let after_restart = client.get(&url).send().await.unwrap();
    assert_eq!(
        after_restart.status(),
        200,
        "must be served from the durable on-disk cache, since the upstream is unreachable"
    );
    assert_eq!(
        after_restart
            .headers()
            .get("x-custom-unrecognised")
            .unwrap(),
        "opaque"
    );
    assert_eq!(after_restart.headers().get("x-empty").unwrap(), "");
    assert_eq!(after_restart.text().await.unwrap(), "headers-e2e body");

    drop(restarted);
}
