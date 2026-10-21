// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Shared fixtures for the integration tests in this directory: cache
//! builders (including a "restart" over the same directory), store and
//! lookup helpers driving the real `Storage` trait, a raw record reader
//! that independently re-derives a published file's layout rather than
//! trusting the code under test, tamper helpers, log capture, and counter
//! reading.
//!
//! Every file in this directory is a module of the crate's own test binary
//! (see `integration_tests` in `src/lib.rs`), and they all share this one
//! module. It is a grab-bag: not every helper has a caller in every build,
//! for instance without the `metrics` feature, so unused ones must not warn.
#![allow(dead_code)]

use bytes::Bytes;
use durable_cache::format;
use durable_cache::format::RecordHeader;
use durable_cache::hooks::HookConfig;
use durable_cache::key_codec;
use durable_cache::writerid::WriterId;
use durable_cache::{FsCache, FsCacheConfig};
use pingora_cache::eviction::CacheEntryKey;
use pingora_cache::key::{CacheKey, CompactCacheKey};
use pingora_cache::storage::{HitHandler, PurgeOutcome, PurgeTarget, PurgeType, Storage};
use pingora_cache::trace::{Span, SpanHandle};
use pingora_cache::CacheMeta;
use pingora_http::ResponseHeader;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

// --- process isolation ------------------------------------------------------

/// Set in a child process to the name of the one test it runs in full.
const OWN_PROCESS: &str = "DURABLE_CACHE_TEST_OWN_PROCESS";

/// Give the calling test a process of its own. The whole suite is one test
/// binary, so the `prometheus` default registry, the logger and the
/// process's memory figures are shared by every test running at the same
/// time. A test that asserts an exact change in any of them would also
/// count other tests' events.
///
/// Returns `true` in the child process, where the test's body runs. In the
/// parent it re-runs this test binary on this one test, fails if the child
/// failed or ran no test at all (a misspelt `test`), and returns `false` so
/// the parent skips the body:
///
/// ```ignore
/// if !support::in_own_process(module_path!(), "tc_name") {
///     return;
/// }
/// ```
pub fn in_own_process(module: &str, test: &str) -> bool {
    // libtest names a test by its module path without the crate's name.
    let name = match module.split_once("::") {
        Some((_crate, path)) => format!("{path}::{test}"),
        None => test.to_owned(),
    };
    if std::env::var(OWN_PROCESS).as_deref() == Ok(name.as_str()) {
        return true;
    }
    let out = std::process::Command::new(std::env::current_exe().unwrap())
        // `--include-ignored` so an `#[ignore]`d test runs in the child too.
        .args([name.as_str(), "--exact", "--include-ignored", "--nocapture"])
        .env(OWN_PROCESS, &name)
        .output()
        .expect("failed to re-run the test binary");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success() && stdout.contains("test result: ok. 1 passed"),
        "{name} failed in its own process\n--- stdout\n{stdout}\n--- stderr\n{stderr}"
    );
    false
}

// --- keys and metadata ----------------------------------------------------

pub fn key(primary: &str) -> CacheKey {
    // Only the primary is hashed; the user tag is left empty.
    CacheKey::new(primary, "")
}

/// A key for `primary` carrying a user tag, which is part of the object's
/// identity (SDS-LAYOUT.SHARDING-01).
pub fn tagged_key(primary: &str, user_tag: &str) -> CacheKey {
    CacheKey::new(primary, user_tag)
}

/// The same key with a `Vary` variance folded in, as `pingora-cache` hands
/// a variant to the backend.
pub fn variant(base: &CacheKey, variance: [u8; 16]) -> CacheKey {
    let mut k = base.clone();
    k.set_variance_key(variance);
    k
}

pub fn compact(key: &CacheKey) -> CompactCacheKey {
    key.to_compact()
}

/// The eviction-manager identity of a key: a key-only entry, since this
/// backend assigns no entry identifier (ADR-COMPAT.ENTRYID-01), which is
/// exactly the shape both `HttpCache` and Eviction Recovery admit.
pub fn entry(key: &CacheKey) -> CacheEntryKey {
    CacheEntryKey::key_only(compact(key))
}

