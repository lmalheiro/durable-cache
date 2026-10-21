// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The three measuring test cases: TC-PERSISTENCE.CAPACITY-01,
//! TC-EFFICIENCY.SPACE-01, and TC-PERFORMANCE.LOOKUP-01.
//!
//! Each is `#[ignore]`d, because each is a measurement at a scale an
//! ordinary `cargo test` run should not pay for, and each takes its scale
//! from the environment so a real run can use the test case's own figures:
//!
//! ```bash
//! # Capacity: store four times the machine's RAM (the test case's setup
//! # is a 1 GiB VM; give the RAM figure and a disk that can hold 4x it).
//! DURABLE_CACHE_RAM_BYTES=$((1<<30)) cargo test -p durable-cache --release \
//!   --lib -- --ignored --nocapture measurements::tc_persistence_capacity
//!
//! # Space overhead, per load, at 10^5 objects.
//! DURABLE_CACHE_SPACE_OBJECTS=100000 cargo test -p durable-cache --release \
//!   --lib -- --ignored --nocapture measurements::tc_efficiency_space
//!
//! # Lookup latency at 10^4 and 10^7 objects, on a dedicated host.
//! DURABLE_CACHE_LOOKUP_LARGE=10000000 cargo test -p durable-cache --release \
//!   --lib -- --ignored --nocapture measurements::tc_performance_lookup
//! ```
//!
//! They refuse to run on an unoptimised build: see [`require_optimised_build`].
//!
//! The defaults are a hundredth of those figures, so the harness itself
//! stays exercisable; the assertions that carry a requirement's bound are
//! made only when the run is at the requirement's own scale, and the
//! measured numbers are always printed.
//!
//! Dropping the page cache between the cold and warm passes needs root
//! (`/proc/sys/vm/drop_caches`); without it the cold pass is skipped and
//! said so, rather than reported as if it had been cold.

use std::time::{Duration, Instant};

use pingora_cache::storage::Storage;

use super::support;

// IMP-TC.PERSISTENCE.CAPACITY-01: satisfy TC-PERSISTENCE.CAPACITY-01
// IMP-TC.EFFICIENCY.SPACE-01: satisfy TC-EFFICIENCY.SPACE-01
// IMP-TC.PERFORMANCE.LOOKUP-01: satisfy TC-PERFORMANCE.LOOKUP-01

/// Stop a measurement on an unoptimised build. Debug builds make the
/// backend's CPU work (CRC-32C, metadata decoding, hashing) many times
/// slower: that distorts the absolute figures, and it compresses a ratio
/// such as TC-PERFORMANCE.LOOKUP-01's, because the same added cost lands on
/// both sides and can hide a real slowdown.
fn require_optimised_build() {
    if cfg!(debug_assertions) {
        panic!("the measuring test cases must run on an optimised build: add --release");
    }
}

fn env_usize(name: &str, default: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

/// This process's resident memory, in bytes.
fn resident_bytes() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: u64 = statm
        .split_whitespace()
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    pages * 4096
}

