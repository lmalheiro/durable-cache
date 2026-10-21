// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Generic child-process control: spawn, wait-until-ready (TCP connect
//! poll), `SIGTERM`/`SIGKILL`/`SIGQUIT`, and `kill(pid, 0)` liveness
//! checks — the primitives every Group D scenario needs (crash
//! simulation, hot-upgrade drain, restart-and-reconstruct).

use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;

/// An ephemeral localhost port, free at the moment of the call. Inherently
/// racy against another process grabbing it first (the listener is
/// released immediately) -- acceptable for spawning a short-lived test
/// fixture, same tradeoff `mock-origin`'s own `bind("127.0.0.1:0")` makes.
pub fn free_port() -> Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").context("failed to bind an ephemeral port")?;
    listener.local_addr().context("failed to read local_addr")
}

pub struct ManagedChild {
    pub child: Child,
}

impl ManagedChild {
    pub fn spawn(mut command: Command) -> Result<Self> {
        let child = command.spawn().context("failed to spawn child process")?;
        Ok(ManagedChild { child })
    }

    pub fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Poll a TCP connect to `addr` until it succeeds or `timeout` elapses
    /// — used to wait for `fixture-proxy`'s listener to come up.
    pub fn wait_until_listening(addr: SocketAddr, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(anyhow!(
                    "timed out waiting for {addr} to accept connections"
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn signal(&self, sig: Signal) -> Result<()> {
        kill(Pid::from_raw(self.pid() as i32), sig).context("failed to signal child")
    }

    /// Graceful shutdown request.
    pub fn term(&self) -> Result<()> {
        self.signal(Signal::SIGTERM)
    }

    /// The Pingora hot-upgrade drain signal (distinct from `SIGTERM`).
    pub fn quit(&self) -> Result<()> {
        self.signal(Signal::SIGQUIT)
    }

    /// Crash simulation — no cleanup path runs.
    pub fn kill_immediately(&self) -> Result<()> {
        self.signal(Signal::SIGKILL)
    }

    /// Whether the process, or its unreaped zombie, still exists.
    pub fn is_alive(&self) -> bool {
        kill(Pid::from_raw(self.pid() as i32), None).is_ok()
    }

    pub fn wait(&mut self, timeout: Duration) -> Result<Option<std::process::ExitStatus>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for ManagedChild {
    fn drop(&mut self) {
        // Best-effort: don't leak child processes across test runs if a
        // test panics before an explicit shutdown.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
