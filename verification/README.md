# Formal verification

The formal checks of durable-cache, and the record of what they establish.
The plan they follow is Appendix D of the specification
(`docs/durable-cache.pdf`), and the decisions behind it are
ADR-VERIFICATION.FORMAL-01 and ADR-VERIFICATION.MODELS-01; where everything
lives is SDS-VERIFICATION.LAYOUT-02.

## The record

[`record.toml`](record.toml) lists each property a check establishes, with
its test case, checker, harness, bounds and the duration of its last run;
each property a later phase of the plan is to check; and what no check
establishes, with the reason. Check it against the repository with

    cargo xtask verify

which fails on an entry whose test case, requirements or harness the
repository does not have (TC-VERIFICATION.RECORD-01), and run one checker's
checks with

    cargo xtask verify --checker kani|loom|exhaustive|log-writes|lean|trace|veil [--daily]

which also reports how long each took (TC-VERIFICATION.RUNS-01).

## Where the checks live

- **Kani harnesses** sit in the module they check, in a `kani_checks`
  module built only by Kani (`#[cfg(kani)]`), each a `#[kani::proof]`
  function named `tc_<test case>_<what>`.
- **loom tests** sit in the module they check, in a `loom_checks` module
  built only in the crate's tests with `--cfg durable_cache_loom`, which
  also swaps the module's synchronisation types for loom's
  (`durable-cache/src/sync.rs`).
- **Exhaustive checks**, ordinary tests that go through every case within
  their bounds where the cases are few enough and Kani cannot finish, sit
  in an `exhaustive_checks` module of the module they check.
- **The `log-writes` replay** of TC-PERSISTENCE.CRASHSAFE-03 is
  `cargo xtask crash-replay --fs <fs> --mode <mode>`
  (`xtask/src/crash_replay.rs`). It runs the two ignored tests at the end of
  `durable-cache/tests/persistence_crashsafe.rs`: the scenario on a file
  system over a logged device, then the check on each state the replay of
  the log reaches. It needs Linux, the kernel's `dm-log-writes` target and
  `sudo` without a password; `--checker log-writes` makes the five runs the
  test case asks for.
- **Lean checks** are theorems of `protocol/`, the Lake project under the
  Lean version Veil pins, each named `tc_<test case>_<what>` in a file that
  carries the test case's `IMP-TC.` marker and ends with `#assert_axioms`,
  which lists the axioms the theorem rests on and fails on one outside the
  reviewed list; `--checker lean` also fails on one the record does not
  name for the check.
- **Veil checks** are `safety` and `invariant` clauses, named
  `tc_<test case>_<what>`, of a Veil model in `protocol/` (the protocol
  model, `Protocol/Files.lean`). The model's file proves every clause
  inductive with `#check_invariants`, Lean checking each of the SMT solver's
  proofs (`veil.smt.trust` off), finds the executions it must allow, explores
  a finite instance, and ends with `#gen_theorems` and `#assert_axioms_in`;
  `--checker veil` builds it and fails on any of these, or on an axiom the
  record does not name.
- **Trace checks** are ignored tests of the crate, in
  `durable-cache/tests/verification_contract.rs`, that drive a storage
  through the workloads of the storage-contract library and write their
  traces; `--checker trace` runs them, then the library's `check-trace` on
  every trace.
- **Lean code** sits here, in a Lake project of its own for each Lean
  version the checks need. Each Lean library a check uses is a package of its
  own under `libs/<name>/`, named when it is created, and written to be
  published on its own: its own namespace, README, licence and copyright
  line, and nothing of this project inside it.

No check has a time limit: how long one may run is to be decided later, from
the durations the record keeps.

## The libraries

- [`libs/HttpCacheStorage`](libs/HttpCacheStorage/README.md): the storage
  contract of `pingora-cache` 0.9.0, as a checker of traces, with a
  reference implementation proved to keep it, and a driver that writes the
  trace of any storage. Its own check, `scripts/check.sh`, runs in the
  `HttpCacheStorage` workflow.
