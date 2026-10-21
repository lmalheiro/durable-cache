// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The backend's counters (ADR-OBSERVABILITY.METRICS-01): registered in the
//! `prometheus` default registry, the one the `pingora-prometheus` exporter
//! gathers from, but only when the `metrics` feature is enabled. It is off
//! by default. The feature gates the dependency as well as the
//! registration: without it the crate links neither `pingora-prometheus`
//! nor `prometheus`, and the logs beside the counters are the only record
//! of what they count: a warning for each withdrawal, and the restart
//! scan's summary line for what it reclaimed.
//!
//! The counters are registered through the `prometheus` crate that
//! `pingora-prometheus` re-exports, so they are in the registry its exporter
//! serves by construction: the two cannot resolve to different versions of
//! `prometheus`, whose default registry is a static of the crate.
//!
//! What each counter means is specified by FDS-OBSERVABILITY.HEADROOM-01
//! (metadata that outgrew its record's headroom) and FDS-STORAGE.RECLAIM-01
//! (the restart-time scan).

#[cfg(feature = "metrics")]
mod imp {
    use std::sync::OnceLock;

    use pingora_prometheus::prometheus::{
        self, register_int_counter, register_int_counter_vec, IntCounter, IntCounterVec,
    };

    use crate::reclaim::ReclaimClass;

    const HEADROOM_EXCEEDED: &str = "durable_cache_meta_headroom_exceeded_total";
    const RECLAIMED_FILES: &str = "durable_cache_reclaimed_files_total";
    const RECLAIMED_BYTES: &str = "durable_cache_reclaimed_bytes_total";

    /// The counter in `cell`, which `register` registers the first time it
    /// is needed.
    ///
    /// Registration is genuinely fallible: `prometheus` rejects a duplicate
    /// metric name in the default registry, which an integrator can easily
    /// cause. Losing a counter must never cost the process — observability
    /// is strictly less important than serving traffic — so a failure
    /// degrades to "no metric", logged once, never propagated.
    fn registered<C>(
        cell: &'static OnceLock<Option<C>>,
        name: &str,
        register: impl FnOnce() -> prometheus::Result<C>,
    ) -> Option<&'static C> {
        cell.get_or_init(|| match register() {
            Ok(c) => Some(c),
            Err(e) => {
                log::warn!(
                    "could not register {name} ({e}); it will not be \
                     reported. Caching itself is unaffected."
                );
                None
            }
        })
        .as_ref()
    }

    fn headroom_exceeded() -> Option<&'static IntCounter> {
        static CELL: OnceLock<Option<IntCounter>> = OnceLock::new();
        registered(&CELL, HEADROOM_EXCEEDED, || {
            register_int_counter!(
                HEADROOM_EXCEEDED,
                "Metadata updates whose new metadata did not fit the record's reserved \
                 headroom, so the record was withdrawn, to be fetched again \
                 (FDS-OBSERVABILITY.HEADROOM-01)."
            )
        })
    }

    fn reclaimed_files() -> Option<&'static IntCounterVec> {
        static CELL: OnceLock<Option<IntCounterVec>> = OnceLock::new();
        registered(&CELL, RECLAIMED_FILES, || {
            register_int_counter_vec!(
                RECLAIMED_FILES,
                "Files the restart-time scan reclaimed, by class (FDS-STORAGE.RECLAIM-01).",
                &["class"]
            )
        })
    }

    fn reclaimed_bytes() -> Option<&'static IntCounterVec> {
        static CELL: OnceLock<Option<IntCounterVec>> = OnceLock::new();
        registered(&CELL, RECLAIMED_BYTES, || {
            register_int_counter_vec!(
                RECLAIMED_BYTES,
                "Bytes the restart-time scan reclaimed, by class (FDS-STORAGE.RECLAIM-01).",
                &["class"]
            )
        })
    }

    /// Every class, for the series registered at zero.
    const CLASSES: [ReclaimClass; 3] = [
        ReclaimClass::Temporary,
        ReclaimClass::Damaged,
        ReclaimClass::Withdrawn,
    ];

    /// Register every counter, with each reclaim class's series, at zero,
    /// so that the exporter serves them from the start rather than from
    /// their first event (FDS-OBSERVABILITY.HEADROOM-01,
    /// FDS-STORAGE.RECLAIM-01). [`crate::FsCache::new`] calls it; calling it
    /// again changes nothing.
    pub fn register() {
        headroom_exceeded();
        for counter in [reclaimed_files(), reclaimed_bytes()].into_iter().flatten() {
            for class in CLASSES {
                counter.with_label_values(&[label(class)]);
            }
        }
    }

    /// One metadata update's new metadata outgrew the record's headroom, so
    /// the record was withdrawn.
    pub fn record_headroom_exceeded() {
        if let Some(c) = headroom_exceeded() {
            c.inc();
        }
    }

    /// The restart-time scan reclaimed `files` files of `class`, holding
    /// `bytes` bytes.
    pub fn record_reclaimed(class: ReclaimClass, files: u64, bytes: u64) {
        if files == 0 && bytes == 0 {
            return;
        }
        if let Some(c) = reclaimed_files() {
            c.with_label_values(&[label(class)]).inc_by(files);
        }
        if let Some(c) = reclaimed_bytes() {
            c.with_label_values(&[label(class)]).inc_by(bytes);
        }
    }

    /// `class`'s value for the reclaim counters' `class` label.
    fn label(class: ReclaimClass) -> &'static str {
        match class {
            ReclaimClass::Temporary => "temporary",
            ReclaimClass::Damaged => "damaged",
            ReclaimClass::Withdrawn => "withdrawn",
        }
    }
}

#[cfg(not(feature = "metrics"))]
mod imp {
    use crate::reclaim::ReclaimClass;

    pub fn register() {}

    pub fn record_headroom_exceeded() {}

    pub fn record_reclaimed(_class: ReclaimClass, _files: u64, _bytes: u64) {}
}

pub use imp::{record_headroom_exceeded, record_reclaimed, register};
