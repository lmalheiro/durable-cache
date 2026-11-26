// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! `cargo xtask crash-replay`: the setup of TC-PERSISTENCE.CRASHSAFE-03. A
//! file system on a device-mapper `log-writes` device, which logs every
//! write the file system sends the device, with its flush and FUA flags; the
//! crate's replay scenario on it, which marks the log after each operation
//! the backend acknowledges; then the log replayed write by write, with the
//! state checked after each write and after each block of a longer one, as
//! a power failure may leave a write torn --- the test case's flush and FUA
//! points among them. Each state is mounted from a copy and checked by the
//! crate's replay check, which knows the marks the replay has passed.
//!
//! Linux only, with `sudo` that asks for no password: the loop devices, the
//! `log-writes` target, `mkfs` and `mount` need root. The builds and the
//! tests run as the user. The replay is this module's own, over the log's
//! format as `drivers/md/dm-log-writes.c` writes it.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// The file systems the replay runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Fs {
    /// ext4, in its default `data=ordered` mode, which writes a file's data
    /// before the metadata that gives the file its length.
    Ext4,
    /// ext4 mounted with `data=writeback` and without delayed allocation,
    /// which records a file's length, and the blocks it allocates, with the
    /// next commit of the journal, whether its data is written or not: only
    /// the store's synchronisation keeps a published name from naming data
    /// the device does not have.
    Ext4Writeback,
    /// XFS, which writes a file's data before it logs the file's new length.
    Xfs,
}

impl Fs {
    /// Its name on the command line.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Ext4 => "ext4",
            Self::Ext4Writeback => "ext4-writeback",
            Self::Xfs => "xfs",
        }
    }

    /// The options it is mounted with.
    fn mount_options(self) -> &'static str {
        match self {
            Self::Ext4Writeback => "data=writeback,nodelalloc",
            Self::Ext4 | Self::Xfs => "defaults",
        }
    }
}

/// How the scenario's stores are made, as the crate's replay tests read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Mode {
    /// Each store synchronised before its rename.
    Strict,
    /// Unsynchronised, with the discard after a host restart on.
    UnsyncedDiscard,
    /// Unsynchronised, with the discard off.
    UnsyncedKeep,
}

impl Mode {
    /// Its name on the command line, and as the crate's replay tests read it.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Strict => "strict",
            Self::UnsyncedDiscard => "unsynced-discard",
            Self::UnsyncedKeep => "unsynced-keep",
        }
    }
}

/// The runs `cargo xtask verify --checker log-writes` makes: each file
/// system with stores synchronised, and ext4 unsynchronised, with the
/// discard on and off, as the test case's step 4 asks.
pub(crate) const MATRIX: [(Fs, Mode); 5] = [
    (Fs::Ext4, Mode::Strict),
    (Fs::Ext4Writeback, Mode::Strict),
    (Fs::Xfs, Mode::Strict),
    (Fs::Ext4, Mode::UnsyncedDiscard),
    (Fs::Ext4, Mode::UnsyncedKeep),
];

/// The scenario and the check, in the crate's test binary.
const SCENARIO: &str =
    "integration_tests::persistence_crashsafe::tc_persistence_crashsafe_03_replay_scenario";
const CHECK: &str =
    "integration_tests::persistence_crashsafe::tc_persistence_crashsafe_03_replay_check";

/// The log's format: its magic number and version, and the flags of an
/// entry.
const LOG_MAGIC: u64 = 0x006a_7366_7773_6872;
const LOG_VERSION: u64 = 1;
const FLUSH: u64 = 1 << 0;
const FUA: u64 = 1 << 1;
const DISCARD: u64 = 1 << 2;
const MARK: u64 = 1 << 3;

/// The mark set once `mkfs` is done: no state before it holds a file system.
const MKFS_MARK: &str = "mkfs";

/// How much of a write the replay applies before checking the state it
/// leaves: a block of the file systems'. A power failure can leave a write
/// torn, its first blocks on the device and the rest not.
const TORN_STEP: u64 = 4096;

