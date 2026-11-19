// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! `cargo xtask bench`: how far a Pingora proxy on durable-cache is from
//! nginx and its `proxy_cache`, measured locally (README, "Benchmark").
//!
//! Three caching reverse proxies stand in front of one origin, an nginx
//! serving fixed bodies: nginx with `proxy_cache`, and fixture-proxy with
//! durable-cache and with the in-memory `MemCache` of pingora-cache. The last
//! separates the cost of the storage from that of the proxy around it. The
//! servers get the same worker count and the same CPUs, and the load
//! generator, oha, gets the others. Every round starts each server with an
//! empty cache, in a different order each round, and the report gives the
//! medians.
//!
//! It is the harness of TC-PERFORMANCE.HITRATE-01, with durable-cache in its
//! default configuration, and of TC-PERFORMANCE.STORERATE-01, with
//! `--durability no-store-sync`: each compares durable-cache's median rates
//! with those of `MemCache`.
//!
//! Beyond the test cases, it measures what the work on the write path needs
//! (the spec's Appendix C, "What a Later Version Should Measure"): the
//! processor time each request costs the server and the most threads it ran,
//! stores of 1 MiB bodies, hits during a burst of stores with the host's
//! dirty data, hits on objects the page cache does not hold, and the system
//! calls durable-cache makes per request, which strace counts.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use crate::{free_port, target_dir, wait_until_listening, workspace_root, ChildGuard};

// IMP-TC.PERFORMANCE.HITRATE-01: satisfy TC-PERFORMANCE.HITRATE-01
// IMP-TC.PERFORMANCE.STORERATE-01: satisfy TC-PERFORMANCE.STORERATE-01

/// The cache-hit scenarios: body sizes, each over `HIT_OBJECTS` objects.
const HIT_SIZES: [&str; 3] = ["1k", "64k", "1m"];
/// How many objects a hit scenario spreads its requests over, as the URL
/// pattern `[0-9]{2}` names them.
const HIT_OBJECTS: usize = 100;
/// Requests that warm a hit scenario, one at a time: enough to fetch each of
/// the `HIT_OBJECTS` objects at least once, short of a chance of about one in
/// five million. An object it missed would show as origin requests in the
/// report.
const WARM_REQUESTS: usize = 2_000;
/// The cache-fill scenarios: body size and request count. Every request
/// names a new URL, so each is a miss and a store.
const FILLS: [(&str, usize); 3] = [("4k", 20_000), ("64k", 10_000), ("1m", 1_000)];
/// The fill sizes TC-PERFORMANCE.STORERATE-01 measures; the others are for
/// the work on the write path.
const STORERATE_SIZES: [&str; 2] = ["4k", "64k"];
/// The burst: `BURST_HIT_SIZE` hits, over the objects of that hit scenario,
/// measured for `BURST_HITS` while `BURST_FILL_CONNECTIONS` connections
/// store `BURST_FILLS` new `BURST_FILL_SIZE` bodies. The stores start
/// `BURST_LEAD` before the hits, so that dirty data has built up, and are
/// counted rather than timed, to bound what a run writes: the hits are
/// measured for less time than the stores take, and the report gives that
/// time, which shows whether they covered the hits.
const BURST_HIT_SIZE: &str = "1k";
const BURST_HITS: Duration = Duration::from_secs(2);
const BURST_FILL_SIZE: &str = "1m";
const BURST_FILLS: usize = 4_000;
const BURST_FILL_CONNECTIONS: usize = 16;
const BURST_LEAD: Duration = Duration::from_secs(1);
/// The cold reads: `COLD_OBJECTS` objects of `COLD_SIZE`, stored, evicted
/// from the page cache, and each requested once over `COLD_CONNECTIONS`
/// connections; then requested once more, cached.
const COLD_SIZE: &str = "1m";
const COLD_OBJECTS: usize = 400;
const COLD_CONNECTIONS: usize = 16;
/// The census: per case, the requests it counts and their connections.
const CENSUS_CASES: [(&str, &str, usize); 4] = [
    ("hit-1k", "/1k/[0-9]{2}", 2_000),
    ("fill-4k", "/4k/n[0-9]{12}", 500),
    ("fill-64k", "/64k/n[0-9]{12}", 500),
    ("fill-1m", "/1m/n[0-9]{12}", 200),
];
const CENSUS_CONNECTIONS: usize = 8;
/// The system calls the census reports, each a column, with the calls of
/// the same kind it adds up.
const CENSUS_CALLS: [(&str, &[&str]); 8] = [
    ("pwrite", &["pwrite64", "pwritev", "pwritev2"]),
    ("pread", &["pread64", "preadv", "preadv2"]),
    ("open", &["openat", "openat2", "open"]),
    ("fdatasync", &["fdatasync", "fsync"]),
    ("rename", &["rename", "renameat", "renameat2"]),
    ("close", &["close"]),
    ("futex", &["futex"]),
    ("all", &["total"]),
];
/// How often the probe samples the server's threads and the dirty data.
const SAMPLE_PERIOD: Duration = Duration::from_millis(100);

/// The scenarios `--only` selects; all of them unless it names some.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, clap::ValueEnum)]
pub enum Scenario {
    /// Cache hits on 1 KiB, 64 KiB and 1 MiB bodies.
    Hits,
    /// Filling the cache with new URLs of 4 KiB, 64 KiB and 1 MiB.
    Fills,
    /// 1 KiB hits while 1 MiB stores run, with the host's dirty data.
    Burst,
    /// 1 MiB hits on objects the page cache does not hold, then cached.
    Cold,
    /// The system calls durable-cache makes per request, which strace counts.
    Census,
}

pub struct Options {
    pub nginx: PathBuf,
    pub oha: PathBuf,
    pub work_dir: PathBuf,
    pub threads: Option<usize>,
    pub server_cpus: Option<String>,
    pub load_cpus: Option<String>,
    pub duration: u64,
    pub connections: usize,
    pub rounds: usize,
    /// fixture-proxy's `--durability` for durable-cache.
    pub durability: String,
    pub scenarios: BTreeSet<Scenario>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Server {
    Nginx,
    DurableCache,
    MemCache,
}

impl Server {
    const ALL: [Server; 3] = [Server::Nginx, Server::DurableCache, Server::MemCache];

    fn label(self) -> &'static str {
        match self {
            Server::Nginx => "nginx proxy_cache",
            Server::DurableCache => "Pingora + durable-cache",
            Server::MemCache => "Pingora + MemCache",
        }
    }

    fn slug(self) -> &'static str {
        match self {
            Server::Nginx => "nginx",
            Server::DurableCache => "durable-cache",
            Server::MemCache => "memcache",
        }
    }

    /// Whether the server keeps its cache on disk: the burst and the cold
    /// reads measure only those.
    fn on_disk(self) -> bool {
        self != Server::MemCache
    }
}

