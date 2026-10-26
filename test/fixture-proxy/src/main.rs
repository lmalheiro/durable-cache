// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! `fixture-proxy`: the disposable Pingora proxy that hosts `FsCache`
//! (`durable-cache`) for the test suite. Not a deliverable — see
//! `docs/durable-cache.pdf` Appendix B and the plan's §2.
//!
//! Constructs `FsCache` (triggering FDS-LAYOUT.STARTUP-01's construction-
//! time shard scan) and a stock `simple_lru` eviction manager, then runs
//! Eviction Recovery either blocking (before `run_forever`) or in the
//! background, per `--recovery-mode` — both modes are required, since
//! TC-EVICTION.RECONSTRUCT-01/TC-COMPAT.EVICTION-01 must hold regardless of
//! which one a given run uses.

mod proxy;

use std::path::PathBuf;

use clap::Parser;
use durable_cache::{hotupgrade, recover, test_support, Durability, FsCache, FsCacheConfig};
use pingora_cache::eviction::simple_lru::Manager as EvictionManager;
use pingora_cache::MemCache;
use pingora_core::prelude::{Opt, Server};
use pingora_core::server::configuration::ServerConf;
use pingora_proxy::http_proxy_service;
use proxy::FixtureProxy;

/// Which `Storage` implementation the fixture hosts. `MemCache` is the
/// reference HLR-COMPLIANCE.RFC-01 measures against: the same proxy, the
/// same origin, only the backend differs.
#[derive(Copy, Clone, Debug, PartialEq, Eq, clap::ValueEnum)]
enum StorageBackend {
    /// This project's file-system backend.
    Fs,
    /// The stock in-memory backend of `pingora-cache`.
    Memcache,
}

/// `durable_cache::Durability`, as the command line names it.
#[derive(Copy, Clone, Debug, clap::ValueEnum)]
enum DurabilityMode {
    /// Synchronise each store before publishing it: the default.
    Strict,
    /// Publish stores without synchronising them first.
    NoStoreSync,
}

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
enum RecoveryMode {
    /// Run Eviction Recovery to completion before serving any traffic.
    Block,
    /// Start serving immediately, with the whole hot-upgrade helper
    /// installed as an integrator installs it: Eviction Recovery runs
    /// concurrently, when the helper starts it.
    Background,
}

#[derive(Parser, Debug)]
struct Cli {
    /// Address to listen on, e.g. 127.0.0.1:6190.
    #[arg(long, default_value = "127.0.0.1:6190")]
    listen_addr: String,

    /// Upstream address to proxy to (cache-tests' origin, or mock-origin).
    #[arg(long)]
    upstream_addr: String,

    /// Whether the upstream connection should use TLS.
    #[arg(long, default_value_t = false)]
    upstream_tls: bool,

    /// SNI to present if `--upstream-tls` is set.
    #[arg(long, default_value = "")]
    upstream_sni: String,

    /// Root of the on-disk cache tree.
    #[arg(long)]
    cache_dir: PathBuf,

    /// Which storage backend to host (TC-COMPLIANCE.RFC-01 runs the suite
    /// against each in turn).
    #[arg(long, value_enum, default_value_t = StorageBackend::Fs)]
    storage: StorageBackend,

    /// `FsCacheConfig::with_durability`: what a store does to make itself
    /// durable before it is published (ADR-PERSISTENCE.DURABILITY-01).
    #[arg(long, value_enum, default_value_t = DurabilityMode::Strict)]
    durability: DurabilityMode,

    /// Whether Eviction Recovery blocks startup or runs in the background.
    #[arg(long, value_enum, default_value_t = RecoveryMode::Block)]
    recovery_mode: RecoveryMode,

    /// `ServerConf::grace_period_seconds`: how long a gracefully exiting
    /// process sleeps before shutting its runtimes down. Unset keeps
    /// the Pingora default of 300s, which a hot-upgrade test cannot wait
    /// out -- the sleep is unconditional, not cut short once connections
    /// drain.
    #[arg(long)]
    grace_period_seconds: Option<u64>,

    /// `ServerConf::graceful_shutdown_timeout_seconds`: the runtime
    /// shutdown timeout that follows the grace period (Pingora default 5s).
    #[arg(long)]
    graceful_shutdown_timeout_seconds: Option<u64>,

    /// `ServerConf::threads`: worker threads for the proxy service.
    /// The Pingora default is 1, which is what a deployment gets unless
    /// it says otherwise; a diagnosis of executor-blocking behaviour needs
    /// to be able to vary it.
    #[arg(long, default_value_t = 1)]
    threads: usize,

    /// `ServerConf::max_blocking_threads`: the most threads the proxy
    /// service's blocking pool may run, where the backend does its file
    /// I/O (FDS-STORAGE.EXECUTOR-01). Unset keeps Tokio's 512; a benchmark
    /// varies it to see what the pool's size costs.
    #[arg(long)]
    max_blocking_threads: Option<usize>,

