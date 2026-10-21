// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Two-process hot-upgrade harness for TC-COMPAT.HOTUPGRADE-01.
//!
//! Real Pingora hot upgrade hands off an *already-bound, already-listening*
//! TCP socket from an old process to a new one over a Unix domain socket
//! (`ServerConf::upgrade_sock`), so both processes share the exact same
//! `--listen-addr` throughout -- unlike this file's earlier scaffold, which
//! assumed two different addresses. The actual sequence, traced from the
//! `pingora-core` source
//! (`server/bootstrap_services.rs`/`server/transfer_fd/mod.rs`):
//!
//! 1. The *incoming* process (started with `-u`/`--upgrade`) blocks inside
//!    `server.bootstrap()`, binding a listener on `upgrade_sock` and
//!    waiting (up to ~5s, retried) to receive the listening fd.
//! 2. `SIGQUIT` to the *outgoing* process makes it connect to that same
//!    `upgrade_sock` and send its fd over (also retried, so exact timing
//!    between steps 1 and 2 isn't critical), then drain in-flight
//!    requests and exit.
//! 3. The incoming process, now holding the live fd, proceeds to
//!    `run_forever()` and starts serving on it -- no new bind, zero
//!    downtime.
//!
//! `fixture-proxy`'s `main.rs` derives `upgrade_sock` (and `pid_file`) from
//! `--cache-dir`, so two processes given the same cache directory already
//! agree on the handoff path with no extra flag needed.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::fixture_proxy::{FixtureProxyHandle, FixtureProxyOptions};
use crate::process::ManagedChild;

/// After handing its listening fd over on `SIGQUIT`, a Pingora process
/// does not exit once its connections drain: it sleeps a hardcoded 5s
/// (`CLOSE_TIMEOUT`), then `grace_period_seconds` (default 300s), then waits
/// up to `graceful_shutdown_timeout_seconds` (default 5s) for its runtimes
/// to drain -- a bound, not a further sleep, since Pingora passes it to
/// `Runtime::shutdown_timeout`. Shrinking the two configurable steps lets
/// the outgoing process exit within a test's patience (~7s).
fn short_graceful_exit_args() -> Vec<String> {
    [
        "--grace-period-seconds",
        "1",
        "--graceful-shutdown-timeout-seconds",
        "1",
    ]
    .map(String::from)
    .to_vec()
}

pub struct HotUpgradePair {
    /// The process serving traffic before the upgrade; still alive (and
    /// possibly still serving, if `complete_upgrade` hasn't run yet) until
    /// signalled.
    pub outgoing: FixtureProxyHandle,
    /// The process that will take over; spawned already, but not
    /// necessarily serving yet -- it's still waiting on the fd handoff
    /// until `complete_upgrade` signals the outgoing process to send it.
    pub incoming: ManagedChild,
    pub listen_addr: SocketAddr,
}

impl HotUpgradePair {
    /// Start the outgoing process (bound and serving normally), then spawn
    /// the incoming one already in `--upgrade` mode, both sharing
    /// `cache_dir` (and therefore `upgrade_sock`) and `listen_addr`. The
    /// incoming process is left mid-bootstrap, blocked waiting for the
    /// handoff -- call `complete_upgrade` to trigger it.
    pub fn start(
        cache_dir: &Path,
        upstream_addr: SocketAddr,
        listen_addr: SocketAddr,
    ) -> Result<Self> {
        let outgoing = FixtureProxyHandle::start(FixtureProxyOptions {
            cache_dir,
            upstream_addr,
            listen_addr,
            extra_args: short_graceful_exit_args(),
        })?;

        let mut incoming_args = short_graceful_exit_args();
        incoming_args.push("--upgrade".to_string());
        let incoming = FixtureProxyHandle::spawn_without_waiting(FixtureProxyOptions {
            cache_dir,
            upstream_addr,
            listen_addr,
            extra_args: incoming_args,
        })?;

        Ok(HotUpgradePair {
            outgoing,
            incoming,
            listen_addr,
        })
    }

    /// Signal the outgoing process to hand off its listening socket and
    /// drain, then confirm the incoming process has taken over and is
    /// serving on the same address.
    pub fn complete_upgrade(&mut self, timeout: Duration) -> Result<()> {
        self.outgoing.proxy.quit()?;
        self.outgoing.proxy.wait(timeout)?;
        ManagedChild::wait_until_listening(self.listen_addr, timeout)?;
        Ok(())
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.listen_addr)
    }
}
