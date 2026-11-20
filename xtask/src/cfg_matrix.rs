// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! `cargo xtask cfg-matrix`: lint and test durable-cache in every build its
//! `#[cfg]` gates select that this host can compile, Linux and other Unix
//! systems alike.
//!
//! The Linux side runs in the workspace itself: clippy, on the library and
//! its tests, for each feature set ([`feature_sets`]), then once more in a
//! release build, which turns `debug_assertions` off; and nextest, with the
//! default features and with all of them.
//!
//! The other-Unix side runs the same steps on a copy of the workspace under
//! the target directory, in which durable-cache's Linux gates select their
//! other side. Every `target_os = "linux"` in its sources and tests becomes
//! `all(target_os = "linux", not(feature = "as-if-not-linux"))`, and every
//! `io_uring` in a gate, the `cfg` durable-cache's build script sets on Linux
//! where stores can write through an `io_uring` ring, becomes
//! `all(io_uring, not(feature = "as-if-not-linux"))`. The copy's manifest
//! declares that feature, and every step enables it. The manifest
//! also drops the Linux-only dependencies, so that code outside the Linux
//! gates cannot use them. What this cannot catch is such code calling what
//! only Linux has in std (`std::os::linux`) or in libc: the copy still
//! builds for Linux.
//!
//! Every step's whole output goes to a log of its own, kept until the next
//! run: a failing test's panic is printed where it happens, mid-run.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use walkdir::WalkDir;

/// The predicate the Linux gates test.
const LINUX: &str = r#"target_os = "linux""#;

/// The `cfg` durable-cache's build script sets on Linux where stores can
/// write through an `io_uring` ring. Its gates hold it in parentheses,
/// `cfg(io_uring)` or `not(io_uring)`, which is how [`rewrite`] tells it
/// from the names that contain it.
const RING: &str = "io_uring";

/// The feature that turns the copy's Linux gates off.
const AS_IF_NOT_LINUX: &str = "as-if-not-linux";

/// A set of durable-cache's features, as cargo's flags select it.
struct FeatureSet {
    /// How the step names it.
    label: String,
    /// The default features are off.
    no_default: bool,
    /// Features named on top.
    features: Vec<String>,
    /// Every feature is on.
    all: bool,
}

impl FeatureSet {
    /// Cargo's flags for this set, with `extra` features on top of it. With
    /// every feature on, `--all-features` alone: the manifest declares the
    /// `extra` features, so they are on too.
    fn args(&self, extra: &[&str]) -> Vec<String> {
        if self.all {
            return vec!["--all-features".to_owned()];
        }
        let mut args = Vec::new();
        if self.no_default {
            args.push("--no-default-features".to_owned());
        }
        let features: Vec<&str> = self
            .features
            .iter()
            .map(String::as_str)
            .chain(extra.iter().copied())
            .collect();
        if !features.is_empty() {
            args.push("--features".to_owned());
            args.push(features.join(","));
        }
        args
    }
}

/// The feature sets each side checks, from durable-cache's manifest: none,
/// the default, each feature outside the default on top of it, and all of
/// them.
fn feature_sets(root: &Path) -> Result<Vec<FeatureSet>> {
    let output = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .current_dir(root)
        .output()
        .context("failed to run cargo metadata")?;
    if !output.status.success() {
        bail!("cargo metadata failed");
    }
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    let features = metadata["packages"]
        .as_array()
        .and_then(|packages| {
            packages
                .iter()
                .find(|package| package["name"] == "durable-cache")
        })
        .and_then(|package| package["features"].as_object())
        .context("cargo metadata lists no features of durable-cache")?;
    let default: BTreeSet<&str> = features
        .get("default")
        .and_then(|default| default.as_array())
        .map(|default| default.iter().filter_map(|f| f.as_str()).collect())
        .unwrap_or_default();
    let mut sets = vec![
        FeatureSet {
            label: "no default features".to_owned(),
            no_default: true,
            features: Vec::new(),
            all: false,
        },
        FeatureSet {
            label: "default features".to_owned(),
            no_default: false,
            features: Vec::new(),
            all: false,
        },
    ];
    for name in features.keys() {
        if name != "default" && !default.contains(name.as_str()) {
            sets.push(FeatureSet {
                label: name.clone(),
                no_default: false,
                features: vec![name.clone()],
                all: false,
            });
        }
    }
    sets.push(FeatureSet {
        label: "all features".to_owned(),
        no_default: false,
        features: Vec::new(),
        all: true,
    });
    Ok(sets)
}

