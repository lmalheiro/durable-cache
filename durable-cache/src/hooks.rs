// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Test hooks: the pause points, fault injection, hash injection, and
//! instrumentation the test cases name in their Setup. The module exists
//! only in this crate's own test build (`cfg(test)`). Every call into it is
//! wrapped in `test_hook!`, and everything else that reaches it is a
//! `#[cfg(test)]` item: `FsCache`'s `hooks` field, `with_test_hooks` and
//! `hooks`, the test build's `map_name`, and `close_removed_file`'s
//! test-only parameters. So no other build contains a hook, a hook call, or
//! anything a hook call's arguments compute.
//!
//! Hooks are per [`crate::FsCache`] instance, so tests running in parallel
//! in one binary never see each other's pauses or faults.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

/// A place in the backend where a test can hold an operation.
///
/// A test arms a pause with [`Hooks::arm_pause`], naming the point and,
/// optionally, the path the operation works on. The first operation to
/// reach that point, on that path, stops there until the test releases it;
/// the pause is then used up. Each point below says where it is, which path
/// it is given, and the test cases whose tests pause there, or the design,
/// for a point only regression tests use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PausePoint {
    /// Reclaim, during the restart scan: after a record is found
    /// reclaimable, before the key's publication lock is taken to unlink
    /// it, or before a stopped writer's temporary is unlinked. Path: the
    /// record or the temporary (TC-STORAGE.RECLAIM-01,
    /// TC-STORAGE.EXECUTOR-01).
    ReclaimBeforeUnlink,
    /// Restart scan: after a record is read, before it is checked again
    /// under the key's publication lock and admitted to the eviction
    /// manager. Path: the record (TC-EVICTION.RECONSTRUCT-01).
    RecoverBeforeAdmit,
    /// Restart scan: after a record is admitted, before the next one.
    /// Path: the record (TC-COMPAT.HOTUPGRADE-01).
    RecoverAfterAdmit,
    /// Restart scan: after every record has been read, before the pass that
    /// removes from the eviction manager the objects whose record has gone
    /// meanwhile. Path: the cache root (TC-EVICTION.RECONSTRUCT-01,
    /// TC-STORAGE.EXECUTOR-01, TC-COMPAT.HOTUPGRADE-01).
    RecoverBeforeGhostPass,
    /// Metadata update: before the key's publication lock is taken, and so
    /// before the record is opened. A test holds an update here to replace
    /// or purge the record first. Path: the canonical record
    /// (FDS-STORAGE.METAUPDATE-01).
    UpdateBeforeLock,
    /// Expiry: before the key's publication lock is taken, and so before the
    /// record is opened. A test holds an expiry here to replace or purge the
    /// record first. Path: the canonical record (FDS-STORAGE.EXPIRE-01).
    ExpireBeforeLock,
    /// Metadata update or expiry, in place: after the new metadata is
    /// written into the record, before the header that commits it, with the
    /// key's publication lock held. The record's checksums fail until then.
    /// Path: the canonical record (TC-LAYOUT.OBJECTFILE-01,
    /// TC-STORAGE.RECLAIM-01).
    InPlaceBetweenWrites,
    /// Expiry: after the record's metadata is read and its expired form
    /// computed, before it is written back, with the key's publication lock
    /// still held. A test holds it here to show that a concurrent expiry or
    /// metadata update waits. Path: the canonical record
    /// (TC-STORAGE.EXPIRE-01).
    ExpireBeforeWrite,
    /// Store, purge, and reclaim: after the key's publication lock is
    /// released (for a purge, after the directory is also synchronised),
    /// before the record that the rename replaced, or the unlink removed, is
    /// closed. That close frees the record. Path: the canonical record
    /// (TC-STORAGE.EXECUTOR-01).
    BeforeFreeingOldRecord,
    /// Store and hit: just before a dropped handler's file is closed, when
    /// the file has no name left, so that the close frees it: on the
    /// blocking pool inside a runtime, or where the handler is dropped
    /// outside one. The file is an aborted or discarded store's temporary, a
    /// record replaced or purged while a hit read it, or a removed temporary
    /// that a partial hit read. Path: that file (FDS-STORAGE.EXECUTOR-01).
    BeforeClosingRemovedFile,
    /// Store: at the start of its miss handler's `Drop`. A test holds a store
    /// here to see on which thread its handler is dropped. Path: the
    /// canonical record (FDS-STORAGE.EXECUTOR-01).
    StoreDropping,
}

/// An I/O operation a test can make fail. Each says which test cases' tests
/// make it fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FaultPoint {
    /// Writing to a store's temporary: its fixed part as the store opens,
    /// or a body chunk (TC-COMPAT.WRITEFAIL-01, TC-COMPAT.ABORTSPACE-01).
    WriteTemp,
    /// Synchronising a store's temporary before it is renamed into place
    /// (TC-COMPAT.WRITEFAIL-01, TC-STORAGE.PUBLISH-01).
    SyncTemp,
    /// Renaming a store's temporary over its canonical name
    /// (TC-COMPAT.WRITEFAIL-01).
    Rename,
    /// Writing the header that withdraws a record whose new metadata outgrew
    /// its region (TC-COMPLIANCE.REVALIDATION-01).
    WithdrawRecord,
    /// Synchronising a shard directory after a purge (TC-STORAGE.PURGE-01).
    SyncDir,
}

type NameOverride = Arc<dyn Fn(&str) -> String + Send + Sync>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

