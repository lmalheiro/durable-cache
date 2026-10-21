// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! `cargo xtask <cmd>` — repo-local dev tooling. Not published, not part
//! of the deliverable crate.

use std::collections::BTreeMap;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the http-tests/cache-tests CLI against a freshly built
    /// fixture-proxy (TC-COMPLIANCE.RFC-01). Requires the
    /// `third_party/cache-tests` submodule and Node.js (both provided by
    /// the Nix devShell — see flake.nix).
    ///
    /// Starts cache-tests' own origin server and runs the whole suite
    /// against the same proxy twice: once on the stock `MemCache` and once
    /// on this backend. It then compares the two test by test, ranking
    /// outcomes pass, then suboptimal, then fail, and fails only on a test
    /// this backend ranks *lower* on than `MemCache` does — the
    /// non-regression HLR-COMPLIANCE.RFC-01 states. It also diffs this
    /// backend's `required` failures against
    /// `test/cache-tests-baseline.json`, which records why each is a
    /// failure of `pingora-cache` itself.
    CacheTests {
        /// Write the raw results JSON (per backend) here as well as
        /// summarising it.
        #[arg(long)]
        results_out: Option<PathBuf>,
        /// Print every allowlisted failure with its recorded reason,
        /// rather than just a count.
        #[arg(long)]
        verbose: bool,
        /// How many times to run the suite per backend. A test whose
        /// outcome differs between backends is only reported as a
        /// difference when every pass agrees, which is what separates a
        /// reproducible difference from a timing-dependent one.
        #[arg(long, default_value_t = 2)]
        passes: usize,
    },
    /// Cross-reference every TC-* ID in docs/durable-cache-requirements.json
    /// against test functions named `tc_<id>` anywhere in the workspace,
    /// and print a coverage table.
    TraceReport,
    /// Check that every source file (Rust, shell, Python, Nix) starts with
    /// the project's SPDX header, after the shebang if it has one: a
    /// `SPDX-FileCopyrightText` line naming the durable-cache authors
    /// (see AUTHORS), then `SPDX-License-Identifier: Apache-2.0`.
    Headers,
    /// Format the workspace with the nightly rustfmt pinned in
    /// `.config/rustfmt-toolchain`, since rustfmt.toml's import options are
    /// nightly-only. In the Nix shell, `RUSTFMT` names that rustfmt already;
    /// elsewhere rustup runs it, once installed with
    /// `rustup toolchain install <pin> --profile minimal --component rustfmt`.
    Fmt {
        /// Report unformatted files instead of formatting them, as CI and
        /// the pre-commit hook do.
        #[arg(long)]
        check: bool,
    },
    /// Point the README's links to repository files at `v<version>`, the tag
    /// of the durable-cache version in its Cargo.toml. crates.io shows the
    /// README each version was published with, so its links name that
    /// version's files; on main they name the latest release's. The release
    /// workflow runs this on every release pull request.
    ReadmeLinks {
        /// Report links that name another ref instead of rewriting them, as
        /// CI does.
        #[arg(long)]
        check: bool,
    },
}

fn workspace_root() -> Result<PathBuf> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    Ok(manifest_dir
        .parent()
        .context("xtask has no parent directory")?
        .to_path_buf())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::CacheTests {
            results_out,
            verbose,
            passes,
        } => cache_tests(results_out.as_deref(), verbose, passes),
        Cmd::TraceReport => trace_report(),
        Cmd::Headers => check_headers(),
        Cmd::Fmt { check } => fmt(check),
        Cmd::ReadmeLinks { check } => readme_links(check),
    }
}

/// Kills its child *process group* on drop, so a failure anywhere below
/// never leaves an origin server or proxy running.
///
/// Killing the direct child is not enough: `npm run` execs node as a
/// grandchild, and a surviving grandchild keeps this process's inherited
/// stdio pipes open, which hangs whatever is reading them long after
/// `xtask` itself has finished. Children are therefore spawned into their
/// own process group (`process_group(0)` makes pgid == child pid) and the
/// whole group is signalled at once.
struct ChildGuard {
    name: &'static str,
    child: Child,
}