/// Purge a key the way `HttpCache` issues an invalidation: an `Active`
/// target naming the logical key (FDS-STORAGE.PURGE-01).
pub async fn purge_active(cache: &'static FsCache, key: &CacheKey) -> PurgeOutcome {
    let compact = compact(key);
    cache
        .purge(
            PurgeTarget::Active(&compact),
            PurgeType::Invalidation,
            &inactive_trace(),
        )
        .await
        .unwrap()
}

/// Purge a compact key directly, for the callers that already hold one.
pub async fn purge_compact(cache: &'static FsCache, key: &CompactCacheKey) -> PurgeOutcome {
    cache
        .purge(
            PurgeTarget::Active(key),
            PurgeType::Invalidation,
            &inactive_trace(),
        )
        .await
        .unwrap()
}

/// Expire a key the way `HttpCache` issues an expiring purge
/// (FDS-STORAGE.EXPIRE-01).
pub async fn expire_active(cache: &'static FsCache, key: &CacheKey) -> PurgeOutcome {
    let compact = compact(key);
    cache
        .expire(PurgeTarget::Active(&compact), &inactive_trace())
        .await
        .unwrap()
}

pub fn meta() -> CacheMeta {
    meta_created(SystemTime::now())
}

pub fn meta_created(created: SystemTime) -> CacheMeta {
    CacheMeta::new(
        created + Duration::from_secs(60),
        created,
        0,
        0,
        ResponseHeader::build(200, None).unwrap(),
    )
}

/// Metadata whose header set carries roughly `filler` bytes of incompressible
/// value, for exercising the metadata region's capacity and what happens
/// when an update outgrows it.
///
/// `pingora-cache` compresses the header segment, so a repetitive filler
/// would shrink to nothing and never grow the region: the value here is a
/// deterministic pseudo-random base32-ish string instead.
pub fn meta_with_header_filler(filler: usize) -> CacheMeta {
    let mut header = ResponseHeader::build(200, None).unwrap();
    header
        .append_header("x-filler", incompressible(filler, 1))
        .unwrap();
    CacheMeta::new(
        SystemTime::now() + Duration::from_secs(60),
        SystemTime::now(),
        0,
        0,
        header,
    )
}

/// The metadata `pingora-cache` hands `update_meta` after a `304` validates
/// the stored response `stored`: `new`'s freshness and header fields, under
/// `stored`'s creation time, which `HttpCache::revalidate_cache_meta` keeps
/// and which names the response the `304` validated.
pub fn revalidation(stored: &CacheMeta, new: &CacheMeta) -> CacheMeta {
    let mut meta = CacheMeta::new(
        new.fresh_until(),
        stored.created(),
        new.stale_while_revalidate_sec(),
        new.stale_if_error_sec(),
        new.response_header_copy(),
    );
    // Also stamps `updated` with the time the `304` arrived, as a
    // revalidation does.
    meta.update_freshness(
        new.fresh_until(),
        new.stale_while_revalidate_sec(),
        new.stale_if_error_sec(),
    );
    meta
}

/// A deterministic string of `len` characters that a compressor cannot
/// shrink appreciably. `seed` varies it between calls.
pub fn incompressible(len: usize, seed: u64) -> String {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            // xorshift64*, enough spread for test payloads.
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let v = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
            char::from(b'a' + (v >> 59) as u8 % 26)
        })
        .collect()
}

/// An untraced span handle — every `Storage` method takes one for
/// distributed tracing, which these tests have no use for.
pub fn inactive_trace() -> SpanHandle {
    Span::inactive().handle()
}

// --- caches ---------------------------------------------------------------

/// Leaked (intentionally never freed, matching how a real process holds its
/// one `FsCache` for its whole lifetime) `'static` cache rooted at a fresh
/// temp dir. Returns the `TempDir` too so it isn't dropped/cleaned up out
/// from under the test while `cache` is still in use.
pub fn leaked_cache() -> (&'static FsCache, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let cache = leaked_cache_at(tmp.path());
    (cache, tmp)
}