/// `cargo xtask crash-replay --fs <fs> --mode <mode>`.
pub(crate) fn crash_replay(root: &Path, target: &Path, fs: Fs, mode: Mode) -> Result<usize> {
    let tests = test_binary(root)?;
    let work = target
        .join("xtask/crash-replay")
        .join(format!("{}-{}", fs.name(), mode.name()));
    if work.exists() {
        std::fs::remove_dir_all(&work)
            .with_context(|| format!("failed to clear {}", work.display()))?;
    }
    std::fs::create_dir_all(&work)?;
    let mnt = work.join("mnt");
    std::fs::create_dir(&mnt)?;
    let before = work.join("boot-before");
    let after = work.join("boot-after");
    std::fs::write(&before, "before-crash\n")?;
    std::fs::write(&after, "after-crash\n")?;

    // XFS wants at least 300 MiB.
    let size: u64 = match fs {
        Fs::Ext4 | Fs::Ext4Writeback => 128 << 20,
        Fs::Xfs => 320 << 20,
    };
    let data = sparse_file(&work.join("data.img"), size)?;
    let log = sparse_file(&work.join("log.img"), 2 << 30)?;

    println!(
        "==> {} {}: the scenario on a log-writes device",
        fs.name(),
        mode.name()
    );
    {
        let data_loop = Loop::attach(&data)?;
        let log_loop = Loop::attach(&log)?;
        let device = format!("durable-cache-lw-{}", std::process::id());
        let mapped = Mapped::create(&device, size / 512, &data_loop.0, &log_loop.0)?;
        match fs {
            Fs::Ext4 | Fs::Ext4Writeback => sudo(&[
                "mkfs.ext4",
                "-q",
                "-F",
                "-E",
                "lazy_itable_init=0,lazy_journal_init=0",
                &mapped.path(),
            ])?,
            Fs::Xfs => sudo(&["mkfs.xfs", "-q", "-f", &mapped.path()])?,
        }
        mapped.mark(MKFS_MARK)?;
        let mounted = Mount::new(&mapped.path(), &mnt, fs.mount_options())?;
        let cache = mnt.join("cache");
        let status = Command::new(&tests)
            .args([SCENARIO, "--exact", "--ignored", "--nocapture"])
            .env("DURABLE_CACHE_REPLAY_DIR", &cache)
            .env("DURABLE_CACHE_REPLAY_DEVICE", &device)
            .env("DURABLE_CACHE_REPLAY_MODE", mode.name())
            .env("DURABLE_CACHE_REPLAY_BOOT", &before)
            .status()
            .context("failed to run the replay scenario")?;
        drop(mounted);
        drop(mapped);
        if !status.success() {
            bail!("the replay scenario failed");
        }
    }

    println!(
        "==> {} {}: each state, write by write and block by block",
        fs.name(),
        mode.name()
    );
    let replay_path = sparse_file(&work.join("replay.img"), size)?;
    let replay = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&replay_path)
        .context("failed to open the replayed image")?;
    let check_path = work.join("check.img");
    let mut log = File::open(&log).context("failed to open the log")?;
    let mut marks: Vec<String> = Vec::new();
    let mut states = 0;
    let mut points = 0;
    let replayed = replay_log(&mut log, &replay, |entry| {
        if let Some(name) = &entry.mark {
            marks.push(name.clone());
        }
        if !marks.iter().any(|mark| mark == MKFS_MARK) {
            return Ok(());
        }
        states += 1;
        if entry.torn.is_none() && entry.flags & (FLUSH | FUA) != 0 {
            points += 1;
        }
        replay.sync_all()?;
        check_state(
            &tests,
            &replay_path,
            &check_path,
            &mnt,
            fs,
            mode,
            &after,
            &marks,
        )
        .with_context(|| {
            let applied = match entry.torn {
                Some(bytes) => format!("{bytes} bytes of log entry {}", entry.index),
                None => format!("log entry {}", entry.index),
            };
            format!(
                "the state after {applied} (the {states}th checked, marks {}) fails",
                marks.join(",")
            )
        })
    })?;
    println!(
        "crash-replay {} {}: {replayed} log entries, {states} states checked \
         ({points} at flush and FUA points), all as TC-PERSISTENCE.CRASHSAFE-03 expects",
        fs.name(),
        mode.name()
    );
    Ok(states)
}

