// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-COMPLIANCE.REVALIDATION-01, steps 11 and 12 — metadata that outgrows
//! its record's headroom is observable (FDS-OBSERVABILITY.HEADROOM-01).
//!
//! Each test runs in a process of its own (`support::in_own_process`): the
//! log and the `prometheus` default registry are process-wide, so another
//! withdrawal made in parallel in the same process would be
//! indistinguishable from this one's.

use super::support;

use pingora_cache::key::CacheHashKey;
use pingora_cache::storage::Storage;

// IMP-TC.COMPLIANCE.REVALIDATION-02: satisfy TC-COMPLIANCE.REVALIDATION-01
// -- the observability half; compat_revalidation.rs covers the rest.

const EXCEEDED: &str = "durable_cache_meta_headroom_exceeded_total";

/// What the warning says.
const WITHDRAWN: &str = "so the record was withdrawn";

/// Step 11: each update whose metadata outgrows the record's headroom logs
/// one warning naming the key's hash and the headroom shortfall, and raises
/// the counter by one. Step 12: without the `metrics` feature the warning is
/// still logged and the counter is not registered.
#[tokio::test]
async fn tc_compliance_revalidation_01_withdrawals_are_logged_and_counted() {
    if !support::in_own_process(
        module_path!(),
        "tc_compliance_revalidation_01_withdrawals_are_logged_and_counted",
    ) {
        return;
    }
    let logs = support::LogCapture::install();
    let (cache, _tmp) = support::leaked_cache_with_headroom(0);
    let key = support::key("/revalidation-observable");
    let stored = support::meta_with_header_filler(8);
    support::store(cache, &key, &stored, b"body bytes").await;

    let before = support::counter_value(EXCEEDED, &[]).unwrap_or(0.0);

    logs.clear();
    let trace = support::inactive_trace();
    assert!(
        !cache
            .update_meta(
                &key,
                &support::revalidation(&stored, &support::meta_with_header_filler(4000)),
                &trace,
            )
            .await
            .expect("the update returns"),
        "the update is not applied"
    );

    let warnings = logs.matching(WITHDRAWN);
    assert_eq!(
        warnings.len(),
        1,
        "one warning per withdrawal: {warnings:?}"
    );
    let warning = &warnings[0];
    assert!(warning.starts_with("WARN"), "it is a warning: {warning}");
    assert!(
        warning.contains(&key.combined()),
        "it names the key by its hex combined hash: {warning}"
    );
    assert!(
        warning.contains("headroom by"),
        "and states the headroom shortfall: {warning}"
    );

    if cfg!(feature = "metrics") {
        assert_eq!(
            support::counter_value(EXCEEDED, &[]).expect("the counter") - before,
            1.0,
            "one withdrawal counted"
        );
    } else {
        assert_eq!(
            support::counter_value(EXCEEDED, &[]),
            None,
            "without the metrics feature no counter is registered"
        );
    }
}

/// Step 11, continued: a withdrawal whose header write fails is neither
/// logged nor counted, since the record was not withdrawn.
#[tokio::test]
async fn tc_compliance_revalidation_01_a_failed_withdrawal_is_neither_logged_nor_counted() {
    use durable_cache::hooks::FaultPoint;
    if !support::in_own_process(
        module_path!(),
        "tc_compliance_revalidation_01_a_failed_withdrawal_is_neither_logged_nor_counted",
    ) {
        return;
    }
    let logs = support::LogCapture::install();
    let (cache, _tmp) = support::leaked_cache_with_headroom(0);
    let key = support::key("/revalidation-failed-withdrawal");
    let stored = support::meta_with_header_filler(8);
    support::store(cache, &key, &stored, b"body").await;

    let before = support::counter_value(EXCEEDED, &[]).unwrap_or(0.0);
    logs.clear();
    cache.hooks().inject_fault(FaultPoint::WithdrawRecord, 1);
    let trace = support::inactive_trace();
    cache
        .update_meta(
            &key,
            &support::revalidation(&stored, &support::meta_with_header_filler(4000)),
            &trace,
        )
        .await
        .expect_err("the withdrawal fails");

    assert!(
        logs.matching(WITHDRAWN).is_empty(),
        "no withdrawal is logged: {:?}",
        logs.matching(WITHDRAWN)
    );
    assert_eq!(
        support::counter_value(EXCEEDED, &[]).unwrap_or(0.0),
        before,
        "and none is counted"
    );
}

/// Steps 11 and 12, before any update: once the backend is built, the
/// counter is served at zero, rather than from the first withdrawal;
/// without the `metrics` feature it is not registered.
#[test]
fn tc_compliance_revalidation_01_the_counter_reads_zero_once_the_backend_is_built() {
    if !support::in_own_process(
        module_path!(),
        "tc_compliance_revalidation_01_the_counter_reads_zero_once_the_backend_is_built",
    ) {
        return;
    }
    let (_cache, _tmp) = support::leaked_cache();
    assert_eq!(
        support::counter_value(EXCEEDED, &[]),
        cfg!(feature = "metrics").then_some(0.0)
    );
}