    /// `FsCacheConfig::with_noatime`: read records without updating their
    /// access time.
    #[arg(long)]
    noatime: bool,

    /// `FsCacheConfig::with_page_cache_reads(false)`: serve every hit from
    /// the blocking pool, as on a file system without non-blocking reads.
    #[arg(long)]
    no_page_cache_reads: bool,

    /// `pingora_core::server::configuration::Opt`'s own flags, flattened
    /// in: `-u/--upgrade`, `-d/--daemon`, `-c/--conf`, ... (needed as-is
    /// for the hot-upgrade scenario, TC-COMPAT.HOTUPGRADE-01 — M9).
    #[command(flatten)]
    pingora: Opt,
}

fn main() {
    env_logger::init();
    let cli = Cli::parse();

    // pid_file/upgrade_sock derived from --cache-dir, not the process
    // default (`/tmp/pingora*`): a hot-upgrade pair always shares one
    // cache directory, so this makes the two processes agree on the same
    // upgrade_sock automatically -- exactly what the handoff needs -- with
    // no extra flag, and keeps concurrent test runs (each with their own
    // temp cache dir) from colliding on a shared /tmp path.
    let conf = ServerConf {
        pid_file: cli
            .cache_dir
            .join("fixture-proxy.pid")
            .display()
            .to_string(),
        upgrade_sock: cli
            .cache_dir
            .join("fixture-proxy-upgrade.sock")
            .display()
            .to_string(),
        grace_period_seconds: cli.grace_period_seconds,
        graceful_shutdown_timeout_seconds: cli.graceful_shutdown_timeout_seconds,
        threads: cli.threads,
        max_blocking_threads: cli.max_blocking_threads,
        ..Default::default()
    };
    let mut server = Server::new_with_opt_and_conf(Some(cli.pingora), conf);
    server.bootstrap();

    if cli.storage == StorageBackend::Memcache {
        // `MemCache`, the in-memory backend of `pingora-cache`: no cache
        // directory, no recovery, no read-only switch — it holds nothing
        // across a process, which is exactly the behaviour
        // TC-COMPLIANCE.RFC-01 compares against within one run.
        let mem_cache: &'static MemCache = Box::leak(Box::new(MemCache::new()));
        let eviction: &'static EvictionManager =
            Box::leak(Box::new(EvictionManager::new(usize::MAX)));
        let fixture = FixtureProxy {
            upstream_addr: cli.upstream_addr,
            upstream_tls: cli.upstream_tls,
            upstream_sni: cli.upstream_sni,
            cache: mem_cache,
            eviction,
        };
        let mut service = http_proxy_service(&server.configuration, fixture);
        service.add_tcp(&cli.listen_addr);
        server.add_service(service);
        server.run_forever();
    }

    let durability = match cli.durability {
        DurabilityMode::Strict => Durability::Strict,
        DurabilityMode::NoStoreSync => Durability::NoStoreSync,
    };
    let config = FsCacheConfig::new(&cli.cache_dir, None)
        .with_durability(durability)
        .with_noatime(cli.noatime)
        .with_page_cache_reads(!cli.no_page_cache_reads);
    let fs_cache = FsCache::new(config)
        .expect("failed to construct FsCache (FDS-LAYOUT.STARTUP-01 shard scan)");
    let fs_cache: &'static FsCache = Box::leak(Box::new(fs_cache));

    let eviction: &'static EvictionManager = Box::leak(Box::new(EvictionManager::new(usize::MAX)));

    match cli.recovery_mode {
        RecoveryMode::Block => {
            // The outgoing half of the hot-upgrade helper alone: it makes
            // the backend read-only when this process receives the upgrade
            // signal (FDS-COMPAT.READONLY-01), which TC-COMPAT.HOTUPGRADE-01
            // exercises, while the recovery runs here.
            server.add_service(test_support::read_only_on_upgrade(&server, fs_cache));
            // A dedicated runtime for the recovery step: `main` itself is
            // sync (as the Pingora examples are — `Server::run_forever`
            // manages its own runtime internally), so an async
            // `recover` call needs one of its own.
            let runtime = tokio::runtime::Runtime::new().expect("failed to build recovery runtime");
            let report = runtime
                .block_on(recover(fs_cache, eviction))
                .expect("Eviction Recovery failed");
            log::info!("Eviction Recovery complete (blocking): {report:?}");
        }
        RecoveryMode::Background => {
            hotupgrade::install(&mut server, fs_cache, eviction, None);
        }
    }

    let fixture = FixtureProxy {
        upstream_addr: cli.upstream_addr,
        upstream_tls: cli.upstream_tls,
        upstream_sni: cli.upstream_sni,
        cache: fs_cache,
        eviction,
    };
    let mut service = http_proxy_service(&server.configuration, fixture);
    service.add_tcp(&cli.listen_addr);
    server.add_service(service);

    server.run_forever();
}
