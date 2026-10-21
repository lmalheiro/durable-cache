// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! TC-STORAGE.EXECUTOR-01: no `Storage` call blocks the worker it is polled
//! on (FDS-STORAGE.EXECUTOR-01).
//!
//! Pingora gives each service one worker thread by default, so a backend
//! that runs its syscalls inline stalls every other connection that worker
//! is driving. That is not visible in a functional test — the data is always
//! correct — so this measures it directly: a single-threaded runtime drives
//! a timekeeper task beside real cache traffic, and the timekeeper's longest
//! gap is what a co-resident request would have waited.
//!
//! The bound is deliberately loose. What it rules out is a backend that
//! holds the worker for the whole of a store or a body read; it is not a
//! latency figure for the storage operations themselves, which
//! TC-PERFORMANCE.LOOKUP-01 measures.

use super::support;

// IMP-TC.STORAGE.EXECUTOR-01: satisfy TC-STORAGE.EXECUTOR-01

use pingora_cache::storage::Storage;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// The longest a single `Storage` call may keep the worker to itself.
///
/// Generous next to the microseconds a tmpfs syscall costs, so this fails on
/// a backend that blocks for an operation's whole duration and not on
/// ordinary scheduling noise.
const MAX_STALL: Duration = Duration::from_millis(50);

/// Ticks every millisecond and records the longest gap it was kept from
/// running — the stall a request sharing this worker would have seen.
struct Timekeeper {
    worst_us: Arc<AtomicU64>,
    stop: Arc<std::sync::atomic::AtomicBool>,
}

impl Timekeeper {
    fn start() -> (Self, tokio::task::JoinHandle<()>) {
        let worst_us = Arc::new(AtomicU64::new(0));
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handle = {
            let worst_us = Arc::clone(&worst_us);
            let stop = Arc::clone(&stop);
            tokio::task::spawn(async move {
                let mut last = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                    let now = Instant::now();
                    let gap = now.duration_since(last).as_micros() as u64;
                    worst_us.fetch_max(gap, Ordering::Relaxed);
                    last = now;
                }
            })
        };
        (Timekeeper { worst_us, stop }, handle)
    }

    async fn finish(self, handle: tokio::task::JoinHandle<()>) -> Duration {
        self.stop.store(true, Ordering::Relaxed);
        let _ = handle.await;
        Duration::from_micros(self.worst_us.load(Ordering::Relaxed))
    }
}

/// A single-threaded runtime: one worker, exactly as `ServerConf::threads`
/// leaves it by default, so any inline syscall shows up as a stall.
fn single_worker_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn tc_storage_executor_01_storage_calls_do_not_stall_the_worker() {
    let rt = single_worker_runtime();
    rt.block_on(async {
        let (cache, _tmp) = support::leaked_cache();
        // Big enough that a body write and a body read are real work rather
        // than a single buffer copy.
        let body: Vec<u8> = (0..2 * 1024 * 1024u32).map(|i| (i % 251) as u8).collect();

        let (timer, handle) = Timekeeper::start();
        // Let the timekeeper settle before measuring.
        tokio::time::sleep(Duration::from_millis(20)).await;

        for i in 0..20 {
            let key = support::key(&format!("/executor-{i}"));
            // Store: get_miss_handler, write_body, finish (fdatasync, rename).
            let stored = support::meta();
            support::store(cache, &key, &stored, &body).await;
            // Read it back: lookup, then read_body to the end.
            let (_meta, read) = support::lookup_body(cache, &key).await.unwrap();
            assert_eq!(read.len(), body.len());
            // Metadata update, expiry, and purge each touch the record.
            cache
                .update_meta(
                    &key,
                    &support::revalidation(&stored, &support::meta()),
                    &support::inactive_trace(),
                )
                .await
                .unwrap();
            support::expire_active(cache, &key).await;
            support::purge_active(cache, &key).await;
        }

        let worst = timer.finish(handle).await;
        assert!(
            worst < MAX_STALL,
            "a co-resident task was kept off the worker for {worst:?}, over the {MAX_STALL:?} \
             bound: a Storage call is running its file I/O inline instead of on the blocking \
             pool (FDS-STORAGE.EXECUTOR-01)"
        );
    });
}

