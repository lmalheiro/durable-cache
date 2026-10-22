// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Running the backend's file I/O off the async executor
//! (FDS-STORAGE.EXECUTOR-01).
//!
//! Every operation this crate performs is an ordinary blocking syscall —
//! `open`, `pread`, `pwrite`, `fdatasync`, `rename`, `unlink`. Pingora gives
//! each service one worker thread by
//! default,[^threads] so a syscall run directly inside an `async fn` stalls
//! every other connection that worker is driving, not just the request that
//! issued it. On a cache whose whole purpose is to hold more than fits in RAM
//! (HLR-PERSISTENCE.CAPACITY-01), a read that misses the page cache is real
//! device latency, so this is the normal case rather than the exception.
//!
//! [^threads]: `ServerConf::threads` defaults to 1; the deliverable cannot
//! assume an integrator raised it.
//!
//! The remedy is Tokio's blocking pool: one handoff per storage operation —
//! not per syscall — so the synchronous code below stays exactly as it was
//! and the worker is free again immediately.
//!
//! On Linux, a lookup of a published record and each read of its body first
//! try the page cache on the worker itself, with calls that fail rather than
//! wait for the device (`crate::nowait`). Only an operation that would wait
//! comes here, whole.

use std::fs::File;
#[cfg(test)]
use std::path::PathBuf;

use pingora_error::{Error, ErrorType, Result};

#[cfg(test)]
use crate::FsCache;

// IMP-STORAGE.EXECUTOR-01: satisfy FDS-STORAGE.EXECUTOR-01

/// Run one storage operation's blocking work on Tokio's blocking pool.
///
/// The closure takes the whole operation, not a single syscall, so a store or
/// a lookup costs one handoff rather than one per `read`/`write`.
///
/// A `JoinError` here means the blocking pool dropped the task — the runtime
/// is shutting down, or the closure panicked. Neither is something a cache
/// lookup can recover from, so both surface as an `InternalError`, which
/// every caller already treats as a failed operation rather than as a miss.
///
/// The crate's own test build re-raises a panic instead, in the task that
/// awaited the operation, with its original message. A panic is a bug, and
/// as an `InternalError` it could pass unnoticed: a test that does not
/// check that operation's result would still pass, and nextest hides the
/// output of passing tests, the panic's message included. Re-raised, it
/// fails the test as a panic in the test's own code would. A test that
/// spawns the operation and never awaits the task still loses it, as with
/// any panic in a task nobody awaits. Every other build, an integrator's
/// included, is unchanged.
pub(crate) async fn run<T, F>(f: F) -> Result<T>
where
    F: FnOnce() -> Result<T> + Send + 'static,
    T: Send + 'static,
{
    // The pool is the calling runtime's, which Pingora builds from its
    // `ServerConf`: `max_blocking_threads` caps the threads (unset keeps
    // tokio's 512; once all are busy, further operations queue rather than
    // fail) and `blocking_threads_ttl_seconds` sets how long an idle one
    // lives (unset keeps tokio's 10 s). With `work_stealing` on, a service
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

/// Close a file that has no name left, without making the caller wait
/// (FDS-STORAGE.EXECUTOR-01).
///
/// Closing such a file frees its storage, and for a large file that can
/// take a long time. The callers are `Drop` implementations, which cannot
/// wait, and under Pingora they usually run on an async worker. So, inside
/// a Tokio runtime, the close runs on the blocking pool, and this function
/// returns at once. Outside a runtime, the file is closed here.
///
/// Two cases come from Tokio itself. If the runtime is shutting down, it
/// drops the task at once, and that closes the file here. If the pool has
/// no thread and the operating system cannot start one, Tokio panics. A
/// panic inside the task is not reported anywhere, even in the test build.
///
/// `cache` and `path` exist only in the test build. They select the pause
/// point `BeforeClosingRemovedFile`.
pub(crate) fn close_removed_file(
    file: File,
    #[cfg(test)] cache: &'static FsCache,
    #[cfg(test)] path: PathBuf,
) {
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
