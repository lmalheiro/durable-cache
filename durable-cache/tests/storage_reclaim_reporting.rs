// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-STORAGE.RECLAIM-01, steps 6 to 9 — what the scan reports.
//!
//! The test runs in a process of its own (`support::in_own_process`): the
//! log and the `prometheus` default registry are process-wide, so a scan
//! running in parallel in the same process would be indistinguishable from
//! this one's.

use super::support;

use durable_cache::recovery::recover;
use pingora_cache::eviction::simple_lru::Manager;

// IMP-TC.STORAGE.RECLAIM-02: satisfy TC-STORAGE.RECLAIM-01 -- the reporting
// half; storage_reclaim.rs covers the classification steps.

/// Steps 6 and 7: the scan logs one summary line giving each class's files
/// and bytes, and — only with the `metrics` feature — raises the reclaim
/// counters by the same figures.
#[tokio::test]
async fn tc_storage_reclaim_01_reports_every_class() {
    if !support::in_own_process(module_path!(), "tc_storage_reclaim_01_reports_every_class") {
        return;
    }
    let logs = support::LogCapture::install();
    let (cache, _tmp) = support::leaked_cache();
    let temp = support::forge_temp(
        cache,
        "bbbb0000000000000000000000000000",
        &support::writer_without_lease(),
        1,
    );
    let temp_bytes = std::fs::metadata(&temp).unwrap().len();

    let damaged = support::key("/report-damaged");
    support::store(cache, &damaged, &support::meta(), b"damaged").await;
    let damaged_bytes = std::fs::metadata(support::canonical_path(cache, &damaged))
        .unwrap()
        .len();
    support::flip_magic_byte(&support::canonical_path(cache, &damaged));

    let withdrawn = support::key("/report-withdrawn");
    support::store(cache, &withdrawn, &support::meta(), b"withdrawn").await;
    let withdrawn_path = support::canonical_path(cache, &withdrawn);
    let withdrawn_bytes = std::fs::metadata(&withdrawn_path).unwrap().len();
    support::set_format_version(&withdrawn_path, durable_cache::format::WITHDRAWN_VERSION);

    let before = [
        support::counter_value(
            "durable_cache_reclaimed_files_total",
            &[("class", "temporary")],
        ),
        support::counter_value(
            "durable_cache_reclaimed_files_total",
            &[("class", "damaged")],
        ),
        support::counter_value(
            "durable_cache_reclaimed_files_total",
            &[("class", "withdrawn")],
        ),
    ];

    logs.clear();
    let manager = Manager::new(usize::MAX);
    let report = recover(cache, &manager).await.unwrap();

    assert_eq!(
        (
            report.reclaimed_temps,
            report.reclaimed_damaged,
            report.reclaimed_withdrawn
        ),
        (1, 1, 1)
    );
    assert_eq!(report.reclaimed_temp_bytes, temp_bytes);
    assert_eq!(report.reclaimed_damaged_bytes, damaged_bytes);
    assert_eq!(report.reclaimed_withdrawn_bytes, withdrawn_bytes);

    // One summary line per scan. Other tests in this binary log their own
    // through the same process-wide logger, so this scan's is identified by
    // the figures it reports, all of which must be on one line.
    let fragments = [
        format!("1 temporary ({temp_bytes} bytes)"),
        format!("1 damaged ({damaged_bytes} bytes)"),
        format!("1 withdrawn ({withdrawn_bytes} bytes)"),
    ];
    let summaries = logs.matching("restart scan complete");
    assert_eq!(
        summaries
            .iter()
            .filter(|line| fragments.iter().all(|f| line.contains(f.as_str())))
            .count(),
        1,
        "exactly one summary line must report every class of this scan ({fragments:?}): {summaries:?}"
    );

    let files = |class: &str| {
        support::counter_value("durable_cache_reclaimed_files_total", &[("class", class)])
    };
    let bytes = |class: &str| {
        support::counter_value("durable_cache_reclaimed_bytes_total", &[("class", class)])
    };
    if cfg!(feature = "metrics") {
        for (i, class) in ["temporary", "damaged", "withdrawn"].iter().enumerate() {
            assert_eq!(
                files(class).unwrap() - before[i].unwrap_or(0.0),
                1.0,
                "{class} files counter"
            );
            assert!(bytes(class).unwrap() > 0.0, "{class} bytes counter");
        }
    } else {
        // Without the feature the log summary above is the only record.
        for class in ["temporary", "damaged", "withdrawn"] {
            assert_eq!(files(class), None, "no counter may be registered");
            assert_eq!(bytes(class), None);
        }
    }
}