/// One measured run: the load generator's figures and the server's, which
/// the probe read from /proc.
#[derive(Clone, Default)]
struct Run {
    requests_per_sec: f64,
    p50: f64,
    p99: f64,
    errors: u64,
    non_200: u64,
    /// Requests completed, failed ones included.
    requests: u64,
    /// The run's length, in seconds.
    seconds: f64,
    /// Requests the origin saw during the run: none in a hit scenario.
    origin_requests: usize,
    /// The server's processor time per request, in microseconds.
    cpu_us: f64,
    /// The most threads the server's processes ran at once.
    threads: usize,
    /// The most dirty data and data under write-back on the host, in MiB.
    dirty_mib: f64,
    writeback_mib: f64,
}

impl Run {
    fn record(&mut self, usage: &Usage) {
        if self.requests > 0 {
            self.cpu_us = usage.cpu_seconds * 1e6 / self.requests as f64;
        }
        self.threads = usage.peaks.threads;
        self.dirty_mib = usage.peaks.dirty_kib as f64 / 1024.0;
        self.writeback_mib = usage.peaks.writeback_kib as f64 / 1024.0;
    }
}

/// A burst's figures: the hits measured, and the stores around them.
struct Burst {
    hits: Run,
    fills: Run,
}

/// The cold reads' figures: the objects evicted, then cached.
struct Cold {
    evicted: Run,
    cached: Run,
}

struct Machine {
    server_cpus: String,
    load_cpus: String,
    threads: usize,
}

/// Every figure of a benchmark, by scenario and server.
#[derive(Default)]
struct Results {
    runs: BTreeMap<(String, Server), Vec<Run>>,
    bursts: BTreeMap<Server, Vec<Burst>>,
    colds: BTreeMap<Server, Vec<Cold>>,
    census: Vec<CensusRow>,
}

/// One case of the census: the requests it made, and the calls of each
/// system call they took.
struct CensusRow {
    case: String,
    requests: u64,
    calls: BTreeMap<String, u64>,
}

pub fn bench(options: Options) -> Result<()> {
    if options.rounds == 0 {
        bail!("--rounds must be at least 1");
    }
    let root = workspace_root()?;
    let nginx_version = tool_version(&options.nginx, "-v", "nginx")?;
    let oha_version = tool_version(&options.oha, "--version", "oha")?;
    if options.scenarios.contains(&Scenario::Census) {
        tool_version(Path::new("strace"), "-V", "strace")?;
    }
    let machine = machine(&options)?;

    println!("==> cargo build --release -p fixture-proxy");
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "fixture-proxy"])
        .current_dir(&root)
        .status()
        .context("failed to invoke cargo build")?;
    if !status.success() {
        bail!("fixture-proxy build failed");
    }
    let fixture_proxy = target_dir(&root)?.join("release/fixture-proxy");

    let work = if options.work_dir.is_absolute() {
        options.work_dir.clone()
    } else {
        root.join(&options.work_dir)
    };
    if work.exists() {
        std::fs::remove_dir_all(&work)
            .with_context(|| format!("failed to empty {}", work.display()))?;
    }
    for dir in ["www", "results", "origin/tmp"] {
        std::fs::create_dir_all(work.join(dir))?;
    }
    let sizes: BTreeSet<&str> = HIT_SIZES
        .iter()
        .chain(FILLS.iter().map(|(size, _)| size))
        .chain([BURST_HIT_SIZE, BURST_FILL_SIZE, COLD_SIZE].iter())
        .copied()
        .collect();
    for size in sizes {
        write_body(&work.join("www").join(size), size)?;
    }

    let origin_addr = free_port()?;
    let origin_conf = work.join("origin/nginx.conf");
    std::fs::write(&origin_conf, origin_config(&work, origin_addr))?;
    println!("==> starting the origin (nginx) on {origin_addr}");
    let _origin = ChildGuard::spawn(
        "origin",
        pinned(&machine.load_cpus, &options.nginx)
            .arg("-p")
            .arg(work.join("origin"))
            .arg("-c")
            .arg(&origin_conf)
            .arg("-e")
            .arg(work.join("origin/error.log")),
    )?;
    wait_until_listening(origin_addr, "the origin", Duration::from_secs(10))?;
    let origin_log = work.join("origin/access.log");

    println!(
        "==> {} round(s); servers on CPUs {} with {} workers each, oha on CPUs {}",
        options.rounds, machine.server_cpus, machine.threads, machine.load_cpus
    );
    // Nothing left to write back from the setup, or from the caches of the
    // last benchmark, when the first server starts.
    sync()?;
    let mut results = Results::default();
    let wants = |scenario| options.scenarios.contains(&scenario);
    for round in 0..options.rounds {
        // A different order each round, so no server always runs first.
        let mut order = Server::ALL;
        order.rotate_left(round % Server::ALL.len());
        for server in order {
            let measures_something = wants(Scenario::Hits)
                || wants(Scenario::Fills)
                || (server.on_disk() && (wants(Scenario::Burst) || wants(Scenario::Cold)));
            if !measures_something {
                continue;
            }
            println!(
                "--> round {} of {}: {}",
                round + 1,
                options.rounds,
                server.label()
            );
            let addr = free_port()?;
            let dir = work.join(format!("round-{round}/{}", server.slug()));
            std::fs::create_dir_all(&dir)?;
            let guard = start_server(
                server,
                &options,
                &machine,
                &fixture_proxy,
                &dir,
                addr,
                origin_addr,
            )?;
            wait_until_listening(addr, server.label(), Duration::from_secs(30))?;
            let group = guard.pid();

            if wants(Scenario::Hits) {
                for size in HIT_SIZES {
                    let url = format!("http://{addr}/{size}/[0-9]{{2}}");
                    warm(&options, &machine, &url, server, size)?;
                    let before = origin_requests(&origin_log, size)?;
                    let duration = format!("{}s", options.duration);
                    let connections = options.connections.to_string();
                    let probe = Probe::start(group)?;
                    let mut run = oha(
                        &options.oha,
                        &machine.load_cpus,
                        &["-z", &duration, "-c", &connections, "-w"],
                        &url,
                    )?;
                    run.record(&probe.finish()?);
                    run.origin_requests = origin_requests(&origin_log, size)? - before;
                    save(&work, round, server, &format!("hit-{size}"), &run)?;
                    results
                        .runs
                        .entry((format!("hit-{size}"), server))
                        .or_default()
                        .push(run);
                }
            }
            if wants(Scenario::Fills) {
                for (size, requests) in FILLS {
                    let url = format!("http://{addr}/{size}/f[0-9]{{12}}");
                    let before = origin_requests(&origin_log, size)?;
                    let probe = Probe::start(group)?;
                    let mut run = oha(
                        &options.oha,
                        &machine.load_cpus,
                        &[
                            "-n",
                            &requests.to_string(),
                            "-c",
                            &options.connections.to_string(),
                        ],
                        &url,
                    )?;
                    run.record(&probe.finish()?);
                    run.origin_requests = origin_requests(&origin_log, size)? - before;
                    save(&work, round, server, &format!("fill-{size}"), &run)?;
                    results
                        .runs
                        .entry((format!("fill-{size}"), server))
                        .or_default()
                        .push(run);
                }
            }
            if server.on_disk() && wants(Scenario::Burst) {
                let burst = burst(&options, &machine, addr, group, server, &origin_log)?;
                save(&work, round, server, "burst-hits", &burst.hits)?;
                save(&work, round, server, "burst-fills", &burst.fills)?;
                results.bursts.entry(server).or_default().push(burst);
            }
            if server.on_disk() && wants(Scenario::Cold) {
                let cold = cold(addr, group, &dir.join("cache"), &origin_log)?;
                save(&work, round, server, "cold-evicted", &cold.evicted)?;
                save(&work, round, server, "cold-cached", &cold.cached)?;
                results.colds.entry(server).or_default().push(cold);
            }
            // nginx never synchronises its cache, so the kernel would write
            // it back during the next server's runs. Deleting it first
            // leaves `sync` little to write, and the work directory one
            // cache at a time.
            drop(guard);
            remove_cache(&dir.join("cache"))?;
            sync()?;
        }
    }
    if wants(Scenario::Census) {
        results.census = census(&options, &machine, &fixture_proxy, origin_addr, &work)?;
    }

    let report = report(
        &results,
        &options,
        &machine,
        &nginx_version,
        &oha_version,
        &work,
    )?;
    println!("\n{report}");
    std::fs::write(work.join("results/report.md"), &report)?;
    println!("(also in {})", work.join("results/report.md").display());
    Ok(())
}