#[derive(Default)]
struct GateState {
    reached: bool,
    released: bool,
}

/// One armed pause: the backend blocks in [`Hooks::pause`] until the test
/// calls [`PauseHandle::release`].
#[derive(Default)]
struct Gate {
    state: Mutex<GateState>,
    cv: Condvar,
}

/// A test's handle on an armed pause.
pub struct PauseHandle {
    gate: Arc<Gate>,
}

impl PauseHandle {
    /// Block until the backend reaches the pause point, or `timeout`
    /// elapses. Returns whether it was reached.
    pub fn wait_reached(&self, timeout: Duration) -> bool {
        let guard = lock(&self.gate.state);
        let (guard, _) = self
            .gate
            .cv
            .wait_timeout_while(guard, timeout, |s| !s.reached)
            .unwrap_or_else(|p| p.into_inner());
        guard.reached
    }

    /// Let the paused backend continue.
    pub fn release(&self) {
        lock(&self.gate.state).released = true;
        self.gate.cv.notify_all();
    }
}

impl Drop for PauseHandle {
    fn drop(&mut self) {
        // Never leave the backend blocked behind a test that gave up. A
        // pause released unreached is discarded by the next operation
        // that reaches its point.
        self.release();
    }
}

struct Armed {
    path: Option<PathBuf>,
    gate: Arc<Gate>,
}

/// Configuration fixed at construction.
#[derive(Clone, Default)]
pub struct HookConfig {
    /// Maps the name a key's files would have to the one the backend
    /// uses for its path (TC-LAYOUT.SHARDING-01).
    pub object_name: Option<NameOverride>,
}

/// The runtime hooks of one backend instance.
#[derive(Default)]
pub struct Hooks {
    pub(crate) config: HookConfig,
    pauses: Mutex<HashMap<PausePoint, Vec<Armed>>>,
    faults: Mutex<HashMap<FaultPoint, usize>>,
    meta_decodes: AtomicU64,
}

impl Hooks {
    pub(crate) fn new(config: HookConfig) -> Self {
        Hooks {
            config,
            ..Default::default()
        }
    }

    /// Arm a one-shot pause at `point`, optionally only for the entry at
    /// `path`.
    ///
    /// A pause blocks the thread that reaches it until the test releases it.
    /// When that thread is a worker of the test's own runtime, as for the
    /// restart scan's reads, the runtime needs a second worker for the test
    /// to go on meanwhile: `#[tokio::test(flavor = "multi_thread",
    /// worker_threads = 2)]`.
    pub fn arm_pause(&self, point: PausePoint, path: Option<&Path>) -> PauseHandle {
        let gate = Arc::new(Gate::default());
        lock(&self.pauses).entry(point).or_default().push(Armed {
            path: path.map(Path::to_path_buf),
            gate: Arc::clone(&gate),
        });
        PauseHandle { gate }
    }

    pub(crate) fn pause(&self, point: PausePoint, path: &Path) {
        let gate = {
            let mut pauses = lock(&self.pauses);
            let Some(armed) = pauses.get_mut(&point) else {
                return;
            };
            // A pause released before anything reached it, its handle
            // dropped, holds nothing: discard it, rather than let it take
            // this operation's turn from a pause armed after it.
            armed.retain(|a| !lock(&a.gate.state).released);
            let Some(i) = armed
                .iter()
                .position(|a| a.path.as_deref().is_none_or(|p| p == path))
            else {
                return;
            };
            armed.remove(i).gate
        };
        let mut state = lock(&gate.state);
        state.reached = true;
        gate.cv.notify_all();
        while !state.released {
            state = gate.cv.wait(state).unwrap_or_else(|p| p.into_inner());
        }
    }

    /// Make the next `times` operations at `point` fail.
    pub fn inject_fault(&self, point: FaultPoint, times: usize) {
        *lock(&self.faults).entry(point).or_default() += times;
    }

    pub(crate) fn fault(&self, point: FaultPoint) -> pingora_error::Result<()> {
        let mut faults = lock(&self.faults);
        match faults.get_mut(&point) {
            Some(n) if *n > 0 => {
                *n -= 1;
                pingora_error::Error::e_explain(
                    pingora_error::ErrorType::Custom("injected_fault"),
                    format!("injected fault at {point:?}"),
                )
            }
            _ => Ok(()),
        }
    }

    /// How many times this backend has decoded a record's metadata.
    pub fn meta_decodes(&self) -> u64 {
        self.meta_decodes.load(Ordering::Relaxed)
    }

    pub(crate) fn count_meta_decode(&self) {
        self.meta_decodes.fetch_add(1, Ordering::Relaxed);
    }
}

mod tests {
    use super::*;

    /// A pause dropped before anything reached it holds nothing, and takes
    /// no operation's turn: a pause armed after it at the same point holds
    /// the first operation to arrive.
    #[test]
    fn a_dropped_pause_takes_no_operations_turn() {
        let hooks = Hooks::default();
        let path = Path::new("/a/record");
        drop(hooks.arm_pause(PausePoint::StoreDropping, Some(path)));
        let second = hooks.arm_pause(PausePoint::StoreDropping, Some(path));
        std::thread::scope(|scope| {
            scope.spawn(|| hooks.pause(PausePoint::StoreDropping, path));
            assert!(
                second.wait_reached(Duration::from_secs(5)),
                "the first operation to arrive is held"
            );
            second.release();
        });
    }
}
