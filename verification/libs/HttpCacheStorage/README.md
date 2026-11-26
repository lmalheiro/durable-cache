# HttpCacheStorage

A Lean 4 model of the storage contract of the `pingora-cache` crate, the
HTTP cache of Pingora®: what the cache expects of a storage backend through
its `Storage`, `HandleHit` and `HandleMiss` traits. It models
`pingora-cache` **0.9.0**.

The model is a checker of traces. A driver makes calls on a storage, one at
a time, and records each with its result; the checker replays the trace and
accepts each result the contract allows in the state the earlier calls left,
or names the first one it does not allow, and why. The library holds:

- **The contract** (`HttpCacheStorage/Contract.lean`), and its summary in
  that file's header: a hit serves only the stored entry or a write in
  flight for its key; a body ends only where it ends; a purge or expiry
  holds once acknowledged; and so on.
- **The trace format and the checker** (`HttpCacheStorage/Trace.lean`,
  `Main.lean`): JSON lines, one call each, read by `check-trace`.
- **A reference implementation** (`HttpCacheStorage/Reference.lean`): an
  in-memory storage that runs, and a proof, `Reference.conforms`, that the
  contract allows every trace it makes, for any calls in any order. The
  proof rests on Lean's three standard axioms alone.
- **A driver** (`driver/`), in Rust: it takes any storage of
  `pingora-cache` 0.9, drives it through a workload drawn from a seed, and
  writes the trace. It never makes a call that would wait on another.
- **Tests** (`HttpCacheStorageTest.lean`): short traces, each of a storage
  that keeps the contract or breaks it once, which `lake test` checks the
  checker judges rightly.

The library depends on the core library of Lean alone, so it builds under
any recent Lean release; `lean-toolchain` names the one it is checked with.

## Checked against `MemCache`

`MemCache`, the in-memory storage that `pingora-cache` ships for tests, is
driven through 16 workloads, each of some 1,500 calls, and every trace must
be one the contract allows:

```sh
scripts/check.sh            # build and prove, test, drive MemCache, check
```

`MemCache` cannot take every call the contract allows, so its workloads
leave some out (`Allow::MEMCACHE` in `driver/src/lib.rs`): a purge while a
write for the key is in flight, after which its miss handler's `finish`
panics; `update_meta` with nothing stored, which panics; a seek with no end
after one with an end, which keeps the earlier end; and a read of a hit
whose write was dropped before its end, which ends the body early as if it
were whole. The contract allows that read to fail, not to end, since the
cache would serve the shortened body as complete.

What `MemCache` does showed one rule too strict: it reports a purge as
having removed an entry for a key whose writes were all dropped, though
nothing was stored. The cache then has nothing to remove from its eviction
manager, so the contract no longer checks what a purge reports where
nothing it targets is stored.

## Checking another storage

Depend on the driver, drive the storage, and check the trace:

```rust
let storage: &'static (dyn Storage + Sync) = /* the storage under test */;
let opts = httpcachestorage_driver::Options::new(seed, httpcachestorage_driver::Allow::ALL);
httpcachestorage_driver::run(storage, &opts, std::fs::File::create("trace.jsonl")?).await?;
```

```sh
lake build check-trace && .lake/build/bin/check-trace trace.jsonl
```

## What the contract leaves out

How many bytes a chunk holds; the sizes `finish` reports; the weights and
accesses reported to the eviction manager; writes that append to a stored
entry (`MissFinishType::Appended`); the eviction manager's own contract; and
anything about concurrency beyond the order of the calls a trace records.

## Licence

Apache-2.0 (`LICENSE`); the copyright holders are listed in `AUTHORS`.

Pingora is a trademark or registered trademark of Cloudflare, Inc. This
library is not affiliated with or endorsed by Cloudflare, Inc.
