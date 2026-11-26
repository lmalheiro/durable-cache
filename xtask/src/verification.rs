// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! `cargo xtask verify`: the record of the properties the formal checks
//! establish (HLR-VERIFICATION.FORMAL-01, ADR-VERIFICATION.FORMAL-01), checked
//! against the repository: each entry's test case and requirements are
//! declared in the specification, and each check's harness exists, named
//! after its test case. With `--checker`, it then runs that checker's checks
//! (FDS-VERIFICATION.RUNS-01).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Where the record is, from the workspace root.
const RECORD: &str = "verification/record.toml";

/// Where the checks' harnesses are, from the workspace root: the crate's
/// sources and its tests, which its library's test binary builds.
const HARNESS_ROOT: &str = "durable-cache";

// IMP-VERIFICATION.LAYOUT-02: satisfy SDS-VERIFICATION.LAYOUT-02

/// The record, `verification/record.toml`, whose comments describe each
/// field.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Record {
    #[serde(default)]
    check: Vec<Check>,
    #[serde(default)]
    planned: Vec<Planned>,
    #[serde(default)]
    unchecked: Vec<Unchecked>,
}

/// A property a check establishes.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Check {
    test_case: String,
    property: String,
    requirements: Vec<String>,
    checker: Checker,
    harness: String,
    bounds: String,
    runs: Runs,
    #[serde(default)]
    assumes: Vec<String>,
    duration: String,
}

/// The checkers whose checks run on the code itself, in this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Checker {
    Kani,
    Loom,
    /// An ordinary test that goes through every case within its bounds, in
    /// an `exhaustive_checks` module, for what Kani cannot follow.
    Exhaustive,
    /// The replay of a device-mapper `log-writes` log to each flush and FUA
    /// point (`cargo xtask crash-replay`), on Linux, with `sudo`.
    #[serde(rename = "log-writes")]
    #[value(name = "log-writes")]
    LogWrites,
}

impl fmt::Display for Checker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Kani => "Kani",
            Self::Loom => "loom",
            Self::Exhaustive => "exhaustive",
            Self::LogWrites => "log-writes",
        })
    }
}

/// When a check runs: on each proposed change, or once a day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Runs {
    Change,
    Daily,
}

/// A property a later phase of the plan is to check.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Planned {
    property: String,
    requirements: Vec<String>,
    checker: String,
    phase: u8,
}

/// What no check establishes, and why.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Unchecked {
    property: String,
    reason: String,
}

// IMP-TC.VERIFICATION.RECORD-01: satisfy TC-VERIFICATION.RECORD-01

/// `cargo xtask verify`: fail when the record does not match the
/// repository; return it when it does.
pub(crate) fn verify(root: &Path) -> Result<Record> {
    let path = root.join(RECORD);
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("failed to read {RECORD}"))?;
    let record: Record =
        toml::from_str(&text).with_context(|| format!("{RECORD} does not parse"))?;
    let declared = declared_ids(&root.join("docs"))?;
    let harnesses = harness_index(&root.join(HARNESS_ROOT))?;
    let problems = problems(&record, &declared, &harnesses);
    if !problems.is_empty() {
        bail!(
            "{RECORD} does not match the repository:\n  {}",
            problems.join("\n  ")
        );
    }
    let (mut kani, mut loom, mut exhaustive, mut replayed, mut daily) = (0, 0, 0, 0, 0);
    for check in &record.check {
        match check.checker {
            Checker::Kani => kani += 1,
            Checker::Loom => loom += 1,
            Checker::Exhaustive => exhaustive += 1,
            Checker::LogWrites => replayed += 1,
        }
        if matches!(check.runs, Runs::Daily) {
            daily += 1;
        }
    }
    println!(
        "verify: {RECORD} holds: {} checked ({kani} by Kani, {loom} by loom, {exhaustive} \
         exhaustively, {replayed} by log-writes replays; {daily} of them daily), {} planned, \
         {} unchecked",
        record.check.len(),
        record.planned.len(),
        record.unchecked.len()
    );
    Ok(record)
}

// IMP-VERIFICATION.RUNS-01: satisfy FDS-VERIFICATION.RUNS-01

/// The loom checks' preemption bound on each change, and in the daily run:
/// how many times a thread may be stopped before it blocks or ends. The
/// checks read it from `LOOM_MAX_PREEMPTIONS`, and fall back to the first
/// (`crate::sync::model` in the crate).
const LOOM_PREEMPTIONS: (u32, u32) = (2, 3);