/// Starts `server`, listening on `addr`, with its files in `dir`.
fn start_server(
    server: Server,
    options: &Options,
    machine: &Machine,
    fixture_proxy: &Path,
    dir: &Path,
    addr: SocketAddr,
    origin_addr: SocketAddr,
) -> Result<ChildGuard> {
    match server {
        Server::Nginx => {
            std::fs::create_dir_all(dir.join("tmp"))?;
            let conf = dir.join("nginx.conf");
            std::fs::write(&conf, cache_config(dir, addr, origin_addr, machine.threads))?;
            ChildGuard::spawn(
                "nginx",
                pinned(&machine.server_cpus, &options.nginx)
                    .arg("-p")
                    .arg(dir)
                    .arg("-c")
                    .arg(&conf)
                    .arg("-e")
                    .arg(dir.join("error.log")),
            )
        }
        Server::DurableCache | Server::MemCache => {
            let storage = if server == Server::DurableCache {
                "fs"
            } else {
                "memcache"
            };
            ChildGuard::spawn(
                "fixture-proxy",
                pinned(&machine.server_cpus, fixture_proxy)
                    .arg("--listen-addr")
                    .arg(addr.to_string())
                    .arg("--upstream-addr")
                    .arg(origin_addr.to_string())
                    .arg("--cache-dir")
                    .arg(dir.join("cache"))
                    .arg("--storage")
                    .arg(storage)
                    .arg("--durability")
                    .arg(&options.durability)
                    .arg("--threads")
                    .arg(machine.threads.to_string()),
            )
        }
    }
}

/// Fetches the objects of a hit scenario once each, at least, one request at
/// a time, so that the measured run finds them stored.
fn warm(options: &Options, machine: &Machine, url: &str, server: Server, size: &str) -> Result<()> {
    let warm = oha(
        &options.oha,
        &machine.load_cpus,
        &["-n", &WARM_REQUESTS.to_string(), "-c", "1"],
        url,
    )?;
    if warm.non_200 > 0 || warm.errors > 0 {
        bail!(
            "{} failed requests while warming hit-{size}",
            server.label()
        );
    }
    Ok(())
}

/// Measures `BURST_HIT_SIZE` hits while new `BURST_FILL_SIZE` bodies are
/// stored, each load on its own half of the load CPUs.
fn burst(
    options: &Options,
    machine: &Machine,
    addr: SocketAddr,
    group: u32,
    server: Server,
    origin_log: &Path,
) -> Result<Burst> {
    let hit_url = format!("http://{addr}/{BURST_HIT_SIZE}/[0-9]{{2}}");
    warm(options, machine, &hit_url, server, BURST_HIT_SIZE)?;
    let (hit_cpus, fill_cpus) = split_cpus(&machine.load_cpus);
    let fill_url = format!("http://{addr}/{BURST_FILL_SIZE}/b[0-9]{{12}}");
    let oha_path = options.oha.clone();
    let fills = std::thread::spawn(move || {
        oha(
            &oha_path,
            &fill_cpus,
            &[
                "-n",
                &BURST_FILLS.to_string(),
                "-c",
                &BURST_FILL_CONNECTIONS.to_string(),
            ],
            &fill_url,
        )
    });
    std::thread::sleep(BURST_LEAD);
    let before = origin_requests(origin_log, BURST_HIT_SIZE)?;
    let probe = Probe::start(group)?;
    let duration = format!("{}s", BURST_HITS.as_secs());
    let mut hits = oha(
        &options.oha,
        &hit_cpus,
        &[
            "-z",
            &duration,
            "-c",
            &options.connections.to_string(),
            "-w",
        ],
        &hit_url,
    )?;
    hits.record(&probe.finish()?);
    hits.origin_requests = origin_requests(origin_log, BURST_HIT_SIZE)? - before;
    let fills = fills
        .join()
        .map_err(|_| anyhow!("the burst's store load panicked"))??;
    Ok(Burst { hits, fills })
}

/// Stores `COLD_OBJECTS` objects, evicts their files from the page cache,
/// and requests each once, then once more, now cached.
fn cold(addr: SocketAddr, group: u32, cache: &Path, origin_log: &Path) -> Result<Cold> {
    let paths: Vec<String> = (0..COLD_OBJECTS)
        .map(|i| format!("/{COLD_SIZE}/c{i:04}"))
        .collect();
    let stored = fetch_each(addr, &paths, COLD_CONNECTIONS)?;
    if stored.errors + stored.non_200 > 0 {
        bail!("the cold scenario failed to store its objects");
    }
    // The last stores may still be publishing as their responses end.
    std::thread::sleep(Duration::from_secs(1));
    // Clean pages only can be dropped.
    sync()?;
    let files = evict(cache)?;
    if files < COLD_OBJECTS {
        bail!(
            "the cold scenario found {files} files under {}, fewer than its {COLD_OBJECTS} objects",
            cache.display()
        );
    }
    let measure = |what: &str| -> Result<Run> {
        let before = origin_requests(origin_log, COLD_SIZE)?;
        let probe = Probe::start(group)?;
        let mut run = fetch_each(addr, &paths, COLD_CONNECTIONS)?;
        run.record(&probe.finish()?);
        run.origin_requests = origin_requests(origin_log, COLD_SIZE)? - before;
        if run.origin_requests > 0 {
            bail!("the cold scenario's {what} reads reached the origin");
        }
        Ok(run)
    };
    let evicted = measure("evicted")?;
    let cached = measure("cached")?;
    Ok(Cold { evicted, cached })
}

