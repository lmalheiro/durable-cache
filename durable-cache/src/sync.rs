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

#[cfg(all(test, durable_cache_loom))]
pub(crate) use loom::sync::{MutexGuard, RwLockReadGuard};

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