impl ChildGuard {
    fn spawn(name: &'static str, cmd: &mut Command) -> Result<Self> {
        let child = cmd
            .process_group(0)
            .spawn()
            .with_context(|| format!("failed to spawn {name}"))?;
        Ok(ChildGuard { name, child })
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let pid = self.child.id() as i32;
        // Negative pid == "the process group with this id", which is the
        // child plus every descendant it spawned.
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(-pid),
            nix::sys::signal::Signal::SIGKILL,
        );
        let _ = self.child.wait();
        eprintln!("  stopped {} (pid {pid} and its process group)", self.name);
    }
}

/// An ephemeral localhost port, free at the moment of the call.
fn free_port() -> Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").context("failed to bind an ephemeral port")?;
    listener.local_addr().context("failed to read local_addr")
}

fn wait_until_listening(addr: SocketAddr, what: &str, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for {what} to listen on {addr}");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Invoke an npm script in the cache-tests checkout. npm exposes its
/// `--foo=bar` config as `npm_config_foo` env vars, so setting them
/// directly is both equivalent and immune to argument-forwarding quirks
/// between npm major versions.
fn npm_command(suite_dir: &Path, script: &str, config: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new("npm");
    cmd.args(["run", "--silent", script]).current_dir(suite_dir);
    for (key, value) in config {
        cmd.env(format!("npm_config_{key}"), value);
    }
    cmd
}

// IMP-TC.COMPLIANCE.RFC-01: satisfy TC-COMPLIANCE.RFC-01 -- runs the
// cache-tests CLI against the same fixture-proxy on each backend in turn
// (the TC's own Setup and Steps: one proxy build whose Storage is
// selectable at start, everything else identical), then applies its
// Expected Result: no test may rank lower on this backend than on
// MemCache. A failure on both is a pingora-cache limitation, outside this
// requirement, and this backend's own `required` failures are triaged
// against the behaviour of pingora-cache in test/cache-tests-baseline.json,
// where only an unallowlisted one fails the run.
fn cache_tests(results_out: Option<&Path>, verbose: bool, passes: usize) -> Result<()> {
    let root = workspace_root()?;
    let suite_dir = root.join("third_party/cache-tests");
    if !suite_dir.join("package.json").exists() {
        bail!(
            "third_party/cache-tests is not present (expected {}).\n\
             Initialize it with: git submodule update --init third_party/cache-tests",
            suite_dir.display()
        );
    }
    if passes == 0 {
        bail!("--passes must be at least 1");
    }

    // cache-tests ships no lockfile, so `npm ci` cannot be used here.
    if !suite_dir.join("node_modules").exists() {
        println!("==> npm install (cache-tests dependencies)");
        let status = Command::new("npm")
            .arg("install")
            .current_dir(&suite_dir)
            .status()
            .context("failed to invoke npm (is Node.js on PATH? the Nix devShell provides it)")?;
        if !status.success() {
            bail!("npm install failed in {}", suite_dir.display());
        }
    }

    // Build the fixture proxy first, so a build failure is never
    // misreported as a compliance failure.
    println!("==> cargo build --release -p fixture-proxy");
    let status = Command::new("cargo")
        .args(["build", "--release", "-p", "fixture-proxy"])
        .current_dir(&root)
        .status()
        .context("failed to invoke cargo build for fixture-proxy")?;
    if !status.success() {
        bail!("fixture-proxy build failed");
    }

    let origin_addr = free_port()?;
    println!("==> starting cache-tests origin on {origin_addr}");
    let origin = ChildGuard::spawn(
        "cache-tests origin",
        npm_command(
            &suite_dir,
            "server",
            &[("port", &origin_addr.port().to_string())],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null()),
    )?;
    wait_until_listening(origin_addr, "cache-tests origin", Duration::from_secs(30))?;

    // The test catalog carries each test's `kind` (required / optimal /
    // check), which the results JSON does not -- both are needed to tell a
    // real failure from an informational one.
    println!("==> exporting the test catalog");
    let catalog_out = npm_command(&suite_dir, "export", &[])
        .output()
        .context("failed to run the cache-tests export script")?;
    if !catalog_out.status.success() {
        bail!(
            "cache-tests export failed: {}",
            String::from_utf8_lossy(&catalog_out.stderr)
        );
    }
    let catalog: serde_json::Value = serde_json::from_slice(&catalog_out.stdout)
        .context("cache-tests export did not produce valid JSON")?;
    let kinds = kinds_by_id(&catalog)?;

    // The same proxy and the same origin for both runs: only the backend
    // differs (TC-COMPLIANCE.RFC-01's setup).
    let mut runs: BTreeMap<&str, Vec<serde_json::Value>> = BTreeMap::new();
    for backend in ["memcache", "fs"] {
        for pass in 1..=passes {
            println!("==> running the suite against {backend} (pass {pass}/{passes})");
            let results = run_suite(&root, &suite_dir, backend, origin_addr)?;
            runs.entry(backend).or_default().push(results);
        }
    }
    drop(origin);
    println!();

    if let Some(path) = results_out {
        let combined = serde_json::json!({
            "memcache": runs["memcache"],
            "fs": runs["fs"],
        });
        std::fs::write(path, serde_json::to_vec_pretty(&combined)?)
            .with_context(|| format!("failed to write results to {}", path.display()))?;
        println!("raw results written to {}", path.display());
    }

    let regressions = report_differential(&kinds, &runs["memcache"], &runs["fs"]);
    // The baseline report covers every pass this backend ran, not just one
    // of them: a required failure counts only when it failed in all of them
    // (TC-COMPLIANCE.RFC-01 step 3, "re-run ... to separate a reproducible
    // difference from a timing-dependent one").
    let baseline = report_cache_tests(&root, &catalog, &runs["fs"], verbose);

    if !regressions.is_empty() {
        bail!(
            "{} test(s) rank lower on this backend than on MemCache",
            regressions.len()
        );
    }
    baseline
}

/// Start a fixture-proxy on `backend`, run the whole suite against it, and
/// return the results JSON.
fn run_suite(
    root: &Path,
    suite_dir: &Path,
    backend: &str,
    origin_addr: SocketAddr,
) -> Result<serde_json::Value> {
    let proxy_addr = free_port()?;
    let cache_dir = tempfile::tempdir().context("failed to create a temp cache directory")?;
    let proxy = ChildGuard::spawn(
        "fixture-proxy",
        Command::new(root.join("target/release/fixture-proxy"))
            .arg("--listen-addr")
            .arg(proxy_addr.to_string())
            .arg("--upstream-addr")
            .arg(origin_addr.to_string())
            .arg("--cache-dir")
            .arg(cache_dir.path())
            .arg("--storage")
            .arg(backend)
            .env("RUST_LOG", "warn")
            .stdout(Stdio::null()),
    )?;
    wait_until_listening(proxy_addr, "fixture-proxy", Duration::from_secs(30))?;

    let cli_out = npm_command(
        suite_dir,
        "cli",
        &[("base", &format!("http://{proxy_addr}"))],
    )
    .output()
    .context("failed to run the cache-tests CLI")?;
    if !cli_out.status.success() {
        bail!(
            "cache-tests CLI failed: {}",
            String::from_utf8_lossy(&cli_out.stderr)
        );
    }
    drop(proxy);
    serde_json::from_slice(&cli_out.stdout).context("cache-tests CLI did not produce valid JSON")
}

fn kinds_by_id(catalog: &serde_json::Value) -> Result<BTreeMap<String, String>> {
    let mut kinds = BTreeMap::new();
    for suite in catalog.as_array().context("catalog is not an array")? {
        for test in suite["tests"]
            .as_array()
            .context("catalog suite has no tests array")?
        {
            let id = test["id"].as_str().context("catalog test has no id")?;
            // cache-tests treats a missing `kind` as required
            // (test-engine/lib/results.mjs's determineTestResult).
            let kind = test["kind"].as_str().unwrap_or("required");
            kinds.insert(id.to_owned(), kind.to_owned());
        }
    }
    Ok(kinds)
}

/// How good an outcome is, for the ranking TC-COMPLIANCE.RFC-01 compares on:
/// pass, then suboptimal, then fail. `None` is an untested test.
fn rank(kind: &str, result: Option<&serde_json::Value>) -> Option<u8> {
    let result = result?;
    if result == &serde_json::Value::Bool(true) {
        return Some(3); // pass (or, for a `check`, "yes")
    }
    // A `Setup` or `AbortError` outcome is the harness failing to get far
    // enough to judge the cache at all, and an `optimal`/`check` failure is
    // cache-tests' own "suboptimal" rung rather than a failure.
    let harness = result
        .get(0)
        .and_then(|v| v.as_str())
        .is_some_and(|name| name == "Setup" || name == "AbortError");
    Some(if harness || kind == "optimal" || kind == "check" {
        2
    } else {
        1
    })
}

/// Compare the two backends test by test, and report every test this
/// backend ranks lower on. Returns the reproducible ones — those that rank
/// lower in every pass, with each backend steady across its own passes.
fn report_differential(
    kinds: &BTreeMap<String, String>,
    memcache: &[serde_json::Value],
    fs: &[serde_json::Value],
) -> Vec<String> {
    let ranks = |runs: &[serde_json::Value], id: &str, kind: &str| -> Vec<Option<u8>> {
        runs.iter().map(|r| rank(kind, r.get(id))).collect()
    };
    let steady = |v: &[Option<u8>]| v.windows(2).all(|w| w[0] == w[1]);

    let mut reproducible = Vec::new();
    let mut timing_dependent = Vec::new();
    let mut better = Vec::new();
    for (id, kind) in kinds {
        let mem = ranks(memcache, id, kind);
        let ours = ranks(fs, id, kind);
        let pairs = || mem.iter().zip(&ours);
        let worse = |m: &Option<u8>, o: &Option<u8>| matches!((m, o), (Some(m), Some(o)) if o < m);

        if pairs().all(|(m, o)| worse(m, o)) && steady(&mem) && steady(&ours) {
            reproducible.push(id.clone());
        } else if pairs().any(|(m, o)| worse(m, o)) {
            timing_dependent.push(id.clone());
        } else if pairs().all(|(m, o)| matches!((m, o), (Some(m), Some(o)) if o > m)) {
            better.push(id.clone());
        }
    }

    println!(
        "TC-COMPLIANCE.RFC-01 differential (this backend vs MemCache, {} pass(es) each):",
        memcache.len()
    );
    println!(
        "  {} test(s) rank lower on this backend, reproducibly",
        reproducible.len()
    );
    for id in &reproducible {
        println!("  ! {id}");
    }
    if !timing_dependent.is_empty() {
        println!(
            "  {} test(s) differed in some passes but not all: timing-dependent,\n\
             \x20    listed for review rather than treated as a regression:",
            timing_dependent.len()
        );
        for id in &timing_dependent {
            println!("  ~ {id}");
        }
    }
    if !better.is_empty() {
        println!("  {} test(s) rank higher on this backend", better.len());
    }
    if reproducible.is_empty() {
        println!("  No test has a reproducibly lower-ranked outcome: no added deviation.");
    }
    println!();
    reproducible
}

/// Join the results against the catalog's `kind` classification, then diff
/// the `required` failures against the checked-in baseline.
fn report_cache_tests(
    root: &Path,
    catalog: &serde_json::Value,
    runs: &[serde_json::Value],
    verbose: bool,
) -> Result<()> {
    let mut kind_by_id: BTreeMap<&str, &str> = BTreeMap::new();
    for suite in catalog.as_array().context("catalog is not an array")? {
        for test in suite["tests"]
            .as_array()
            .context("catalog suite has no tests array")?
        {
            let id = test["id"].as_str().context("catalog test has no id")?;
            // cache-tests treats a missing `kind` as required
            // (test-engine/lib/results.mjs's determineTestResult).
            kind_by_id.insert(id, test["kind"].as_str().unwrap_or("required"));
        }
    }

    let baseline_path = root.join("test/cache-tests-baseline.json");
    let baseline_raw = std::fs::read_to_string(&baseline_path)
        .with_context(|| format!("failed to read {}", baseline_path.display()))?;
    let baseline: serde_json::Value = serde_json::from_str(&baseline_raw)?;
    let allowed = baseline["allowed_required_failures"]
        .as_object()
        .context("baseline has no allowed_required_failures object")?;

    // The last pass is what the by-kind tally reports, so the printed
    // numbers stay one run's worth and comparable with earlier output; which
    // failures *gate* the command is decided across every pass below.
    let results = runs
        .last()
        .context("at least one pass")?
        .as_object()
        .context("results is not an object")?;
    let passed_in =
        |run: &serde_json::Value, id: &str| run.get(id) == Some(&serde_json::Value::Bool(true));
    let mut counts: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut new_failures: Vec<&str> = Vec::new();
    let mut allowlisted: Vec<&str> = Vec::new();
    let mut intermittent: Vec<(&str, usize)> = Vec::new();

    for (id, result) in results {
        let kind = kind_by_id.get(id.as_str()).copied().unwrap_or("required");
        let passed = result == &serde_json::Value::Bool(true);
        let entry = counts.entry(kind).or_insert((0, 0));
        if passed {
            entry.0 += 1;
        } else {
            entry.1 += 1;
        }

        if kind != "required" {
            continue;
        }
        // A required failure gates the command only when it is reproducible:
        // failing in every pass. A test that passed at least once did so
        // with this same build against this same origin, so whatever made
        // the other pass fail is not a property of the backend -- it is the
        // suite's own flake floor, which is real (transport errors and setup
        // failures both appear in it).
        let failures = runs.iter().filter(|r| !passed_in(r, id)).count();
        if failures == 0 {
            continue;
        }
        if allowed.contains_key(id.as_str()) {
            allowlisted.push(id);
        } else if failures == runs.len() {
            new_failures.push(id);
        } else {
            intermittent.push((id, failures));
        }
    }

    println!("cache-tests results by kind (cache-tests' own classification):");
    for (kind, (passed, failed)) in &counts {
        let note = match *kind {
            "required" => "  <- gates this command",
            "optimal" => "  (informational: cache-tests renders these as warnings)",
            "check" => "  (informational: a yes/no observation, not a pass/fail)",
            _ => "",
        };
        println!("  {kind:<9} {passed:>4} passed  {failed:>4} failed{note}");
    }
    println!();

    // A `FLAKY:`-prefixed reason marks a test observed landing both ways
    // across runs; a passing run does not make such an entry stale.
    let stale: Vec<&str> = allowed
        .iter()
        .filter(|(_, reason)| !reason.as_str().unwrap_or("").starts_with("FLAKY:"))
        .map(|(id, _)| id.as_str())
        .filter(|id| runs.iter().all(|r| passed_in(r, id)))
        .collect();
    if !stale.is_empty() {
        println!(
            "NOTE: {} baselined test(s) now PASS -- remove them from {} to keep\n\
             the allowlist honest (not treated as an error, so a fix never breaks CI):",
            stale.len(),
            baseline_path.display()
        );
        for id in &stale {
            println!("  + {id}");
        }
        println!();
    }

    println!(
        "{} required failure(s) allowlisted in {}",
        allowlisted.len(),
        baseline_path.display()
    );
    if !intermittent.is_empty() {
        println!(
            "\n{} required test(s) failed in some passes but not all, so they are\n\
             reported rather than treated as failures -- the suite's own flake floor:",
            intermittent.len()
        );
        for (id, failures) in &intermittent {
            println!("  ~ {id} (failed {failures}/{} passes)", runs.len());
        }
    }
    if verbose {
        for id in &allowlisted {
            let reason = allowed[*id].as_str().unwrap_or("");
            println!("  - {id}\n      {reason}");
        }
        println!();
    }

    if new_failures.is_empty() {
        println!("\nNo unallowlisted required failures. TC-COMPLIANCE.RFC-01 baseline is clean.");
        return Ok(());
    }

    println!(
        "\n{} NEW required failure(s) not in the baseline:",
        new_failures.len()
    );
    for id in &new_failures {
        let detail = results[*id].to_string();
        println!("  ! {id}\n      {detail}");
    }
    println!(
        "\nEach of these must be triaged against the behaviour of pingora-cache\n\
         itself before being attributed to this backend (TC-COMPLIANCE.RFC-01).\n\
         If it is genuinely a failure of pingora-cache, add it to {} with the\n\
         source citation that establishes that; otherwise it is a real regression.",
        baseline_path.display()
    );
    bail!("{} unallowlisted required failure(s)", new_failures.len());
}

fn trace_report() -> Result<()> {
    let root = workspace_root()?;
    let requirements_path = root.join("docs/durable-cache-requirements.json");
    let raw = std::fs::read_to_string(&requirements_path)
        .with_context(|| format!("failed to read {}", requirements_path.display()))?;
    let json: serde_json::Value = serde_json::from_str(&raw)?;
    let object = json
        .as_object()
        .context("requirements JSON root is not an object")?;

    let tc_ids: Vec<&String> = object.keys().filter(|k| k.starts_with("TC-")).collect();

    // Collect every `tc_...` test fn name, and every `IMP-TC.<ID>` marker,
    // anywhere in the workspace. Both are traceability evidence: a test fn
    // is the usual form, but a TC verified by tooling rather than by a
    // `#[test]` (TC-COMPLIANCE.RFC-01, which `cargo xtask cache-tests`
    // discharges) has only the marker -- the same marker `reqman-report.py`
    // reads, so the two tools agree by construction.
    let mut found_fns: Vec<String> = Vec::new();
    let mut found_markers: Vec<String> = Vec::new();
    for entry in walkdir::WalkDir::new(&root).into_iter().filter_entry(|e| {
        e.file_name() != "target" && e.file_name() != "third_party" && e.file_name() != ".git"
    }) {
        let entry = entry?;
        if entry.file_type().is_file() && entry.path().extension().is_some_and(|e| e == "rs") {
            let contents = std::fs::read_to_string(entry.path()).unwrap_or_default();
            for line in contents.lines() {
                let line = line.trim_start();

                // Strip the modifiers a test fn may legitimately carry.
                // Missing `async fn` here silently under-reported every
                // #[tokio::test] in the workspace.
                let mut decl = line;
                for prefix in ["pub(crate) ", "pub ", "async ", "unsafe ", "const "] {
                    decl = decl.strip_prefix(prefix).unwrap_or(decl);
                }
                if let Some(rest) = decl.strip_prefix("fn ") {
                    if let Some(name) = rest.split(['(', '<']).next() {
                        if name.starts_with("tc_") {
                            found_fns.push(name.to_string());
                        }
                    }
                }

                if let Some(rest) = line.strip_prefix("// IMP-TC.") {
                    if let Some(imp_id) = rest.split(':').next() {
                        found_markers.push(format!("TC-{imp_id}"));
                    }
                }
            }
        }
    }

    // (has a test fn, has an IMP marker)
    let mut coverage: BTreeMap<&str, (bool, bool)> = BTreeMap::new();
    for id in &tc_ids {
        // e.g. "TC-LAYOUT.SHARDING-01" -> "tc_layout_sharding_01" (the ID
        // already carries the "TC" prefix, so `slug` needs no extra one).
        let slug = id.to_lowercase().replace(['-', '.'], "_");
        let has_fn = found_fns.iter().any(|f| f.starts_with(&slug));
        let has_marker = found_markers.iter().any(|m| m == id.as_str());
        coverage.insert(id.as_str(), (has_fn, has_marker));
    }

    let covered = coverage.values().filter(|(f, m)| *f || *m).count();
    println!(
        "TC-* traceability: {covered}/{} covered  \
         (fn = a `tc_<id>_...` test fn, IMP = an `IMP-TC.<id>` marker)\n",
        coverage.len()
    );
    for (id, (has_fn, has_marker)) in &coverage {
        let mark = if *has_fn || *has_marker { "x" } else { " " };
        let how = match (has_fn, has_marker) {
            (true, true) => "fn + IMP",
            (true, false) => "fn",
            (false, true) => "IMP",
            (false, false) => "-",
        };
        println!("  [{mark}] {id:<32} {how}");
    }

    let uncovered: Vec<&str> = coverage
        .iter()
        .filter(|(_, (f, m))| !*f && !*m)
        .map(|(id, _)| *id)
        .collect();
    if !uncovered.is_empty() {
        bail!(
            "{} TC-* ID(s) have neither a test fn nor an IMP marker: {}",
            uncovered.len(),
            uncovered.join(", ")
        );
    }

    Ok(())
}

/// The comment marker of a file the header check covers, or `None` for a
/// file it does not: documents, data, and third-party files keep their own.
fn header_comment(path: &str) -> Option<&'static str> {
    if path.ends_with(".rs") {
        Some("//")
    } else if [".sh", ".py", ".nix"].iter().any(|ext| path.ends_with(ext))
        || path.starts_with(".githooks/")
    {
        Some("#")
    } else {
        None
    }
}

