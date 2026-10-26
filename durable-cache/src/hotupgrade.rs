// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The hot-upgrade helper (FDS-COMPAT.READONLY-01): the two halves a hot
//! upgrade needs, installed on a Pingora server.
//!
//! - **Outgoing side.** The upgrade watcher subscribes to the server's
//!   execution-phase broadcast and calls [`FsCache::set_read_only`] when the
//!   phase reaches `GracefulUpgradeTransferringFds`. From then on the
//!   backend publishes nothing, refuses purges, expiries and metadata
//!   updates, and keeps serving reads, while the process drains its
//!   sessions. A graceful *terminate* announces `GracefulTerminate` instead
//!   and leaves the backend writable: no incoming process takes over the
//!   cache, so stores that complete while sessions drain should still be
//!   published.
//! - **Incoming side.** The recovery service runs [`crate::recover`] as a
//!   background service. In a process that takes over from an outgoing one,
//!   it starts only once the server holds the listening sockets and a
//!   further `CLOSE_TIMEOUT` (5 s) has elapsed, by which time the outgoing
//!   process's backend is read-only and everything it published is on disk.
//!   On any other start, with no outgoing process, it starts at once.
//!
//! [`install`] adds both to the server, and makes recovery wait for the
//! server's bootstrap service when it has one. An integrator that installs
//! neither gets two independent writers during the upgrade window, which
//! FRE-COMPAT.EVICTION-01 rules out.

use std::time::Duration;

use async_trait::async_trait;
use pingora_cache::eviction::EvictionManager;
use pingora_core::server::{ExecutionPhase, Server, ShutdownWatch};
use pingora_core::services::background::{
    background_service, BackgroundService, GenBackgroundService,
};
use pingora_core::services::ServiceHandle;
use tokio::sync::{broadcast, Mutex};

use crate::{recovery, FsCache};

// IMP-COMPAT.READONLY-01: satisfy FDS-COMPAT.READONLY-01

/// How long the incoming process of a hot upgrade waits, once it holds the
/// listening sockets, before its restart scan: until the outgoing process's
/// backend is read-only (ADR-COMPAT.READONLY-01). The outgoing process
/// starts that switch when its server announces the upgrade, before it
/// sends the sockets, and `set_read_only` then waits only for the changes
/// in progress, each of which holds the read-only gate for a rename, an
/// unlink or two `pwrite`s, never an `fdatasync`. The switch takes
/// milliseconds, and the wait leaves ample margin. The figure is the
/// `CLOSE_TIMEOUT` of `pingora-core`, which keeps it private: how long both
/// processes accept connections after the hand-over.
pub(crate) const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Makes the backend read-only when the server announces the upgrade
/// transfer.
pub(crate) struct UpgradeWatcher {
    cache: &'static FsCache,
    /// Subscribed when the watcher is built, so no phase announced before
    /// the service starts can be missed. `start` gets only `&self`, so it
    /// reaches the receiver through a lock, which it holds for as long as
    /// it watches: tokio's, which no panic can poison.
    phases: Mutex<broadcast::Receiver<ExecutionPhase>>,
}

