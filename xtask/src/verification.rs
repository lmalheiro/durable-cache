// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! `cargo xtask verify`: the record of the properties the formal checks
//! establish (HLR-VERIFICATION.FORMAL-01, ADR-VERIFICATION.FORMAL-01), checked
//! against the repository: each entry's test case and requirements are
//! declared in the specification, and each check's harness exists, named
//! after its test case.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// Where the record is, from the workspace root.
const RECORD: &str = "verification/record.toml";

/// Where the checks' harnesses are, from the workspace root.
const HARNESS_ROOT: &str = "durable-cache/src";

// IMP-VERIFICATION.LAYOUT-01: satisfy SDS-VERIFICATION.LAYOUT-01

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
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Checker {
    Kani,
    Loom,
}

/// When a check runs: on each proposed change, or once a day.
#[derive(Debug, Clone, Copy, Deserialize)]
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
/// repository.
pub(crate) fn verify(root: &Path) -> Result<()> {
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
    let (mut kani, mut loom, mut daily) = (0, 0, 0);
    for check in &record.check {
        match check.checker {
            Checker::Kani => kani += 1,
            Checker::Loom => loom += 1,
        }
        if matches!(check.runs, Runs::Daily) {
            daily += 1;
        }
    }
    println!(
        "verify: {RECORD} holds: {} checked ({kani} by Kani, {loom} by loom; {daily} of them \
         daily), {} planned, {} unchecked",
        record.check.len(),
        record.planned.len(),
        record.unchecked.len()
    );
    Ok(())
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
}