/// One cargo command of the matrix.
struct Step {
    label: String,
    dir: PathBuf,
    args: Vec<String>,
}

/// The steps of one side, run in `dir`, each with the `extra` features.
fn side_steps(
    side: &str,
    dir: &Path,
    sets: &[FeatureSet],
    extra: &[&str],
    tests: bool,
) -> Vec<Step> {
    let step = |what: &str, args: Vec<String>| Step {
        label: format!("{side}: {what}"),
        dir: dir.to_path_buf(),
        args,
    };
    let clippy = |set: &FeatureSet, release: bool| {
        let mut args = ["clippy", "-p", "durable-cache", "--all-targets"]
            .map(String::from)
            .to_vec();
        if release {
            args.push("--release".to_owned());
        }
        args.extend(set.args(extra));
        args.extend(["--", "-D", "warnings"].map(String::from));
        args
    };
    let mut steps: Vec<Step> = sets
        .iter()
        .map(|set| step(&format!("clippy, {}", set.label), clippy(set, false)))
        .collect();
    let default = sets
        .iter()
        .find(|set| !set.no_default && !set.all && set.features.is_empty())
        .expect("feature_sets always has the default set");
    let all = sets
        .iter()
        .find(|set| set.all)
        .expect("feature_sets always has the set of all features");
    steps.push(step("clippy, release build", clippy(default, true)));
    if tests {
        for set in [default, all] {
            let mut args = ["nextest", "run", "-p", "durable-cache", "--no-fail-fast"]
                .map(String::from)
                .to_vec();
            args.extend(set.args(extra));
            steps.push(step(&format!("nextest, {}", set.label), args));
        }
    }
    steps
}

/// `cargo xtask cfg-matrix`.
pub(crate) fn cfg_matrix(root: &Path, tests: bool) -> Result<()> {
    let target = crate::target_dir(root)?;
    let work = target.join("xtask").join("cfg-matrix");
    let copy = work.join("other-unix");
    let logs = work.join("logs");
    if logs.exists() {
        fs::remove_dir_all(&logs).with_context(|| format!("failed to empty {}", logs.display()))?;
    }
    fs::create_dir_all(&logs)?;
    sync_copy(root, &copy)?;

    let sets = feature_sets(root)?;
    let mut steps = side_steps("linux", root, &sets, &[], tests);
    steps.extend(side_steps(
        "other unix",
        &copy,
        &sets,
        &[AS_IF_NOT_LINUX],
        tests,
    ));

    let mut failed = Vec::new();
    for (n, step) in steps.iter().enumerate() {
        let name: String = step
            .label
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let log_path = logs.join(format!("{:02}-{name}.log", n + 1));
        let log = File::create(&log_path)
            .with_context(|| format!("failed to create {}", log_path.display()))?;
        let start = Instant::now();
        let status = Command::new("cargo")
            .args(&step.args)
            .current_dir(&step.dir)
            .env("CARGO_TARGET_DIR", &target)
            .stdout(log.try_clone()?)
            .stderr(log)
            .status()
            .with_context(|| format!("failed to run cargo for {}", step.label))?;
        let verdict = if status.success() { "ok    " } else { "FAILED" };
        println!(
            "{verdict} {} ({:.0} s)",
            step.label,
            start.elapsed().as_secs_f64()
        );
        if !status.success() {
            failed.push(log_path);
        }
    }
    if !failed.is_empty() {
        bail!(
            "{} of {} steps failed; their whole logs:\n  {}",
            failed.len(),
            steps.len(),
            failed
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join("\n  ")
        );
    }
    println!("cfg-matrix: all {} steps passed", steps.len());
    Ok(())
}