/// Steps 6 and 7, before the scan: once the backend is built, both reclaim
/// counters are served, with every class at zero, rather than from the
/// first reclaim of that class; without the `metrics` feature neither is
/// registered.
#[test]
fn tc_storage_reclaim_01_the_counters_read_zero_once_the_backend_is_built() {
    if !support::in_own_process(
        module_path!(),
        "tc_storage_reclaim_01_the_counters_read_zero_once_the_backend_is_built",
    ) {
        return;
    }
    let (_cache, _tmp) = support::leaked_cache();
    let expected = cfg!(feature = "metrics").then_some(0.0);
    for counter in [
        "durable_cache_reclaimed_files_total",
        "durable_cache_reclaimed_bytes_total",
    ] {
        for class in ["temporary", "damaged", "withdrawn"] {
            assert_eq!(
                support::counter_value(counter, &[("class", class)]),
                expected,
                "{counter}, class {class}"
            );
        }
    }
}

/// Step 8: a scan of a read-only backend says in its summary line that it
/// stopped, rather than that it completed.
#[tokio::test]
async fn tc_storage_reclaim_01_a_read_only_scan_says_it_stopped() {
    if !support::in_own_process(
        module_path!(),
        "tc_storage_reclaim_01_a_read_only_scan_says_it_stopped",
    ) {
        return;
    }
    let logs = support::LogCapture::install();
    let (cache, _tmp) = support::leaked_cache();
    let key = support::key("/report-read-only");
    support::store(cache, &key, &support::meta(), b"body").await;
    cache.set_read_only();

    logs.clear();
    let manager = Manager::new(usize::MAX);
    let report = recover(cache, &manager).await.expect("the scan completes");
    assert_eq!(report.admitted, 0);
    let summaries = logs.matching("restart scan");
    assert_eq!(
        summaries
            .iter()
            .filter(|line| line.contains("restart scan stopped, the backend being read-only"))
            .count(),
        1,
        "one summary line, saying the scan stopped: {summaries:?}"
    );
    assert!(
        !summaries
            .iter()
            .any(|line| line.contains("restart scan complete")),
        "{summaries:?}"
    );
}

/// Step 9: a shard directory the scan cannot list and a record it cannot
/// open are skipped, each named in a warning, and counted, in the report
/// and in the summary line. Every other record is admitted.
#[tokio::test]
async fn tc_storage_reclaim_01_reports_what_it_could_not_read() {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    if !support::in_own_process(
        module_path!(),
        "tc_storage_reclaim_01_reports_what_it_could_not_read",
    ) {
        return;
    }
    let logs = support::LogCapture::install();
    let (cache, _tmp) = support::leaked_cache();
    let keys = [
        support::key("/report-unlistable"),
        support::key("/report-unreadable"),
        support::key("/report-readable"),
    ];
    for key in &keys {
        support::store(cache, key, &support::meta(), b"body").await;
    }
    let paths: Vec<_> = keys
        .iter()
        .map(|key| support::canonical_path(cache, key))
        .collect();
    let unlistable = paths[0].parent().expect("a shard directory").to_path_buf();
    let unreadable = paths[1].clone();
    assert!(
        paths[1..].iter().all(|path| !path.starts_with(&unlistable)),
        "the unlistable directory holds only its own record"
    );

    let set_mode = |path: &Path, mode: u32| {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .expect("the permissions are set");
    };
    let mode_of = |path: &Path| {
        std::fs::metadata(path)
            .expect("the path exists")
            .permissions()
            .mode()
    };
    let (dir_mode, file_mode) = (mode_of(&unlistable), mode_of(&unreadable));
    set_mode(&unlistable, 0o000);
    set_mode(&unreadable, 0o000);
    if std::fs::File::open(&unreadable).is_ok() {
        set_mode(&unlistable, dir_mode);
        set_mode(&unreadable, file_mode);
        eprintln!("skipped: this user can open a file with no permission bits (root?)");
        return;
    }

    logs.clear();
    let manager = Manager::new(usize::MAX);
    let report = recover(cache, &manager).await;
    set_mode(&unlistable, dir_mode);
    set_mode(&unreadable, file_mode);
    let report = report.expect("the scan completes");

    assert_eq!(report.skipped, 2, "the directory and the record");
    assert_eq!(report.admitted, 1, "the readable record");
    let warnings = logs.matching("WARN restart scan skipped");
    for skipped in [&unlistable, &unreadable] {
        assert!(
            warnings
                .iter()
                .any(|line| line.contains(&skipped.display().to_string())),
            "a warning names {}: {warnings:?}",
            skipped.display()
        );
    }
    assert_eq!(
        logs.matching("skipped 2 it could not read").len(),
        1,
        "the summary line counts both: {:?}",
        logs.matching("restart scan")
    );
}