/// Check one replayed state: mount a copy of it, which replays the file
/// system's journal as a restart would, and run the crate's check on it.
#[allow(clippy::too_many_arguments)]
fn check_state(
    tests: &Path,
    replay: &Path,
    check: &Path,
    mnt: &Path,
    fs: Fs,
    mode: Mode,
    boot: &Path,
    marks: &[String],
) -> Result<()> {
    let status = Command::new("cp")
        .arg("--sparse=always")
        .arg(replay)
        .arg(check)
        .status()
        .context("failed to copy the replayed state")?;
    if !status.success() {
        bail!("cp failed");
    }
    let check_loop = Loop::attach(check)?;
    let mounted = Mount::new(&check_loop.0, mnt, fs.mount_options())?;
    let output = Command::new(tests)
        .args([CHECK, "--exact", "--ignored", "--nocapture"])
        .env("DURABLE_CACHE_REPLAY_DIR", mnt.join("cache"))
        .env("DURABLE_CACHE_REPLAY_MODE", mode.name())
        .env("DURABLE_CACHE_REPLAY_BOOT", boot)
        .env("DURABLE_CACHE_REPLAY_MARKS", marks.join(","))
        .output()
        .context("failed to run the replay check")?;
    drop(mounted);
    drop(check_loop);
    if !output.status.success() {
        bail!(
            "the check failed:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// One entry of the log, as the replay hands it on.
struct Entry {
    index: u64,
    flags: u64,
    mark: Option<String>,
    /// `Some(bytes)` when only the first `bytes` of the entry's data are
    /// applied so far, as a write torn by a power failure leaves them.
    torn: Option<u64>,
}

/// Replay `log` onto `replay`, entry by entry, calling `each` after each one
/// is applied, and after each `TORN_STEP` bytes of a longer write. Returns
/// how many entries there were.
fn replay_log(
    log: &mut File,
    replay: &File,
    mut each: impl FnMut(&Entry) -> Result<()>,
) -> Result<u64> {
    let mut super_block = [0u8; 28];
    log.read_exact(&mut super_block)
        .context("failed to read the log's super block")?;
    let field =
        |at: usize| u64::from_le_bytes(super_block[at..at + 8].try_into().expect("8 bytes"));
    if field(0) != LOG_MAGIC || field(8) != LOG_VERSION {
        bail!("not a log-writes log of version {LOG_VERSION}");
    }
    let entries = field(16);
    let sector_size = u64::from(u32::from_le_bytes(
        super_block[24..28].try_into().expect("4 bytes"),
    ));
    if !sector_size.is_power_of_two() || sector_size < 512 {
        bail!("the log's sector size is {sector_size}");
    }

    let mut offset = sector_size;
    let mut header = vec![0u8; sector_size as usize];
    let mut data = Vec::new();
    for index in 0..entries {
        log.read_exact_at(&mut header, offset)
            .with_context(|| format!("failed to read log entry {index}"))?;
        offset += sector_size;
        let field = |at: usize| u64::from_le_bytes(header[at..at + 8].try_into().expect("8 bytes"));
        let (sector, sectors, flags, data_len) = (field(0), field(8), field(16), field(24));
        let start = sector * sector_size;
        let len = sectors * sector_size;
        let mut mark = None;
        if flags & MARK != 0 {
            let name = header
                .get(32..32 + data_len as usize)
                .context("a mark longer than its sector")?;
            mark = Some(String::from_utf8_lossy(name).into_owned());
        } else if flags & DISCARD != 0 {
            punch_hole(replay, start, len)?;
        } else if len > 0 {
            data.resize(len as usize, 0);
            log.read_exact_at(&mut data, offset)
                .with_context(|| format!("failed to read the data of log entry {index}"))?;
            offset += len;
            for (at, block) in (0..)
                .step_by(TORN_STEP as usize)
                .zip(data.chunks(TORN_STEP as usize))
            {
                replay
                    .write_all_at(block, start + at)
                    .with_context(|| format!("failed to replay log entry {index}"))?;
                let applied = at + block.len() as u64;
                if applied < len {
                    each(&Entry {
                        index,
                        flags,
                        mark: None,
                        torn: Some(applied),
                    })?;
                }
            }
        }
        each(&Entry {
            index,
            flags,
            mark,
            torn: None,
        })?;
    }
    Ok(entries)
}

/// Discard `len` bytes at `start` of the replayed image: a hole, reading as
/// zeros, as a device that discards returns them.
fn punch_hole(file: &File, start: u64, len: u64) -> Result<()> {
    if len == 0 {
        return Ok(());
    }
    let (Ok(start), Ok(len)) = (i64::try_from(start), i64::try_from(len)) else {
        bail!("a discard past any image");
    };
    nix::fcntl::fallocate(
        file.as_raw_fd(),
        nix::fcntl::FallocateFlags::FALLOC_FL_PUNCH_HOLE
            | nix::fcntl::FallocateFlags::FALLOC_FL_KEEP_SIZE,
        start,
        len,
    )
    .context("failed to replay a discard")?;
    Ok(())
}

/// A file of `size` bytes, all a hole.
fn sparse_file(path: &Path, size: u64) -> Result<PathBuf> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to create {}", path.display()))?;
    file.set_len(size)?;
    Ok(path.to_owned())
}

/// The crate's library test binary, built as the user.
fn test_binary(root: &Path) -> Result<PathBuf> {
    let output = Command::new("cargo")
        .args([
            "test",
            "-p",
            "durable-cache",
            "--lib",
            "--no-run",
            "--message-format=json-render-diagnostics",
        ])
        .current_dir(root)
        .output()
        .context("failed to build the crate's tests")?;
    if !output.status.success() {
        bail!("building the crate's tests failed");
    }
    let mut executables: BTreeMap<String, PathBuf> = BTreeMap::new();
    for line in String::from_utf8(output.stdout)?.lines() {
        let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if message["reason"] == "compiler-artifact" && message["profile"]["test"] == true {
            if let Some(executable) = message["executable"].as_str() {
                let name = message["target"]["name"].as_str().unwrap_or_default();
                executables.insert(name.to_owned(), PathBuf::from(executable));
            }
        }
    }
    executables
        .remove("durable_cache")
        .context("cargo built no test binary of the crate's library")
}

/// `sudo -n` `args`, failing if it does.
fn sudo(args: &[&str]) -> Result<()> {
    let status = Command::new("sudo")
        .arg("-n")
        .args(args)
        .status()
        .with_context(|| format!("failed to run sudo {}", args.join(" ")))?;
    if !status.success() {
        bail!("sudo {} failed", args.join(" "));
    }
    Ok(())
}

/// `sudo -n` `args`, returning what it prints.
fn sudo_output(args: &[&str]) -> Result<String> {
    let output = Command::new("sudo")
        .arg("-n")
        .args(args)
        .output()
        .with_context(|| format!("failed to run sudo {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "sudo {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// A loop device over a file, detached when dropped.
struct Loop(String);

impl Loop {
    fn attach(file: &Path) -> Result<Self> {
        let file = file.to_str().context("a path that is not UTF-8")?;
        Ok(Self(sudo_output(&["losetup", "--find", "--show", file])?))
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        if let Err(e) = sudo(&["losetup", "--detach", &self.0]) {
            eprintln!("{e}");
        }
    }
}

/// A `log-writes` device over `data`, logging to `log`, removed when
/// dropped.
struct Mapped(String);

impl Mapped {
    fn create(name: &str, sectors: u64, data: &str, log: &str) -> Result<Self> {
        let table = format!("0 {sectors} log-writes {data} {log}");
        sudo(&["dmsetup", "create", name, "--table", &table])?;
        Ok(Self(name.to_owned()))
    }

    fn path(&self) -> String {
        format!("/dev/mapper/{}", self.0)
    }

    fn mark(&self, name: &str) -> Result<()> {
        sudo(&["dmsetup", "message", &self.0, "0", "mark", name])
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        if let Err(e) = sudo(&["dmsetup", "remove", &self.0]) {
            eprintln!("{e}");
        }
    }
}

/// A mount of `device` at `at`, owned by the user, unmounted when dropped.
struct Mount(PathBuf);

impl Mount {
    fn new(device: &str, at: &Path, options: &str) -> Result<Self> {
        let at_str = at.to_str().context("a path that is not UTF-8")?;
        sudo(&["mount", "-o", options, device, at_str])?;
        let mount = Self(at.to_owned());
        let owner = format!(
            "{}:{}",
            nix::unistd::getuid().as_raw(),
            nix::unistd::getgid().as_raw()
        );
        sudo(&["chown", &owner, at_str])?;
        Ok(mount)
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        if let Some(at) = self.0.to_str() {
            if let Err(e) = sudo(&["umount", at]) {
                eprintln!("{e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A log of three entries --- a write of two blocks at byte 8192, a
    /// mark, a flush --- written as `dm-log-writes` writes one.
    fn log_of(sector_size: u64) -> Vec<u8> {
        let ss = sector_size as usize;
        let mut log = vec![0u8; ss];
        log[0..8].copy_from_slice(&LOG_MAGIC.to_le_bytes());
        log[8..16].copy_from_slice(&LOG_VERSION.to_le_bytes());
        log[16..24].copy_from_slice(&3u64.to_le_bytes());
        log[24..28].copy_from_slice(&(sector_size as u32).to_le_bytes());
        let entry = |sector: u64, sectors: u64, flags: u64, data_len: u64, tail: &[u8]| {
            let mut header = vec![0u8; ss];
            for (at, value) in [(0, sector), (8, sectors), (16, flags), (24, data_len)] {
                header[at..at + 8].copy_from_slice(&value.to_le_bytes());
            }
            header[32..32 + tail.len()].copy_from_slice(tail);
            header
        };
        let sectors = 2 * TORN_STEP / sector_size;
        log.extend(entry(sectors, sectors, 0, 0, b""));
        log.extend(vec![0xAB; 2 * TORN_STEP as usize]);
        log.extend(entry(0, 0, MARK, 4, b"mkfs"));
        log.extend(entry(0, 0, FLUSH, 0, b""));
        log
    }

    /// The replay writes each entry's data where it belongs, a block at a
    /// time, handing on the torn state after the first; reads each mark's
    /// name; and hands on each entry's flags, in order.
    #[test]
    fn tc_persistence_crashsafe_03_the_replay_reads_the_log_format() {
        for sector_size in [512u64, 4096] {
            let dir = tempfile::tempdir().expect("a directory");
            let log_path = dir.path().join("log");
            std::fs::write(&log_path, log_of(sector_size)).expect("the log");
            let replay_path =
                sparse_file(&dir.path().join("replay"), 4 * TORN_STEP).expect("the image");
            let replay = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&replay_path)
                .expect("the image opens");
            let mut seen = Vec::new();
            let entries = replay_log(
                &mut File::open(&log_path).expect("the log opens"),
                &replay,
                |entry| {
                    let mut written = vec![0u8; 2 * TORN_STEP as usize];
                    replay
                        .read_exact_at(&mut written, 2 * TORN_STEP)
                        .expect("the image reads");
                    let blocks =
                        written.iter().filter(|byte| **byte == 0xAB).count() as u64 / TORN_STEP;
                    seen.push((
                        entry.index,
                        entry.flags,
                        entry.mark.clone(),
                        entry.torn,
                        blocks,
                    ));
                    Ok(())
                },
            )
            .expect("the log replays");
            assert_eq!(entries, 3);
            assert_eq!(
                seen,
                [
                    (0, 0, None, Some(TORN_STEP), 1),
                    (0, 0, None, None, 2),
                    (1, MARK, Some("mkfs".to_owned()), None, 2),
                    (2, FLUSH, None, None, 2)
                ]
            );
            let mut image = Vec::new();
            File::open(&replay_path)
                .expect("the image opens")
                .read_to_end(&mut image)
                .expect("the image reads");
            let block = TORN_STEP as usize;
            assert!(image[..2 * block].iter().all(|byte| *byte == 0));
            assert!(image[2 * block..].iter().all(|byte| *byte == 0xAB));
        }
    }
}
