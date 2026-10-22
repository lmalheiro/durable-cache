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

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::{free_port, wait_until_listening, workspace_root, ChildGuard};

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
const FILLS: [(&str, usize); 2] = [("4k", 20_000), ("64k", 10_000)];

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
}

/// One measured run, as oha reported it.
struct Run {
    requests_per_sec: f64,
    p50: f64,
    p99: f64,
    errors: u64,
    non_200: u64,
    /// Requests the origin saw during the run: none in a hit scenario.
    origin_requests: usize,
}

struct Machine {
    server_cpus: String,
    load_cpus: String,
    threads: usize,
}

pub fn bench(options: Options) -> Result<()> {
    if options.rounds == 0 {
        bail!("--rounds must be at least 1");
    }
    let root = workspace_root()?;
    let nginx_version = tool_version(&options.nginx, "-v", "nginx")?;
    let oha_version = tool_version(&options.oha, "--version", "oha")?;
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
    for size in HIT_SIZES.iter().chain(FILLS.iter().map(|(size, _)| size)) {
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
    let mut runs: BTreeMap<(String, Server), Vec<Run>> = BTreeMap::new();
    for round in 0..options.rounds {
        // A different order each round, so no server always runs first.
        let mut order = Server::ALL;
        order.rotate_left(round % Server::ALL.len());
        for server in order {
            println!(
                "--> round {} of {}: {}",
                round + 1,
                options.rounds,
                server.label()
            );
            let addr = free_port()?;
            let dir = work.join(format!("round-{round}/{}", server.slug()));
            std::fs::create_dir_all(&dir)?;
            let guard = match server {
                Server::Nginx => {
                    std::fs::create_dir_all(dir.join("tmp"))?;
                    let conf = dir.join("nginx.conf");
                    std::fs::write(
                        &conf,
                        cache_config(&dir, addr, origin_addr, machine.threads),
                    )?;
                    ChildGuard::spawn(
                        "nginx",
                        pinned(&machine.server_cpus, &options.nginx)
                            .arg("-p")
                            .arg(&dir)
                            .arg("-c")
                            .arg(&conf)
                            .arg("-e")
                            .arg(dir.join("error.log")),
                    )?
                }
                Server::DurableCache | Server::MemCache => {
                    let storage = if server == Server::DurableCache {
                        "fs"
                    } else {
                        "memcache"
                    };
                    ChildGuard::spawn(
                        "fixture-proxy",
                        pinned(&machine.server_cpus, &fixture_proxy)
                            .arg("--listen-addr")
                            .arg(addr.to_string())
                            .arg("--upstream-addr")
                            .arg(origin_addr.to_string())
                            .arg("--cache-dir")
                            .arg(dir.join("cache"))
                            .arg("--storage")
                            .arg(storage)
                            .arg("--threads")
                            .arg(machine.threads.to_string()),
                    )?
                }
            };
            wait_until_listening(addr, server.label(), Duration::from_secs(30))?;

            for size in HIT_SIZES {
                let url = format!("http://{addr}/{size}/[0-9]{{2}}");
                let warm = oha(
                    &options.oha,
                    &machine.load_cpus,
                    &["-n", &WARM_REQUESTS.to_string(), "-c", "1"],
                    &url,
                )?;
                if warm.non_200 > 0 || warm.errors > 0 {
                    bail!(
                        "{} failed requests while warming hit-{size}",
                        server.label()
                    );
                }
                let before = origin_requests(&origin_log, size)?;
                let duration = format!("{}s", options.duration);
                let connections = options.connections.to_string();
                let mut run = oha(
                    &options.oha,
                    &machine.load_cpus,
                    &["-z", &duration, "-c", &connections, "-w"],
                    &url,
                )?;
                run.origin_requests = origin_requests(&origin_log, size)? - before;
                save(&work, round, server, &format!("hit-{size}"), &run)?;
                runs.entry((format!("hit-{size}"), server))
                    .or_default()
                    .push(run);
            }
            for (size, requests) in FILLS {
                let url = format!("http://{addr}/{size}/f[0-9]{{12}}");
                let before = origin_requests(&origin_log, size)?;
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
                run.origin_requests = origin_requests(&origin_log, size)? - before;
                save(&work, round, server, &format!("fill-{size}"), &run)?;
                runs.entry((format!("fill-{size}"), server))
                    .or_default()
                    .push(run);
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

    let report = report(
        &runs,
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

fn target_dir(root: &Path) -> Result<PathBuf> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()
        .context("failed to run cargo metadata")?;
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    metadata["target_directory"]
        .as_str()
        .map(PathBuf::from)
        .context("cargo metadata names no target directory")
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
        origin_requests: 0,
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
        "originRequests": run.origin_requests,
    });
    std::fs::write(path, serde_json::to_string_pretty(&json)?)?;
    Ok(())
}

fn median(mut values: Vec<f64>) -> f64 {
    values.sort_by(|a, b| a.total_cmp(b));
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

fn millis(seconds: f64) -> String {
    format!("{:.2} ms", seconds * 1000.0)
}

fn report(
    runs: &BTreeMap<(String, Server), Vec<Run>>,
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
         {nginx_version}, {oha_version}; caches in {}. Medians of {} round(s), \
         {} connections; errors and origin requests are totals over the \
         rounds.\n",
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
        options.rounds,
        options.connections,
    )?;
    // Hits from the smallest body to the largest, then fills.
    let scenarios = HIT_SIZES
        .iter()
        .map(|size| format!("hit-{size}"))
        .chain(FILLS.iter().map(|(size, _)| format!("fill-{size}")));
    for scenario in &scenarios.collect::<Vec<_>>() {
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
            "| server | req/s | vs nginx | p50 | p99 | errors | origin requests |"
        )?;
        writeln!(out, "|---|---:|---:|---:|---:|---:|---:|")?;
        let nginx_rps = runs
            .get(&(scenario.clone(), Server::Nginx))
            .map(|runs| median(runs.iter().map(|r| r.requests_per_sec).collect()));
        for server in Server::ALL {
            let Some(server_runs) = runs.get(&(scenario.clone(), server)) else {
                continue;
            };
            let rps = median(server_runs.iter().map(|r| r.requests_per_sec).collect());
            let p50 = median(server_runs.iter().map(|r| r.p50).collect());
            let p99 = median(server_runs.iter().map(|r| r.p99).collect());
            let errors: u64 = server_runs.iter().map(|r| r.errors + r.non_200).sum();
            let origin: usize = server_runs.iter().map(|r| r.origin_requests).sum();
            let ratio = nginx_rps.map_or("-".to_owned(), |n| format!("{:.2}", rps / n));
            writeln!(
                out,
                "| {} | {:.0} | {ratio} | {} | {} | {errors} | {origin} |",
                server.label(),
                rps,
                millis(p50),
                millis(p99),
            )?;
        }
        writeln!(out)?;
    }
    Ok(out)
}
