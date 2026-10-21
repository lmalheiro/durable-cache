// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPAT.HOTUPGRADE-01, from step 6 on — the hot-upgrade helper itself.
//!
//! Steps 1 to 5 run two real processes (`test/compat/tests/hotupgrade.rs`).
//! These run a Pingora server inside the test, with the helper installed as
//! an integrator installs it; where a step needs an outgoing process, the
//! test stands in for it.

use std::net::TcpListener;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::sync::OnceLock;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use durable_cache::hooks::PausePoint;
use durable_cache::hotupgrade::{self, CLOSE_TIMEOUT};
use durable_cache::FsCache;
use pingora_cache::eviction::simple_lru::Manager;
use pingora_cache::eviction::{CacheEntryKey, CacheEntryKeyRef, EvictionManager};
use pingora_cache::CacheKey;
use pingora_core::server::configuration::{Opt, ServerConf};
use pingora_core::server::{Fds, RunArgs, Server, ShutdownSignal, ShutdownSignalWatch};
use pingora_core::services::background::BackgroundService;
use tokio::sync::{mpsc, Mutex};

use super::support;

// IMP-TC.COMPAT.HOTUPGRADE-02: satisfy TC-COMPAT.HOTUPGRADE-01 -- the
// helper's steps; test/compat/tests/hotupgrade.rs covers the two processes.

/// Step 6: a server that bootstraps as a service, with the helper given that
/// service's handle, starts its restart scan only once the outgoing process
/// has handed its socket over and the close timeout has elapsed since. A
/// wait that began with the server instead would run out 3.5 s after the
/// hand-over here.
#[test]
fn tc_compat_hotupgrade_01_the_scan_waits_for_the_takeover() {
    const RECORDS: usize = 4;
    let (cache, _tmp) = populated(RECORDS);
    let eviction = Timed::leaked();
    let sockets = tempfile::tempdir().expect("a directory for the upgrade socket");
    let upgrade_sock = sockets.path().join("upgrade.sock");

    let mut server = server(true, &upgrade_sock);
    let bootstrap = server.bootstrap_as_a_service();
    hotupgrade::install(&mut server, cache, eviction, Some(&bootstrap));
    let running = Running::start(server);

    // The outgoing process, which the test stands in for, hands its
    // listening socket over 1.5 s after the server started: well within the
    // 5 s that bootstrap waits for it.
    std::thread::sleep(Duration::from_millis(1500));
    let listener = TcpListener::bind("127.0.0.1:0").expect("a listening socket");
    let mut fds = Fds::new();
    fds.add(
        listener.local_addr().expect("its address").to_string(),
        listener.as_raw_fd(),
    );
    fds.send_to_sock(upgrade_sock.to_str().expect("a UTF-8 path"))
        .expect("the socket is handed over");
    let handed_over = Instant::now();

    eviction.wait_for(RECORDS);
    let waited = eviction
        .first_admission()
        .saturating_duration_since(handed_over);
    assert!(
        waited >= CLOSE_TIMEOUT,
        "the scan must start no sooner than the close timeout after the \
         takeover (FDS-COMPAT.READONLY-01); it started {waited:?} after it"
    );
    running.stop(ShutdownSignal::FastShutdown);
}

/// Step 6, continued: an ordinary server, taking over no socket, has no
/// outgoing process to wait for, and scans at once.
#[test]
fn tc_compat_hotupgrade_01_an_ordinary_start_scans_at_once() {
    const RECORDS: usize = 4;
    let (cache, _tmp) = populated(RECORDS);
    let eviction = Timed::leaked();
    let sockets = tempfile::tempdir().expect("a directory for the upgrade socket");
    let mut server = server(false, &sockets.path().join("upgrade.sock"));
    server.bootstrap();
    hotupgrade::install(&mut server, cache, eviction, None);
    let started = Instant::now();
    let running = Running::start(server);

    eviction.wait_for(RECORDS);
    let waited = eviction
        .first_admission()
        .saturating_duration_since(started);
    assert!(
        waited < CLOSE_TIMEOUT,
        "with no outgoing process, the scan must not wait for the close \
         timeout; it started {waited:?} after the server"
    );
    running.stop(ShutdownSignal::FastShutdown);
}

/// Step 7: the upgrade signal makes the backend read-only, even when the
/// server has no socket to hand over.
#[test]
fn tc_compat_hotupgrade_01_an_upgrade_makes_the_backend_read_only() {
    let (cache, _tmp) = support::leaked_cache();
    run_until(cache, ShutdownSignal::GracefulUpgrade);
    assert!(
        cache.is_read_only(),
        "the upgrade signal makes the backend read-only (FDS-COMPAT.READONLY-01)"
    );
}

/// Step 7: a graceful terminate leaves the backend writable, since no
/// incoming process takes the cache over.
#[test]
fn tc_compat_hotupgrade_01_a_terminate_leaves_the_backend_writable() {
    let (cache, _tmp) = support::leaked_cache();
    run_until(cache, ShutdownSignal::GracefulTerminate);
    assert!(
        !cache.is_read_only(),
        "a terminate leaves the backend writable (FDS-COMPAT.READONLY-01)"
    );
}

