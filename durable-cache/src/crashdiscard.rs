// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The discard after a host restart (ADR-PERSISTENCE.UNSYNCED-01,
//! FDS-PERSISTENCE.CRASHDISCARD-01).
//!
//! With `Durability::NoStoreSync`, a store is renamed into place without
//! synchronising its temporary first, so a host crash can leave a published
//! name over a body that never reached the device. The read validation
//! turns a body left short or empty into a miss; a body of the right length
//! that reads back as zeros it cannot see. With the discard switched on
//! (`FsCacheConfig::with_discard_after_host_restart`, Linux only), the
//! backend can tell whether that can have happened:
//!
//! - **The marker.** Before the first unsynchronised store is renamed into
//!   place, the instance writes, durably, a marker beside its writer lease,
//!   `writers/<pid>-<nonce>.unsynced`, holding the current boot's identity
//!   ([`Marker::ensure`]).
//! - **The flush.** [`Marker::flush`] flushes the file system that holds
//!   the cache, so everything published is on the device, and then removes
//!   the instance's marker and those of instances that have stopped.
//! - **At construction**, whatever the configuration, a marker naming
//!   another boot means stores published unsynchronised may have been lost.
//!   The backend then moves every first-level shard directory into
//!   `discarded-<nonce>` under the root, before it serves anything, and only
//!   then removes the markers ([`at_construction`]). The moved tree is
//!   removed in the background ([`remove_discarded`]), outside the tree the
//!   restart scan walks.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use pingora_error::{Error, ErrorType, OrErr, Result};

#[cfg(test)]
use crate::hooks::{FaultPoint, Hooks};
use crate::writerid::{self, WriterId, LEASE_DIR};
use crate::{sharding, FsCacheConfig};

// IMP-PERSISTENCE.CRASHDISCARD-01: satisfy FDS-PERSISTENCE.CRASHDISCARD-01

/// Where Linux keeps the current boot's identity, a random UUID drawn at
/// every boot.
pub(crate) const BOOT_ID_PATH: &str = "/proc/sys/kernel/random/boot_id";
/// A marker's extension: `<pid>-<nonce>.unsynced`.
const MARKER_SUFFIX: &str = "unsynced";
/// The extra extension of a marker being written, before it is renamed
/// into place.
const STAGING_SUFFIX: &str = "new";
/// The prefix of a directory of discarded shards: `discarded-<nonce>`.
const DISCARDED_PREFIX: &str = "discarded-";

/// Whether the configuration switches the discard on: never elsewhere than
/// on Linux, where the option does not exist.
pub(crate) fn discard_on(config: &FsCacheConfig) -> bool {
    #[cfg(target_os = "linux")]
    {
        config.discard_after_host_restart()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        false
    }
}

/// This instance's marker.
pub(crate) struct Marker {
    /// The current boot's identity, read at construction with the discard
    /// on; `None` with it off, and then no marker is ever written.
    boot_id: Option<String>,
    /// Whether the marker is in place. Held while the marker is written, so
    /// the stores that reach that point together write it once and all wait
    /// for it; and held by a flush from before it flushes the file system
    /// until it has removed the marker, so no store can find the marker in
    /// place meanwhile and publish data that flush may have missed.
    written: Mutex<bool>,
}

impl Marker {
    pub(crate) fn new(boot_id: Option<String>) -> Self {
        Marker {
            boot_id,
            written: Mutex::new(false),
        }
    }

    fn written(&self) -> MutexGuard<'_, bool> {
        self.written.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Before a store published without synchronisation is renamed into
    /// place, with all its data written: make sure the marker of instance
    /// `id` is in place under `root`, writing it if not. Nothing with the
    /// discard off. Whether this call wrote it.
    pub(crate) fn ensure(
        &self,
        root: &Path,
        id: &WriterId,
        #[cfg(test)] hooks: &Hooks,
    ) -> io::Result<bool> {
        let Some(boot_id) = &self.boot_id else {
            return Ok(false);
        };
        let mut written = self.written();
        if *written {
            return Ok(false);
        }
        write_marker(
            root,
            id,
            boot_id,
            #[cfg(test)]
            hooks,
        )?;
        *written = true;
        Ok(true)
    }

    /// Flush the file system that holds the cache at `root`, then remove the
    /// marker of instance `id` and those of instances whose lease is no
    /// longer held: the flush made what they published durable too.
    pub(crate) fn flush(
        &self,
        root: &Path,
        id: &WriterId,
        #[cfg(test)] hooks: &Hooks,
    ) -> io::Result<()> {
        let mut written = self.written();
        sync_file_system(
            root,
            #[cfg(test)]
            hooks,
        )?;
        // Before the marker goes, so that a removal that fails halfway, or
        // whose directory sync fails, never leaves this instance believing
        // in a marker that is gone: the next unsynchronised store writes it
        // again. A removal a host crash undoes errs on the safe side.
        *written = false;
        let removed = remove_markers(root, |writer| {
            writer == *id || !writerid::writer_may_be_running(root, &writer)
        })?;
        if removed > 0 {
            sync_dir(
                &root.join(LEASE_DIR),
                #[cfg(test)]
                hooks,
            )?;
        }
        Ok(())
    }
}

