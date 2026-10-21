// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Builds and spawns `test/fixture-proxy` against a temp cache directory,
//! pointed at whatever upstream the test needs (cache-tests' origin or
//! `durable-cache-mock-origin`).

use crate::process::ManagedChild;
use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

pub struct FixtureProxyHandle {
    pub proxy: ManagedChild,
    pub listen_addr: SocketAddr,
}

pub struct FixtureProxyOptions<'a> {
    pub cache_dir: &'a Path,
    pub upstream_addr: SocketAddr,
    pub listen_addr: SocketAddr,
    /// Extra pingora flags, e.g. `["-u"]` for a hot-upgrade incoming process.
    pub extra_args: Vec<String>,
}

impl FixtureProxyHandle {
    pub fn start(opts: FixtureProxyOptions<'_>) -> Result<Self> {
        let listen_addr = opts.listen_addr;
        let proxy = Self::spawn_without_waiting(opts)?;
        ManagedChild::wait_until_listening(listen_addr, Duration::from_secs(10))?;

        Ok(FixtureProxyHandle { proxy, listen_addr })
    }

    /// Spawn without waiting for the listener to come up -- for the
    /// hot-upgrade incoming process (`hotupgrade.rs`), which only starts
    /// accepting on `--listen-addr` once it has received the outgoing
    /// process's file descriptor over the upgrade socket; polling that
    /// address before then would just time out for the wrong reason.
    pub fn spawn_without_waiting(opts: FixtureProxyOptions<'_>) -> Result<ManagedChild> {
        let run = escargot::CargoBuild::new()
            .bin("fixture-proxy")
            .manifest_path(workspace_manifest_path())
            .run()
            .context("failed to build fixture-proxy")?;

        let mut command: Command = run.command();
        command
            .arg("--listen-addr")
            .arg(opts.listen_addr.to_string())
            .arg("--upstream-addr")
            .arg(opts.upstream_addr.to_string())
            .arg("--cache-dir")
            .arg(opts.cache_dir)
            .args(&opts.extra_args);

        ManagedChild::spawn(command)
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.listen_addr)
    }
}

fn workspace_manifest_path() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR here is test/harness/; the workspace root is two
    // levels up.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test/fixture-proxy/Cargo.toml")
}