/// TC-PERSISTENCE.CAPACITY-01: the cache holds, and serves, a set of
/// objects whose total size exceeds the RAM of its host, with the process's
/// own resident memory staying far below it.
#[tokio::test]
#[ignore = "a capacity measurement: set DURABLE_CACHE_RAM_BYTES and give it disk"]
async fn tc_persistence_capacity_01_cache_larger_than_host_ram() {
    require_optimised_build();
    if !support::in_own_process(
        module_path!(),
        "tc_persistence_capacity_01_cache_larger_than_host_ram",
    ) {
        return;
    }
    // The test case's setup is a 1 GiB machine; the default here is a
    // hundredth of that, which exercises the same path in seconds.
    let ram = env_usize("DURABLE_CACHE_RAM_BYTES", 10 * 1024 * 1024);
    let target = ram * 4;
    let object = vec![b'c'; 256 * 1024];

    let (cache, tmp) = support::leaked_cache();
    let baseline = resident_bytes();
    let mut stored = 0usize;
    let mut keys = Vec::new();
    let mut peak_rss = baseline;
    while stored < target {
        let key = support::key(&format!("/capacity-{}", keys.len()));
        support::store(cache, &key, &support::meta(), &object).await;
        stored += object.len();
        keys.push(key);
        peak_rss = peak_rss.max(resident_bytes());
    }

    // Everything is still there after a restart, and nothing was evicted:
    // the eviction budget is above what this stored.
    let restarted = support::leaked_cache_at(tmp.path());
    for key in &keys {
        let (_meta, body) = support::lookup_body(restarted, key)
            .await
            .unwrap_or_else(|| panic!("{key:?} must still be cached"));
        assert_eq!(body.len(), object.len());
    }

    println!(
        "capacity: stored {stored} bytes ({} objects) against a stated RAM of {ram}; \
         resident memory peaked at {peak_rss} bytes (baseline {baseline})",
        keys.len()
    );
    assert!(
        stored >= target,
        "the cache must hold at least four times the stated RAM"
    );
    // What caching cost in memory, rather than what the test binary itself
    // occupies: at a scaled-down RAM figure the baseline alone can exceed
    // it, and it is the growth that would decide whether a real proxy is
    // killed for lack of memory.
    let growth = peak_rss.saturating_sub(baseline);
    assert!(
        (growth as usize) < ram / 4,
        "caching {stored} bytes grew resident memory by {growth}, which is not \
         well below the stated RAM of {ram}"
    );
    if ram >= 256 * 1024 * 1024 {
        // At a realistic machine size the absolute figure is meaningful too.
        assert!(
            peak_rss < ram as u64,
            "the process's resident memory ({peak_rss}) must stay below the machine's \
             RAM ({ram}); the cache's size is bounded by the disk, not by memory"
        );
    }
}

/// TC-EFFICIENCY.SPACE-01: measure the on-disk overhead HLR-EFFICIENCY.SPACE-01
/// defines — allocated blocks minus the objects' own metadata and body
/// bytes, over those bytes — for the three loads the test case names.
#[tokio::test]
#[ignore = "a space measurement: set DURABLE_CACHE_SPACE_OBJECTS for the full 10^5"]
async fn tc_efficiency_space_01_overhead_per_load() {
    require_optimised_build();
    let objects = env_usize("DURABLE_CACHE_SPACE_OBJECTS", 1_000);
    let loads: [(&str, &dyn Fn(usize) -> usize); 3] = [
        // (a) a deployment-like mix, (b) only small objects, (c) only large.
        ("deployment mix", &|i| match i % 10 {
            0..=5 => 2 * 1024,
            6..=8 => 64 * 1024,
            _ => 1024 * 1024,
        }),
        ("below 4 KiB", &|i| 512 + (i % 8) * 400),
        ("1 MiB and above", &|i| (1 + i % 3) * 1024 * 1024),
    ];

    for (name, size_of) in loads {
        let (cache, tmp) = support::leaked_cache();
        let mut object_bytes = 0u64;
        for i in 0..objects {
            let key = support::key(&format!("/space-{i}"));
            let body = vec![b's'; size_of(i)];
            support::store(cache, &key, &support::meta(), &body).await;
            let header = support::read_header(&support::canonical_path(cache, &key));
            // The objects' own bytes: the metadata segments the record
            // holds, plus the body.
            object_bytes +=
                header.internal_len() as u64 + header.header_len() as u64 + header.body_len();
        }
        let occupied = support::tree_disk_usage(tmp.path());
        let overhead = (occupied as f64 - object_bytes as f64) / object_bytes as f64 * 100.0;
        println!(
            "space: load {name:>16}: {objects} objects, {object_bytes} object bytes, \
             {occupied} bytes of allocated blocks, overhead {overhead:.1}%"
        );

        if name == "deployment mix" && objects >= 100_000 {
            assert!(
                overhead <= 25.0,
                "the deployment load's overhead must be at most 25%: {overhead:.1}%"
            );
        }
    }
    if objects < 100_000 {
        println!(
            "space: run with DURABLE_CACHE_SPACE_OBJECTS=100000 to measure at the \
             test case's own scale; the 25% target is only asserted there"
        );
    }
}