/// A cache over an existing directory: what a "restart" means in these
/// tests, since nothing but the directory survives a process.
pub fn leaked_cache_at(root: &Path) -> &'static FsCache {
    leaked_cache_configured(FsCacheConfig::new(root, None), HookConfig::default())
}

pub fn leaked_cache_with(config: FsCacheConfig) -> &'static FsCache {
    leaked_cache_configured(config, HookConfig::default())
}

pub fn leaked_cache_configured(config: FsCacheConfig, hooks: HookConfig) -> &'static FsCache {
    let cache = FsCache::with_test_hooks(config, hooks).unwrap();
    Box::leak(Box::new(cache))
}

/// A fresh cache that reserves `meta_headroom` bytes of metadata headroom.
pub fn leaked_cache_with_headroom(meta_headroom: u32) -> (&'static FsCache, tempfile::TempDir) {
    let tmp = tempfile::tempdir().unwrap();
    let config = FsCacheConfig::new(tmp.path(), Some(meta_headroom));
    (leaked_cache_with(config), tmp)
}

// --- driving the Storage trait --------------------------------------------

/// Store `body` under `key` and return the record size `finish` reported.
pub async fn store(
    cache: &'static FsCache,
    key: &CacheKey,
    meta: &CacheMeta,
    body: &[u8],
) -> usize {
    store_chunks(cache, key, meta, &[body]).await
}

/// Store a body delivered as several `write_body` chunks.
pub async fn store_chunks(
    cache: &'static FsCache,
    key: &CacheKey,
    meta: &CacheMeta,
    chunks: &[&[u8]],
) -> usize {
    let trace = inactive_trace();
    let mut handler = cache.get_miss_handler(key, meta, &trace).await.unwrap();
    for (i, chunk) in chunks.iter().enumerate() {
        let eof = i + 1 == chunks.len();
        handler
            .write_body(Bytes::copy_from_slice(chunk), eof)
            .await
            .unwrap();
    }
    match handler.finish().await.unwrap() {
        pingora_cache::storage::MissFinishType::Created(size) => size,
        // This backend always writes a fresh per-writer temporary, so a
        // completed store is never an append.
        pingora_cache::storage::MissFinishType::Appended(..) => {
            panic!("expected a Created finish, got an Appended one")
        }
    }
}

/// Look a key up and read its whole body, or `None` on a miss.
pub async fn lookup_body(cache: &'static FsCache, key: &CacheKey) -> Option<(CacheMeta, Vec<u8>)> {
    let trace = inactive_trace();
    let (meta, mut hit) = cache.lookup(key, &trace).await.unwrap()?;
    Some((meta, drain_body(&mut hit).await.unwrap()))
}

/// Look a key up, expecting a hit, and return the handler unread.
pub async fn lookup_hit(cache: &'static FsCache, key: &CacheKey) -> (CacheMeta, HitHandler) {
    let trace = inactive_trace();
    cache
        .lookup(key, &trace)
        .await
        .unwrap()
        .expect("expected a hit")
}

/// Read a hit handler to the end of its body.
pub async fn drain_body(hit: &mut HitHandler) -> Result<Vec<u8>, Box<pingora_error::Error>> {
    let mut body = Vec::new();
    while let Some(chunk) = hit.read_body().await? {
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Drop a temporary written by `writer` into the shard of `hash`, without
/// going through a store: an interrupted store's leftover
/// (TC-STORAGE.RECLAIM-01).
pub fn forge_temp(cache: &FsCache, hash: &str, writer: &WriterId, counter: u64) -> PathBuf {
    let path = durable_cache::sharding::temp_path(cache.root(), hash, writer, counter).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"an interrupted store's bytes").unwrap();
    path
}

/// A writer that stopped long ago: no lease of its exists under any root.
pub fn writer_without_lease() -> WriterId {
    WriterId {
        pid: 4711,
        nonce: 0x0bad_0000_0000_0001,
    }
}

/// Where writer `id`'s lease is: spelled out from the layout
/// ADR-STORAGE.WRITERID-01 gives, `writers/<pid>-<nonce>.lock`, rather than
/// asked of the backend, so a test checks that layout.
pub fn lease_path(root: &Path, id: &WriterId) -> PathBuf {
    root.join("writers").join(format!("{id}.lock"))
}

/// The device and inode numbers of the file at `path`, which tell it apart
/// from every other file while it exists.
#[cfg(target_os = "linux")]
pub fn file_id(path: &Path) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::metadata(path).expect("the file exists");
    (metadata.dev(), metadata.ino())
}