/// `cargo xtask headers`: fail when a tracked source file lacks the SPDX
/// licence header.
fn check_headers() -> Result<()> {
    let root = workspace_root()?;
    let listing = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(&root)
        .output()
        .context("failed to run git ls-files")?;
    if !listing.status.success() {
        bail!("git ls-files failed");
    }
    let mut checked = 0;
    let mut missing = Vec::new();
    for path in String::from_utf8(listing.stdout)?.split('\0') {
        let Some(c) = header_comment(path) else {
            continue;
        };
        checked += 1;
        let text = std::fs::read_to_string(root.join(path))
            .with_context(|| format!("failed to read {path}"))?;
        let mut lines = text.lines().peekable();
        if lines.peek().is_some_and(|l| l.starts_with("#!")) {
            lines.next();
        }
        let copyright = lines.next().unwrap_or_default();
        let licence = lines.next().unwrap_or_default();
        let ok = copyright.starts_with(&format!("{c} SPDX-FileCopyrightText: "))
            && copyright.ends_with(" The durable-cache authors")
            && licence == format!("{c} SPDX-License-Identifier: Apache-2.0");
        if !ok {
            missing.push(path.to_owned());
        }
    }
    if !missing.is_empty() {
        bail!(
            "{} of {checked} source files lack the SPDX header:\n  {}",
            missing.len(),
            missing.join("\n  ")
        );
    }
    println!("headers: all {checked} source files carry the SPDX header");
    Ok(())
}