/// Step 8: a server that starts shutting down while the recovery service
/// waits spares it the scan, and the service says so.
#[test]
fn tc_compat_hotupgrade_01_a_shutdown_during_the_wait_skips_the_scan() {
    if !support::in_own_process(
        module_path!(),
        "tc_compat_hotupgrade_01_a_shutdown_during_the_wait_skips_the_scan",
    ) {
        return;
    }
    let logs = support::LogCapture::install();
    let (cache, _tmp) = populated(2);
    let eviction = Timed::leaked();
    let service = hotupgrade::recovery_service(cache, eviction, Duration::from_secs(60)).task();

    let runtime = tokio::runtime::Runtime::new().expect("a runtime for the service");
    runtime.block_on(async {
        let (shutdown, watch) = tokio::sync::watch::channel(false);
        let running = tokio::spawn(async move { service.start(watch).await });
        shutdown
            .send(true)
            .expect("the service watches for the shutdown");
        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("the service gives up its wait at once")
            .expect("the service's task ends");
    });
    assert_eq!(eviction.total_items(), 0, "the scan never starts");
    assert!(
        logs.lines()
            .iter()
            .any(|line| line.contains("restart scan not started, the server shutting down")),
        "the service says why it did not scan"
    );
}

/// Step 8: a server that starts shutting down during the scan stops it at
/// its next entry, and the scan's summary line says why.
#[test]
fn tc_compat_hotupgrade_01_a_shutdown_during_the_scan_stops_it() {
    if !support::in_own_process(
        module_path!(),
        "tc_compat_hotupgrade_01_a_shutdown_during_the_scan_stops_it",
    ) {
        return;
    }
    const RECORDS: usize = 4;
    let logs = support::LogCapture::install();
    let (cache, _tmp) = populated(RECORDS);
    let eviction = Timed::leaked();
    shut_down_at(cache, eviction, PausePoint::RecoverAfterAdmit, || {});
    assert_eq!(
        eviction.total_items(),
        1,
        "the scan stops at the entry after its first admission"
    );
    assert_stopped_for_shutdown(&logs, "admitted 1;");
}

/// Step 8: a server that starts shutting down once the walk is done spares
/// the scan its ghost-entry checks, so the entry of a record removed
/// meanwhile stays.
#[test]
fn tc_compat_hotupgrade_01_a_shutdown_after_the_walk_skips_the_ghost_pass() {
    if !support::in_own_process(
        module_path!(),
        "tc_compat_hotupgrade_01_a_shutdown_after_the_walk_skips_the_ghost_pass",
    ) {
        return;
    }
    const RECORDS: usize = 2;
    let logs = support::LogCapture::install();
    let (cache, _tmp) = populated(RECORDS);
    let eviction = Timed::leaked();
    shut_down_at(cache, eviction, PausePoint::RecoverBeforeGhostPass, || {
        std::fs::remove_file(support::canonical_path(cache, &record_key(0)))
            .expect("a record is removed");
    });
    assert_eq!(
        eviction.total_items(),
        RECORDS,
        "the removed record's entry stays: no ghost-entry check ran"
    );
    assert_stopped_for_shutdown(&logs, "admitted 2;");
}

// --- the recovery service --------------------------------------------------

/// Run the helper's recovery service over `cache` until its scan reaches
/// `point`, do `meanwhile`, shut its server down, and wait for the service
/// to finish.
fn shut_down_at(
    cache: &'static FsCache,
    eviction: &'static Timed,
    point: PausePoint,
    meanwhile: impl FnOnce(),
) {
    let service = hotupgrade::recovery_service(cache, eviction, Duration::ZERO).task();
    let paused = cache.hooks().arm_pause(point, None);
    // Two workers: the pause holds the scan's.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime for the service");
    runtime.block_on(async {
        let (shutdown, watch) = tokio::sync::watch::channel(false);
        let running = tokio::spawn(async move { service.start(watch).await });
        assert!(
            paused.wait_reached(Duration::from_secs(5)),
            "the scan reaches {point:?}"
        );
        meanwhile();
        shutdown
            .send(true)
            .expect("the service watches for the shutdown");
        paused.release();
        tokio::time::timeout(Duration::from_secs(5), running)
            .await
            .expect("the scan stops")
            .expect("the service's task ends");
    });
}

/// The scan's summary line says that it stopped for the server shutting
/// down, and contains `admitted`.
fn assert_stopped_for_shutdown(logs: &support::LogCapture, admitted: &str) {
    let lines = logs.lines();
    let summary = lines
        .iter()
        .find(|line| line.contains("restart scan stopped, the server shutting down"))
        .expect("the summary line says the server was shutting down");
    assert!(summary.contains(admitted), "{summary}");
}

// --- the server ------------------------------------------------------------