/// The device and inode numbers of every file this process has open, read
/// from `/proc/self/fd`, which still lists a file that has lost its last
/// name: a file listed there has not been freed by the kernel.
#[cfg(target_os = "linux")]
pub fn open_files() -> std::collections::HashSet<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    std::fs::read_dir("/proc/self/fd")
        .expect("/proc/self/fd can be listed")
        .filter_map(|entry| std::fs::metadata(entry.ok()?.path()).ok())
        .map(|metadata| (metadata.dev(), metadata.ino()))
        .collect()
}

/// Where the cache keeps a key's object: its own derivation, so a tagged key
/// and the test hash hook resolve as the backend resolves them.
pub fn canonical_path(cache: &FsCache, key: &CacheKey) -> PathBuf {
    cache.canonical_path(&cache.object_name(key)).unwrap()
}

pub fn shard_dir_of(cache: &FsCache, key: &CacheKey) -> PathBuf {
    canonical_path(cache, key).parent().unwrap().to_path_buf()
}

/// Every directory entry in a key's shard directory.
pub fn shard_entries(cache: &FsCache, key: &CacheKey) -> Vec<String> {
    let dir = shard_dir_of(cache, key);
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// The per-writer temporaries in a key's shard directory.
pub fn temp_entries(cache: &FsCache, key: &CacheKey) -> Vec<String> {
    shard_entries(cache, key)
        .into_iter()
        .filter(|n| n.ends_with(".tmp"))
        .collect()
}

// --- reading and tampering with records -----------------------------------

/// Read a canonical record file back from disk, independently re-deriving
/// segment offsets from the header rather than reusing any writer-side
/// state, and check it exactly as a real reader would
/// (SDS-LAYOUT.OBJECTFILE-01). Returns the header and the body bytes.
pub fn read_and_validate_record(path: &Path) -> (RecordHeader, Vec<u8>) {
    let all = std::fs::read(path).unwrap();
    let header = header_of(&all);
    assert_eq!(header.magic(), format::MAGIC, "magic");
    assert_eq!(header.version(), format::FORMAT_VERSION, "version");
    assert_eq!(
        header.compute_header_crc(),
        header.header_crc(),
        "header CRC"
    );

    let key_start = format::HEADER_LEN;
    let key_end = key_start + header.key_len() as usize;
    let meta_start = header.meta_region_offset();
    let internal_end = meta_start + header.internal_len() as usize;
    let meta_end = internal_end + header.header_len() as usize;
    assert_eq!(
        format::compute_meta_crc(
            &all[key_start..key_end],
            &all[meta_start..internal_end],
            &all[internal_end..meta_end],
        ),
        header.meta_crc(),
        "metadata CRC"
    );

    let body_offset = header.body_offset();
    assert_eq!(
        all.len() as u64,
        header.record_len(),
        "file length must be exactly the computed extent"
    );
    let body = all[body_offset..body_offset + header.body_len() as usize].to_vec();
    #[cfg(feature = "body-crc")]
    assert_eq!(crc32c::crc32c(&body), header.body_crc(), "body CRC");
    #[cfg(not(feature = "body-crc"))]
    assert_eq!(
        header.body_crc(),
        0,
        "no body CRC without the body-crc feature"
    );
    (header, body)
}

/// The record header of a file, without validating it.
pub fn read_header(path: &Path) -> RecordHeader {
    header_of(&std::fs::read(path).unwrap())
}

/// The header at the start of a record's bytes, without validating it.
fn header_of(record: &[u8]) -> RecordHeader {
    use zerocopy::FromBytes;
    RecordHeader::read_from_prefix(record)
        .expect("a record is at least a header long")
        .0
}

/// Flip one byte at `offset`, corrupting a record in place.
pub fn flip_byte_at(path: &Path, offset: u64) {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    let mut byte = [0u8; 1];
    file.seek(SeekFrom::Start(offset)).unwrap();
    std::io::Read::read_exact(&mut file, &mut byte).unwrap();
    byte[0] ^= 0xFF;
    file.seek(SeekFrom::Start(offset)).unwrap();
    file.write_all(&byte).unwrap();
}

/// Flip one byte of a published record's `magic` field — the cheapest way
/// to make a record fail validation.
pub fn flip_magic_byte(path: &Path) {
    flip_byte_at(path, format::MAGIC_OFFSET as u64);
}

/// Overwrite the variance flag in a record's key segment, then reseal its
/// metadata and header CRCs. Both are re-derived here, independently of the
/// code under test, so the record passes every record check and only
/// decoding its key can reject it (TC-LAYOUT.KEYSEGMENT-01).
pub fn set_key_variance_flag(path: &Path, flag: u8) {
    let mut all = std::fs::read(path).unwrap();
    let header = read_header(path);
    let key_start = format::KEY_OFFSET;
    let key_end = key_start + header.key_len() as usize;
    // The flag follows the primary hash (SDS-LAYOUT.KEYSEGMENT-01).
    all[key_start + key_codec::HASH_LEN] = flag;

    let internal_end = key_end + header.internal_len() as usize;
    let meta_end = internal_end + header.header_len() as usize;
    let crc = crc32c::crc32c(&all[key_start..key_end]);
    let crc = crc32c::crc32c_append(crc, &all[key_end..internal_end]);
    let crc = crc32c::crc32c_append(crc, &all[internal_end..meta_end]);
    all[format::META_CRC_OFFSET..format::META_CRC_OFFSET + 4].copy_from_slice(&crc.to_le_bytes());
    let crc = crc32c::crc32c(&all[..format::HEADER_CRC_OFFSET]);
    all[format::HEADER_CRC_OFFSET..format::HEADER_CRC_OFFSET + 4]
        .copy_from_slice(&crc.to_le_bytes());
    std::fs::write(path, &all).unwrap();
}

/// Rewrite one of a record's `u32` header fields, the one at `offset`, and
/// reseal its header CRC. The CRC is re-derived here, independently of the
/// code under test, so the header passes its checksum and only the field is
/// wrong.
pub fn set_header_u32(path: &Path, offset: usize, value: u32) {
    use std::os::unix::fs::FileExt;
    let mut header = [0u8; format::HEADER_LEN];
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect("the record can be opened for writing");
    file.read_exact_at(&mut header, 0)
        .expect("the record has a header to read");
    header[offset..offset + size_of::<u32>()].copy_from_slice(&value.to_le_bytes());
    let crc = crc32c::crc32c(&header[..format::HEADER_CRC_OFFSET]);
    header[format::HEADER_CRC_OFFSET..format::HEADER_CRC_OFFSET + 4]
        .copy_from_slice(&crc.to_le_bytes());
    file.write_all_at(&header, 0)
        .expect("the header can be written back");
}

/// Rewrite a record's format `version` field and reseal its header CRC, as a
/// record written by another revision of this backend would look. The CRC is
/// re-derived here, independently of the code under test, so only the
/// version makes the record foreign.
pub fn set_format_version(path: &Path, version: u32) {
    use std::os::unix::fs::FileExt;
    let mut header = [0u8; format::HEADER_LEN];
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    file.read_exact_at(&mut header, 0).unwrap();
    header[format::VERSION_OFFSET..format::VERSION_OFFSET + 4]
        .copy_from_slice(&version.to_le_bytes());
    let crc = crc32c::crc32c(&header[..format::HEADER_CRC_OFFSET]);
    header[format::HEADER_CRC_OFFSET..format::HEADER_CRC_OFFSET + 4]
        .copy_from_slice(&crc.to_le_bytes());
    file.write_all_at(&header, 0).unwrap();
}

/// Flip a byte inside a record's body without changing its length.
pub fn flip_body_byte(path: &Path) {
    let header = read_header(path);
    assert!(header.body_len() > 0, "record has no body to corrupt");
    flip_byte_at(path, header.body_offset() as u64);
}

pub fn truncate_by(path: &Path, bytes: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    let len = file.metadata().unwrap().len();
    file.set_len(len - bytes).unwrap();
}

pub fn extend_by(path: &Path, bytes: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    let len = file.metadata().unwrap().len();
    file.set_len(len + bytes).unwrap();
}

/// The bytes a directory tree occupies on disk, in allocated blocks.
pub fn tree_disk_usage(root: &Path) -> u64 {
    tree_usage(root, true)
}

/// The bytes the files under a directory tree occupy on disk, in allocated
/// blocks, leaving out the directories' own blocks. A shard directory a
/// store creates belongs to the layout, not to that store, and stays; and
/// how many blocks a directory holds depends on the file system (none on
/// tmpfs, 4 KiB and never shrinking on ext4), so only file blocks show
/// whether a store released what it consumed.
pub fn tree_file_usage(root: &Path) -> u64 {
    tree_usage(root, false)
}

fn tree_usage(root: &Path, count_dirs: bool) -> u64 {
    fn walk(dir: &Path, count_dirs: bool, total: &mut u64) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if count_dirs || !meta.is_dir() {
                // 512-byte blocks, as `du` counts them.
                *total += std::os::unix::fs::MetadataExt::blocks(&meta) * 512;
            }
            if meta.is_dir() {
                walk(&entry.path(), count_dirs, total);
            }
        }
    }
    let mut total = 0;
    walk(root, count_dirs, &mut total);
    total
}

