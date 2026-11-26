#!/bin/sh
# SPDX-FileCopyrightText: 2026 The HttpCacheStorage authors
# SPDX-License-Identifier: Apache-2.0

# Check the library against what it models: build it, which checks its
# proofs; run its tests; then drive MemCache, the in-memory storage of the
# pingora-cache crate, through SEEDS workloads (16 by default) and check each
# trace against the contract.
#
#   scripts/check.sh [OUT_DIR] [SEEDS]
set -eu
cd "$(dirname "$0")/.."
out="${1:-.lake/traces}"
seeds="${2:-16}"
lake build
lake test
cargo run --quiet --release --manifest-path driver/Cargo.toml -- "$out" "$seeds"
.lake/build/bin/check-trace "$out"/memcache-*.jsonl