/// Bring the other-Unix copy at `copy` up to date with the working tree at
/// `root`: every file git tracks or would track, except the specification
/// under `docs/` and the submodules, which no build reads. durable-cache's
/// sources, tests and manifest are rewritten ([`rewrite`]). The files gone
/// from the working tree leave the copy first, so that a path that turned
/// from a directory into a file, or back, finds its place free. A file is
/// written only when its content changes, so that cargo rebuilds only what
/// did.
fn sync_copy(root: &Path, copy: &Path) -> Result<()> {
    let listing = Command::new("git")
        .args([
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ])
        .current_dir(root)
        .output()
        .context("failed to run git ls-files")?;
    if !listing.status.success() {
        bail!("git ls-files failed");
    }
    let mut files = BTreeMap::new();
    let mut rewritten_sources = 0;
    for path in String::from_utf8(listing.stdout)?.split('\0') {
        if path.is_empty() || path.starts_with("docs/") {
            continue;
        }
        let source = root.join(path);
        // A file deleted from the working tree but not yet from the index,
        // or a submodule, which is a directory.
        let Ok(metadata) = fs::metadata(&source) else {
            continue;
        };
        if metadata.is_dir() {
            continue;
        }
        let content =
            fs::read(&source).with_context(|| format!("failed to read {}", source.display()))?;
        let content = match rewrite(path, &content)? {
            Some(text) => {
                if path.ends_with(".rs") {
                    rewritten_sources += 1;
                }
                text.into_bytes()
            }
            None => content,
        };
        files.insert(PathBuf::from(path), content);
    }
    if rewritten_sources == 0 {
        bail!(
            "no `{LINUX}` gate found under durable-cache/src or durable-cache/tests: \
             update xtask/src/cfg_matrix.rs, or the other side checks nothing"
        );
    }
    if copy.exists() {
        for entry in WalkDir::new(copy).min_depth(1).contents_first(true) {
            let entry = entry?;
            let relative = entry.path().strip_prefix(copy)?;
            let removed = if entry.file_type().is_dir() {
                // Only an empty directory goes: one that still holds files
                // refuses, which is what is wanted.
                match fs::remove_dir(entry.path()) {
                    Err(e) if e.kind() == io::ErrorKind::DirectoryNotEmpty => Ok(()),
                    removed => removed,
                }
            } else if files.contains_key(relative) {
                Ok(())
            } else {
                fs::remove_file(entry.path())
            };
            removed.with_context(|| format!("failed to remove {}", entry.path().display()))?;
        }
    }
    for (path, content) in &files {
        let destination = copy.join(path);
        if fs::read(&destination).ok().as_deref() != Some(content.as_slice()) {
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&destination, content)
                .with_context(|| format!("failed to write {}", destination.display()))?;
        }
    }
    Ok(())
}