/// Run an ordinary server with the helper installed on `cache` until it
/// has finished after `signal`. The signal is sent at once: the watcher
/// subscribed to the server's phases when it was installed, and the
/// server's one-second grace period leaves it the time to act on them.
fn run_until(cache: &'static FsCache, signal: ShutdownSignal) {
    let eviction: &'static Manager = Box::leak(Box::new(Manager::new(usize::MAX)));
    let sockets = tempfile::tempdir().expect("a directory for the upgrade socket");
    let mut server = server(false, &sockets.path().join("upgrade.sock"));
    server.bootstrap();
    hotupgrade::install(&mut server, cache, eviction, None);
    Running::start(server).stop(signal);
}

/// A server configured as an integrator's, taking its listening sockets
/// over through `upgrade_sock` if `upgrading`.
fn server(upgrading: bool, upgrade_sock: &Path) -> Server {
    let opt = Opt {
        upgrade: upgrading,
        ..Opt::default()
    };
    let conf = ServerConf {
        upgrade_sock: upgrade_sock.display().to_string(),
        // A graceful shutdown that ends within a second or two.
        grace_period_seconds: Some(1),
        graceful_shutdown_timeout_seconds: Some(1),
        ..ServerConf::default()
    };
    Server::new_with_opt_and_conf(opt, conf)
}

/// A server running on a thread of its own, as `run_forever` runs it, and
/// receiving its signals from the test.
struct Running {
    signals: mpsc::UnboundedSender<ShutdownSignal>,
    thread: JoinHandle<()>,
}

impl Running {
    fn start(server: Server) -> Running {
        let (signals, received) = mpsc::unbounded_channel();
        let watch = Signals(Mutex::new(received));
        let thread = std::thread::spawn(move || {
            server.run(RunArgs {
                shutdown_signal: Box::new(watch),
            })
        });
        Running { signals, thread }
    }

    /// Send the server `signal`, and wait for it to finish.
    fn stop(self, signal: ShutdownSignal) {
        self.signals
            .send(signal)
            .expect("the server listens for its signals");
        self.thread.join().expect("the server finishes");
    }
}

/// The process's signals, as the test sends them.
struct Signals(Mutex<mpsc::UnboundedReceiver<ShutdownSignal>>);

#[async_trait::async_trait]
impl ShutdownSignalWatch for Signals {
    async fn recv(&self) -> ShutdownSignal {
        let signal = self.0.lock().await.recv().await;
        // A test that went away without sending one wants the server gone.
        signal.unwrap_or(ShutdownSignal::FastShutdown)
    }
}

/// A cache holding `records` published objects, under the keys
/// [`record_key`] gives.
fn populated(records: usize) -> (&'static FsCache, tempfile::TempDir) {
    let (cache, tmp) = support::leaked_cache();
    let runtime = tokio::runtime::Runtime::new().expect("a runtime to store with");
    runtime.block_on(async {
        for i in 0..records {
            support::store(cache, &record_key(i), &support::meta(), b"body").await;
        }
    });
    (cache, tmp)
}

/// The key of the `i`th object [`populated`] stores.
fn record_key(i: usize) -> CacheKey {
    support::key(&format!("/hotupgrade-{i}"))
}

// --- the eviction manager --------------------------------------------------

/// The stock manager, noting when it first hears of an admission.
struct Timed {
    inner: Manager,
    first_admission: OnceLock<Instant>,
}

impl Timed {
    fn leaked() -> &'static Timed {
        Box::leak(Box::new(Timed {
            inner: Manager::new(usize::MAX),
            first_admission: OnceLock::new(),
        }))
    }

    fn first_admission(&self) -> Instant {
        *self.first_admission.get().expect("an admission")
    }

    /// Wait until the manager holds `items` entries.
    fn wait_for(&self, items: usize) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.total_items() < items {
            assert!(
                Instant::now() < deadline,
                "the scan admitted only {} of {items} records",
                self.total_items()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

#[async_trait::async_trait]
impl EvictionManager for Timed {
    fn total_size(&self) -> usize {
        self.inner.total_size()
    }
    fn total_items(&self) -> usize {
        self.inner.total_items()
    }
    fn evicted_size(&self) -> usize {
        self.inner.evicted_size()
    }
    fn evicted_items(&self) -> usize {
        self.inner.evicted_items()
    }
    fn admit(
        &self,
        item: CacheEntryKey,
        size: usize,
        fresh_until: SystemTime,
    ) -> Vec<CacheEntryKey> {
        self.first_admission.get_or_init(Instant::now);
        self.inner.admit(item, size, fresh_until)
    }
    fn increment_weight(
        &self,
        item: &CacheEntryKey,
        delta: usize,
        max_weight: Option<usize>,
    ) -> Vec<CacheEntryKey> {
        self.inner.increment_weight(item, delta, max_weight)
    }
    fn remove(&self, item: CacheEntryKeyRef<'_>) {
        self.inner.remove(item)
    }
    fn access(&self, item: &CacheEntryKey, size: usize, fresh_until: SystemTime) -> bool {
        self.inner.access(item, size, fresh_until)
    }
    fn peek(&self, item: &CacheEntryKey) -> bool {
        self.inner.peek(item)
    }
    async fn save(&self, dir_path: &str) -> pingora_error::Result<()> {
        self.inner.save(dir_path).await
    }
    async fn load(&self, dir_path: &str) -> pingora_error::Result<()> {
        self.inner.load(dir_path).await
    }
}
