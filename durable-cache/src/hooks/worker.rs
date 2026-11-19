// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The worker faults: calls on the async worker, from the page cache only,
//! that a test makes fail (TC-STORAGE.PAGECACHE-01). Linux only, as those
//! calls are.

use std::path::{Path, PathBuf};

use super::{lock, Hooks};

/// A call on the async worker, from the page cache only, that a test can
/// make fail: report that it would wait for the device, or that the system
/// does not support it (TC-STORAGE.PAGECACHE-01).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerCall {
    /// A lookup's open of a record or of a store's temporary. Path: that
    /// file.
    Open,
    /// A lookup's read of a record's fixed part. Path: the record or
    /// temporary.
    RecordRead,
    /// A read of a body chunk. Path: the record or temporary read.
    BodyRead,
}

/// One armed worker fault: the call to make fail, on which file, after how
/// many such calls go through, and with what kind of error.
pub(super) struct WorkerFault {
    call: WorkerCall,
    path: PathBuf,
    skip: usize,
    kind: std::io::ErrorKind,
}

impl Hooks {
    /// Make one `call` on the worker, on the file at `path`, report that it
    /// would wait: the first after `skip` such calls go through.
    pub fn arm_worker_wait(&self, call: WorkerCall, path: &Path, skip: usize) {
        self.arm_worker_fault(call, path, skip, std::io::ErrorKind::WouldBlock);
    }

    /// Make one `call` on the worker, on the file at `path`, report that the
    /// system does not support it: the first after `skip` such calls go
    /// through.
    pub fn arm_worker_unsupported(&self, call: WorkerCall, path: &Path, skip: usize) {
        self.arm_worker_fault(call, path, skip, std::io::ErrorKind::Unsupported);
    }

    /// Make one `call` on the worker, on the file at `path`, report that the
    /// file does not exist, as when it is removed just before: the first
    /// after `skip` such calls go through.
    pub fn arm_worker_missing(&self, call: WorkerCall, path: &Path, skip: usize) {
        self.arm_worker_fault(call, path, skip, std::io::ErrorKind::NotFound);
    }

    fn arm_worker_fault(
        &self,
        call: WorkerCall,
        path: &Path,
        skip: usize,
        kind: std::io::ErrorKind,
    ) {
        lock(&self.worker_faults).push(WorkerFault {
            call,
            path: path.to_path_buf(),
            skip,
            kind,
        });
    }

    /// The error this `call` on `path` is to fail with, if a test armed one.
    pub(crate) fn worker_fault(&self, call: WorkerCall, path: &Path) -> Option<std::io::Error> {
        let mut faults = lock(&self.worker_faults);
        let i = faults
            .iter()
            .position(|w| w.call == call && w.path == path)?;
        if faults[i].skip > 0 {
            faults[i].skip -= 1;
            return None;
        }
        let kind = faults.remove(i).kind;
        Some(std::io::Error::new(kind, format!("injected at {call:?}")))
    }
}
