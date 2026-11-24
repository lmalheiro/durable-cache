// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The synchronisation types of the modules whose concurrency loom checks
//! (ADR-VERIFICATION.FORMAL-01): the standard library's in every build but
//! one, and loom's own in the crate's tests built with
//! `--cfg durable_cache_loom`, where loom runs each test under every
//! interleaving of its threads. Each name is one declaration with a `#[cfg]`
//! for each, so that the code that uses them reads the same in both.
//!
//! The name is the crate's own rather than the customary `loom`, which
//! applies to every crate a build compiles: some of this crate's
//! dependencies swap their own types under `cfg(loom)`, and do not build
//! that way as dependencies. Only test builds swap, so a build of this crate
//! as a dependency never does.

#[cfg(not(all(test, durable_cache_loom)))]
pub(crate) use std::sync::{Mutex, MutexGuard, RwLock, RwLockReadGuard};
// loom runs its threads on one of the process's own, so a thread-local of
// the standard library's would be shared among them.
#[cfg(not(all(test, durable_cache_loom)))]
pub(crate) use std::thread_local;

#[cfg(all(test, durable_cache_loom))]
pub(crate) use loom::sync::{MutexGuard, RwLockReadGuard};

/// loom's `thread_local!`, for the declarations the standard library's
/// takes with a `const` initialiser, which loom's does not: there the cell
/// is initialised with the block's value when its thread first reads it.
#[cfg(all(test, durable_cache_loom))]
macro_rules! loom_thread_local {
    ($(#[$attr:meta])* static $name:ident: $t:ty = const { $init:expr };) => {
        loom::thread_local! { $(#[$attr])* static $name: $t = $init; }
    };
}
#[cfg(all(test, durable_cache_loom))]
pub(crate) use loom_thread_local as thread_local;

/// loom's `Mutex`, with the two methods of the standard library's that loom
/// leaves out: loom never poisons a lock, so none is ever poisoned, and
/// clearing does nothing.
#[cfg(all(test, durable_cache_loom))]
pub(crate) struct Mutex<T>(loom::sync::Mutex<T>);

#[cfg(all(test, durable_cache_loom))]
impl<T> Mutex<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(loom::sync::Mutex::new(value))
    }

    pub(crate) fn lock(&self) -> std::sync::LockResult<MutexGuard<'_, T>> {
        self.0.lock()
    }

    pub(crate) fn try_lock(&self) -> std::sync::TryLockResult<MutexGuard<'_, T>> {
        self.0.try_lock()
    }

    pub(crate) fn is_poisoned(&self) -> bool {
        false
    }

    pub(crate) fn clear_poison(&self) {}
}

/// loom's `RwLock`, with the standard library's poison methods, as
/// [`Mutex`] has them.
#[cfg(all(test, durable_cache_loom))]
pub(crate) struct RwLock<T>(loom::sync::RwLock<T>);

#[cfg(all(test, durable_cache_loom))]
impl<T> RwLock<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(loom::sync::RwLock::new(value))
    }

    pub(crate) fn read(&self) -> std::sync::LockResult<RwLockReadGuard<'_, T>> {
        self.0.read()
    }

    pub(crate) fn write(&self) -> std::sync::LockResult<loom::sync::RwLockWriteGuard<'_, T>> {
        self.0.write()
    }

    pub(crate) fn is_poisoned(&self) -> bool {
        false
    }

    pub(crate) fn clear_poison(&self) {}
}

pub(crate) mod atomic {
    //! The atomics, swapped as the locks are.

    #[cfg(not(all(test, durable_cache_loom)))]
    pub(crate) use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    #[cfg(all(test, durable_cache_loom))]
    pub(crate) use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
}

/// Run `f` under loom, every interleaving of its threads up to the
/// preemption bound `LOOM_MAX_PREEMPTIONS` sets, or 2 if it is unset, which
/// `cargo xtask verify --checker loom` uses on each change: unbounded, a
/// check of three threads runs for hours.
#[cfg(all(test, durable_cache_loom))]
pub(crate) fn model<F>(f: F)
where
    F: Fn() + Sync + Send + 'static,
{
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound.get_or_insert(2);
    builder.check(f);
}