/// `cargo xtask verify --checker <checker>`: check the record, compare its
/// checks for `checker` with the harnesses the checker finds, then run the
/// checks due, one at a time: those the record marks `change`, and with
/// `daily` all of them, the loom checks with a deeper bound. Report each
/// one's result and duration, on the terminal and in the job's summary on
/// GitHub. No run has a time limit (FDS-VERIFICATION.RUNS-01).
pub(crate) fn run_checks(root: &Path, checker: Checker, daily: bool) -> Result<()> {
    let record = verify(root)?;
    let found = match checker {
        Checker::Kani => kani_harnesses(root)?,
        Checker::Loom => module_tests(root, true, "loom_checks")?,
        Checker::Exhaustive => module_tests(root, false, "exhaustive_checks")?,
        Checker::LogWrites => harness_index(&root.join(HARNESS_ROOT))?
            .into_keys()
            .filter(|name| name.as_str() == REPLAY_CHECK)
            .map(|name| (name, "the log-writes replays".to_owned()))
            .collect(),
    };
    let recorded: Vec<&Check> = record
        .check
        .iter()
        .filter(|check| check.checker == checker)
        .collect();
    let differences = differences(checker, &recorded, &found);
    if !differences.is_empty() {
        bail!(
            "{RECORD} and the {checker} harnesses differ:\n  {}",
            differences.join("\n  ")
        );
    }

    let mut results = Vec::new();
    for check in due(&recorded, daily) {
        let path = &found[&check.harness];
        println!("==> {checker}: {path}");
        let mut command = match checker {
            Checker::Kani => kani_command(root, path),
            Checker::Loom => loom_command(root, path, daily),
            Checker::Exhaustive => crate_tests(root, false, &["--exact", path]),
            Checker::LogWrites => {
                let start = Instant::now();
                let passed = replay_matrix(root);
                results.push((check, passed, start.elapsed()));
                continue;
            }
        };
        let start = Instant::now();
        let status = command
            .status()
            .with_context(|| format!("failed to run {checker} on {path}"))?;
        results.push((check, status.success(), start.elapsed()));
    }

    let report = report(checker, daily, &results);
    println!("{report}");
    if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY") {
        std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&summary)
            .and_then(|mut file| file.write_all(report.as_bytes()))
            .context("failed to write the job's summary")?;
    }
    let failed = results.iter().filter(|(_, passed, _)| !passed).count();
    if failed > 0 {
        bail!("{failed} {checker} check(s) failed");
    }
    Ok(())
}

/// How the record's checks for `checker` and the harnesses the checker
/// finds, by name, differ: a harness the record lacks, or a check of the
/// record the checker does not find.
fn differences(
    checker: Checker,
    recorded: &[&Check],
    found: &BTreeMap<String, String>,
) -> Vec<String> {
    let names: BTreeSet<&str> = recorded
        .iter()
        .map(|check| check.harness.as_str())
        .collect();
    let mut differences: Vec<String> = found
        .iter()
        .filter(|(name, _)| !names.contains(name.as_str()))
        .map(|(_, path)| format!("{path} is a {checker} harness the record does not list"))
        .collect();
    differences.extend(
        names
            .iter()
            .filter(|name| !found.contains_key(**name))
            .map(|name| format!("the record's {name} is no harness {checker} finds")),
    );
    differences
}

/// The crate's test the log-writes replay runs on each state.
const REPLAY_CHECK: &str = "tc_persistence_crashsafe_03_replay_check";

/// The log-writes replays, on every file system and mode of
/// `crash_replay::MATRIX`: whether all pass. Each reports its own failure.
fn replay_matrix(root: &Path) -> bool {
    let target = match crate::target_dir(root) {
        Ok(target) => target,
        Err(e) => {
            eprintln!("{e:#}");
            return false;
        }
    };
    let mut passed = true;
    for (fs, mode) in crate::crash_replay::MATRIX {
        if let Err(e) = crate::crash_replay::crash_replay(root, &target, fs, mode) {
            eprintln!("crash-replay {} {}: {e:#}", fs.name(), mode.name());
            passed = false;
        }
    }
    passed
}

