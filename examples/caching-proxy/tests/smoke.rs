// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The example end to end: `caching-proxy` in front of the mock origin, asked
//! for the same URL twice, answers the second request from its cache.

use durable_cache_harness::process::{free_port, ManagedChild};
use durable_cache_mock_origin::script::Scenario;
use durable_cache_mock_origin::MockOrigin;
use std::process::Command;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_second_request_is_answered_from_the_cache() {
    let origin = MockOrigin::spawn()
        .await
        .expect("failed to start the mock origin");
    origin.set_scenario(
        "/hello",
        Scenario::fixed(
            200,
            vec![("cache-control".into(), "max-age=300".into())],
            "hello from the origin",
        ),
    );

    let cache_dir = tempfile::tempdir().expect("failed to create the cache directory");
    let listen = free_port().expect("failed to find a free port");
    let mut command = Command::new(env!("CARGO_BIN_EXE_caching-proxy"));
    command
        .arg("--listen")
        .arg(listen.to_string())
        .arg("--upstream")
        .arg(origin.addr.to_string())
        .arg("--cache-dir")
        .arg(cache_dir.path());
    // Killed when dropped, at the end of the test.
    let _proxy = ManagedChild::spawn(command).expect("failed to start caching-proxy");
    ManagedChild::wait_until_listening(listen, Duration::from_secs(10))
        .expect("caching-proxy did not start listening");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("failed to build the HTTP client");
    let url = format!("http://{listen}/hello");
    for expected in ["miss", "hit"] {
        let response = client.get(&url).send().await.expect("request failed");
        assert_eq!(response.status(), 200);
        let cache_status = response
            .headers()
            .get("x-cache-status")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        assert_eq!(cache_status.as_deref(), Some(expected));
        let body = response.text().await.expect("failed to read the body");
        assert_eq!(body, "hello from the origin");
    }
    assert_eq!(
        origin.fetch_count("/hello"),
        1,
        "the origin was asked more than once"
    );
}