fn fmt(check: bool) -> Result<()> {
    let root = workspace_root()?;
    let mut cargo = Command::new("cargo");
    // The Nix shell points `RUSTFMT` at the pinned rustfmt, which `cargo fmt`
    // then runs; with rustup, `+<pin>` selects it.
    let pin = if std::env::var_os("RUSTFMT").is_none_or(|rustfmt| rustfmt.is_empty()) {
        let pin = std::fs::read_to_string(root.join(".config/rustfmt-toolchain"))
            .context("failed to read .config/rustfmt-toolchain")?;
        let pin = pin.trim().to_owned();
        cargo.arg(format!("+{pin}"));
        Some(pin)
    } else {
        None
    };
    cargo.args(["fmt", "--all"]).current_dir(&root);
    if check {
        cargo.args(["--", "--check"]);
    }
    let status = cargo.status().context("failed to run cargo fmt")?;
    if status.success() {
        return Ok(());
    }
    if let Some(pin) = pin {
        eprintln!(
            "(the nightly rustfmt installs with \
             `rustup toolchain install {pin} --profile minimal --component rustfmt`)"
        );
    }
    if check {
        bail!("the workspace is not formatted: run `cargo xtask fmt`");
    }
    bail!("cargo fmt failed");
}

/// The README's links to repository files, each followed by the ref it names
/// and the path: `blob/<ref>/<file>` and `tree/<ref>/<directory>`.
const REPO_FILE_LINKS: [&str; 2] = [
    "https://github.com/lmalheiro/durable-cache/blob/",
    "https://github.com/lmalheiro/durable-cache/tree/",
];

