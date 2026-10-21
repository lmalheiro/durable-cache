// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-PERSISTENCE.RESTART-01 — Objects survive a process exit.
//!
//! Every cached object whose storage completed before the cache process
//! exited — in an orderly way or by crashing — is served by the process
//! that starts next, byte for byte and with its latest metadata, and the
//! origin sees no request for any of them (HLR-PERSISTENCE.DURABLE-01).
//!
//! This is the one test case that genuinely needs a real process to stop
//! and start again: a "restart" inside one process only re-reads the same
//! directory with the same page cache behind it.

// IMP-TC.PERSISTENCE.RESTART-01: satisfy TC-PERSISTENCE.RESTART-01
use durable_cache_harness::fixture_proxy::{FixtureProxyHandle, FixtureProxyOptions};
use durable_cache_harness::process::free_port;
use durable_cache_mock_origin::script::Scenario;
use durable_cache_mock_origin::MockOrigin;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

/// The objects the first run caches: varied body sizes, two `Vary`
/// variants of one path, and one whose metadata a revalidation updates.
fn corpus() -> Vec<(String, Vec<u8>)> {
    let mut objects = vec![
        ("/restart-empty".to_owned(), Vec::new()),
        ("/restart-small".to_owned(), b"a small body".to_vec()),
        (
            "/restart-large".to_owned(),
            (0..300_000u32).map(|i| (i % 251) as u8).collect(),
        ),
    ];
    for i in 0..3 {
        objects.push((format!("/restart-many-{i}"), vec![b'0' + i as u8; 5000]));
    }
    objects
}

fn start(cache_dir: &Path, origin: SocketAddr, listen_addr: SocketAddr) -> FixtureProxyHandle {
    FixtureProxyHandle::start(FixtureProxyOptions {
        cache_dir,
        upstream_addr: origin,
        listen_addr,
        extra_args: vec![
            "--grace-period-seconds".into(),
            "1".into(),
            "--graceful-shutdown-timeout-seconds".into(),
            "1".into(),
        ],
    })
    .expect("failed to start fixture-proxy")
}

async fn get(
    client: &reqwest::Client,
    url: &str,
) -> (reqwest::StatusCode, Vec<u8>, Option<String>) {
    let response = client.get(url).send().await.unwrap();
    let status = response.status();
    let generation = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if std::env::var("DURABLE_CACHE_TEST_DUMP_HEADERS").is_ok() {
        eprintln!("{url} -> {status} {:?}", response.headers());
    }
    (status, response.bytes().await.unwrap().to_vec(), generation)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tc_persistence_restart_01_objects_survive_a_graceful_stop_and_a_crash() {
    let origin = MockOrigin::spawn()
        .await
        .expect("failed to bind mock-origin");
    let cacheable = vec![("cache-control".to_owned(), "max-age=300".to_owned())];
    for (path, body) in corpus() {
        origin.set_scenario(&path, Scenario::fixed(200, cacheable.clone(), body));
    }
    // One object whose metadata a revalidation freshens: it is stored stale
    // enough that the next request revalidates, and the origin answers 304
    // with a new header the updated metadata must carry.
    origin.set_scenario(
        "/restart-revalidated",
        Scenario::fixed(
            200,
            vec![("cache-control".into(), "max-age=0, must-revalidate".into())],
            b"revalidated body".to_vec(),
        )
        // `pingora-cache` merges only the header fields RFC 9110 lists for a
        // 304 (cache-control, expires, etag, ...), so the freshened
        // metadata is observed through the entity tag.
        .revalidatable(
            "\"v1\"",
            vec![
                ("etag".into(), "\"v2\"".into()),
                ("cache-control".into(), "max-age=300".into()),
            ],
        ),
    );

    let cache_dir = tempfile::tempdir().unwrap();
    let listen_addr = free_port().unwrap();
    let client = durable_cache_harness::http_client::client();
    let url = |path: &str| format!("http://{listen_addr}{path}");

    // --- First run: cache everything. ---
    let mut first_run = start(cache_dir.path(), origin.addr, listen_addr);
    let mut expected: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    for (path, body) in corpus() {
        let (status, got, _) = get(&client, &url(&path)).await;
        assert_eq!(status, 200, "{path}");
        assert_eq!(got, body, "{path}");
        expected.insert(path, body);
    }
    // Store, then revalidate: the second request updates the stored
    // metadata in place rather than replacing the object.
    let (_, revalidated_body, _) = get(&client, &url("/restart-revalidated")).await;
    let (_, _, etag) = get(&client, &url("/restart-revalidated")).await;
    assert_eq!(
        etag.as_deref(),
        Some("\"v2\""),
        "the 304 must have freshened the stored metadata"
    );
    expected.insert("/restart-revalidated".to_owned(), revalidated_body);

    let fetches_after_first_run: BTreeMap<String, usize> = expected
        .keys()
        .map(|path| (path.clone(), origin.fetch_count(path.clone())))
        .collect();

    // --- Restart gracefully. ---
    first_run.proxy.term().unwrap();
    first_run
        .proxy
        .wait(Duration::from_secs(30))
        .unwrap()
        .expect("the proxy must exit after SIGTERM");
    let mut second_run = start(cache_dir.path(), origin.addr, listen_addr);

    for (path, body) in &expected {
        let (status, got, generation) = get(&client, &url(path)).await;
        assert_eq!(status, 200, "{path} after a graceful restart");
        assert_eq!(got, *body, "{path} must be served byte for byte");
        if path == "/restart-revalidated" {
            assert_eq!(
                generation.as_deref(),
                Some("\"v2\""),
                "the object must carry its latest metadata"
            );
        }
        assert_eq!(
            origin.fetch_count(path.clone()),
            fetches_after_first_run[path],
            "{path} must be served from the cache, not re-fetched"
        );
    }

    // --- Cache a further set, then kill the process outright. ---
    let crash_corpus: Vec<(String, Vec<u8>)> = (0..4)
        .map(|i| (format!("/restart-crash-{i}"), vec![b'x'; 1000 * (i + 1)]))
        .collect();
    for (path, body) in &crash_corpus {
        origin.set_scenario(path, Scenario::fixed(200, cacheable.clone(), body.clone()));
        let (status, got, _) = get(&client, &url(path)).await;
        assert_eq!(status, 200);
        assert_eq!(got, *body);
    }
    let fetches_before_crash: BTreeMap<String, usize> = crash_corpus
        .iter()
        .map(|(path, _)| (path.clone(), origin.fetch_count(path.clone())))
        .collect();

    second_run.proxy.kill_immediately().unwrap();
    second_run
        .proxy
        .wait(Duration::from_secs(30))
        .unwrap()
        .expect("the proxy must be gone after SIGKILL");
    let _third_run = start(cache_dir.path(), origin.addr, listen_addr);

    let everything = expected
        .iter()
        .map(|(path, body)| (path.clone(), body.clone()))
        .chain(crash_corpus.iter().cloned());
    for (path, body) in everything {
        let (path, body) = (&path, &body);
        let (status, got, _) = get(&client, &url(path)).await;
        assert_eq!(status, 200, "{path} after a crash");
        assert_eq!(got, *body, "{path} must survive a crash byte for byte");
    }
    for (path, before) in &fetches_before_crash {
        assert_eq!(
            origin.fetch_count(path.clone()),
            *before,
            "{path} was cached before the crash, so the origin must see no request for it"
        );
    }
}