/// The same bound while a partial-hit follower streams from an in-flight
/// store: the write path and the read path are both active, which is when
/// the backend has the most opportunity to hold the worker.
#[test]
fn tc_storage_executor_01_streaming_does_not_stall_the_worker() {
    let rt = single_worker_runtime();
    rt.block_on(async {
        let (cache, _tmp) = support::leaked_cache();
        let key = support::key("/executor-streaming");
        let chunk = vec![7u8; 1024 * 1024];
        let trace = support::inactive_trace();

        let (timer, handle) = Timekeeper::start();
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut miss = cache
            .get_miss_handler(&key, &support::meta(), &trace)
            .await
            .unwrap();
        // A follower attached to the in-flight store, so each chunk is also
        // read back through the partial-hit path (FDS-STORAGE.PARTIAL-01).
        let (_meta, mut hit) = cache.lookup(&key, &trace).await.unwrap().unwrap();

        let mut written = 0usize;
        for _ in 0..64 {
            miss.write_body(bytes::Bytes::from(chunk.clone()), false)
                .await
                .unwrap();
            written += chunk.len();
            let got = hit.read_body().await.unwrap().expect("a chunk");
            assert!(!got.is_empty());
        }
        miss.write_body(bytes::Bytes::new(), true).await.unwrap();
        miss.finish().await.unwrap();
        drop(hit);

        let worst = timer.finish(handle).await;
        assert!(written > 0);
        assert!(
            worst < MAX_STALL,
            "a co-resident task was kept off the worker for {worst:?} while a follower streamed \
             an in-flight store, over the {MAX_STALL:?} bound (FDS-STORAGE.EXECUTOR-01)"
        );
    });
}

/// Step 6: the restart scan frees what it removes on the blocking pool: a
/// damaged record it reclaims, a stopped writer's temporary, and a record its
/// own admission evicts at once, the eviction manager having no room. Each
/// removal is held where it frees its file, and another task on the scan's
/// single worker still runs meanwhile.
#[test]
fn tc_storage_executor_01_the_restart_scan_frees_files_off_the_worker() {
    use durable_cache::hooks::PausePoint;
    use durable_cache::recovery::recover;
    use pingora_cache::eviction::simple_lru::Manager;

    for case in [
        "a damaged record",
        "a stopped writer's temporary",
        "an eviction victim",
    ] {
        let rt = single_worker_runtime();
        let (cache, _tmp) = support::leaked_cache();
        let (point, path, capacity) = rt.block_on(async {
            match case {
                "a damaged record" => {
                    let key = support::key("/executor-scan-damaged");
                    support::store(cache, &key, &support::meta(), b"damaged").await;
                    let path = support::canonical_path(cache, &key);
                    support::flip_magic_byte(&path);
                    (PausePoint::BeforeFreeingOldRecord, path, usize::MAX)
                }
                "a stopped writer's temporary" => {
                    let temp = support::forge_temp(
                        cache,
                        "cccc0000000000000000000000000000",
                        &support::writer_without_lease(),
                        1,
                    );
                    (PausePoint::ReclaimBeforeUnlink, temp, usize::MAX)
                }
                _ => {
                    let key = support::key("/executor-scan-victim");
                    support::store(cache, &key, &support::meta(), b"evicted").await;
                    let path = support::canonical_path(cache, &key);
                    (PausePoint::BeforeFreeingOldRecord, path, 0)
                }
            }
        });
        let pause = cache.hooks().arm_pause(point, Some(&path));

        // A task on the scan's worker that answers whenever it gets to run.
        let (probe, mut probes) =
            tokio::sync::mpsc::unbounded_channel::<std::sync::mpsc::Sender<()>>();
        let scanning = std::thread::spawn(move || {
            rt.block_on(async move {
                tokio::spawn(async move {
                    while let Some(answer) = probes.recv().await {
                        let _ = answer.send(());
                    }
                });
                let manager = Manager::new(capacity);
                recover(cache, &manager).await.expect("the scan completes")
            })
        });
        assert!(
            pause.wait_reached(Duration::from_secs(5)),
            "{case}: the scan reaches the removal"
        );
        let (answer, answered) = std::sync::mpsc::channel();
        probe.send(answer).expect("the probe is listening");
        let free = answered.recv_timeout(Duration::from_secs(5)).is_ok();
        pause.release();
        scanning.join().expect("the scanning thread completes");
        assert!(
            free,
            "{case}: the scan's worker stayed blocked while the file was freed \
             (FDS-STORAGE.EXECUTOR-01)"
        );
    }
}