/// The copy's text of the file at `path`, if it differs from the working
/// tree's: durable-cache's sources and tests with their Linux gates, the
/// [`RING`] ones among them, turned off by [`AS_IF_NOT_LINUX`], and its
/// manifest declaring that feature, with its Linux-only tables
/// (dependencies, dev- and build-dependencies) for no target.
fn rewrite(path: &str, content: &[u8]) -> Result<Option<String>> {
    let source = (path.starts_with("durable-cache/src/")
        || path.starts_with("durable-cache/tests/"))
        && path.ends_with(".rs");
    if source {
        let text = std::str::from_utf8(content).with_context(|| format!("{path} is not UTF-8"))?;
        let ring = format!("({RING})");
        if !text.contains(LINUX) && !text.contains(&ring) {
            return Ok(None);
        }
        let off =
            |predicate: &str| format!(r#"all({predicate}, not(feature = "{AS_IF_NOT_LINUX}"))"#);
        let text = text
            .replace(LINUX, &off(LINUX))
            .replace(&ring, &format!("({})", off(RING)));
        return Ok(Some(text));
    }
    if path == "durable-cache/Cargo.toml" {
        let text = std::str::from_utf8(content).with_context(|| format!("{path} is not UTF-8"))?;
        let features = "\n[features]\n";
        if !text.contains(features) {
            bail!("{path} has no [features] table to declare {AS_IF_NOT_LINUX} in");
        }
        // `any()` holds on no target, and turns every table for Linux, or
        // for Linux on some architectures, into one for none.
        let text: String = text
            .split_inclusive('\n')
            .map(|line| {
                if line.starts_with("[target.'cfg(") {
                    line.replace(LINUX, "any()")
                } else {
                    line.to_owned()
                }
            })
            .collect();
        let text = text.replace(features, &format!("{features}{AS_IF_NOT_LINUX} = []\n"));
        return Ok(Some(text));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both sides of a Linux gate select the other side, in durable-cache's
    /// sources and tests only.
    #[test]
    fn the_linux_gates_select_their_other_side() {
        let source = "#[cfg(target_os = \"linux\")]\nmod a;\n\
                      #[cfg(not(target_os = \"linux\"))]\nmod b;\n";
        let rewritten = rewrite("durable-cache/src/lib.rs", source.as_bytes())
            .expect("a source")
            .expect("its gates are rewritten");
        assert_eq!(
            rewritten,
            "#[cfg(all(target_os = \"linux\", not(feature = \"as-if-not-linux\")))]\nmod a;\n\
             #[cfg(not(all(target_os = \"linux\", not(feature = \"as-if-not-linux\"))))]\nmod b;\n"
        );
        let test = rewrite("durable-cache/tests/support/mod.rs", source.as_bytes());
        assert!(
            test.expect("a test").is_some(),
            "the tests are rewritten too"
        );
        let other = rewrite("xtask/src/main.rs", source.as_bytes());
        assert!(
            other.expect("another crate").is_none(),
            "other crates are left"
        );
        let ungated = rewrite("durable-cache/src/lib.rs", b"mod a;\n");
        assert!(ungated.expect("a source").is_none(), "nothing to rewrite");
    }

    /// Both sides of an `io_uring` gate select the other side, in a file
    /// with no other Linux gate, and the names that merely contain
    /// `io_uring` are left.
    #[test]
    fn the_ring_gates_select_their_other_side() {
        let source = "#[cfg(io_uring)]\nmod uring;\n\
                      #[cfg(not(io_uring))]\nmod noop_uring;\n\
                      fn on(c: Config) -> bool { c.io_uring() && with_io_uring(c) }\n";
        let rewritten = rewrite("durable-cache/src/lib.rs", source.as_bytes())
            .expect("a source")
            .expect("its gates are rewritten");
        assert_eq!(
            rewritten,
            "#[cfg(all(io_uring, not(feature = \"as-if-not-linux\")))]\nmod uring;\n\
             #[cfg(not(all(io_uring, not(feature = \"as-if-not-linux\"))))]\nmod noop_uring;\n\
             fn on(c: Config) -> bool { c.io_uring() && with_io_uring(c) }\n"
        );
    }

    /// The copy's manifest declares the feature, and its Linux-only tables
    /// hold for no target.
    #[test]
    fn the_manifest_declares_the_feature_and_drops_the_linux_tables() {
        let manifest = "[dependencies]\nlog = \"0.4\"\n\n\
                        [target.'cfg(target_os = \"linux\")'.dependencies]\nrustix = \"1\"\n\n\
                        [target.'cfg(all(target_os = \"linux\", target_arch = \"x86_64\"))'.dependencies]\n\
                        io-uring = \"0.7\"\n\n\
                        [target.'cfg(target_os = \"linux\")'.dev-dependencies]\nlibc = \"0.2\"\n\n\
                        [features]\ndefault = []\n";
        let rewritten = rewrite("durable-cache/Cargo.toml", manifest.as_bytes())
            .expect("a manifest")
            .expect("it is rewritten");
        assert_eq!(
            rewritten,
            "[dependencies]\nlog = \"0.4\"\n\n\
             [target.'cfg(any())'.dependencies]\nrustix = \"1\"\n\n\
             [target.'cfg(all(any(), target_arch = \"x86_64\"))'.dependencies]\n\
             io-uring = \"0.7\"\n\n\
             [target.'cfg(any())'.dev-dependencies]\nlibc = \"0.2\"\n\n\
             [features]\nas-if-not-linux = []\ndefault = []\n"
        );
        let without_features = rewrite("durable-cache/Cargo.toml", b"[dependencies]\n");
        assert!(
            without_features.is_err(),
            "no table to declare the feature in"
        );
    }

    fn set(no_default: bool, features: &[&str], all: bool) -> FeatureSet {
        FeatureSet {
            label: String::new(),
            no_default,
            features: features.iter().map(|f| f.to_string()).collect(),
            all,
        }
    }

    /// Each feature set's flags, with an extra feature on top.
    #[test]
    fn feature_sets_turn_into_cargo_flags() {
        assert!(set(false, &[], false).args(&[]).is_empty());
        assert_eq!(
            set(true, &[], false).args(&["x"]),
            ["--no-default-features", "--features", "x"]
        );
        assert_eq!(
            set(false, &["metrics"], false).args(&["x"]),
            ["--features", "metrics,x"]
        );
        assert_eq!(set(false, &[], true).args(&["x"]), ["--all-features"]);
    }
}