/// Drops the cached pages of every file under `dir` (`POSIX_FADV_DONTNEED`):
/// how many files it found. The pages must be clean, written back first.
fn evict(dir: &Path) -> Result<usize> {
    let mut files = 0;
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let file = std::fs::File::open(entry.path())
            .with_context(|| format!("failed to open {}", entry.path().display()))?;
        nix::fcntl::posix_fadvise(
            file.as_raw_fd(),
            0,
            0,
            nix::fcntl::PosixFadviseAdvice::POSIX_FADV_DONTNEED,
        )
        .with_context(|| format!("failed to evict {}", entry.path().display()))?;
        files += 1;
    }
    Ok(files)
}

/// Counts the system calls of a durable-cache fixture-proxy, one strace
/// attachment per case, and returns, per case, the requests and the calls.
fn census(
    options: &Options,
    machine: &Machine,
    fixture_proxy: &Path,
    origin_addr: SocketAddr,
    work: &Path,
) -> Result<Vec<CensusRow>> {
    println!("--> census: durable-cache under strace");
    let dir = work.join("census");
    std::fs::create_dir_all(&dir)?;
    let addr = free_port()?;
    let guard = start_server(
        Server::DurableCache,
        options,
        machine,
        fixture_proxy,
        &dir,
        addr,
        origin_addr,
    )?;
    wait_until_listening(addr, "the census server", Duration::from_secs(30))?;
    warm(
        options,
        machine,
        &format!("http://{addr}/1k/[0-9]{{2}}"),
        Server::DurableCache,
        "1k",
    )?;
    let mut rows = Vec::new();
    for (case, path, requests) in CENSUS_CASES {
        let summary = dir.join(format!("{case}.strace"));
        // In the C locale, so that its summary's numbers use decimal points.
        let mut strace = Command::new("strace")
            .env("LC_ALL", "C")
            .args(["-f", "-c", "-o"])
            .arg(&summary)
            .arg("-p")
            .arg(guard.pid().to_string())
            .stderr(std::process::Stdio::null())
            .spawn()
            .context("failed to start strace")?;
        wait_traced(guard.pid())?;
        let run = oha(
            &options.oha,
            &machine.load_cpus,
            &[
                "-n",
                &requests.to_string(),
                "-c",
                &CENSUS_CONNECTIONS.to_string(),
            ],
            &format!("http://{addr}{path}"),
        )?;
        // strace writes its summary as it detaches, on SIGINT.
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(strace.id() as i32),
            nix::sys::signal::Signal::SIGINT,
        )?;
        strace.wait()?;
        if run.errors + run.non_200 > 0 {
            bail!("the census's {case} requests failed");
        }
        rows.push(CensusRow {
            case: case.to_owned(),
            requests: run.requests,
            calls: strace_counts(&summary)?,
        });
    }
    drop(guard);
    remove_cache(&dir.join("cache"))?;
    Ok(rows)
}

/// Waits until a tracer is attached to process `pid`.
fn wait_traced(pid: u32) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status"))?;
        let traced = status
            .lines()
            .find_map(|line| line.strip_prefix("TracerPid:"))
            .is_some_and(|tracer| tracer.trim() != "0");
        if traced {
            // Every thread is attached once the first is; a short pause
            // covers the rest.
            std::thread::sleep(Duration::from_millis(200));
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("strace did not attach to process {pid}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The calls per system call of an strace `-c` summary, its `total` row
/// included.
fn strace_counts(path: &Path) -> Result<BTreeMap<String, u64>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let mut counts = BTreeMap::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // `% time  seconds  usecs/call  calls  [errors]  syscall`
        let percent = fields.first().map_or("", |field| field.as_ref());
        if fields.len() < 5 || percent.replace(',', ".").parse::<f64>().is_err() {
            continue;
        }
        let Ok(calls) = fields[3].parse::<u64>() else {
            continue;
        };
        if let Some(name) = fields.last() {
            counts.insert((*name).to_owned(), calls);
        }
    }
    if counts.is_empty() {
        bail!("strace wrote no counts to {}", path.display());
    }
    Ok(counts)
}

/// Splits a CPU list in two halves, for two loads; both get the whole list
/// when it has fewer than two CPUs.
fn split_cpus(cpus: &str) -> (String, String) {
    let list: Vec<&str> = cpus.split(',').filter(|cpu| !cpu.is_empty()).collect();
    if list.len() < 2 {
        return (cpus.to_owned(), cpus.to_owned());
    }
    let (first, second) = list.split_at(list.len() / 2);
    (first.join(","), second.join(","))
}

/// What a server's processes used during a run, read from /proc: their
/// processor time, sampled peaks of their threads and of the host's dirty
/// data, from `start` to `finish`.
struct Probe {
    group: u32,
    start_ticks: u64,
    stop: Arc<AtomicBool>,
    sampler: JoinHandle<Peaks>,
}

#[derive(Clone, Copy, Default)]
struct Peaks {
    threads: usize,
    dirty_kib: u64,
    writeback_kib: u64,
}

struct Usage {
    cpu_seconds: f64,
    peaks: Peaks,
}

impl Probe {
    /// Starts measuring the processes of process group `group`, the server
    /// and any process it started.
    fn start(group: u32) -> Result<Self> {
        let start_ticks = group_ticks(group)?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = Arc::clone(&stop);
        let sampler = std::thread::spawn(move || {
            let mut peaks = Peaks::default();
            while !stopped.load(Ordering::Relaxed) {
                peaks.threads = peaks.threads.max(group_threads(group));
                let (dirty, writeback) = dirty_kib();
                peaks.dirty_kib = peaks.dirty_kib.max(dirty);
                peaks.writeback_kib = peaks.writeback_kib.max(writeback);
                std::thread::sleep(SAMPLE_PERIOD);
            }
            peaks
        });
        Ok(Probe {
            group,
            start_ticks,
            stop,
            sampler,
        })
    }

    fn finish(self) -> Result<Usage> {
        let end_ticks = group_ticks(self.group)?;
        self.stop.store(true, Ordering::Relaxed);
        let peaks = self
            .sampler
            .join()
            .map_err(|_| anyhow!("the /proc sampler panicked"))?;
        Ok(Usage {
            cpu_seconds: end_ticks.saturating_sub(self.start_ticks) as f64 / clock_ticks()?,
            peaks,
        })
    }
}

/// The fields of `/proc/<pid>/stat` after the command name, which may hold
/// spaces: `state` is the first, `pgrp` the third, `utime` and `stime` the
/// twelfth and thirteenth.
fn stat_fields(pid: &str) -> Option<Vec<String>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    Some(rest.split_whitespace().map(str::to_owned).collect())
}

/// The ids of the processes in process group `group`.
fn group_pids(group: u32) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.bytes().all(|b| b.is_ascii_digit()))
        .filter(|pid| {
            stat_fields(pid)
                .is_some_and(|fields| fields.get(2).is_some_and(|g| *g == group.to_string()))
        })
        .collect()
}