#[async_trait]
impl BackgroundService for UpgradeWatcher {
    async fn start(&self, _shutdown: ShutdownWatch) {
        let mut phases = self.phases.lock().await;
        loop {
            match phases.recv().await {
                Ok(ExecutionPhase::GracefulUpgradeTransferringFds) => {
                    self.cache.set_read_only();
                    return;
                }
                Ok(_) => continue,
                // This subscriber fell behind: a missed phase may have been
                // the upgrade, and leaving the backend writable is the
                // unsafe direction, so treat a lag as the signal it may
                // have carried.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    log::warn!(
                        "missed {n} execution phase(s); \
                         making the backend read-only to be safe"
                    );
                    self.cache.set_read_only();
                    return;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    }
}

/// Runs Eviction Recovery once, after `delay`, unless the server starts
/// shutting down first. A shutdown during the scan stops it at its next
/// entry.
pub(crate) struct RecoveryService {
    cache: &'static FsCache,
    eviction: &'static (dyn EvictionManager + Send + Sync),
    delay: Duration,
}

#[async_trait]
impl BackgroundService for RecoveryService {
    async fn start(&self, mut shutdown: ShutdownWatch) {
        // On the incoming side of a hot upgrade this waits out the window in
        // which the outgoing process may still publish; on any other start
        // there is nothing to wait for (`recovery_delay`). A server that
        // starts shutting down meanwhile is spared the scan: the eviction
        // manager it would fill is about to be discarded.
        if tokio::time::timeout(self.delay, shutting_down(&mut shutdown))
            .await
            .is_ok()
        {
            log::info!("restart scan not started, the server shutting down");
            return;
        }
        // The scan logs its own summary line, which says whether it
        // completed or stopped, and why.
        let shutting_down = || *shutdown.borrow();
        if let Err(e) = recovery::recover_until(self.cache, self.eviction, &shutting_down).await {
            log::error!("Eviction Recovery failed: {e}");
        }
    }
}

/// Resolves once the server starts shutting down.
async fn shutting_down(shutdown: &mut ShutdownWatch) {
    // An error means the server dropped the watch, which it does only as it
    // goes: as good as shutting down.
    let _ = shutdown.wait_for(|&shutting_down| shutting_down).await;
}

/// The two services' names, which also name their runtimes' threads. Linux
/// keeps a thread's name in `TASK_COMM_LEN` (16) bytes, its terminating NUL
/// among them: 15 characters, the `BG ` prefix Pingora adds included, so
/// the two differ within their first 12.
const WATCHER_NAME: &str = "upgrade watcher";
const RECOVERY_NAME: &str = "eviction recovery";

/// The outgoing half: a service that makes the backend read-only when the
/// upgrade signal arrives.
pub(crate) fn read_only_on_upgrade(
    server: &Server,
    cache: &'static FsCache,
) -> GenBackgroundService<UpgradeWatcher> {
    background_service(
        WATCHER_NAME,
        UpgradeWatcher {
            cache,
            phases: Mutex::new(server.watch_execution_phase()),
        },
    )
}

/// The incoming half: a service that runs Eviction Recovery `delay` after it
/// starts. [`install`] passes [`recovery_delay`]'s; tests pass their own.
pub(crate) fn recovery_service(
    cache: &'static FsCache,
    eviction: &'static (dyn EvictionManager + Send + Sync),
    delay: Duration,
) -> GenBackgroundService<RecoveryService> {
    background_service(
        RECOVERY_NAME,
        RecoveryService {
            cache,
            eviction,
            delay,
        },
    )
}

/// Both halves at once, added to `server`.
///
/// Recovery's scan must not start before the server has taken over the
/// listening sockets. A direct [`Server::bootstrap`] call takes them over
/// before any service starts: pass `None` for `bootstrap`. A server that
/// bootstraps as a service instead must pass the handle
/// [`Server::bootstrap_as_a_service`] returned, and recovery then waits
/// for that service to report ready, which it does once it holds the
/// sockets.
///
/// ```no_run
/// # use pingora_core::prelude::Server;
/// # use pingora_cache::eviction::simple_lru::Manager;
/// # use durable_cache::{FsCache, FsCacheConfig};
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let mut server = Server::new(None)?;
/// server.bootstrap();
/// let config = FsCacheConfig::new("/var/cache/pingora", None);
/// let cache: &'static FsCache = Box::leak(Box::new(FsCache::new(config)?));
/// let eviction: &'static Manager = Box::leak(Box::new(Manager::new(usize::MAX)));
///
/// durable_cache::hotupgrade::install(&mut server, cache, eviction, None);
/// # Ok(())
/// # }
/// ```
pub fn install(
    server: &mut Server,
    cache: &'static FsCache,
    eviction: &'static (dyn EvictionManager + Send + Sync),
    bootstrap: Option<&ServiceHandle>,
) {
    let watcher = read_only_on_upgrade(server, cache);
    server.add_service(watcher);
    let delay = recovery_delay(server);
    let recovery = server.add_service(recovery_service(cache, eviction, delay));
    if let Some(bootstrap) = bootstrap {
        recovery.add_dependency(bootstrap);
    }
}

/// How long recovery waits before its scan. A process that takes the
/// listening sockets over from an outgoing one, started with the Pingora
/// upgrade flag, waits the close timeout, by when the outgoing process's
/// backend is read-only. Any other has no outgoing process to wait for.
fn recovery_delay(server: &Server) -> Duration {
    let upgrading = server.options.as_ref().is_some_and(|opt| opt.upgrade);
    if upgrading {
        CLOSE_TIMEOUT
    } else {
        Duration::ZERO
    }
}

#[cfg(test)]
mod tests {
    use pingora_cache::eviction::simple_lru::Manager;
    use pingora_core::services::ServiceWithDependents;

    use super::*;
    use crate::FsCacheConfig;

    /// How much of a thread's name Linux keeps: `TASK_COMM_LEN` (16) bytes,
    /// less the terminating NUL.
    const THREAD_NAME_LEN: usize = 15;

    fn leaked_cache() -> (&'static FsCache, tempfile::TempDir) {
        // In the system's temporary directory, maybe a tmpfs, without
        // non-blocking reads: this test is not about reads.
        let tmp = tempfile::tempdir().expect("a cache directory");
        let config = FsCacheConfig::new(tmp.path(), None).with_page_cache_reads(false);
        let cache = FsCache::new(config).expect("a cache");
        (Box::leak(Box::new(cache)), tmp)
    }

    /// The two services' threads have distinct names: Linux keeps
    /// [`THREAD_NAME_LEN`] characters of a thread's name, and the services'
    /// names differ within them.
    #[test]
    fn the_services_threads_have_distinct_names() {
        let (cache, _tmp) = leaked_cache();
        let eviction: &'static Manager = Box::leak(Box::new(Manager::new(1)));
        let server = Server::new(None).expect("a server");
        let watcher = read_only_on_upgrade(&server, cache);
        let recovery = recovery_service(cache, eviction, Duration::ZERO);
        let thread_name = |service: &dyn ServiceWithDependents| -> String {
            service.name().chars().take(THREAD_NAME_LEN).collect()
        };
        assert_ne!(thread_name(&watcher), thread_name(&recovery));
    }

    /// A watcher that fell behind the broadcast takes the phases it missed
    /// for the upgrade's, and makes the backend read-only.
    #[tokio::test]
    async fn a_lagging_watcher_makes_the_backend_read_only() {
        let (cache, _tmp) = leaked_cache();
        // Room for one phase: the second pushes the first out unread.
        let (phases, receiver) = broadcast::channel(1);
        phases.send(ExecutionPhase::Running).expect("a subscriber");
        phases
            .send(ExecutionPhase::GracefulTerminate)
            .expect("a subscriber");
        let watcher = UpgradeWatcher {
            cache,
            phases: Mutex::new(receiver),
        };
        let (_shutdown, watch) = tokio::sync::watch::channel(false);
        tokio::time::timeout(Duration::from_secs(5), watcher.start(watch))
            .await
            .expect("the watcher stops watching");
        assert!(cache.is_read_only());
    }
}