/// The path of instance `id`'s marker under `root`.
pub(crate) fn marker_path(root: &Path, id: &WriterId) -> PathBuf {
    root.join(LEASE_DIR).join(format!("{id}.{MARKER_SUFFIX}"))
}

/// The staging name a marker is written under.
fn staging_path(marker: &Path) -> PathBuf {
    let mut path = marker.as_os_str().to_owned();
    path.push(format!(".{STAGING_SUFFIX}"));
    PathBuf::from(path)
}

/// Write instance `id`'s marker, naming `boot_id`, durably: a staging file,
/// synchronised, renamed into place, and the lease directory synchronised.
fn write_marker(
    root: &Path,
    id: &WriterId,
    boot_id: &str,
    #[cfg(test)] hooks: &Hooks,
) -> io::Result<()> {
    let marker = marker_path(root, id);
    let staging = staging_path(&marker);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&staging)?;
    file.write_all(format!("{boot_id}\n").as_bytes())?;
    file.sync_data()?;
    std::fs::rename(&staging, &marker)?;
    sync_dir(
        &root.join(LEASE_DIR),
        #[cfg(test)]
        hooks,
    )
}

/// The writer a marker or a staging marker belongs to, if `name` is one,
/// spelled exactly as this module writes it.
fn marker_writer(name: &str) -> Option<WriterId> {
    let id = name
        .strip_suffix(&format!(".{MARKER_SUFFIX}"))
        .or_else(|| name.strip_suffix(&format!(".{MARKER_SUFFIX}.{STAGING_SUFFIX}")))?;
    WriterId::parse(id)
}

/// Remove the markers, staging ones included, of the writers `which`
/// selects: how many were removed.
fn remove_markers(root: &Path, which: impl Fn(WriterId) -> bool) -> io::Result<usize> {
    let entries = match std::fs::read_dir(root.join(LEASE_DIR)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(e),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let Some(writer) = entry.file_name().to_str().and_then(marker_writer) else {
            continue;
        };
        if !which(writer) {
            continue;
        }
        match std::fs::remove_file(entry.path()) {
            Ok(()) => removed += 1,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(removed)
}

/// The boot identity each marker in place names, or `None` for one that
/// cannot be read. Staging markers are left out: a store waits for its
/// marker's rename, so none was published on the strength of one. So are
/// the markers of writers still running, whose lease is held: an instance
/// that runs now was started in this boot, and its stores are in the page
/// cache, whatever its marker says.
fn marker_boots(root: &Path) -> io::Result<Vec<Option<String>>> {
    let entries = match std::fs::read_dir(root.join(LEASE_DIR)) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut boots = Vec::new();
    for entry in entries {
        let entry = entry?;
        let writer = entry
            .file_name()
            .to_str()
            .and_then(|name| name.strip_suffix(&format!(".{MARKER_SUFFIX}")))
            .and_then(WriterId::parse);
        if writer.is_some_and(|writer| !writerid::writer_may_be_running(root, &writer)) {
            boots.push(read_boot_id(&entry.path()).ok());
        }
    }
    Ok(boots)
}

/// The boot identity in the file at `path`, without its line end.
fn read_boot_id(path: &Path) -> io::Result<String> {
    let id = std::fs::read_to_string(path)?.trim().to_owned();
    if id.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "empty"));
    }
    Ok(id)
}

/// The check at construction (FDS-PERSISTENCE.CRASHDISCARD-01), before the
/// shard-directory cache is built: if a marker names a boot other than the
/// current one, read from `boot_id_path`, discard the cache. Returns the
/// current boot's identity with the discard on, for this instance's marker.
///
/// With the discard on, a boot identity that cannot be read is an error. A
/// backend with it off reads the identity only if it finds markers, and
/// discards the cache if it cannot read it, as it cannot tell their boot
/// from the current one; it discards too for a marker it cannot read.
pub(crate) fn at_construction(
    root: &Path,
    discard: bool,
    boot_id_path: &Path,
    #[cfg(test)] hooks: &Hooks,
) -> Result<Option<String>> {
    let boots = marker_boots(root).or_err_with(ErrorType::FileReadError, || {
        format!(
            "reading the unsynchronised-store markers in {}",
            root.join(LEASE_DIR).display()
        )
    })?;
    let current = if discard {
        match read_boot_id(boot_id_path) {
            Ok(id) => Some(id),
            Err(e) => {
                return Error::e_because(
                    ErrorType::FileReadError,
                    format!(
                        "reading the boot identity from {}, which \
                         FsCacheConfig::with_discard_after_host_restart needs",
                        boot_id_path.display()
                    ),
                    e,
                )
            }
        }
    } else if boots.is_empty() {
        None
    } else {
        read_boot_id(boot_id_path).ok()
    };
    let stale = boots
        .iter()
        .any(|boot| boot.is_none() || boot.as_deref() != current.as_deref());
    if stale {
        log::warn!(
            "{} was written without synchronisation before a host restart \
             and was never flushed: discarding every record and temporary",
            root.display()
        );
        discard_cache(
            root,
            #[cfg(test)]
            hooks,
        )
        .or_err_with(ErrorType::FileWriteError, || {
            format!("discarding the cache under {}", root.display())
        })?;
    }
    Ok(current.filter(|_| discard))
}