/// The processor time, in clock ticks, the processes of `group` have used.
fn group_ticks(group: u32) -> Result<u64> {
    let mut ticks = 0;
    for pid in group_pids(group) {
        let Some(fields) = stat_fields(&pid) else {
            continue;
        };
        for index in [11, 12] {
            ticks += fields
                .get(index)
                .and_then(|field| field.parse::<u64>().ok())
                .ok_or_else(|| anyhow!("unexpected /proc/{pid}/stat"))?;
        }
    }
    Ok(ticks)
}

/// The threads of the processes of `group`.
fn group_threads(group: u32) -> usize {
    group_pids(group)
        .iter()
        .filter_map(|pid| std::fs::read_to_string(format!("/proc/{pid}/status")).ok())
        .filter_map(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("Threads:"))
                .and_then(|n| n.trim().parse::<usize>().ok())
        })
        .sum()
}

/// The host's dirty data and data under write-back, in KiB
/// (`/proc/meminfo`).
fn dirty_kib() -> (u64, u64) {
    let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |name: &str| {
        meminfo
            .lines()
            .find_map(|line| line.strip_prefix(name))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|kib| kib.parse::<u64>().ok())
            .unwrap_or(0)
    };
    (field("Dirty:"), field("Writeback:"))
}

/// Clock ticks per second, as the processor times of /proc count them.
fn clock_ticks() -> Result<f64> {
    static TICKS: OnceLock<f64> = OnceLock::new();
    if let Some(ticks) = TICKS.get() {
        return Ok(*ticks);
    }
    let output = Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .context("failed to run getconf CLK_TCK")?;
    let ticks: f64 = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse()
        .context("getconf printed no clock tick rate")?;
    Ok(*TICKS.get_or_init(|| ticks))
}

/// Requests every path of `paths` from `addr` once, over `connections`
/// kept-alive HTTP/1.1 connections, and reports the rate and the latency as
/// oha would. oha cannot: from a list of URLs, it picks at random.
fn fetch_each(addr: SocketAddr, paths: &[String], connections: usize) -> Result<Run> {
    let next = AtomicUsize::new(0);
    let latencies = Mutex::new(Vec::with_capacity(paths.len()));
    let errors = AtomicUsize::new(0);
    let non_200 = AtomicUsize::new(0);
    let start = Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..connections {
            scope.spawn(|| {
                let mut connection: Option<BufReader<TcpStream>> = None;
                let mut mine = Vec::new();
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = paths.get(index) else {
                        break;
                    };
                    let began = Instant::now();
                    match get(&mut connection, addr, path) {
                        Ok(200) => mine.push(began.elapsed().as_secs_f64()),
                        Ok(_) => {
                            non_200.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(_) => {
                            errors.fetch_add(1, Ordering::Relaxed);
                            connection = None;
                        }
                    }
                }
                latencies
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .extend(mine);
            });
        }
    });
    let seconds = start.elapsed().as_secs_f64();
    let mut latencies = latencies
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    latencies.sort_by(f64::total_cmp);
    let percentile = |p: f64| {
        if latencies.is_empty() {
            f64::NAN
        } else {
            latencies[((latencies.len() - 1) as f64 * p).round() as usize]
        }
    };
    Ok(Run {
        requests_per_sec: paths.len() as f64 / seconds,
        p50: percentile(0.50),
        p99: percentile(0.99),
        errors: errors.into_inner() as u64,
        non_200: non_200.into_inner() as u64,
        requests: paths.len() as u64,
        seconds,
        ..Run::default()
    })
}