/// The checks due: those the record marks `change`, and with `daily` all.
fn due<'a>(recorded: &[&'a Check], daily: bool) -> Vec<&'a Check> {
    recorded
        .iter()
        .copied()
        .filter(|check| daily || check.runs == Runs::Change)
        .collect()
}

/// The report of a run: each check, its test case, its result, and how
/// long it took, as a Markdown table.
fn report(checker: Checker, daily: bool, results: &[(&Check, bool, Duration)]) -> String {
    let mut report = format!(
        "### {checker} checks, {}\n\n| Check | Test case | Result | Duration |\n|---|---|---|---|\n",
        if daily { "daily" } else { "on a change" }
    );
    for (check, passed, took) in results {
        report.push_str(&format!(
            "| `{}` | {} | {} | {} |\n",
            check.harness,
            check.test_case,
            if *passed { "passed" } else { "FAILED" },
            human(*took)
        ));
    }
    if results.is_empty() {
        report.push_str("| (none due) | | | |\n");
    }
    report
}

/// A duration as the record states one: seconds to a tenth under a
/// minute, then minutes and seconds.
fn human(took: Duration) -> String {
    let secs = took.as_secs_f64();
    if secs < 60.0 {
        format!("{secs:.1} s")
    } else {
        let whole = took.as_secs();
        format!("{} min {} s", whole / 60, whole % 60)
    }
}

/// The Kani harnesses of the crate, each name mapped to its path, as
/// `cargo kani list` gives them. It takes no package, so it runs in the
/// crate's directory, and writes the listing there.
fn kani_harnesses(root: &Path) -> Result<BTreeMap<String, String>> {
    let crate_dir = root.join("durable-cache");
    let status = Command::new("cargo")
        .args(["kani", "list", "-Z", "stubbing", "--format", "json"])
        .current_dir(&crate_dir)
        .status()
        .context("failed to run cargo kani list; is Kani installed?")?;
    if !status.success() {
        bail!("cargo kani list failed");
    }
    let listing = crate_dir.join("kani-list.json");
    let text = std::fs::read_to_string(&listing)
        .with_context(|| format!("failed to read {}", listing.display()))?;
    std::fs::remove_file(&listing).ok();
    let json: serde_json::Value = serde_json::from_str(&text)?;
    let mut harnesses = BTreeMap::new();
    for paths in json["standard-harnesses"]
        .as_object()
        .context("the listing names no standard harnesses")?
        .values()
    {
        for path in paths.as_array().context("a file's harnesses are no list")? {
            let path = path.as_str().context("a harness path is no string")?;
            let name = path.rsplit("::").next().unwrap_or(path);
            harnesses.insert(name.to_owned(), path.to_owned());
        }
    }
    Ok(harnesses)
}

/// `cargo kani` on the one harness at `path`.
fn kani_command(root: &Path, path: &str) -> Command {
    let mut command = Command::new("cargo");
    command
        .args(["kani", "-p", "durable-cache", "-Z", "stubbing"])
        .args(["--harness", path, "--exact"])
        .current_dir(root);
    command
}

/// `cargo test` of the crate's library, in a release build, with loom's cfg
/// if `loom`, and the arguments that follow its `--`.
fn crate_tests(root: &Path, loom: bool, test_args: &[&str]) -> Command {
    let mut command = Command::new("cargo");
    command
        .args(["test", "-p", "durable-cache", "--lib", "--release", "--"])
        .args(test_args)
        .current_dir(root);
    if loom {
        let rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
        command.env(
            "RUSTFLAGS",
            format!("{rustflags} --cfg durable_cache_loom").trim_start(),
        );
    }
    command
}