/// Step 7: the restart scan yields to other tasks between the records it
/// admits, and between the records its ghost-entry check visits. With every
/// record's file removed between the walk and that check, the eviction
/// manager hears an admission and a removal for each record, and another
/// task on the scan's single worker runs between any two of those calls.
#[test]
fn tc_storage_executor_01_the_restart_scan_yields_between_records() {
    use durable_cache::hooks::PausePoint;
    use durable_cache::recovery::recover;
    use pingora_cache::eviction::simple_lru::Manager;
    use pingora_cache::eviction::{CacheEntryKey, CacheEntryKeyRef, EvictionManager};
    use std::sync::Mutex;
    use std::time::SystemTime;

    type Events = Arc<Mutex<Vec<&'static str>>>;

    fn note(events: &Events, event: &'static str) {
        events.lock().expect("the event log").push(event);
    }

    /// The stock manager, noting each admission and removal in `events`.
    struct Recording {
        inner: Manager,
        events: Events,
    }

    #[async_trait::async_trait]
    impl EvictionManager for Recording {
        fn total_size(&self) -> usize {
            self.inner.total_size()
        }
        fn total_items(&self) -> usize {
            self.inner.total_items()
        }
        fn evicted_size(&self) -> usize {
            self.inner.evicted_size()
        }
        fn evicted_items(&self) -> usize {
            self.inner.evicted_items()
        }
        fn admit(
            &self,
            item: CacheEntryKey,
            size: usize,
            fresh_until: SystemTime,
        ) -> Vec<CacheEntryKey> {
            note(&self.events, "admit");
            self.inner.admit(item, size, fresh_until)
        }
        fn increment_weight(
            &self,
            item: &CacheEntryKey,
            delta: usize,
            max_weight: Option<usize>,
        ) -> Vec<CacheEntryKey> {
            self.inner.increment_weight(item, delta, max_weight)
        }
        fn remove(&self, item: CacheEntryKeyRef<'_>) {
            note(&self.events, "remove");
            self.inner.remove(item)
        }
        fn access(&self, item: &CacheEntryKey, size: usize, fresh_until: SystemTime) -> bool {
            self.inner.access(item, size, fresh_until)
        }
        fn peek(&self, item: &CacheEntryKey) -> bool {
            self.inner.peek(item)
        }
        async fn save(&self, dir_path: &str) -> pingora_error::Result<()> {
            self.inner.save(dir_path).await
        }
        async fn load(&self, dir_path: &str) -> pingora_error::Result<()> {
            self.inner.load(dir_path).await
        }
    }

    const RECORDS: usize = 16;
    let rt = single_worker_runtime();
    let (cache, _tmp) = support::leaked_cache();
    let paths: Vec<_> = rt.block_on(async {
        let mut paths = Vec::new();
        for i in 0..RECORDS {
            let key = support::key(&format!("/executor-scan-{i}"));
            support::store(cache, &key, &support::meta(), b"body").await;
            paths.push(support::canonical_path(cache, &key));
        }
        paths
    });
    let events: Events = Arc::new(Mutex::new(Vec::new()));
    let pause = cache
        .hooks()
        .arm_pause(PausePoint::RecoverBeforeGhostPass, Some(cache.root()));

    let scanning = {
        let events = Arc::clone(&events);
        std::thread::spawn(move || {
            rt.block_on(async move {
                // Another task on the scan's worker, noting each turn it gets.
                let others = Arc::clone(&events);
                tokio::spawn(async move {
                    loop {
                        note(&others, "other");
                        tokio::task::yield_now().await;
                    }
                });
                let manager = Recording {
                    inner: Manager::new(usize::MAX),
                    events,
                };
                recover(cache, &manager).await.expect("the scan completes")
            })
        })
    };
    assert!(
        pause.wait_reached(Duration::from_secs(5)),
        "the scan reaches its ghost-entry check"
    );
    for path in &paths {
        std::fs::remove_file(path).expect("the record is removed");
    }
    pause.release();
    let report = scanning.join().expect("the scanning thread completes");
    assert_eq!(report.admitted, 0, "every admitted record has gone");

    let events = events.lock().expect("the event log");
    let calls: Vec<_> = events.iter().filter(|e| **e != "other").collect();
    assert_eq!(
        calls.iter().filter(|e| ***e == "admit").count(),
        RECORDS,
        "an admission per record"
    );
    assert_eq!(
        calls.iter().filter(|e| ***e == "remove").count(),
        RECORDS,
        "a removal per record"
    );
    assert!(
        events
            .windows(2)
            .all(|w| w[0] == "other" || w[1] == "other"),
        "the scan called the eviction manager twice without letting another task run \
         (FDS-STORAGE.EXECUTOR-01): {events:?}"
    );
}
