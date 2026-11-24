# Formal verification

The formal checks of durable-cache, and the record of what they establish.
The plan they follow is Appendix D of the specification
(`docs/durable-cache.pdf`), and the decisions behind it are
ADR-VERIFICATION.FORMAL-01 and ADR-VERIFICATION.MODELS-01; where everything
lives is SDS-VERIFICATION.LAYOUT-01.

## The record

[`record.toml`](record.toml) lists each property a check establishes, with
its test case, checker, harness, bounds and the duration of its last run;
each property a later phase of the plan is to check; and what no check
establishes, with the reason. Check it against the repository with

    cargo xtask verify

which fails on an entry whose test case, requirements or harness the
repository does not have (TC-VERIFICATION.RECORD-01).

## Where the checks live

- **Kani harnesses** sit in the module they check, in a `kani_checks`
  module built only by Kani (`#[cfg(kani)]`), each a `#[kani::proof]`
  function named `tc_<test case>_<what>`.
- **loom tests** sit in the module they check, in a `loom_checks` module
  built only in the crate's tests with `--cfg durable_cache_loom`, which
  also swaps the module's synchronisation types for loom's
  (`durable-cache/src/sync.rs`).
- **Lean code** sits here, in a Lake project of its own for each Lean
  version the checks need. Each Lean library a check uses is a package of its
  own under `libs/<name>/`, named when it is created, and written to be
  published on its own: its own namespace, README, licence and copyright
  line, and nothing of this project inside it.

No check has a time limit: how long one may run is to be decided later, from
the durations the record keeps.