/// The checks of the crate's tests in the modules named `module`, each name
/// mapped to its path, as the test binary lists them, built with loom's cfg
/// if `loom`.
fn module_tests(root: &Path, loom: bool, module: &str) -> Result<BTreeMap<String, String>> {
    let output = crate_tests(root, loom, &["--list"])
        .output()
        .with_context(|| format!("failed to list the tests of the {module} modules"))?;
    if !output.status.success() {
        bail!(
            "listing the tests of the {module} modules failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let segment = format!("::{module}::");
    Ok(String::from_utf8(output.stdout)?
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .filter(|path| path.contains(&segment))
        .map(|path| {
            let name = path.rsplit("::").next().unwrap_or(path);
            (name.to_owned(), path.to_owned())
        })
        .collect())
}

/// The loom check at `path`, with the preemption bound of the run.
fn loom_command(root: &Path, path: &str, daily: bool) -> Command {
    let bound = if daily {
        LOOM_PREEMPTIONS.1
    } else {
        LOOM_PREEMPTIONS.0
    };
    let mut command = crate_tests(root, true, &["--exact", path]);
    command.env("LOOM_MAX_PREEMPTIONS", bound.to_string());
    command
}

/// What is wrong with `record`, given the IDs the specification declares
/// and how many functions of each `tc_` name the harnesses' sources hold.
fn problems(
    record: &Record,
    declared: &BTreeSet<String>,
    harnesses: &BTreeMap<String, usize>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let mut undeclared = |what: &str, ids: &[String]| {
        for id in ids.iter().filter(|id| !declared.contains(*id)) {
            problems.push(format!(
                "{what} names {id}, which the specification does not declare"
            ));
        }
    };
    for check in &record.check {
        let what = format!("the check {}", check.harness);
        undeclared(&what, std::slice::from_ref(&check.test_case));
        undeclared(&what, &check.requirements);
    }
    for planned in &record.planned {
        undeclared(
            &format!("the planned \"{}\"", planned.property),
            &planned.requirements,
        );
    }

    let mut seen = BTreeSet::new();
    for check in &record.check {
        let slug = format!(
            "{}_",
            check.test_case.to_lowercase().replace(['-', '.'], "_")
        );
        if !check.harness.starts_with(&slug) {
            problems.push(format!(
                "the check {} is not named after its test case {}: its name starts {slug}",
                check.harness, check.test_case
            ));
        }
        match harnesses.get(&check.harness) {
            None => problems.push(format!(
                "the check {} has no function of that name under {HARNESS_ROOT}",
                check.harness
            )),
            Some(1) => {}
            Some(n) => problems.push(format!(
                "the check {} names {n} functions under {HARNESS_ROOT}, not one",
                check.harness
            )),
        }
        if !seen.insert(&check.harness) {
            problems.push(format!("the check {} is recorded twice", check.harness));
        }
        for (field, value) in [
            ("property", &check.property),
            ("bounds", &check.bounds),
            ("duration", &check.duration),
        ] {
            if value.trim().is_empty() {
                problems.push(format!("the check {} has an empty {field}", check.harness));
            }
        }
        if check.requirements.is_empty() {
            problems.push(format!("the check {} names no requirement", check.harness));
        }
        if check
            .assumes
            .iter()
            .any(|assumption| assumption.trim().is_empty())
        {
            problems.push(format!(
                "the check {} lists an empty assumption",
                check.harness
            ));
        }
    }
    for planned in &record.planned {
        if !(1..=5).contains(&planned.phase) {
            problems.push(format!(
                "the planned \"{}\" names phase {}, which the plan does not have",
                planned.property, planned.phase
            ));
        }
        if planned.requirements.is_empty() || planned.checker.trim().is_empty() {
            problems.push(format!(
                "the planned \"{}\" names no requirement or no checker",
                planned.property
            ));
        }
    }
    for unchecked in &record.unchecked {
        if unchecked.reason.trim().is_empty() {
            problems.push(format!(
                "\"{}\" is unchecked with no reason",
                unchecked.property
            ));
        }
    }
    problems
}

/// The IDs the specification declares, each in a `\begin{requirement}{ID}`
/// of a `.tex` file under `docs`.
fn declared_ids(docs: &Path) -> Result<BTreeSet<String>> {
    const OPENING: &str = "\\begin{requirement}{";
    let mut ids = BTreeSet::new();
    for entry in
        std::fs::read_dir(docs).with_context(|| format!("failed to list {}", docs.display()))?
    {
        let path = entry?.path();
        if path.extension().is_none_or(|e| e != "tex") {
            continue;
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        for line in text.lines() {
            if let Some(id) = line
                .trim_start()
                .strip_prefix(OPENING)
                .and_then(|rest| rest.split_once('}'))
                .map(|(id, _)| id)
            {
                ids.insert(id.to_owned());
            }
        }
    }
    Ok(ids)
}

/// How many functions of each `tc_` name the Rust files under `dir` declare.
fn harness_index(dir: &Path) -> Result<BTreeMap<String, usize>> {
    let mut index = BTreeMap::new();
    for entry in walkdir::WalkDir::new(dir) {
        let entry = entry?;
        if entry.path().extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(entry.path())
            .with_context(|| format!("failed to read {}", entry.path().display()))?;
        for line in text.lines() {
            let mut decl = line.trim_start();
            for prefix in ["pub(crate) ", "pub ", "async ", "unsafe ", "const "] {
                decl = decl.strip_prefix(prefix).unwrap_or(decl);
            }
            if let Some(name) = decl
                .strip_prefix("fn ")
                .and_then(|rest| rest.split(['(', '<']).next())
                .filter(|name| name.starts_with("tc_"))
            {
                *index.entry(name.to_owned()).or_default() += 1;
            }
        }
    }
    Ok(index)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record of one check, as the repository might hold it.
    const ONE_CHECK: &str = r#"
        [[check]]
        test_case = "TC-LAYOUT.KEYSEGMENT-01"
        property = "Decoding an encoded key gives the key back."
        requirements = ["SDS-LAYOUT.KEYSEGMENT-01"]
        checker = "kani"
        harness = "tc_layout_keysegment_01_round_trip"
        bounds = "keys of up to 8 bytes of tag"
        runs = "change"
        duration = "1 s (example)"

        [[planned]]
        property = "Every index the bitset computes is in range."
        requirements = ["FDS-LAYOUT.STARTUP-01"]
        checker = "kani"
        phase = 1

        [[unchecked]]
        property = "The kernel's side of io_uring."
        reason = "The kernel completes the ring's writes."
    "#;

    fn declared() -> BTreeSet<String> {
        [
            "TC-LAYOUT.KEYSEGMENT-01",
            "SDS-LAYOUT.KEYSEGMENT-01",
            "FDS-LAYOUT.STARTUP-01",
        ]
        .map(String::from)
        .into()
    }

    fn harnesses(names: &[&str]) -> BTreeMap<String, usize> {
        names.iter().map(|name| (name.to_string(), 1)).collect()
    }

    fn parse(text: &str) -> Record {
        toml::from_str(text).expect("the test's record parses")
    }

    /// A record whose every entry matches the repository holds.
    #[test]
    fn tc_verification_record_01_a_matching_record_holds() {
        let problems = problems(
            &parse(ONE_CHECK),
            &declared(),
            &harnesses(&["tc_layout_keysegment_01_round_trip"]),
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    /// A check whose harness the sources do not hold, or hold twice, fails.
    #[test]
    fn tc_verification_record_01_a_check_needs_its_harness_once() {
        let missing = problems(&parse(ONE_CHECK), &declared(), &harnesses(&[]));
        assert!(
            missing.iter().any(|p| p.contains("has no function")),
            "{missing:?}"
        );
        let mut twice = harnesses(&[]);
        twice.insert("tc_layout_keysegment_01_round_trip".to_owned(), 2);
        let doubled = problems(&parse(ONE_CHECK), &declared(), &twice);
        assert!(
            doubled.iter().any(|p| p.contains("names 2 functions")),
            "{doubled:?}"
        );
    }

    /// A check not named after its test case fails.
    #[test]
    fn tc_verification_record_01_a_check_is_named_after_its_test_case() {
        let renamed = ONE_CHECK.replace(
            "tc_layout_keysegment_01_round_trip",
            "tc_layout_sharding_01_round_trip",
        );
        let problems = problems(
            &parse(&renamed),
            &declared(),
            &harnesses(&["tc_layout_sharding_01_round_trip"]),
        );
        assert!(
            problems.iter().any(|p| p.contains("not named after")),
            "{problems:?}"
        );
    }

    /// An entry naming an ID the specification does not declare fails, as
    /// does an unchecked property with no reason.
    #[test]
    fn tc_verification_record_01_entries_name_declared_ids_and_reasons() {
        let undeclared = ONE_CHECK.replace("FDS-LAYOUT.STARTUP-01", "FDS-LAYOUT.STARTUP-09");
        let problems_found = problems(
            &parse(&undeclared),
            &declared(),
            &harnesses(&["tc_layout_keysegment_01_round_trip"]),
        );
        assert!(
            problems_found
                .iter()
                .any(|p| p.contains("FDS-LAYOUT.STARTUP-09")),
            "{problems_found:?}"
        );
        let no_reason = ONE_CHECK.replace("The kernel completes the ring's writes.", " ");
        let problems_found = problems(
            &parse(&no_reason),
            &declared(),
            &harnesses(&["tc_layout_keysegment_01_round_trip"]),
        );
        assert!(
            problems_found.iter().any(|p| p.contains("with no reason")),
            "{problems_found:?}"
        );
    }

    /// A field the record does not define fails to parse, so a misspelt
    /// field cannot pass unnoticed.
    #[test]
    fn tc_verification_record_01_unknown_fields_fail() {
        let misspelt = ONE_CHECK.replace("bounds =", "bound =");
        assert!(toml::from_str::<Record>(&misspelt).is_err());
    }

    /// The repository's own record holds.
    #[test]
    fn tc_verification_record_01_the_repository_record_holds() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask has a parent directory");
        verify(root).expect("the repository's record holds");
    }

    // IMP-TC.VERIFICATION.RUNS-01: satisfy TC-VERIFICATION.RUNS-01

    /// Two checks, one for each change and one daily.
    const TWO_CHECKS: &str = r#"
        [[check]]
        test_case = "TC-STORAGE.CHANGELOCKS-01"
        property = "One change per key at a time."
        requirements = ["FDS-COMPAT.READONLY-02"]
        checker = "loom"
        harness = "tc_storage_changelocks_01_one_change_per_key"
        bounds = "two threads"
        runs = "change"
        duration = "0.1 s (example)"

        [[check]]
        test_case = "TC-STORAGE.CHANGELOCKS-01"
        property = "The gate admits no change once read-only."
        requirements = ["FDS-COMPAT.READONLY-02"]
        checker = "loom"
        harness = "tc_storage_changelocks_01_deeper"
        bounds = "three threads"
        runs = "daily"
        duration = "9 s (example)"
    "#;

    fn found(names: &[&str]) -> BTreeMap<String, String> {
        names
            .iter()
            .map(|name| (name.to_string(), format!("locks::loom_checks::{name}")))
            .collect()
    }

    /// A harness the checker finds that the record lacks, and a check of the
    /// record the checker does not find, are both reported, by name.
    #[test]
    fn tc_verification_runs_01_the_record_and_the_harnesses_must_agree() {
        let record = parse(TWO_CHECKS);
        let recorded: Vec<&Check> = record.check.iter().collect();
        let agree = found(&[
            "tc_storage_changelocks_01_one_change_per_key",
            "tc_storage_changelocks_01_deeper",
        ]);
        assert!(differences(Checker::Loom, &recorded, &agree).is_empty());
        let differ = found(&[
            "tc_storage_changelocks_01_one_change_per_key",
            "tc_storage_changelocks_01_unrecorded",
        ]);
        let differences = differences(Checker::Loom, &recorded, &differ);
        assert_eq!(differences.len(), 2, "{differences:?}");
        assert!(differences
            .iter()
            .any(|d| d.contains("_unrecorded is a loom harness")));
        assert!(differences
            .iter()
            .any(|d| d.contains("_deeper is no harness")));
    }

    /// A run on a change runs the checks marked `change`; a daily run runs
    /// all of them.
    #[test]
    fn tc_verification_runs_01_a_change_runs_its_checks_and_a_day_all() {
        let record = parse(TWO_CHECKS);
        let recorded: Vec<&Check> = record.check.iter().collect();
        let names = |checks: Vec<&Check>| -> Vec<String> {
            checks.iter().map(|check| check.harness.clone()).collect()
        };
        assert_eq!(
            names(due(&recorded, false)),
            ["tc_storage_changelocks_01_one_change_per_key"]
        );
        assert_eq!(due(&recorded, true).len(), 2);
    }

    /// The report gives each check run its test case, its result and its
    /// duration, and says when none was due.
    #[test]
    fn tc_verification_runs_01_the_report_gives_each_result_and_duration() {
        let record = parse(TWO_CHECKS);
        let results = [
            (&record.check[0], true, Duration::from_millis(1_250)),
            (&record.check[1], false, Duration::from_secs(125)),
        ];
        let daily = report(Checker::Loom, true, &results);
        assert!(daily.contains(
            "| `tc_storage_changelocks_01_one_change_per_key` | TC-STORAGE.CHANGELOCKS-01 | passed | 1.2 s |"
        ), "{daily}");
        assert!(daily.contains("| FAILED | 2 min 5 s |"), "{daily}");
        assert!(report(Checker::Kani, false, &[]).contains("(none due)"));
    }
}
