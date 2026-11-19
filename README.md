# durable-cache

> durable-cache is an independent project. It is not affiliated with,
> endorsed by, or sponsored by Cloudflare, Inc., which develops the Pingora®
> framework it works with.

**Latest Pingora Compatible Version:** 0.9.x

A file-system-backed, durable storage backend for Pingora: an implementation
of the `Storage` trait of [`pingora-cache`].

Cached HTTP objects live as ordinary files on an ordinary file system, so the
cache survives process restarts and its total size is bounded by the disk
rather than by RAM. Each object is one self-describing record — a fixed
44-byte header, the serialised cache key, a metadata region with growth
headroom, then the body — synchronised to stable storage by default, and
published by an atomic `rename(2)`. Every lookup validates the header and
metadata by CRC-32C, so a torn, truncated, or foreign record is reported as a
cache miss instead of being served.

[`pingora-cache`]: https://docs.rs/pingora-cache

**Specification:** [PDF][spec-pdf], committed alongside its LaTeX sources.

<!-- Links to repository files are absolute: crates.io, which shows this
README too, resolves relative links against the crate's own directory,
durable-cache/, not against the repository root. They name the tag of the
version in durable-cache/Cargo.toml, so each version's README on crates.io
links to that version's files; on main, those of the latest release. The
dependency line under "Using it" names that version the same way. The
release workflow moves both in each release pull request, and CI checks
them (`cargo xtask readme-links --check`). -->
[spec-pdf]: https://github.com/lmalheiro/durable-cache/blob/v0.2.0/docs/durable-cache.pdf
[changelog]: https://github.com/lmalheiro/durable-cache/blob/v0.2.0/durable-cache/CHANGELOG.md

## Status