// --- observability --------------------------------------------------------

/// Captures log records so a test can assert on the warnings the designs
/// require (FDS-OBSERVABILITY.HEADROOM-01, FDS-STORAGE.RECLAIM-01).
#[derive(Clone, Default)]
pub struct LogCapture {
    lines: Arc<Mutex<Vec<String>>>,
}

impl LogCapture {
    /// Install as the process logger. Only the first call in a test binary
    /// wins, so every capture in one binary shares the same buffer; tests
    /// match on their own key or metric name.
    pub fn install() -> LogCapture {
        static INSTALLED: std::sync::OnceLock<LogCapture> = std::sync::OnceLock::new();
        INSTALLED
            .get_or_init(|| {
                let capture = LogCapture::default();
                let boxed: Box<dyn log::Log> = Box::new(capture.clone());
                // Leaked deliberately: `log` requires a `&'static` logger.
                let _ = log::set_logger(Box::leak(boxed));
                log::set_max_level(log::LevelFilter::Debug);
                capture
            })
            .clone()
    }

    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Every captured line containing `needle`.
    pub fn matching(&self, needle: &str) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|l| l.contains(needle))
            .collect()
    }

    pub fn clear(&self) {
        self.lines.lock().unwrap_or_else(|p| p.into_inner()).clear();
    }
}

impl log::Log for LogCapture {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        self.lines
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(format!("{} {}", record.level(), record.args()));
    }

    fn flush(&self) {}
}

/// The value of a counter in the `prometheus` default registry, or `None`
/// if no such metric is registered (which is what a build without the
/// `metrics` feature must show).
pub fn counter_value(name: &str, labels: &[(&str, &str)]) -> Option<f64> {
    // `prometheus` 0.14 regenerated its protobuf model; the `get_*`
    // accessors are deprecated shims over these.
    pingora_prometheus::prometheus::gather()
        .into_iter()
        .find(|f| f.name() == name)?
        .metric
        .iter()
        .find(|m| {
            labels
                .iter()
                .all(|(k, v)| m.label.iter().any(|l| l.name() == *k && l.value() == *v))
        })
        // `prometheus` 0.14 regenerated its protobuf model: the accessor is
        // `value()`, not `get_value()`.
        .map(|m| m.counter.value())
}
