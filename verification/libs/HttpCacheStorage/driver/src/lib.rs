// SPDX-FileCopyrightText: 2026 The HttpCacheStorage authors
// SPDX-License-Identifier: Apache-2.0

//! Drives a storage backend of the `pingora-cache` crate, version 0.9,
//! through its `Storage`, `HandleHit` and `HandleMiss` traits with a
//! workload drawn from a seed, and writes the trace of its calls, one JSON
//! line each, in the format `check-trace` reads (`HttpCacheStorage/Trace.lean`).
//!
//! The calls are made one at a time. A write stays in flight across many
//! calls, so lookups, reads, purges and expiries meet it at every stage; a
//! hit is read a chunk at a time, among the other calls. The driver never
//! makes a call that would wait for another to happen: it reads a hit of a
//! write in flight only when the write has finished, or holds bytes the hit
//! has not read that came before its last chunk, since a storage may hold
//! the last chunk back until `finish`.
//!
//! [`Allow`] leaves out the calls a storage under test cannot take.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::time::{Duration, SystemTime};

use bytes::Bytes;
use pingora_cache::eviction::{CacheEntryId, CacheEntryKey};
use pingora_cache::key::{CacheHashKey, CacheKey};
use pingora_cache::storage::{
    HitHandler, MissFinishType, MissHandler, PurgeOutcome, PurgeTarget, PurgeType, Storage,
};
use pingora_cache::trace::{Span, SpanHandle};
use pingora_cache::CacheMeta;
use pingora_http::ResponseHeader;

/// The header each metadata carries its label in.
pub const LABEL: &str = "x-trace-label";

/// How long a storage call may take before the driver gives up on it: the
/// driver never makes a call that waits for another, so one that takes
/// longer has hung.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);

/// The calls a storage under test can take. Each is allowed by the contract;
/// a storage that cannot take one, for a reason of its own, is driven
/// without it.
#[derive(Debug, Clone, Copy)]
pub struct Allow {
    /// Purge a key while a write for it is in flight.
    pub purge_during_write: bool,
    /// Read a hit of a write after the write was dropped before its end.
    pub read_after_abandon: bool,
    /// Call `update_meta` for a key whose lookup did not hit a stored entry.
    pub update_unknown: bool,
    /// Seek with no end on a hit after a seek with one.
    pub open_seek_after_bounded: bool,
}

impl Allow {
    /// Every call.
    pub const ALL: Allow = Allow {
        purge_during_write: true,
        read_after_abandon: true,
        update_unknown: true,
        open_seek_after_bounded: true,
    };

    /// The calls `MemCache`, the crate's in-memory storage, can take
    /// (`src/memory.rs` of `pingora-cache` 0.9.0): its miss handler's
    /// `finish` panics once a purge has removed the write it belongs to;
    /// its `update_meta` panics when nothing is stored for the key; a seek
    /// with no end keeps the end of an earlier seek; and a hit of a write
    /// that was dropped ends its body early, as if whole, where the contract
    /// lets it fail but not end.
    pub const MEMCACHE: Allow = Allow {
        purge_during_write: false,
        read_after_abandon: false,
        update_unknown: false,
        open_seek_after_bounded: false,
    };
}

/// A workload.
#[derive(Debug, Clone)]
pub struct Options {
    /// The seed every choice is drawn from.
    pub seed: u64,
    /// How many calls to try.
    pub calls: usize,
    /// How many keys; every other one with a variance.
    pub keys: usize,
    /// The sizes a write's body is drawn from.
    pub sizes: Vec<usize>,
    pub allow: Allow,
}

impl Options {
    pub fn new(seed: u64, allow: Allow) -> Self {
        Options {
            seed,
            calls: 2_000,
            keys: 3,
            sizes: vec![0, 1, 100, 4_096, 5_000, 70_000],
            allow,
        }
    }
}

/// What a run made.
#[derive(Debug, Default, Clone, Copy)]
pub struct Summary {
    /// The calls traced.
    pub calls: usize,
    /// Of them, lookups that hit.
    pub hits: usize,
    /// Writes finished.
    pub stored: usize,
}