/// Move every first-level shard directory under `root` into a new
/// `discarded-<nonce>` directory, synchronise the root, and only then remove
/// every marker and synchronise the lease directory: a crash before that
/// leaves the markers, and the discard is done again at the next start.
fn discard_cache(root: &Path, #[cfg(test)] hooks: &Hooks) -> io::Result<()> {
    let discarded = root.join(format!(
        "{DISCARDED_PREFIX}{:016x}",
        writerid::random_nonce()
    ));
    std::fs::create_dir(&discarded)?;
    for first in sharding::first_level_dirs(root) {
        let Some(name) = first.file_name() else {
            continue;
        };
        match std::fs::rename(&first, discarded.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    sync_dir(
        root,
        #[cfg(test)]
        hooks,
    )?;
    remove_markers(root, |_| true)?;
    sync_dir(
        &root.join(LEASE_DIR),
        #[cfg(test)]
        hooks,
    )
}

/// Whether `name` names a directory of discarded shards, spelled exactly as
/// [`discard_cache`] names one.
fn is_discarded_name(name: &str) -> bool {
    name.strip_prefix(DISCARDED_PREFIX).is_some_and(|nonce| {
        nonce.len() == 16
            && nonce
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    })
}

/// Remove every directory of discarded shards under `root`, this start's or
/// one a crash left behind, in the background: on the blocking pool inside
/// a Tokio runtime, on a thread of its own outside one. Failures are logged.
pub(crate) fn remove_discarded(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let dirs: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_str().is_some_and(is_discarded_name))
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .map(|entry| entry.path())
        .collect();
    if dirs.is_empty() {
        return;
    }
    let remove = move || {
        for dir in dirs {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => log::info!("removed the discarded shards in {}", dir.display()),
                Err(e) => log::warn!(
                    "could not remove the discarded shards in {}: {e}",
                    dir.display()
                ),
            }
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(runtime) => drop(runtime.spawn_blocking(remove)),
        Err(_) => {
            if let Err(e) = std::thread::Builder::new()
                .name("durable-cache-discard".into())
                .spawn(remove)
            {
                log::warn!("could not start removing the discarded shards: {e}");
            }
        }
    }
}

/// `fsync` the directory at `path`, so the entries just changed in it
/// survive a host crash.
fn sync_dir(path: &Path, #[cfg(test)] hooks: &Hooks) -> io::Result<()> {
    test_hook! { injected(hooks, FaultPoint::SyncCacheDir)?; }
    File::open(path)?.sync_all()
}

/// The fault a test armed at `point`, as an I/O error.
#[cfg(test)]
fn injected(hooks: &Hooks, point: FaultPoint) -> io::Result<()> {
    hooks
        .fault(point)
        .map_err(|e| io::Error::other(e.to_string()))
}

/// Flush the file system that holds `root`: with `syncfs` on Linux, and
/// elsewhere with `sync`, which flushes every file system, and on some
/// systems returns before the writes complete. No marker depends on the
/// latter: the discard exists only on Linux.
fn sync_file_system(root: &Path, #[cfg(test)] hooks: &Hooks) -> io::Result<()> {
    test_hook! { injected(hooks, FaultPoint::SyncFileSystem)?; }
    #[cfg(target_os = "linux")]
    {
        rustix::fs::syncfs(File::open(root)?).map_err(io::Error::from)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = root;
        // SAFETY: `sync` takes no arguments and touches no memory of this
        // process.
        unsafe { libc::sync() };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Markers and staging markers parse back to their writer, and nothing
    /// else does: a lease, another extension, another spelling.
    #[test]
    fn only_markers_name_a_writer() {
        let id = WriterId::parse("4711-6fd5a44b00000001").expect("an identity");
        assert_eq!(marker_writer("4711-6fd5a44b00000001.unsynced"), Some(id));
        assert_eq!(
            marker_writer("4711-6fd5a44b00000001.unsynced.new"),
            Some(id)
        );
        for name in [
            "4711-6fd5a44b00000001.lock",
            "4711-6fd5a44b00000001.unsynced.bak",
            "04711-6fd5a44b00000001.unsynced",
            "notes.unsynced",
        ] {
            assert_eq!(marker_writer(name), None, "{name}");
        }
    }

    /// Only the names `discard_cache` writes are discarded trees.
    #[test]
    fn only_discarded_names_are_removed() {
        assert!(is_discarded_name("discarded-00ff00ff00ff00ff"));
        for name in [
            "discarded-",
            "discarded-00FF00FF00FF00FF",
            "discarded-00ff00ff00ff00f",
            "discarded-00ff00ff00ff00ff0",
            "00",
            "writers",
        ] {
            assert!(!is_discarded_name(name), "{name}");
        }
    }
}
