// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Running the backend's file I/O off the async executor
//! (FDS-STORAGE.EXECUTOR-04).
//!
//! Every operation this crate performs is an ordinary blocking syscall —
//! `open`, `pread`, `pwrite`, `fdatasync`, `rename`, `unlink`. Pingora gives
//! each service one worker thread by default,[^threads] so a syscall run
//! directly inside an `async fn` stalls every other connection that worker
//! is driving, not just the request that issued it. On a cache whose whole
//! purpose is to hold more than fits in RAM (HLR-PERSISTENCE.CAPACITY-01), a
//! read that misses the page cache is real device latency, so this is the
//! normal case rather than the exception.
//!
//! [^threads]: `ServerConf::threads` defaults to 1; the deliverable cannot
//! assume an integrator raised it.
//!
//! The remedy is Tokio's blocking pool: one handoff per storage operation —
//! not per syscall — so the synchronous code below stays exactly as it was
//! and the worker is free again immediately.
//!
//! On Linux, unless the configuration turns them off, a lookup and each read
//! of a hit's body, a partial hit's included, first try the page cache on
//! the worker itself, with calls that fail rather than wait for the device
//! (`crate::nowait`). What the worker leaves comes here: a lookup again from
//! the start, and the rest of a body read.

use std::fs::File;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use pingora_error::{Error, ErrorType, Result};

use crate::FsCache;

// IMP-STORAGE.EXECUTOR-04: satisfy FDS-STORAGE.EXECUTOR-04

/// Run one storage operation's blocking work on Tokio's blocking pool.
///
/// The closure takes the whole operation, not a single syscall, so a store
/// or a lookup costs one handoff rather than one per `read`/`write`.
///
/// A `JoinError` here means the blocking pool dropped the task — the runtime
/// is shutting down, or the closure panicked. Neither is something a cache
/// lookup can recover from, so both surface as an `InternalError`, which
/// every caller already treats as a failed operation rather than as a miss.
///
/// The crate's own test build re-raises a panic instead, in the task that
/// awaited the operation, with its original message. A panic is a bug, and
/// as an `InternalError` it could pass unnoticed: a test that does not check
/// that operation's result would still pass, and nextest hides the output of
/// passing tests, the panic's message included. Re-raised, it fails the test
/// as a panic in the test's own code would. A test that spawns the operation
/// and never awaits the task still loses it, as with any panic in a task
/// nobody awaits. Every other build, an integrator's included, is unchanged.
pub(crate) async fn run<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    // The pool is the calling runtime's, which Pingora builds from its
    // `ServerConf`: `max_blocking_threads` caps the threads (unset keeps
    // Tokio's 512; once all are busy, further operations queue rather than
    // fail) and `blocking_threads_ttl_seconds` sets how long an idle one
    // lives (unset keeps Tokio's 10 s). With `work_stealing` on, a service
    // shares one pool among its workers; with it off, each worker thread has
    // a pool of its own.
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        #[cfg(test)]
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Error::e_because(
            ErrorType::InternalError,
            "blocking storage task did not complete",
            e,
        ),
    }
}

// IMP-STORAGE.INLINEWRITES-01: satisfy FDS-STORAGE.INLINEWRITES-01

/// The inline slots (ADR-STORAGE.INLINEWRITES-01): at most this many of the
/// operations of unsynchronised stores run on the async workers that poll
/// them at once, rather than on the blocking pool.
///
/// Those operations create a store's temporary, write it, and rename it, so
/// they wait for no device unless the kernel throttles writers, as it does
/// once the host's dirty data passes its limit. A worker inside one then
/// stalls every task it holds, so fewer of them than the runtime has workers
/// may run at once: at most the runtime's workers less one, read from Tokio
/// at each call, whatever the configuration asks. A runtime of one worker,
/// Pingora's default, or one with work stealing off, which runs a runtime
/// per worker, therefore runs none inline.
pub(crate) struct InlineSlots {
    /// The slots the configuration asks for.
    limit: usize,
    /// The slots taken now.
    taken: AtomicUsize,
    /// Whether the cut to the runtime's workers has been logged.
    cut_logged: AtomicBool,
}