/// A small generator of choices (SplitMix64).
struct Choices(u64);

impl Choices {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn chance(&mut self, one_in: usize) -> bool {
        self.below(one_in) == 0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    InFlight,
    Finished,
    Abandoned,
}

struct Writer {
    key: usize,
    handler: Option<MissHandler>,
    body: Vec<u8>,
    written: usize,
    /// The bytes written before the last chunk: a hit of the write in
    /// flight can read them without waiting for `finish`.
    before_last: usize,
    eof: bool,
    stage: Stage,
    tag: Option<Vec<u8>>,
}

enum Source {
    Stored { len: usize },
    Write(u64),
}

struct Hit {
    key: usize,
    handler: Option<HitHandler>,
    source: Source,
    pos: usize,
    bounded: bool,
    done: bool,
}

/// The byte at `offset` of the body of the write labelled `n`.
fn body_byte(n: u64, offset: usize) -> u8 {
    ((n as usize)
        .wrapping_mul(31)
        .wrapping_add(offset.wrapping_mul(7))
        % 251) as u8
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        write!(s, "{b:02x}").expect("writing to a String");
    }
    s
}

fn opt(n: Option<u64>) -> String {
    n.map_or_else(|| "null".to_owned(), |n| n.to_string())
}

fn meta_json(label: &str, fresh: bool) -> String {
    format!(r#"{{"label":"{label}","fresh":{fresh}}}"#)
}

/// Metadata fresh for an hour, labelled `label`, of a response created at
/// `created`.
fn labelled_meta(label: &str, created: SystemTime) -> CacheMeta {
    let mut header = ResponseHeader::build(200, None).expect("a response header");
    header
        .insert_header(LABEL, label)
        .expect("the label header");
    CacheMeta::new(
        SystemTime::now() + Duration::from_secs(3_600),
        created,
        0,
        0,
        header,
    )
}

fn label_of(meta: &CacheMeta) -> String {
    meta.headers()
        .get(LABEL)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned()
}

async fn timed<T>(what: &str, f: impl std::future::Future<Output = T>) -> T {
    match tokio::time::timeout(CALL_TIMEOUT, f).await {
        Ok(v) => v,
        Err(_) => panic!("{what} did not return within {CALL_TIMEOUT:?}"),
    }
}

struct Driver<'o, W: Write> {
    storage: &'static (dyn Storage + Sync),
    opts: &'o Options,
    out: W,
    trace: SpanHandle,
    choose: Choices,
    keys: Vec<CacheKey>,
    names: Vec<String>,
    writers: HashMap<u64, Writer>,
    labels: HashMap<String, u64>,
    hits: HashMap<u64, Hit>,
    /// The keys the driver knows to be stored, for `Allow::update_unknown`.
    stored: Vec<bool>,
    /// The last identity seen for each key.
    ids: Vec<Option<u64>>,
    next_hit: u64,
    next_writer: u64,
    summary: Summary,
}