**Work in progress.** The crate has not run in production yet, and its API
may change before 1.0; the [changelog][changelog] lists what each release
changed. Every artifact of the specification is still `draft`, pending the
authors' review; the human review of the tests and tooling continues in
[#14](https://github.com/lmalheiro/durable-cache/issues/14), and the known
gaps are [open issues](https://github.com/lmalheiro/durable-cache/issues).

The crate implements the full `Storage` surface — `lookup`, `get_miss_handler`,
`purge`, `update_meta`, plus streaming partial hits and `expire`,
which marks an object stale in place and keeps the body, so the revalidation
that follows is answered by a `304` from the cache rather than a full refetch
(ADR-STORAGE.EXPIRE-01). Beyond `Storage`, `recover()`
rebuilds the eviction manager's view from disk at startup, and
`hotupgrade::install` wires that recovery, and the switch that makes the
outgoing process's backend read-only, into a Pingora server.

Every artifact of the specification is traced, and every test case has a test
function or an `IMP` marker, as `cargo xtask trace-report` and the
specification's own traceability report check. That report's one open
obligation is deliberate: REQ-EFFICIENCY.FRAGMENTATION-01 has no satisfying
decision, because ADR-EFFICIENCY.COMPACTION-01 defers compaction and
declares the conflict. See [Caveats](#caveats) for the test cases that need
root, a scratch device, or a scale a laptop cannot reach.

## Using it

```toml
[dependencies]
durable-cache = "0.2"
```

```rust
use durable_cache::{hotupgrade, FsCache, FsCacheConfig};
use pingora_cache::eviction::simple_lru::Manager;
use pingora_core::prelude::Server;

let mut server = Server::new(None)?;
server.bootstrap();

// `Storage`'s methods take `&'static self`, so the backend is held for the
// process's whole lifetime. On Linux, by default, this process must own the
// cache directory, on a file system with non-blocking reads (see Threads
// and Access times, below).
let config = FsCacheConfig::new("/var/cache/durable-cache", None);
let cache: &'static FsCache = Box::leak(Box::new(FsCache::new(config)?));
let eviction: &'static Manager = Box::leak(Box::new(Manager::new(usize::MAX)));

// Install both halves of the hot-upgrade integration: the upgrade watcher,
// which makes the backend read-only when the upgrade signal arrives, and
// Eviction Recovery as a background service that rebuilds the eviction view
// from disk once the sockets are taken over. Required for hot upgrades;
// without it two processes write the cache at once. A server that
// bootstraps as a service passes `Some(&handle)`, the handle
// `bootstrap_as_a_service` returned, instead of `None`, so that recovery
// waits for that service.
hotupgrade::install(&mut server, cache, eviction, None);

// Then, inside your `ProxyHttp::request_cache_filter`:
session.cache.enable(cache, Some(eviction), None, None, None);
```

The same setup as a complete program is
[`examples/caching-proxy`](https://github.com/lmalheiro/durable-cache/tree/v0.2.0/examples/caching-proxy): a reverse proxy that
caches on disk, survives restarts and supports hot upgrades, with a smoke
test.

Without a Pingora server, call `recover(cache, eviction).await?` once at
start yourself: it rebuilds the eviction view and reclaims what earlier runs
left behind. It is safe to run before serving or concurrently with it, and
there is no snapshot and no delta log — a clean shutdown and a crash take the
identical path.

**Threads.** Every storage operation is a blocking syscall, so the backend
runs each one on tokio's blocking pool rather than on the worker Pingora
polls it from (ADR-STORAGE.EXECUTOR-02). That matters because
`ServerConf::threads` defaults to 1: a backend that ran its I/O inline would
stall every other connection on that worker for the length of a `fdatasync`
or a page-cache miss. On Linux, hits are the exception when what they read
is in the page cache: the lookup and its body reads run on the worker, with
`openat2` and `preadv2` flags that fail rather than wait for the device, and
whatever would wait goes to the pool. That includes partial hits on a store
still being written, through which Pingora also serves each fill's own
response. These page-cache reads are on by default, through an option that
exists only on Linux (`FsCacheConfig::with_page_cache_reads`). They need
Linux 5.12 and a file system with non-blocking reads, as ext4, XFS and
btrfs have and, as of Linux 6.12, tmpfs and overlayfs do not (overlayfs is
the file system of a container without a volume): `FsCache::new` fails,
and logs the error, where they are missing, and
`with_page_cache_reads(false)` switches them off. The pool is sized by two
`ServerConf` settings: `max_blocking_threads` (unset keeps tokio's 512) and
`blocking_threads_ttl_seconds`, how long an idle thread lives (unset keeps
10 s). The pool's threads mostly wait on the disk, so the maximum caps how
many storage operations can wait on it at once. Beyond it they queue: misses
slow down, but nothing fails. Raise it when a slow device meets a heavy miss
rate, with many stores waiting in `fdatasync` together. With `work_stealing`
off, each worker thread has a pool of its own of that size.

**Errors.** Every fallible call, `FsCache::new`, `FsCache::flush`, `recover`
and each `Storage` method, fails with the Pingora error type,
`pingora_error::Error`; the crate has no error type of its own. A file
operation that fails has the matching `File*Error` type, a context saying
what was being done to which file (an object's file is named by its key's
hash), and the underlying error, an `io::Error` or the reason a record
failed validation, as its cause. Two types are part of the contract:
`update_meta` on a key with no stored object fails with
`Custom("key_not_found")`, and once the outgoing process of a hot upgrade has
made its backend read-only, `purge`, `expire` and `update_meta` fail with
`Custom("cache_read_only")`.

**User tags.** A `CacheKey`'s `user_tag` is part of an object's identity,
as the Pingora eviction managers count it. A URL cached under two tags is two
objects, each stored, evicted and purged on its own, so set the tag to the
object's owner or tenant (SDS-LAYOUT.SHARDING-01).

`FsCacheConfig::new` also takes `meta_headroom`: the space reserved ahead
of each body so a revalidation can write its metadata in place. A
revalidation that outgrows it withdraws the object, which the next request
fetches again (see [Caveats](#caveats)). `None` selects the design's
default, 512 bytes.
Each body chunk is written to its record's file before its `write_body` call
returns; the backend holds no body bytes in memory beyond the chunk in hand.

**Access times.** Under the default `relatime`, the first read of a record
after a store wrote it updates its access time, and on ext4 that starts a
journal transaction, which can stall the worker serving the hit. So by
default the backend reads records with `O_NOATIME`, through an option that
exists only on Linux (`FsCacheConfig::with_noatime`); the crate never reads
access times. The kernel allows that flag only to a file's owner, or to
root, so `FsCache::new` fails unless this process owns the cache root, and a
read of a record another user owns fails: `with_noatime(false)` switches it
off. A file system mounted `noatime` or `lazytime` does not need it.

**Durability.** By default each store is synchronised to the device
(`fdatasync`) before it is published, so a host crash or a power loss can
lose a store but never publish a record whose data is not on the device.
`FsCacheConfig::with_durability(Durability::NoStoreSync)` publishes stores
without that wait, which takes most of a fill's time on a slow device.
A process crash or a restart still loses nothing, but a host crash may lose
the stores the kernel had not written back yet. It may also leave a record
whose body never reached the device: a short one is a miss, but one
zero-filled under a valid header would be served
(ADR-PERSISTENCE.DURABILITY-02). File systems that record a file's length
only once its data is written, as ext4 in its default `data=ordered` mode,
XFS and btrfs do, leave such a record short. For the others, which may
record the length first, `FsCacheConfig::with_discard_after_host_restart(true)`
(Linux only) closes the gap: before the first unsynchronised store, the
backend writes a marker naming the current boot, `FsCache::flush` removes it
once everything is on the device, and a backend that finds a marker from
another boot starts empty (ADR-PERSISTENCE.UNSYNCED-01). The hot-upgrade
helper flushes after making the backend read-only, and at the end of a
shutdown's grace period; without it, call `flush` once the server has
stopped, which `Server::run` allows and `run_forever` does not. A cache not
flushed is emptied by the next host restart.

**Features.** `body-crc` (on by default) computes each body's CRC-32C as it
is stored and records it in the header; without it (`default-features =
false`) records carry zero. Serving a hit does not check it: integrity at
rest is the storage layer's (see [Caveats](#caveats)).
`body-crc-verify` (off by default, implies `body-crc`) makes a read-time
check available, switched on with
`FsCacheConfig::with_body_crc_verification(true)`: a read that delivered a
whole body whose checksum does not match logs an error naming the file, and
the response still ends normally, since every byte has already gone to the
client (ADR-PERSISTENCE.DURABILITY-02).

`metrics` (off by default) registers the backend's counters
in the `prometheus` default registry, where the Pingora exporter,
`pingora-prometheus`, serves them: the metadata updates that outgrew the
headroom, and the files and bytes the restart scan reclaims, by
class. Enable it where you run
that exporter. The counters are registered through the `prometheus` crate
that `pingora-prometheus` re-exports, so they are in the registry it serves
by construction. Without the feature, this crate links neither crate and
registers nothing; the warning logs beside each counter are unconditional
(ADR-OBSERVABILITY.METRICS-01).

`test-support` exposes internals to this workspace's own test crates. It is
not API and is exempt from semver: do not enable it.

## Repository layout

Only `durable-cache/` ships. Everything else is specification, an example,
test scaffolding, or tooling.

```
.
├── durable-cache/     ← THE DELIVERABLE. The only published crate.
│   ├── src/
│   │   ├── lib.rs               re-exports FsCache, FsCacheConfig, Durability,
│   │   │                        recover, RecoveryReport; hotupgrade is the one
│   │   │                        public module, the rest are crate-private
│   │   ├── storage_impl.rs      impl Storage for FsCache — glues the modules below
│   │   │
│   │   │                        ── on-disk contract ──
│   │   ├── format.rs            the record header: a #[repr(C)] struct, its offsets
│   │   │                        via mem::offset_of! pinned by const asserts, the
│   │   │                        edit guard that reseals it, and the checksums
│   │   ├── record.rs            reading a record and validating it
│   │   ├── key_codec.rs         cache key <-> the record's key segment
│   │   ├── sharding.rs          <root>/<name[0:2]>/<name[2:4]>/<name>, the name
│   │   │                        from the key's hash and its user tag
│   │   ├── dircache.rs          8 KiB bitset of which of the 65,536 shards exist
│   │   │
│   │   │                        ── operations ──
│   │   ├── miss.rs              write path: get_miss_handler / HandleMiss
│   │   ├── lookup/
│   │   │   ├── mod.rs           read path: lookup / HandleHit, incl. partial hits
│   │   │   ├── body_check.rs    the read-time body check (feature `body-crc-verify`)
│   │   │   ├── page_cache.rs    Linux: lookups and body reads tried first on
│   │   │   │                    the worker, from the page cache
│   │   │   └── noop_page_cache.rs  elsewhere: its stand-in, every read on the pool
│   │   ├── publish.rs           sync, atomic rename, discard; holds the old record open
│   │   ├── crashdiscard.rs      unsynchronised-store markers, flush, and the discard
│   │   │                        after a host restart
│   │   ├── metaupdate.rs        update_meta: in place, or withdraws when outgrown
│   │   ├── expire.rs            expire: marks an object stale in place, keeping its body
│   │   ├── purge.rs             unlink the canonical name; an invalidation also syncs the shard dir
│   │   ├── inflight.rs          in-flight registry backing partial reads
│   │   ├── reclaim.rs           classifies orphaned temps and leases, damaged and withdrawn records
│   │   ├── recovery.rs          recover(): one tree walk that admits + reclaims
│   │   ├── hotupgrade.rs        read-only on the upgrade signal + the recovery service
│   │   ├── locks.rs             the per-key publication lock and the read-only gate
│   │   ├── writerid.rs          the writer's lease, and the names of its temporaries
│   │   ├── config.rs            FsCacheConfig
│   │   ├── error.rs             the two error types callers can tell apart
│   │   ├── blocking.rs          runs file I/O on tokio's blocking pool, not on
│   │   │                        the service worker (Linux page-cache reads aside)
│   │   ├── nowait.rs            Linux: page-cache reads on the worker, which
│   │   │                        fail rather than wait for the device
│   │   ├── platform.rs          the calls that differ between Linux and other
│   │   │                        Unix systems: opening, reading on the pool, flushing
│   │   ├── hooks/               test-only pauses, faults and instrumentation
│   │   └── metrics.rs           prometheus counters (feature `metrics`)
│   └── tests/                 integration tests against the real Storage trait
│                              (no HTTP); named tc_<requirement-id>_*. Built into
│                              the crate's own test binary, so they can use the
│                              cfg(test) hooks; no other build contains them
│
├── test/                      ← test-only. Every member is publish = false.
│   ├── fixture-proxy/           disposable Pingora proxy hosting FsCache
│   ├── mock-origin/             in-process scripted origin (chunked, delayed, …)
│   ├── harness/                 process spawn/signal control, HTTP client,
│   │                            filesystem tamper helpers, hot-upgrade pair;
│   │                            reaches internals via the test-support feature
│   ├── compat/                  end-to-end tests driving a real proxy process
│   └── cache-tests-baseline.json  triaged allowlist for the RFC suite (below)
│
├── examples/caching-proxy/    a runnable reverse proxy that caches with the
│                              crate, and its smoke test; publish = false
│
├── docs/                      ← the specification this code implements
│   ├── durable-cache.tex      master document; *-hlr/-refine/-design/-tc/… are
│   │                            its chapters (stakeholder reqs → ADRs → design →
│   │                            test cases), in a model inspired by SysML®
│   │                            requirement diagrams
│   ├── tools/reqman.sty         the LaTeX requirement environment + relations
│   ├── tools/reqman-report.py   builds the traceability report and the JSON export
│   ├── tools/reqman-html.py     renders the spec as one reader-mode-friendly page
│   ├── durable-cache-externals.csv  links requirements to external references
│   └── rfc9110/9111/3864        the standards the compliance chapter cites
│
├── xtask/                     cargo xtask cache-tests | trace-report | headers |
│                              fmt | readme-links | bench | cfg-matrix
├── third_party/cache-tests/   git submodule, pinned — the HTTP cache test suite
├── .github/workflows/         ci.yml, compliance.yml, deny.yml, pr-title.yml and
│                              release-plz.yml (see below)
├── .githooks/                 opt-in hooks: pre-commit checks formatting, pre-push
│                              that the specification still builds
├── .claude/skills/            AI-assistant skills for writing, checking and
│                              querying the specification (reqman-context is
│                              used below)
├── setup-debian.sh            installs the build and test dependencies on Debian 13
├── deny.toml                  cargo-deny policy: advisories, licences, sources
├── release-plz.toml           release settings (see Releasing, below)
├── flake.nix                  toolchain: Rust, Node, OpenSSL; inputs pinned by commit
├── rust-toolchain.toml        the pinned Rust toolchain
├── rustfmt.toml               formatting, with nightly-only import options
├── .config/                   nextest.toml; rustfmt-toolchain, the nightly that
│                              formats the workspace
├── AUTHORS                    the copyright holders
└── LICENSE                    Apache-2.0
```

## Building and testing

The Nix flake provides the exact toolchain (`nix develop`). On Debian 13,
`sudo ./setup-debian.sh` installs everything below and is safe to re-run.
Otherwise you need the Rust toolchain pinned in `rust-toolchain.toml`, Node
≥ 14.8 for the RFC suite, and OpenSSL. On macOS, which CI does not check
(see [Caveats](#caveats)), set
`export OPENSSL_DIR="$(brew --prefix openssl@3)"` and
`PKG_CONFIG_PATH="$OPENSSL_DIR/lib/pkgconfig"` for a cold build.

```bash
cargo test -p durable-cache   # Storage-level tests, no HTTP, no processes
cargo test -p durable-cache --features metrics   # the same, with metrics
cargo test -p durable-cache --no-default-features       # without the body CRC
cargo test -p durable-cache --features body-crc-verify  # with the read-time check
cargo test -p compat                  # end-to-end through a real fixture-proxy
cargo test -p caching-proxy           # the example, in front of a mock origin
cargo xtask trace-report              # every test case has a test fn or IMP marker
cargo xtask cache-tests               # the RFC 9111 suite (needs Node + submodule)
cargo xtask cfg-matrix                # every cfg build, on Linux and other Unix
cargo deny --manifest-path durable-cache/Cargo.toml --exclude-dev check
                                      # advisories, licences, duplicates, sources
```

`trace-report` reads `docs/durable-cache-requirements.json`, which is not
committed: build the specification first (see
[The specification](#the-specification)), and rebuild it after editing a
`.tex` file, or the report checks a stale list of test cases.

**Other Unix systems.** No CI job builds durable-cache for a system other
than Linux. `cargo xtask cfg-matrix` lints and tests it here in every build
its `#[cfg]` gates select: clippy for each feature set and for a release
build, and nextest with the default features and with all of them, first in
the workspace, then in a copy under the target directory whose Linux gates
select their other-Unix side. `--no-tests` lints only. Each step's whole log
stays under `<target>/xtask/cfg-matrix/logs` until the next run. What it
cannot catch is code outside the Linux gates that calls what only Linux has
in std (`std::os::linux`) or in libc: the copy still builds for Linux.

**Other targets.** CI builds for 64-bit glibc only. The 32-bit glibc and
the musl code (`durable-cache/src/nowait.rs`) can be checked on an amd64
host that has the two targets and the C toolchains the dependencies' build
scripts need for them (`gcc-multilib`, `musl-tools`). On Debian 13,
`setup-debian.sh` installs them; the Nix shell does not.

```bash
cargo check -p durable-cache --lib --target i686-unknown-linux-gnu
cargo check -p durable-cache --lib --target x86_64-unknown-linux-musl
```

**Formatting.** `rustfmt.toml` sorts imports into std, external and crate
groups and merges them per module, with options only nightly rustfmt has.
`cargo xtask fmt` formats the workspace with the nightly pinned in
`.config/rustfmt-toolchain`, and `cargo xtask fmt --check` is what CI runs.
The Nix shell and `setup-debian.sh` provide that rustfmt; with rustup alone,
install it once:

```bash
rustup toolchain install "$(cat .config/rustfmt-toolchain)" --profile minimal --component rustfmt
```

A stable `cargo fmt` warns about those options and leaves imports as they
are; so does an editor's format-on-save unless it runs that nightly (for
rust-analyzer, `rust-analyzer.rustfmt.extraArgs` set to `["+<the pin>"]`).
The repository's git hooks are opt-in, once per clone,
`git config core.hooksPath .githooks`: `pre-commit` then refuses a commit
that changes Rust code while the workspace is not formatted, and `pre-push`
one that touches `docs/` while the specification does not build.

Every feature build must pass: FDS-OBSERVABILITY.HEADROOM-01 and
FDS-STORAGE.RECLAIM-01 each require the log to carry what the counters would
have reported when `metrics` is off, and the body CRC's two optional builds
change what is stored and what a read checks.

The three measuring test cases are `#[ignore]`d and take their scale from the
environment, since a laptop should not pay for them by default. They refuse
to run on an unoptimised build, hence `--release`:

```bash
# space overhead per load, latency at two cache sizes, capacity beyond RAM
cargo test -p durable-cache --release --lib -- --ignored --nocapture integration_tests::measurements
```

`cargo xtask cache-tests` is TC-COMPLIANCE.RFC-01: it starts the `cache-tests`
origin and runs the whole suite twice against the same proxy — once on
the stock `MemCache` of `pingora-cache`, once on this backend — and compares
the two test by test, ranking outcomes pass, then suboptimal, then fail. It
fails when a test ranks **lower** on this backend than on `MemCache`, which
is the non-regression HLR-COMPLIANCE.RFC-01 asks for: the backend must add
no deviation of its own, not fix those of `pingora-cache`. Each backend is
run `--passes` times (2 by default), and a difference counts only when every
pass agrees; the rest are reported as timing-dependent.

It also diffs this backend's own `required` failures against
`test/cache-tests-baseline.json`, where every entry records why that failure
belongs to `pingora-cache`, traced to its source, and fails on any `required`
failure the baseline does not list. That check spans every pass too: a
failure gates the command only when it failed in all of them, since a test
that passed at least once did so against the same build and the same origin.
One that failed some passes is reported as the suite's own flake floor —
which is real, and includes transport errors and setup failures that never
got far enough to judge the cache at all.

Do **not** use `cargo test -- --include-ignored` to "run everything": most
ignored tests in `test/compat/` are deliberate `unimplemented!()` stubs for
HTTP-level checks that are already covered at the crate level, and they will
panic. The real ones are the two hot-upgrade tests in `hotupgrade.rs`, and
they run like this (Linux only):

```bash
# hot upgrade: two fixture-proxy processes, no root needed
cargo nextest run -p compat --run-ignored only -E 'binary(hotupgrade)'
```

### Benchmark

`cargo xtask bench` measures, on your machine, how far a Pingora proxy on
durable-cache is from nginx and its `proxy_cache`. Three caching proxies stand
in front of one origin, an nginx serving fixed bodies: nginx with
`proxy_cache`, and `fixture-proxy` on durable-cache and on `MemCache`, the
in-memory backend of `pingora-cache`, which separates the cost of the storage
from that of the proxy. Every round starts each server with an empty cache.
The report gives the medians of three rounds, with each server's processor
time per request and the most threads it ran, for these scenarios:

- `hits`: cache hits on 1 KiB, 64 KiB and 1 MiB bodies;
- `fills`: filling the cache with new URLs of 4 KiB, 64 KiB and 1 MiB;
- `burst`: 1 KiB hits, for 2 s, while 16 connections store 4,000 new 1 MiB
  bodies, with the host's peak dirty data and data under write-back (the
  servers with a cache on disk only);
- `cold`: 1 MiB hits on 400 objects that were evicted from the page cache,
  then the same objects cached (the servers with a cache on disk only);
- `census`: the system calls durable-cache makes per request, hits and
  fills, which strace counts; a store's pool handoffs are its `pwrite`
  calls.

`--only` runs some of them, as in `--only burst,cold`; the test-case
verdicts need `hits` and `fills`.

```bash
cargo xtask bench --work-dir /var/tmp/durable-cache-bench
```

`--durability no-store-sync` runs durable-cache with its stores published
without synchronisation, as TC-PERFORMANCE.STORERATE-01 measures them; the
default, `strict`, is durable-cache's own default, which
TC-PERFORMANCE.HITRATE-01 measures.

It needs `nginx` and [`oha`](https://github.com/hatoo/oha), the load
generator; `--nginx` and `--oha` name them if they are not on the `PATH`.
The census also needs `strace`. nginx runs as an ordinary user, with its
files in the work directory. Put that directory on the disk you mean to
measure, since on a tmpfs `fdatasync` costs nothing; it needs about 7 GB
free, as each server's cache is deleted after its runs. The burst writes
about 4 GB to the disk per server and round. The servers and the load generator get separate physical cores
(`--server-cpus` and `--load-cpus` override the split), and each server gets
one worker per server CPU (`--threads` overrides it): `worker_processes` for
nginx, and for fixture-proxy `ServerConf::threads`, whose default is 1. The
benchmark does not run in CI, whose shared runners are too noisy to compare
numbers.

Two differences shape the results:

- By default, durable-cache makes every store durable with `fdatasync`
  before publishing it, its crash safety; nginx never synchronises its cache
  files. The fill scenarios show that cost, which
  `--durability no-store-sync` leaves out.
- nginx sends a hit's body from the file with `sendfile`, without copying it
  through user space; durable-cache reads it in 128 KiB chunks.

### Continuous integration

| Workflow          | Runs on                                   | What it does |
|-------------------|-------------------------------------------|--------------|
| `ci.yml`          | pushes to `main`, pull requests           | rustfmt (the pinned nightly), SPDX headers, the README's links and dependency line naming this version, clippy, the crate's tests in each of its four feature configurations, the compat tests, the example's smoke test, the library built with its minimum Rust (1.85), and `cargo publish --dry-run` |
| `compliance.yml`  | pushes to `main`, nightly, manual         | the RFC 9111 suite, the hot-upgrade tests, and the measuring test cases at CI scale |
| `deny.yml`        | dependency or policy changes, nightly, manual | `cargo deny` over the published crate's dependencies (`deny.toml`): security advisories, licences, duplicate versions, sources |
| `pr-title.yml`    | pull requests                             | the title is a conventional commit subject (`type(scope): description`), since pull requests are squash-merged and the title becomes the commit's subject |
| `release-plz.yml` | pushes to `main`                          | keeps the release pull request up to date, its README's links and dependency line naming the new version; when one is merged, tags the version, publishes it with the token in the `crates-io` environment and creates the GitHub release (see below) |

### Releasing

release-plz keeps a release pull request open on `main`: the next version of
`durable-cache`, chosen from the commit subjects since the last release
(cargo-semver-checks flags breaking API changes), and its changelog entry. The
entry groups the changes a user gets (added, fixed, performance, changed,
reverted, dependencies, documentation) and leaves out CI, tooling, formatting
and test-only commits. Merging the pull request releases that version:
release-plz tags it `v<version>`, publishes it to crates.io and creates the
GitHub release, with the changelog entry as its notes. Any other push to `main`
releases nothing. To release:

1. Merge everything else the release should include first. When `main`
   moves after you edit the release pull request, release-plz closes it and
   opens a fresh one without your edits.
2. On the release pull request's branch, rebuild the specification with the
   version's tag, so its source links point at that release, and edit
   `durable-cache/CHANGELOG.md` if needed:
   `cd docs && REQMAN_SOURCE_REF=v<version> latexmk -g -pdf durable-cache`.
   The workflow has already pointed the README's links and dependency line
   at that version. Commit
   and push: your push runs CI on the pull request. The workflow's own pushes
   do not, so a release pull request you leave as it is needs closing and
   reopening for CI to run.
3. Merge it.

Publishing reads a crates.io token from the repository's `crates-io`
environment, which only runs on `main` may use, and only the release job reads
it, only for that merge. Create the token on crates.io with the
publish-update scope alone, as the crate already exists, for the crate
`durable-cache`, with an expiry, and store it there:

```bash
gh secret set CARGO_REGISTRY_TOKEN --env crates-io
```

Now that the crate is on crates.io, trusted publishing can replace the
token: configure it on crates.io for this repository, the workflow
`release-plz.yml` and the environment `crates-io`, give the release job
`id-token: write`, and delete the secret.

## The specification

This project is requirements-driven: `docs/` holds a requirements document
(stakeholder requirements → decisions/ADRs → design → test cases), and the
code is traced back to it. Its artifact types and the relationships between
them are inspired by SysML® requirement diagrams, without following them
strictly. Source files carry `IMP-<ID>` marker comments:

```rust
// IMP-STORAGE.PUBLISH-02: satisfy FDS-STORAGE.PUBLISH-02
```

`reqman-report.py` extracts those during the document build, so the
traceability report shows which code satisfies which requirement, and links
each marker to its line on GitHub: on `main`, or on the ref
`REQMAN_SOURCE_REF` names. `REQMAN_SOURCE_URL=file` links to the files of
the checkout that builds it instead, for reading locally.

```bash
cd docs && latexmk -pdf durable-cache   # builds the PDF *and* refreshes the
                                          # requirements JSON that tooling reads
```

Note that `docs/durable-cache-requirements.json` is a build artifact derived
from the LaTeX run — editing a `.tex` file does not update it until you rebuild.
The JSON is not committed; the [PDF][spec-pdf] is, so it can be read without
a LaTeX toolchain. Nothing rebuilds it automatically: after editing a `.tex`
file, rebuild and commit `docs/durable-cache.pdf` with the change.

To have `git push` refuse commits that touch `docs/` when the specification no
longer builds, enable the repository's hooks once per clone (they also check
formatting before each commit; see
[Building and testing](#building-and-testing)):

```bash
git config core.hooksPath .githooks
```

For a reading copy in a browser — one self-contained page, structured so
Firefox's and Safari's reader modes pick it up:

```bash
cd docs && python3 tools/reqman-html.py    # writes docs/durable-cache.html
```

It is generated from the same build the PDF comes from, so run `latexmk`
first. The PDF stays canonical: the HTML drops the requirement and sequence
diagrams and the traceability report, keeping the prose, the tables and every
cross-reference as a link. It is gitignored, being derived.

To query a requirement without reading LaTeX:

```bash
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py \
    durable-cache FDS-STORAGE.PUBLISH-02
```

A pull request that changes what an existing requirement says gives it its
next version, `-01` becoming `-02`, and every reference follows: the
specification, code comments, `IMP` markers and the test functions named
after a test case. `--changed` checks this against a revision and shows what
each change reaches:

```bash
python3 ./.claude/skills/reqman-context/scripts/reqman-context.py \
    durable-cache --changed main
```

## Caveats

- **Linux is the one tested platform.** CI builds and tests on Linux only.
  The crate relies on Unix file APIs (`flock`, positional reads and
  writes), so it does not build on Windows; other Unix systems, macOS
  included, are untested
  ([#6](https://github.com/lmalheiro/durable-cache/issues/6),
  [#7](https://github.com/lmalheiro/durable-cache/issues/7)).
- **On musl, the crate needs musl 1.2.5 or later.** It calls `preadv2`,
  which musl has from 1.2.5 on, and no CI job builds for musl. Rust's musl
  targets bundle 1.2.5 from Rust 1.93 on; against an older musl, a program
  that uses the crate does not link, whatever its configuration.
- **The hot-upgrade tests are Linux-only.** `test/compat/tests/hotupgrade.rs`
  needs the Pingora fd transfer, which `pingora-core` implements only on Linux.
  It is `#![cfg(target_os = "linux")]` and compiles to nothing elsewhere, so
  a macOS run of the suite does not exercise it; its tests pass on Debian 13,
  and `compliance.yml` runs them in CI.
- **Hot-upgrade exit timing.** After handing over its listener, a Pingora
  process sleeps a fixed 5s and then `grace_period_seconds` (default 300s)
  before starting its runtime shutdown, however soon its connections drain.
  The shutdown then waits at most `graceful_shutdown_timeout_seconds`
  (default 5s) for the runtimes to drain. `fixture-proxy` exposes
  `--grace-period-seconds` / `--graceful-shutdown-timeout-seconds`, and the
  hot-upgrade harness shortens both; an integrator should size them for
  their own longest in-flight request.
- **Host-crash safety, not durability.** By default a store is `fdatasync`ed
  before its rename, and in every mode an invalidating purge `fsync`s its
  shard directory, so a crash can never serve wrong content or resurrect an
  invalidated object — but the shard directory is not synchronised after a
  publication, so a very recently stored object may simply be gone after a
  power failure (a miss). An in-place metadata update is not synchronised
  either: after a crash it reads back as the old set, the new set, or a
  miss, never a mixture (ADR-PERSISTENCE.DURABILITY-02). With
  `Durability::NoStoreSync`, a crash can also leave a zero-filled body to be
  served, on a file system that may record a file's length before its data,
  unless the discard after a host restart is on (see **Durability** above,
  and HLR-PERSISTENCE.CRASHSAFE-02).
- **Storage that honours flushes and keeps data intact.** Those guarantees
  assume the storage stack makes a flush it acknowledges durable, as a
  local disk does; a device or virtualisation layer that acknowledges
  flushes it has not made can lose a purge or leave a published name over
  wrong bytes. Detecting data corrupted at rest is the storage layer's job,
  not the cache's: serving a hit does not check the body, so to catch it put
  the cache directory on a data-checksumming file system (ZFS, btrfs,
  bcachefs) or on `dm-integrity`, which turn a corrupted block into a read
  error, and the read ends with an error. ext4 and XFS checksum only their
  own metadata. The `body-crc-verify` feature can log mismatches, but only
  after the corrupted bytes have been sent (HLR-PERSISTENCE.CRASHSAFE-02).
- **One host, a local file system.** Every process sharing a cache directory
  must run on the same host, with the directory on a local file system: each
  backend instance holds a lease in the directory's `writers/`, locked with
  `flock`, and the restart scan decides whether a temporary's writer is
  still running by trying that lock (ADR-STORAGE.WRITERID-01). NFS emulates
  `flock` with locks that do not work this way. Processes in separate PID
  namespaces, such as containers on one host sharing a volume, may share a
  directory.
- **One header dictionary per cache directory.** Every process sharing a
  cache directory must load the same header compression dictionary into
  `pingora-cache` (`set_compression_dict_content` or
  `set_compression_dict_path`), or none at all. `pingora-cache` gives no way
  to tell which dictionary a stored record's headers need, so the backend
  cannot detect a mismatch. After changing the dictionary, delete the cache
  directory or point the new process at a new one
  (FRE-COMPAT.FORMATUPGRADE-01).
- **Metadata that outgrows its headroom costs a refetch.** A revalidation
  writes the new headers in place when they fit the space reserved ahead of
  the body (`meta_headroom`). When they do not, the backend withdraws the
  object: the response at hand is still served, but the next request misses
  and fetches the whole object from the origin, and if the origin fails
  before then, there is no stale copy left to serve. Each case logs a
  warning giving the shortfall and, with `metrics`, raises
  `durable_cache_meta_headroom_exceeded_total`; raise the headroom if it
  keeps rising. A later version may move outgrown metadata after the body
  instead, so that nothing is fetched again (ADR-STORAGE.METAMOVE-01).
- **Hot upgrades need the helper.** `hotupgrade::install` is what makes the
  outgoing process's backend read-only and delays the incoming scan; without
  it both processes write the shared cache.
- **The host-crash replay harness is not implemented.**
  TC-PERSISTENCE.CRASHSAFE-02's setup replays a device-mapper `log-writes`
  device to every flush point, which needs root, a scratch block device, and
  the kernel's replay tool. `tests/persistence_crashsafe.rs` instead
  constructs the on-disk states such a replay produces (a torn in-place
  update or withdrawal, lost or corrupted body blocks, an unfinished store,
  an acknowledged purge, and, with stores unsynchronised, a body that never
  reached the device) and checks the same contract.
- **Built for one Pingora release line at a time.** Any patch release of the
  line named at the top is accepted; moving to the next minor release is a
  deliberate revision, because the `Storage` and eviction surfaces this
  backend relies on may change between minor releases.
- **The record format belongs to the backend, not to Pingora.** Each record
  carries a format version (`format::FORMAT_VERSION`), and a Pingora update
  that leaves the format alone keeps the cache warm. When the format does
  change, every existing record reads as a miss and the cache starts cold
  (FRE-COMPAT.FORMATUPGRADE-01). Nothing is mis-decoded, and nothing needs
  deleting by hand: the restart scan reclaims the old records as it walks
  the tree. A golden-bytes test fails if a `pingora-cache` release changes
  the metadata encoding records rely on, so that change always gets a
  decision.

## Security

Please report a vulnerability privately, as [SECURITY.md](https://github.com/lmalheiro/durable-cache/blob/v0.2.0/SECURITY.md)
describes, not in a public issue.

## Authors

The code was written with AI assistance (Anthropic's Claude), directed and
reviewed by the human authors listed in [AUTHORS](https://github.com/lmalheiro/durable-cache/blob/v0.2.0/AUTHORS), who hold its
copyright. Before the first release, the crate's own source had a
file-by-file human review; the review of its tests and tooling continues in
[#14](https://github.com/lmalheiro/durable-cache/issues/14). Every source
file carries an SPDX header naming the authors and the licence;
`cargo xtask headers` checks it, and CI runs it.

## Licence

Apache-2.0; see [LICENSE](https://github.com/lmalheiro/durable-cache/blob/v0.2.0/LICENSE).

## Pingora references

- [How the Pingora proxy was built][pingora-blog-2022], on the Cloudflare
  blog, 14 September 2022: a proxy written in Rust that replaced the
  nginx-based one Cloudflare ran before.
- [The Pingora framework released as open source][pingora-blog-2024], on the
  Cloudflare blog, 28 February 2024: a Rust framework for building HTTP
  proxies, clients and servers, under the Apache License 2.0.
- [The Cloudflare cache on a Pingora-based proxy][pingora-changelog-2026],
  in the Cloudflare changelog, 4 May 2026: the cache now runs on a new proxy
  built on Pingora.

[pingora-blog-2022]: https://blog.cloudflare.com/how-we-built-pingora-the-proxy-that-connects-cloudflare-to-the-internet/
[pingora-blog-2024]: https://blog.cloudflare.com/pingora-open-source/
[pingora-changelog-2026]: https://developers.cloudflare.com/changelog/post/2026-05-04-pingora-powers-cache/

## Trademarks

Pingora is a trademark or registered trademark of Cloudflare, Inc. SysML is a
registered trademark of Object Management Group, Inc. in the United States
and/or other countries. All other trademarks are the property of their
respective owners.