impl InlineSlots {
    pub(crate) fn new(limit: usize) -> Self {
        Self {
            limit,
            taken: AtomicUsize::new(0),
            cut_logged: AtomicBool::new(false),
        }
    }

    /// Take a slot, if one is free on the runtime that polls the caller.
    fn try_take(&self) -> Option<InlineSlot<'_>> {
        if self.limit == 0 {
            return None;
        }
        let workers = tokio::runtime::Handle::try_current()
            .ok()?
            .metrics()
            .num_workers();
        let usable = self.limit.min(workers.saturating_sub(1));
        if usable < self.limit && !self.cut_logged.swap(true, Ordering::Relaxed) {
            log::info!(
                "{} inline write slots asked for, {usable} used: the runtime has {workers} \
                 worker(s), and one must always stay free",
                self.limit
            );
        }
        self.taken
            .fetch_update(Ordering::Acquire, Ordering::Relaxed, |taken| {
                (taken < usable).then_some(taken + 1)
            })
            .ok()?;
        Some(InlineSlot(self))
    }
}

/// A taken inline slot, given back when dropped.
struct InlineSlot<'a>(&'a InlineSlots);

impl Drop for InlineSlot<'_> {
    fn drop(&mut self) {
        self.0.taken.fetch_sub(1, Ordering::Release);
    }
}

/// Run a store's operation on the worker that polls it, in an inline slot,
/// if `may_run_inline` and a slot is free, and on the blocking pool as
/// [`run`] does otherwise (ADR-STORAGE.INLINEWRITES-01). `f` learns where it
/// runs: `true` on the worker.
///
/// The caller decides that the operation may run on a worker: one that
/// waits for no device by nature, of a store that is not synchronised.
pub(crate) async fn run_store_op<T, F>(
    cache: &'static FsCache,
    may_run_inline: bool,
    f: F,
) -> Result<T>
where
    F: FnOnce(bool) -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    if may_run_inline {
        if let Some(_slot) = cache.inline_slots.try_take() {
            test_hook! { cache.hooks.count_inline_op(); }
            return f(true);
        }
    }
    run(move || f(false)).await
}

/// Close a file that may have no name left, without making the caller wait
/// (FDS-STORAGE.EXECUTOR-04).
///
/// Closing such a file frees its storage, and for a large file that can take
/// a long time. The callers cannot wait: `Drop` implementations, and the
/// lookups in `lookup::page_cache`, which give up a file they opened on an
/// async worker; under Pingora, the `Drop` implementations usually run on
/// one too. So, inside a Tokio runtime, the close runs on the blocking pool,
/// and this function returns at once. Outside a runtime, the file is closed
/// here.
///
/// Two cases come from Tokio itself. If the runtime is shutting down, it
/// drops the task at once, and that closes the file here. If the pool has no
/// thread and the operating system cannot start one, Tokio panics. A panic
/// inside the task is not reported anywhere, even in the test build.
///
/// `cache` and `path`, the file's, select the test build's pause point
/// `BeforeClosingRemovedFile`.
pub(crate) fn close_removed_file(file: File, cache: &'static FsCache, path: &Path) {
    // The pause runs on the pool, with a path of its own.
    #[cfg(test)]
    let path = path.to_path_buf();
    #[cfg(not(test))]
    let _ = (cache, path);
    let close = move || {
        test_hook! {
            cache.hooks.pause(crate::hooks::PausePoint::BeforeClosingRemovedFile, &path);
        }
        drop(file);
    };
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => drop(runtime.spawn_blocking(close)),
        Err(_) => close(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// In the crate's test build, a panic in a storage operation reaches the
    /// code that awaited it, with its own message, even when that code
    /// ignores the operation's result, as this test does.
    #[tokio::test]
    #[should_panic(expected = "a bug in a storage operation")]
    async fn a_panicking_operation_panics_whoever_awaits_it_in_the_test_build() {
        let _ = run(|| -> Result<()> { panic!("a bug in a storage operation") }).await;
    }
}