/// One GET of `path` on `connection`, which it opens to `addr` if needed:
/// the response's status, its body read and dropped.
fn get(connection: &mut Option<BufReader<TcpStream>>, addr: SocketAddr, path: &str) -> Result<u16> {
    if connection.is_none() {
        let stream = TcpStream::connect(addr)?;
        stream.set_nodelay(true)?;
        *connection = Some(BufReader::new(stream));
    }
    let reader = connection
        .as_mut()
        .expect("the connection was opened above");
    write!(
        reader.get_mut(),
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: */*\r\n\r\n"
    )?;
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let status: u16 = line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| anyhow!("no status line: {line:?}"))?;
    let mut length = None;
    let mut chunked = false;
    let mut close = false;
    loop {
        line.clear();
        reader.read_line(&mut line)?;
        let header = line.trim_end();
        if header.is_empty() {
            break;
        }
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse::<u64>().ok(),
            "transfer-encoding" => chunked = value.eq_ignore_ascii_case("chunked"),
            "connection" => close = value.eq_ignore_ascii_case("close"),
            _ => {}
        }
    }
    if chunked {
        loop {
            line.clear();
            reader.read_line(&mut line)?;
            let size = u64::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)?;
            if size == 0 {
                // The trailer section, then its blank line.
                loop {
                    line.clear();
                    reader.read_line(&mut line)?;
                    if line.trim_end().is_empty() {
                        break;
                    }
                }
                break;
            }
            std::io::copy(&mut reader.by_ref().take(size + 2), &mut std::io::sink())?;
        }
    } else {
        let length = length.ok_or_else(|| anyhow!("a response without a length"))?;
        std::io::copy(&mut reader.by_ref().take(length), &mut std::io::sink())?;
    }
    if close {
        *connection = None;
    }
    Ok(status)
}

/// A command running `program` on `cpus`, or unpinned when `cpus` is empty.
fn pinned(cpus: &str, program: &Path) -> Command {
    if cpus.is_empty() {
        Command::new(program)
    } else {
        let mut command = Command::new("taskset");
        command.arg("-c").arg(cpus).arg(program);
        command
    }
}

fn tool_version(program: &Path, flag: &str, name: &str) -> Result<String> {
    let output = Command::new(program).arg(flag).output().with_context(|| {
        format!(
            "{name} not found at `{}`: install it or name it with --{name} \
             (README, \"Benchmark\")",
            program.display()
        )
    })?;
    // nginx prints its version on stderr.
    let text = String::from_utf8_lossy(if output.stdout.is_empty() {
        &output.stderr
    } else {
        &output.stdout
    })
    .trim()
    .to_owned();
    Ok(text)
}

/// Splits the machine's physical cores in two, with all the hardware threads
/// of each core on the same side, so that the servers and the load
/// generator never share a core: the first half for the servers, the second
/// for oha. `--server-cpus` and `--load-cpus` override the split, and an
/// empty value leaves a side unpinned.
fn machine(options: &Options) -> Result<Machine> {
    let mut cores: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
    let count = std::thread::available_parallelism().map_or(1, |n| n.get());
    for cpu in 0..count {
        let topology = format!("/sys/devices/system/cpu/cpu{cpu}/topology");
        let read = |file: &str| std::fs::read_to_string(format!("{topology}/{file}"));
        if let (Ok(package), Ok(core)) = (read("physical_package_id"), read("core_id")) {
            cores
                .entry((package.trim().to_owned(), core.trim().to_owned()))
                .or_default()
                .push(cpu);
        }
    }
    let cores: Vec<Vec<usize>> = cores.into_values().collect();
    let list = |cores: &[Vec<usize>]| {
        let mut cpus: Vec<usize> = cores.iter().flatten().copied().collect();
        cpus.sort_unstable();
        cpus.iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(",")
    };
    let (default_servers, default_load) = if cores.len() >= 2 {
        let (servers, load) = cores.split_at(cores.len() / 2);
        (list(servers), list(load))
    } else {
        (String::new(), String::new())
    };
    let server_cpus = options.server_cpus.clone().unwrap_or(default_servers);
    let load_cpus = options.load_cpus.clone().unwrap_or(default_load);
    let threads = match options.threads {
        Some(threads) => threads,
        None if server_cpus.is_empty() => count / 2,
        None => server_cpus.split(',').count(),
    }
    .max(1);
    Ok(Machine {
        server_cpus,
        load_cpus,
        threads,
    })
}

/// A body of `size` (`4k`, `1m`) of random bytes, which no layer can
/// compress.
fn write_body(path: &Path, size: &str) -> Result<()> {
    let (number, unit) = size.split_at(size.len() - 1);
    let bytes = number.parse::<usize>()?
        * match unit {
            "k" => 1024,
            "m" => 1024 * 1024,
            _ => bail!("unknown size {size}"),
        };
    let mut body = vec![0u8; bytes];
    let mut state: u64 = 0x9E37_79B9_7F4A_7C15;
    for chunk in body.chunks_mut(8) {
        // xorshift64: enough to defeat compression, no dependency needed.
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        chunk.copy_from_slice(&state.to_le_bytes()[..chunk.len()]);
    }
    std::fs::write(path, body).with_context(|| format!("failed to write {}", path.display()))
}

/// Deletes a server's cache directory, if it made one: MemCache does not.
fn remove_cache(cache: &Path) -> Result<()> {
    match std::fs::remove_dir_all(cache) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("failed to delete {}", cache.display()))
        }
        _ => Ok(()),
    }
}

/// Writes every dirty page back to its disk, as the `sync` command does.
fn sync() -> Result<()> {
    let status = Command::new("sync")
        .status()
        .context("failed to run sync")?;
    if !status.success() {
        bail!("sync failed");
    }
    Ok(())
}

/// The temporary-file directives of the `http` block, under `tmp` rather
/// than the paths under /var that nginx takes from its build.
fn temp_paths(tmp: &Path) -> String {
    ["client_body", "proxy", "fastcgi", "uwsgi", "scgi"]
        .iter()
        .map(|kind| format!("  {kind}_temp_path {}/{kind};\n", tmp.display()))
        .collect()
}

/// The origin: one nginx worker serving `www/<size>` for any URL under
/// `/<size>/`, cacheable for an hour, and logging each request so that a
/// hit scenario can check it caused none.
fn origin_config(work: &Path, addr: SocketAddr) -> String {
    let dir = work.join("origin");
    format!(
        "worker_processes 1;\n\
         pid {dir}/nginx.pid;\n\
         daemon off;\n\
         events {{ worker_connections 8192; }}\n\
         http {{\n\
         {temp}\
         \x20 access_log {dir}/access.log;\n\
         \x20 sendfile on;\n\
         \x20 keepalive_requests 1000000;\n\
         \x20 server {{\n\
         \x20   listen {addr};\n\
         \x20   root {www};\n\
         \x20   location ~ ^/(?<size>[0-9]+[km])/ {{\n\
         \x20     add_header Cache-Control \"public, max-age=3600\";\n\
         \x20     try_files /$size =404;\n\
         \x20   }}\n\
         \x20 }}\n\
         }}\n",
        dir = dir.display(),
        temp = temp_paths(&dir.join("tmp")),
        www = work.join("www").display(),
    )
}

/// nginx as a caching proxy, configured the way one would deploy it for
/// throughput: `sendfile`, kept-alive upstream connections, one listening
/// socket per worker (`reuseport`) and no access log. Like fixture-proxy, it
/// runs without a cache lock, which these scenarios would not use anyway: a
/// hit run causes no misses, and a fill run never repeats a URL. It also
/// leaves `tcp_nopush` off: on kept-alive connections it stalls every 64 KiB
/// response by the client's delayed ACK, about 40 ms, which cut nginx to
/// 1,500 requests per second where it serves 65,000.
fn cache_config(dir: &Path, addr: SocketAddr, origin: SocketAddr, workers: usize) -> String {
    format!(
        "worker_processes {workers};\n\
         pid {dir}/nginx.pid;\n\
         daemon off;\n\
         events {{ worker_connections 8192; }}\n\
         http {{\n\
         {temp}\
         \x20 access_log off;\n\
         \x20 sendfile on;\n\
         \x20 keepalive_requests 1000000;\n\
         \x20 proxy_cache_path {dir}/cache levels=1:2 keys_zone=bench:64m max_size=16g inactive=1h use_temp_path=off;\n\
         \x20 upstream origin {{ server {origin}; keepalive 64; }}\n\
         \x20 server {{\n\
         \x20   listen {addr} reuseport;\n\
         \x20   location / {{\n\
         \x20     proxy_pass http://origin;\n\
         \x20     proxy_http_version 1.1;\n\
         \x20     proxy_set_header Connection \"\";\n\
         \x20     proxy_cache bench;\n\
         \x20     proxy_cache_key $uri;\n\
         \x20     add_header X-Cache-Status $upstream_cache_status;\n\
         \x20   }}\n\
         \x20 }}\n\
         }}\n",
        dir = dir.display(),
        temp = temp_paths(&dir.join("tmp")),
    )
}

/// Runs oha on `cpus` against URLs generated from the pattern `url`, and
/// reads its JSON report.
fn oha(oha: &Path, cpus: &str, args: &[&str], url: &str) -> Result<Run> {
    let output = pinned(cpus, oha)
        .args(args)
        .args([
            "--no-tui",
            "--output-format",
            "json",
            "--rand-regex-url",
            url,
        ])
        .output()
        .context("failed to run oha")?;
    if !output.status.success() {
        bail!(
            "oha failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).context("oha printed no JSON report")?;
    let statuses = report["statusCodeDistribution"].as_object();
    let errors = report["errorDistribution"]
        .as_object()
        .map_or(0, |errors| errors.values().filter_map(|n| n.as_u64()).sum());
    let non_200 = statuses.map_or(0, |statuses| {
        statuses
            .iter()
            .filter(|(status, _)| status.as_str() != "200")
            .filter_map(|(_, n)| n.as_u64())
            .sum()
    });
    let answered: u64 = statuses.map_or(0, |statuses| {
        statuses.values().filter_map(|n| n.as_u64()).sum()
    });
    Ok(Run {
        requests_per_sec: report["summary"]["requestsPerSec"].as_f64().unwrap_or(0.0),
        p50: report["latencyPercentiles"]["p50"]
            .as_f64()
            .unwrap_or(f64::NAN),
        p99: report["latencyPercentiles"]["p99"]
            .as_f64()
            .unwrap_or(f64::NAN),
        errors,
        non_200,
        requests: answered + errors,
        seconds: report["summary"]["total"].as_f64().unwrap_or(f64::NAN),
        ..Run::default()
    })
}

/// How many requests for `/<size>/...` the origin has logged so far.
fn origin_requests(log: &Path, size: &str) -> Result<usize> {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let needle = format!("\"GET /{size}/");
    Ok(text.lines().filter(|line| line.contains(&needle)).count())
}

fn save(work: &Path, round: usize, server: Server, scenario: &str, run: &Run) -> Result<()> {
    let path = work.join(format!(
        "results/round-{round}-{}-{scenario}.json",
        server.slug()
    ));
    let json = serde_json::json!({
        "requestsPerSec": run.requests_per_sec,
        "p50": run.p50,
        "p99": run.p99,
        "errors": run.errors,
        "non200": run.non_200,
        "requests": run.requests,
        "seconds": run.seconds,
        "originRequests": run.origin_requests,
        "cpuMicrosPerRequest": run.cpu_us,
        "peakThreads": run.threads,
        "peakDirtyMiB": run.dirty_mib,
        "peakWritebackMiB": run.writeback_mib,
    });
    std::fs::write(path, serde_json::to_string_pretty(&json)?)?;
    Ok(())
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(|a, b| a.total_cmp(b));
    let n = values.len();
    if n == 0 {
        return f64::NAN;
    }
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// The median of `field` over `runs`.
fn median_of(runs: &[&Run], field: impl Fn(&Run) -> f64) -> f64 {
    median(runs.iter().map(|run| field(run)).collect())
}

fn millis(seconds: f64) -> String {
    format!("{:.2} ms", seconds * 1000.0)
}

fn report(
    results: &Results,
    options: &Options,
    machine: &Machine,
    nginx_version: &str,
    oha_version: &str,
    work: &Path,
) -> Result<String> {
    let mut out = String::new();
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            info.lines()
                .find(|line| line.starts_with("model name"))
                .and_then(|line| line.split(':').nth(1))
                .map(|name| name.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown CPU".to_owned());
    writeln!(out, "# durable-cache benchmark\n")?;
    writeln!(
        out,
        "{cpu}; servers on CPUs {} with {} workers each, oha on CPUs {}; \
         {nginx_version}, {oha_version}; caches in {}; durable-cache stores \
         {}. Medians of {} round(s), {} connections; errors and origin \
         requests are totals over the rounds. CPU is the server's processor \
         time per request; threads, the most the server ran.\n",
        if machine.server_cpus.is_empty() {
            "(unpinned)"
        } else {
            &machine.server_cpus
        },
        machine.threads,
        if machine.load_cpus.is_empty() {
            "(unpinned)"
        } else {
            &machine.load_cpus
        },
        work.display(),
        options.durability,
        options.rounds,
        options.connections,
    )?;
    // Hits from the smallest body to the largest, then fills.
    let scenarios = HIT_SIZES
        .iter()
        .map(|size| format!("hit-{size}"))
        .chain(FILLS.iter().map(|(size, _)| format!("fill-{size}")));
    for scenario in &scenarios.collect::<Vec<_>>() {
        if !Server::ALL
            .iter()
            .any(|server| results.runs.contains_key(&(scenario.clone(), *server)))
        {
            continue;
        }
        let title = if let Some(size) = scenario.strip_prefix("hit-") {
            format!(
                "Cache hits, {size} bodies ({HIT_OBJECTS} objects, {} s per run)",
                options.duration
            )
        } else if let Some(size) = scenario.strip_prefix("fill-") {
            let requests = FILLS
                .iter()
                .find(|(s, _)| *s == size)
                .map_or(0, |(_, n)| *n);
            format!("Filling the cache, {size} bodies ({requests} new URLs per run)")
        } else {
            scenario.clone()
        };
        writeln!(out, "## {title}\n")?;
        writeln!(
            out,
            "| server | req/s | vs nginx | vs MemCache | p50 | p99 | CPU/req | threads | errors | origin requests |"
        )?;
        writeln!(out, "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|")?;
        let nginx_rps = median_rps(&results.runs, scenario, Server::Nginx);
        let memcache_rps = median_rps(&results.runs, scenario, Server::MemCache);
        for server in Server::ALL {
            let Some(server_runs) = results.runs.get(&(scenario.clone(), server)) else {
                continue;
            };
            let runs: Vec<&Run> = server_runs.iter().collect();
            let rps = median_of(&runs, |r| r.requests_per_sec);
            let errors: u64 = runs.iter().map(|r| r.errors + r.non_200).sum();
            let origin: usize = runs.iter().map(|r| r.origin_requests).sum();
            let ratio = nginx_rps.map_or("-".to_owned(), |n| format!("{:.2}", rps / n));
            let vs_memcache = memcache_rps.map_or("-".to_owned(), |m| format!("{:.2}", rps / m));
            writeln!(
                out,
                "| {} | {:.0} | {ratio} | {vs_memcache} | {} | {} | {:.0} µs | {:.0} | {errors} | {origin} |",
                server.label(),
                rps,
                millis(median_of(&runs, |r| r.p50)),
                millis(median_of(&runs, |r| r.p99)),
                median_of(&runs, |r| r.cpu_us),
                median_of(&runs, |r| r.threads as f64),
            )?;
        }
        writeln!(out)?;
    }
    write_bursts(&mut out, results, options)?;
    write_colds(&mut out, results)?;
    write_census(&mut out, results, options)?;
    write_verdicts(&mut out, &results.runs, options)?;
    Ok(out)
}

/// The burst's table: the hits during the stores, beside the hits alone.
fn write_bursts(out: &mut String, results: &Results, options: &Options) -> Result<()> {
    if results.bursts.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "## Hits during a burst of stores\n\n\
         {BURST_HIT_SIZE} hits for {} s over {} connections, while \
         {BURST_FILL_CONNECTIONS} connections store {BURST_FILLS} new \
         {BURST_FILL_SIZE} bodies, starting {} s earlier. Dirty and \
         write-back are the host's peaks during the hits.\n",
        BURST_HITS.as_secs(),
        options.connections,
        BURST_LEAD.as_secs()
    )?;
    writeln!(
        out,
        "| server | hits req/s | p50 | p99 | p99 without the burst | stores req/s | stores' length | peak dirty | peak write-back | CPU/hit | origin requests |"
    )?;
    writeln!(
        out,
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"
    )?;
    for (server, bursts) in &results.bursts {
        let hits: Vec<&Run> = bursts.iter().map(|b| &b.hits).collect();
        let fills: Vec<&Run> = bursts.iter().map(|b| &b.fills).collect();
        let alone = results
            .runs
            .get(&(format!("hit-{BURST_HIT_SIZE}"), *server))
            .map_or("-".to_owned(), |runs| {
                millis(median(runs.iter().map(|r| r.p99).collect()))
            });
        let origin: usize = hits.iter().map(|r| r.origin_requests).sum();
        writeln!(
            out,
            "| {} | {:.0} | {} | {} | {alone} | {:.0} | {:.1} s | {:.0} MiB | {:.0} MiB | {:.0} µs | {origin} |",
            server.label(),
            median_of(&hits, |r| r.requests_per_sec),
            millis(median_of(&hits, |r| r.p50)),
            millis(median_of(&hits, |r| r.p99)),
            median_of(&fills, |r| r.requests_per_sec),
            median_of(&fills, |r| r.seconds),
            median_of(&hits, |r| r.dirty_mib),
            median_of(&hits, |r| r.writeback_mib),
            median_of(&hits, |r| r.cpu_us),
        )?;
    }
    writeln!(out)?;
    Ok(())
}

/// The cold reads' table: the objects evicted from the page cache, then
/// cached.
fn write_colds(out: &mut String, results: &Results) -> Result<()> {
    if results.colds.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "## Hits on objects the page cache does not hold\n\n\
         {COLD_OBJECTS} objects of {COLD_SIZE}, stored, written back and \
         evicted from the page cache (`POSIX_FADV_DONTNEED`), then each \
         requested once over {COLD_CONNECTIONS} connections; then each once \
         more, cached.\n"
    )?;
    writeln!(
        out,
        "| server | evicted: req/s | p50 | p99 | CPU/req | cached: req/s | p50 | p99 | CPU/req |"
    )?;
    writeln!(out, "|---|---:|---:|---:|---:|---:|---:|---:|---:|")?;
    for (server, colds) in &results.colds {
        let evicted: Vec<&Run> = colds.iter().map(|c| &c.evicted).collect();
        let cached: Vec<&Run> = colds.iter().map(|c| &c.cached).collect();
        writeln!(
            out,
            "| {} | {:.0} | {} | {} | {:.0} µs | {:.0} | {} | {} | {:.0} µs |",
            server.label(),
            median_of(&evicted, |r| r.requests_per_sec),
            millis(median_of(&evicted, |r| r.p50)),
            millis(median_of(&evicted, |r| r.p99)),
            median_of(&evicted, |r| r.cpu_us),
            median_of(&cached, |r| r.requests_per_sec),
            millis(median_of(&cached, |r| r.p50)),
            millis(median_of(&cached, |r| r.p99)),
            median_of(&cached, |r| r.cpu_us),
        )?;
    }
    writeln!(out)?;
    Ok(())
}

/// The census's table: system calls per request.
fn write_census(out: &mut String, results: &Results, options: &Options) -> Result<()> {
    if results.census.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "## System calls per request, durable-cache\n\n\
         Counted by strace over all the server's threads, with stores \
         {}, over {CENSUS_CONNECTIONS} connections; a fill is a miss and a \
         store. A store writes once per handoff to the blocking pool: its \
         fixed part as it opens, each body chunk, and its header as it \
         finishes, so its pool handoffs are its `pwrite` calls.\n",
        options.durability
    )?;
    write!(out, "| case | requests |")?;
    for (column, _) in CENSUS_CALLS {
        write!(out, " {column} |")?;
    }
    writeln!(out)?;
    write!(out, "|---|---:|")?;
    for _ in CENSUS_CALLS {
        write!(out, "---:|")?;
    }
    writeln!(out)?;
    for row in &results.census {
        write!(out, "| {} | {} |", row.case, row.requests)?;
        for (_, names) in CENSUS_CALLS {
            let calls: u64 = names.iter().filter_map(|name| row.calls.get(*name)).sum();
            write!(out, " {:.2} |", calls as f64 / row.requests.max(1) as f64)?;
        }
        writeln!(out)?;
    }
    writeln!(out)?;
    Ok(())
}

/// The median request rate of `server` in `scenario`, if it ran.
fn median_rps(
    runs: &BTreeMap<(String, Server), Vec<Run>>,
    scenario: &str,
    server: Server,
) -> Option<f64> {
    runs.get(&(scenario.to_owned(), server))
        .map(|runs| median(runs.iter().map(|r| r.requests_per_sec).collect()))
}

/// The expected results of TC-PERFORMANCE.HITRATE-01 and
/// TC-PERFORMANCE.STORERATE-01: durable-cache's median rate at least 0.60
/// (hits) or 0.40 (fills, with stores unsynchronised) of MemCache's, no
/// request failing, and the origin asked for nothing during a hit run and
/// for each new URL of a fill run once.
fn write_verdicts(
    out: &mut String,
    runs: &BTreeMap<(String, Server), Vec<Run>>,
    options: &Options,
) -> Result<()> {
    let check = |scenario: &str, bound: f64, origin_expected: usize| -> Option<(bool, String)> {
        let durable = runs.get(&(scenario.to_owned(), Server::DurableCache))?;
        let ratio = median_rps(runs, scenario, Server::DurableCache)?
            / median_rps(runs, scenario, Server::MemCache)?;
        let errors: u64 = durable.iter().map(|r| r.errors + r.non_200).sum();
        let origin: usize = durable.iter().map(|r| r.origin_requests).sum();
        let pass = ratio >= bound && errors == 0 && origin == origin_expected;
        Some((
            pass,
            format!("{scenario}: {ratio:.2} of MemCache (at least {bound:.2}), {errors} errors, {origin} origin requests (expected {origin_expected})"),
        ))
    };
    writeln!(out, "## Test-case verdicts\n")?;
    let write = |out: &mut String, id: &str, results: Vec<Option<(bool, String)>>| -> Result<()> {
        let pass = results
            .iter()
            .all(|r| r.as_ref().is_some_and(|(pass, _)| *pass));
        writeln!(out, "- **{id}: {}**", if pass { "pass" } else { "FAIL" })?;
        for result in results {
            let line = result.map_or("a scenario did not run".to_owned(), |(_, line)| line);
            writeln!(out, "  - {line}")?;
        }
        Ok(())
    };
    if options.scenarios.contains(&Scenario::Hits) {
        write(
            out,
            "TC-PERFORMANCE.HITRATE-01",
            HIT_SIZES
                .iter()
                .map(|size| check(&format!("hit-{size}"), 0.60, 0))
                .collect(),
        )?;
    } else {
        writeln!(
            out,
            "- TC-PERFORMANCE.HITRATE-01: not measured; it needs the `hits` scenario"
        )?;
    }
    if !options.scenarios.contains(&Scenario::Fills) {
        writeln!(
            out,
            "- TC-PERFORMANCE.STORERATE-01: not measured; it needs the `fills` scenario"
        )?;
    } else if options.durability == "no-store-sync" {
        write(
            out,
            "TC-PERFORMANCE.STORERATE-01",
            FILLS
                .iter()
                .filter(|(size, _)| STORERATE_SIZES.contains(size))
                .map(|(size, requests)| {
                    check(&format!("fill-{size}"), 0.40, requests * options.rounds)
                })
                .collect(),
        )?;
    } else {
        writeln!(
            out,
            "- TC-PERFORMANCE.STORERATE-01: not measured; it needs `--durability no-store-sync`"
        )?;
    }
    writeln!(out)?;
    Ok(())
}
