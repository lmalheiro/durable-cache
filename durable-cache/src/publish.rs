// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The steps that publish a record, and that clean up around it
//! (FDS-STORAGE.PUBLISH-01, ADR-STORAGE.ATOMICPUBLISH-01,
//! ADR-PERSISTENCE.DURABILITY-01).
//!
//! A store ([`crate::miss`]) writes a temporary in the canonical name's own
//! directory, synchronises its data to stable storage ([`sync_temp`]), and
//! then, under the key's lock, renames it over the canonical name
//! ([`rename`]). The directory is not synchronised after the rename: a host
//! crash may lose the publication, but the name never refers to data that
//! is not on stable storage. A temporary that will not be published is removed
//! ([`discard_temp`]). The record that a rename replaces, or that a purge's
//! unlink removes, is held open across that call ([`hold_open`]), so that
//! it is freed after the lock is released rather than under it.

use std::fs::File;
use std::io;
use std::path::Path;

/// `fdatasync` the temporary: after this returns, its data is on stable
/// storage, so the rename below can never publish a name over data that is
/// not (ADR-PERSISTENCE.DURABILITY-01).
pub fn sync_temp(file: &File) -> io::Result<()> {
    file.sync_data()
}

/// Publish `temp_path` as `canonical_path` with one `rename`, which
/// replaces any record already there in the same step. The temporary is
/// always in the canonical name's own directory, so the rename is atomic by
/// placement (ADR-STORAGE.ATOMICPUBLISH-01).
///
/// The caller holds the key's publication lock and the read-only gate.
pub fn rename(temp_path: &Path, canonical_path: &Path) -> io::Result<()> {
    std::fs::rename(temp_path, canonical_path)
}

/// Open the record whose last name a rename or an unlink is about to take,
/// so that the kernel frees it when the returned file is dropped, rather
/// than inside that call. The caller makes that call under the key's
/// publication lock, and freeing a large file can take a long time,
/// depending on the file system and the device, so it must not happen
/// while the lock is held: the caller drops the file once the lock is
/// released.
///
/// `None` if there is no record at `path`. If it cannot be opened, that is
/// logged as a warning and `None` is returned: the rename or unlink then
/// frees the record itself, under the lock.
pub fn hold_open(path: &Path) -> Option<File> {
    match File::open(path) {
        Ok(file) => Some(file),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => {
            log::warn!(
                "could not open {} before replacing or removing it, \
                 so it is freed under the key's lock: {e}",
                path.display()
            );
            None
        }
    }
}

/// Remove a temporary that will never be published: a store's, when the
/// store failed, was abandoned, or completed once the backend was
/// read-only. Best effort: a temporary already gone is not an error, any
/// other failure is only logged, and the restart-time scan reclaims what is
/// left (FDS-STORAGE.RECLAIM-01).
pub fn discard_temp(temp_path: &Path) {
    if let Err(e) = std::fs::remove_file(temp_path) {
        if e.kind() != io::ErrorKind::NotFound {
            log::warn!(
                "could not remove the temporary {}: {e}",
                temp_path.display()
            );
        }
    }
}