/// TC-PERFORMANCE.LOOKUP-01: the 99th-percentile lookup latency at the
/// larger object count must be no more than 25 % above the smaller one's,
/// for hits and for misses, cold and warm.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "a latency measurement: set DURABLE_CACHE_LOOKUP_LARGE for the full 10^7"]
async fn tc_performance_lookup_01_latency_flat_with_size() {
    require_optimised_build();
    let small = env_usize("DURABLE_CACHE_LOOKUP_SMALL", 10_000);
    let large = env_usize("DURABLE_CACHE_LOOKUP_LARGE", 100_000);
    let lookups = env_usize("DURABLE_CACHE_LOOKUP_SAMPLES", 2_000);
    assert!(large > small, "the larger cache must hold more objects");

    let (cache, _tmp) = support::leaked_cache();
    let mut stored = 0usize;
    let mut measurements = Vec::new();
    for target in [small, large] {
        while stored < target {
            let key = support::key(&format!("/lookup-{stored}"));
            support::store(cache, &key, &support::meta(), b"small body").await;
            stored += 1;
        }
        for cold in [true, false] {
            if cold && !drop_page_cache() {
                println!("lookup: cannot drop the page cache (needs root); skipping the cold pass");
                continue;
            }
            let (hits, misses) = measure(cache, stored, lookups).await;
            println!(
                "lookup: {stored} objects, {} pass: p99 hit {:?}, p99 miss {:?}",
                if cold { "cold" } else { "warm" },
                hits,
                misses
            );
            measurements.push((stored, cold, hits, misses));
        }
    }

    for &(_, cold, small_hit, small_miss) in measurements.iter().filter(|m| m.0 == small) {
        for &(_, large_cold, large_hit, large_miss) in
            measurements.iter().filter(|m| m.0 == large && m.1 == cold)
        {
            assert_eq!(cold, large_cold);
            for (what, small_p99, large_p99) in [
                ("hits", small_hit, large_hit),
                ("misses", small_miss, large_miss),
            ] {
                let ratio = large_p99.as_secs_f64() / small_p99.as_secs_f64().max(f64::EPSILON);
                println!(
                    "lookup: {what} at {large} objects are {ratio:.2}x the p99 at {small} \
                     ({}, bound 1.25x)",
                    if cold { "cold" } else { "warm" }
                );
                if large >= 10_000_000 {
                    assert!(
                        ratio <= 1.25,
                        "the p99 for {what} at 10^7 objects must be within 25% of the p99 at \
                         10^4: {ratio:.2}x"
                    );
                }
            }
        }
    }
    if large < 10_000_000 {
        println!(
            "lookup: run with DURABLE_CACHE_LOOKUP_SMALL=10000 \
             DURABLE_CACHE_LOOKUP_LARGE=10000000 to measure at the test case's own \
             scale; the 1.25x bound is only asserted there"
        );
    }
}

/// The 99th-percentile latency of `lookups` hits and of `lookups` misses
/// over uniformly chosen keys.
async fn measure(
    cache: &'static durable_cache::FsCache,
    stored: usize,
    lookups: usize,
) -> (Duration, Duration) {
    let trace = support::inactive_trace();
    let mut hits = Vec::with_capacity(lookups);
    let mut misses = Vec::with_capacity(lookups);
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    for _ in 0..lookups {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let pick = (state % stored as u64) as usize;

        let hit_key = support::key(&format!("/lookup-{pick}"));
        let started = Instant::now();
        let found = cache.lookup(&hit_key, &trace).await.unwrap();
        hits.push(started.elapsed());
        assert!(found.is_some(), "/lookup-{pick} must be a hit");

        let miss_key = support::key(&format!("/lookup-never-stored-{pick}"));
        let started = Instant::now();
        let found = cache.lookup(&miss_key, &trace).await.unwrap();
        misses.push(started.elapsed());
        assert!(found.is_none());
    }
    (p99(&mut hits), p99(&mut misses))
}

fn p99(samples: &mut [Duration]) -> Duration {
    samples.sort();
    samples[samples.len() * 99 / 100]
}

/// Drop the operating system's page cache, so a pass is genuinely cold.
/// Returns false when this process may not (i.e. is not root).
fn drop_page_cache() -> bool {
    use std::io::Write;
    let Ok(mut f) = std::fs::OpenOptions::new()
        .write(true)
        .open("/proc/sys/vm/drop_caches")
    else {
        return false;
    };
    f.write_all(b"3").is_ok()
}