impl<W: Write> Driver<'_, W> {
    fn emit(&mut self, line: String) -> io::Result<()> {
        self.summary.calls += 1;
        writeln!(self.out, "{line}")
    }

    fn in_flight(&self, key: usize) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .writers
            .iter()
            .filter(|(_, w)| w.key == key && w.stage == Stage::InFlight)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    fn open_hits(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.hits.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    fn open_writers(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self
            .writers
            .iter()
            .filter(|(_, w)| w.stage == Stage::InFlight)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    async fn lookup(&mut self) -> io::Result<()> {
        if self.hits.len() >= 6 {
            let oldest = self.open_hits()[0];
            return self.finish_hit(oldest).await;
        }
        let key = self.choose.below(self.keys.len());
        self.lookup_key(key).await.map(drop)
    }

    /// Look `key` up, and return the creation time of what it hit when that
    /// is the stored entry.
    async fn lookup_key(&mut self, key: usize) -> io::Result<Option<SystemTime>> {
        let mut stored_hit = None;
        let flying = self.in_flight(key);
        let tag = if self.storage.support_streaming_partial_write()
            && !flying.is_empty()
            && self.choose.chance(4)
        {
            let w = flying[self.choose.below(flying.len())];
            self.writers[&w].tag.clone().map(|t| (w, t))
        } else {
            None
        };
        let k = self.keys[key].clone();
        let result = match &tag {
            Some((_, t)) => {
                timed(
                    "lookup_streaming_write",
                    self.storage
                        .lookup_streaming_write(&k, Some(t), &self.trace),
                )
                .await
            }
            None => timed("lookup", self.storage.lookup(&k, &self.trace)).await,
        };
        let result_json = match result {
            Err(_) => r#""error""#.to_owned(),
            Ok(None) => r#""miss""#.to_owned(),
            Ok(Some((meta, handler))) => {
                self.summary.hits += 1;
                let h = self.next_hit;
                self.next_hit += 1;
                let label = label_of(&meta);
                let fresh = meta.is_fresh(SystemTime::now());
                let id = handler.entry_id().map(CacheEntryId::get);
                if id.is_some() {
                    self.ids[key] = id;
                }
                let source = match self.labels.get(&label) {
                    Some(w) if self.writers[w].stage == Stage::InFlight => Source::Write(*w),
                    Some(w) => Source::Stored {
                        len: self.writers[w].body.len(),
                    },
                    None => Source::Stored { len: 100 },
                };
                if matches!(source, Source::Stored { .. }) {
                    stored_hit = Some(meta.created());
                }
                self.hits.insert(
                    h,
                    Hit {
                        key,
                        handler: Some(handler),
                        source,
                        pos: 0,
                        bounded: false,
                        done: false,
                    },
                );
                format!(
                    r#"{{"hit":{h},"meta":{},"id":{}}}"#,
                    meta_json(&label, fresh),
                    opt(id)
                )
            }
        };
        let line = format!(
            r#"{{"op":"lookup","key":"{}","tag":{},"result":{result_json}}}"#,
            self.names[key],
            opt(tag.map(|(w, _)| w))
        );
        self.emit(line)?;
        Ok(stored_hit)
    }

    /// Whether reading `h` now could wait for a write.
    fn read_would_wait(&self, h: &Hit) -> bool {
        match h.source {
            Source::Stored { .. } => false,
            Source::Write(w) => {
                let wr = &self.writers[&w];
                match wr.stage {
                    Stage::Finished => false,
                    Stage::Abandoned => !self.opts.allow.read_after_abandon,
                    Stage::InFlight => h.pos >= wr.before_last,
                }
            }
        }
    }

    async fn read(&mut self) -> io::Result<()> {
        let ready: Vec<u64> = self
            .open_hits()
            .into_iter()
            .filter(|id| {
                let h = &self.hits[id];
                !h.done && !self.read_would_wait(h)
            })
            .collect();
        if ready.is_empty() {
            return Ok(());
        }
        let id = ready[self.choose.below(ready.len())];
        let hit = self.hits.get_mut(&id).expect("an open hit");
        let handler = hit.handler.as_mut().expect("a hit's handler");
        let result = timed(&format!("read_body of hit {id}"), handler.read_body()).await;
        let result_json = match result {
            Err(_) => {
                hit.done = true;
                r#""error""#.to_owned()
            }
            Ok(None) => {
                hit.done = true;
                r#""done""#.to_owned()
            }
            Ok(Some(data)) => {
                hit.pos += data.len();
                format!(r#"{{"chunk":"{}"}}"#, hex(&data))
            }
        };
        self.emit(format!(
            r#"{{"op":"read","hit":{id},"result":{result_json}}}"#
        ))
    }

    async fn seek(&mut self) -> io::Result<()> {
        let open = self.open_hits();
        if open.is_empty() {
            return Ok(());
        }
        let id = open[self.choose.below(open.len())];
        let allow = self.opts.allow;
        let len = match self.hits[&id].source {
            Source::Stored { len } => len,
            Source::Write(w) => self.writers[&w].body.len(),
        };
        let start = self.choose.below(len + 2);
        let hit = self.hits.get_mut(&id).expect("an open hit");
        let handler = hit.handler.as_mut().expect("a hit's handler");
        if !handler.can_seek() {
            return Ok(());
        }
        let stop = if (hit.bounded && !allow.open_seek_after_bounded) || self.choose.chance(2) {
            Some(start + 1 + self.choose.below(len + 1))
        } else {
            None
        };
        let ok = handler.seek(start, stop).is_ok();
        if ok {
            hit.pos = start;
            hit.done = false;
            hit.bounded = hit.bounded || stop.is_some();
        }
        self.emit(format!(
            r#"{{"op":"seek","hit":{id},"start":{start},"stop":{},"ok":{ok}}}"#,
            opt(stop.map(|s| s as u64))
        ))
    }

    async fn finish_hit(&mut self, id: u64) -> io::Result<()> {
        let mut hit = self.hits.remove(&id).expect("an open hit");
        let handler = hit.handler.take().expect("a hit's handler");
        let key = self.keys[hit.key].clone();
        let _ = timed(
            "HandleHit::finish",
            handler.finish(self.storage, &key, &self.trace),
        )
        .await;
        self.emit(format!(r#"{{"op":"finish_hit","hit":{id}}}"#))
    }

    async fn some_finish_hit(&mut self) -> io::Result<()> {
        let open = self.open_hits();
        if open.is_empty() {
            return Ok(());
        }
        let id = open[self.choose.below(open.len())];
        self.finish_hit(id).await
    }

    async fn miss_handler(&mut self) -> io::Result<()> {
        if self.open_writers().len() >= 4 {
            return Ok(());
        }
        let key = self.choose.below(self.keys.len());
        let w = self.next_writer;
        self.next_writer += 1;
        let label = format!("w{w}");
        let meta = labelled_meta(&label, SystemTime::now());
        let k = self.keys[key].clone();
        let result = timed(
            "get_miss_handler",
            self.storage.get_miss_handler(&k, &meta, &self.trace),
        )
        .await;
        let result_json = match result {
            Err(_) => r#""error""#.to_owned(),
            Ok(handler) => {
                let id = handler.entry_id().map(CacheEntryId::get);
                if id.is_some() {
                    self.ids[key] = id;
                }
                let size = self.opts.sizes[self.choose.below(self.opts.sizes.len())];
                let body = (0..size).map(|o| body_byte(w, o)).collect();
                let tag = handler.streaming_write_tag().map(<[u8]>::to_vec);
                self.writers.insert(
                    w,
                    Writer {
                        key,
                        handler: Some(handler),
                        body,
                        written: 0,
                        before_last: 0,
                        eof: false,
                        stage: Stage::InFlight,
                        tag,
                    },
                );
                self.labels.insert(label.clone(), w);
                format!(r#"{{"writer":{w},"id":{}}}"#, opt(id))
            }
        };
        let line = format!(
            r#"{{"op":"miss_handler","key":"{}","meta":{},"result":{result_json}}}"#,
            self.names[key],
            meta_json(&label, true)
        );
        self.emit(line)
    }

    async fn write(&mut self) -> io::Result<()> {
        let ready: Vec<u64> = self
            .open_writers()
            .into_iter()
            .filter(|w| !self.writers[w].eof)
            .collect();
        if ready.is_empty() {
            return Ok(());
        }
        let w = ready[self.choose.below(ready.len())];
        let step = [1usize, 7, 512, 4_096, 16_384][self.choose.below(5)];
        let wr = self.writers.get_mut(&w).expect("a writer");
        let end = (wr.written + step).min(wr.body.len());
        let data = Bytes::copy_from_slice(&wr.body[wr.written..end]);
        let eof = end == wr.body.len();
        let handler = wr.handler.as_mut().expect("a writer's handler");
        let ok = timed("write_body", handler.write_body(data.clone(), eof))
            .await
            .is_ok();
        if ok {
            wr.written = end;
            if !eof {
                wr.before_last = end;
            }
            wr.eof = eof;
        } else {
            wr.handler = None;
            wr.stage = Stage::Abandoned;
        }
        self.emit(format!(
            r#"{{"op":"write","writer":{w},"data":"{}","eof":{eof},"ok":{ok}}}"#,
            hex(&data)
        ))?;
        if !ok {
            self.emit(format!(r#"{{"op":"drop","writer":{w}}}"#))?;
        }
        Ok(())
    }

    async fn finish(&mut self) -> io::Result<()> {
        let ready: Vec<u64> = self
            .open_writers()
            .into_iter()
            .filter(|w| self.writers[w].eof)
            .collect();
        if ready.is_empty() {
            return Ok(());
        }
        let w = ready[self.choose.below(ready.len())];
        let wr = self.writers.get_mut(&w).expect("a writer");
        let handler = wr.handler.take().expect("a writer's handler");
        let result = timed("HandleMiss::finish", handler.finish()).await;
        let result_json = match result {
            Err(_) => {
                wr.stage = Stage::Abandoned;
                r#""error""#.to_owned()
            }
            Ok(done) => {
                wr.stage = Stage::Finished;
                self.stored[wr.key] = true;
                self.summary.stored += 1;
                match done {
                    MissFinishType::Created(n) => format!(r#"{{"created":{n}}}"#),
                    MissFinishType::Appended(n, max) => {
                        format!(r#"{{"appended":[{n},{}]}}"#, opt(max.map(|m| m as u64)))
                    }
                }
            }
        };
        self.emit(format!(
            r#"{{"op":"finish","writer":{w},"result":{result_json}}}"#
        ))
    }

    async fn drop_writer(&mut self) -> io::Result<()> {
        let open = self.open_writers();
        if open.is_empty() {
            return Ok(());
        }
        let w = open[self.choose.below(open.len())];
        let wr = self.writers.get_mut(&w).expect("a writer");
        drop(wr.handler.take());
        wr.stage = Stage::Abandoned;
        self.emit(format!(r#"{{"op":"drop","writer":{w}}}"#))
    }

    /// A target for `key`: mostly the active entry, sometimes an exact one,
    /// by key alone, by the last identity seen, or by one never given.
    fn target(&mut self, key: usize) -> (CacheEntryKey, bool, String) {
        let compact = self.keys[key].to_compact();
        match self.choose.below(5) {
            0 => (
                CacheEntryKey::key_only(compact),
                false,
                r#"{"exact":null}"#.to_owned(),
            ),
            1 => {
                let id = self.ids[key].unwrap_or(0) + u64::from(self.choose.chance(2)) * 1_000;
                (
                    CacheEntryKey::identified(compact, CacheEntryId::new(id)),
                    false,
                    format!(r#"{{"exact":{id}}}"#),
                )
            }
            _ => (
                CacheEntryKey::key_only(compact),
                true,
                r#""active""#.to_owned(),
            ),
        }
    }

    fn outcome(result: &pingora_error::Result<PurgeOutcome>) -> String {
        match result {
            Err(_) => r#""error""#.to_owned(),
            Ok(PurgeOutcome::NotFound) => r#""not_found""#.to_owned(),
            Ok(PurgeOutcome::Expired) => r#""expired""#.to_owned(),
            Ok(PurgeOutcome::Purged(id)) => {
                format!(r#"{{"purged":{}}}"#, opt(id.map(CacheEntryId::get)))
            }
        }
    }

    async fn purge(&mut self, expire: bool) -> io::Result<()> {
        let key = self.choose.below(self.keys.len());
        if !expire && !self.opts.allow.purge_during_write && !self.in_flight(key).is_empty() {
            return Ok(());
        }
        let (entry, active, target_json) = self.target(key);
        let target = if active {
            PurgeTarget::Active(entry.key())
        } else {
            PurgeTarget::Exact(&entry)
        };
        let eviction = self.choose.chance(3);
        let result = if expire {
            timed("expire", self.storage.expire(target, &self.trace)).await
        } else {
            let kind = if eviction {
                PurgeType::Eviction
            } else {
                PurgeType::Invalidation
            };
            timed("purge", self.storage.purge(target, kind, &self.trace)).await
        };
        if matches!(result, Ok(PurgeOutcome::Purged(_) | PurgeOutcome::NotFound)) {
            self.stored[key] = false;
        }
        let outcome = Self::outcome(&result);
        let line = if expire {
            format!(
                r#"{{"op":"expire","key":"{}","target":{target_json},"outcome":{outcome}}}"#,
                self.names[key]
            )
        } else {
            let kind = if eviction { "eviction" } else { "invalidation" };
            format!(
                r#"{{"op":"purge","key":"{}","target":{target_json},"kind":"{kind}","outcome":{outcome}}}"#,
                self.names[key]
            )
        };
        self.emit(line)
    }

    /// A revalidation, as `pingora-cache` makes one: a lookup, then new
    /// metadata for the response it hit, which keeps that response's
    /// creation time.
    async fn update_meta(&mut self) -> io::Result<()> {
        if self.hits.len() >= 6 {
            let oldest = self.open_hits()[0];
            return self.finish_hit(oldest).await;
        }
        let key = self.choose.below(self.keys.len());
        if !self.opts.allow.update_unknown && !self.stored[key] {
            return Ok(());
        }
        let created = match self.lookup_key(key).await? {
            Some(created) => created,
            None if self.opts.allow.update_unknown => SystemTime::now(),
            None => return Ok(()),
        };
        let n = self.next_writer;
        self.next_writer += 1;
        let label = format!("u{n}");
        let meta = labelled_meta(&label, created);
        let k = self.keys[key].clone();
        let result = timed(
            "update_meta",
            self.storage.update_meta(&k, &meta, &self.trace),
        )
        .await;
        let result_json = match result {
            Err(_) => r#""error""#.to_owned(),
            Ok(b) => b.to_string(),
        };
        let line = format!(
            r#"{{"op":"update_meta","key":"{}","meta":{},"result":{result_json}}}"#,
            self.names[key],
            meta_json(&label, true)
        );
        self.emit(line)
    }
}

/// Drive `storage` through the workload `opts` describes, writing the trace
/// of its calls to `out`.
pub async fn run(
    storage: &'static (dyn Storage + Sync),
    opts: &Options,
    out: impl Write,
) -> io::Result<Summary> {
    let mut keys = Vec::new();
    for i in 0..opts.keys {
        let mut key = CacheKey::new(format!("trace-key-{i}"), "");
        if i % 2 == 1 {
            key.set_variance_key([i as u8; 16]);
        }
        keys.push(key);
    }
    let names = keys.iter().map(CacheHashKey::combined).collect();
    let mut d = Driver {
        storage,
        opts,
        out,
        trace: Span::inactive().handle(),
        choose: Choices(opts.seed),
        stored: vec![false; keys.len()],
        ids: vec![None; keys.len()],
        keys,
        names,
        writers: HashMap::new(),
        labels: HashMap::new(),
        hits: HashMap::new(),
        next_hit: 0,
        next_writer: 0,
        summary: Summary::default(),
    };
    for _ in 0..opts.calls {
        match d.choose.below(20) {
            0..=4 => d.lookup().await?,
            5..=8 => d.read().await?,
            9 => d.seek().await?,
            10 => d.some_finish_hit().await?,
            11..=12 => d.miss_handler().await?,
            13..=15 => d.write().await?,
            16 => d.finish().await?,
            17 => {
                if d.choose.chance(4) {
                    d.drop_writer().await?
                } else {
                    d.finish().await?
                }
            }
            18 => {
                let expire = d.choose.chance(3);
                d.purge(expire).await?
            }
            _ => d.update_meta().await?,
        }
    }
    // End the hits and writes still open, so nothing outlives the run.
    for id in d.open_hits() {
        d.finish_hit(id).await?;
    }
    for w in d.open_writers() {
        let wr = d.writers.get_mut(&w).expect("a writer");
        drop(wr.handler.take());
        wr.stage = Stage::Abandoned;
        d.emit(format!(r#"{{"op":"drop","writer":{w}}}"#))?;
    }
    d.out.flush()?;
    Ok(d.summary)
}