fn readme_links(check: bool) -> Result<()> {
    let root = workspace_root()?;
    let metadata = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(&root)
        .output()
        .context("failed to run cargo metadata")?;
    if !metadata.status.success() {
        bail!("cargo metadata failed");
    }
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout)?;
    let version = metadata["packages"]
        .as_array()
        .and_then(|packages| packages.iter().find(|p| p["name"] == "durable-cache"))
        .and_then(|package| package["version"].as_str())
        .context("cargo metadata lists no version of durable-cache")?;
    let tag = format!("v{version}");

    let path = root.join("README.md");
    let readme = std::fs::read_to_string(&path).context("failed to read README.md")?;
    let mut rewritten = String::with_capacity(readme.len());
    let mut stale = Vec::new();
    let mut rest = readme.as_str();
    while let Some((at, prefix)) = REPO_FILE_LINKS
        .iter()
        .filter_map(|prefix| rest.find(prefix).map(|at| (at, *prefix)))
        .min()
    {
        let ref_start = at + prefix.len();
        let ref_len = rest[ref_start..]
            .find('/')
            .with_context(|| format!("a README link at `{prefix}` names no path"))?;
        let named = &rest[ref_start..ref_start + ref_len];
        if named != tag {
            stale.push(format!("{prefix}{named}/"));
        }
        rewritten.push_str(&rest[..ref_start]);
        rewritten.push_str(&tag);
        rest = &rest[ref_start + ref_len..];
    }
    rewritten.push_str(rest);
    stale.sort();
    stale.dedup();

    if check {
        if !stale.is_empty() {
            bail!(
                "README.md links name another ref than {tag}, the version in \
                 durable-cache/Cargo.toml: {}. Run `cargo xtask readme-links`.",
                stale.join(", ")
            );
        }
        println!("README.md links name {tag}");
    } else if stale.is_empty() {
        println!("README.md links already name {tag}");
    } else {
        std::fs::write(&path, rewritten).context("failed to write README.md")?;
        println!(
            "README.md links now name {tag}, instead of: {}",
            stale.join(", ")
        );
    }
    Ok(())
}
