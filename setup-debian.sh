#!/usr/bin/env bash
# SPDX-FileCopyrightText: 2026 The durable-cache authors
# SPDX-License-Identifier: Apache-2.0

#
# setup-debian.sh — install the toolchain required to build the durable-cache
# specification (docs/) and the durable-cache implementation (Rust
# workspace, its tests, and the cache-tests RFC suite) on Debian 13 "trixie".
#
# Usage:
#   sudo ./setup-debian.sh            # install, then verify
#   ./setup-debian.sh --check         # verify only, no root needed
#   sudo ./setup-debian.sh --build    # install, verify, then build the docs
#
# Idempotent: the script keeps no state of its own. Every step first inspects
# the system (dpkg database, rustup's toolchain list, the installed nextest
# binary's version) and only acts on what is missing or out of date, so it is
# safe to re-run at any time, including after an interrupted run.
#
# What it installs and why:
#   * LaTeX  — latexmk drives pdflatex; docs/latexmkrc adds a post-processing
#              hook that shells out to python3 tools/reqman-report.py.
#   * dot    — reqman-report.py calls `dot -Tplain` to lay out the partial
#              requirement diagrams before emitting TikZ.
#   * jinja2 — reqman-report.py renders every generated .tex fragment (tracing
#              report, requirement tables, coverage) through Jinja2 templates.
#   * Rust   — Debian's rustup package, plus the toolchain pinned by
#              rust-toolchain.toml (with rustfmt and clippy), installed for the
#              invoking user rather than root.
#   * C deps — a C compiler and pkg-config + libssl-dev for openssl-sys
#              (the pingora-core `openssl` feature), cmake for libz-ng-sys.
#   * nextest — cargo-nextest, which CI uses to run the test suites; not
#              packaged by Debian, so the upstream prebuilt binary (GitHub
#              release, SHA-256 verified) is installed
#              to /usr/local/bin.
#   * cargo-deny — the dependency checks CI runs (deny.toml); installed like
#              nextest, from the upstream prebuilt binary (GitHub release,
#              SHA-256 verified) to /usr/local/bin.
#   * Node   — nodejs + npm for http-tests/cache-tests' CLI and origin server
#              (`cargo xtask cache-tests`); its package.json needs node >=14.8.
#
# Debian 13 ships an externally-managed Python (PEP 668), so jinja2 comes from
# apt rather than pip; no virtualenv is needed.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DOC_DIR="$REPO_ROOT/docs"
JOBNAME="durable-cache"

DO_INSTALL=1
DO_BUILD=0
case "${1:-}" in
    --check) DO_INSTALL=0 ;;
    --build) DO_BUILD=1 ;;
    "")      ;;
    *)       echo "usage: $0 [--check|--build]" >&2; exit 2 ;;
esac

# The Rust toolchain lives in a user's home (~/.rustup), so it is installed
# for the user who invoked sudo, not for root.
TARGET_USER="${SUDO_USER:-$(id -un)}"

# Run a command as TARGET_USER, with that user's HOME.
as_target_user() {
    if [ "$(id -un)" = "$TARGET_USER" ]; then
        "$@"
    else
        sudo -H -u "$TARGET_USER" -- "$@"
    fi
}

# ---------------------------------------------------------------------------
# Rust toolchain pin
# ---------------------------------------------------------------------------
# Read from rust-toolchain.toml at the repo root, the one place the toolchain
# is pinned. The script stops rather than guess when that file is missing or
# does not define both the channel and the components.
TOOLCHAIN_FILE="$REPO_ROOT/rust-toolchain.toml"
if [ ! -f "$TOOLCHAIN_FILE" ]; then
    echo "ERROR: $TOOLCHAIN_FILE not found; it pins the Rust toolchain to install" >&2
    exit 1
fi
RUST_CHANNEL="$(sed -n 's/^[[:space:]]*channel[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$TOOLCHAIN_FILE" | head -n1)"
components="$(sed -n 's/^[[:space:]]*components[[:space:]]*=[[:space:]]*\[\(.*\)\].*/\1/p' "$TOOLCHAIN_FILE" | head -n1)"
if [ -z "$RUST_CHANNEL" ] || [ -z "$components" ]; then
    echo "ERROR: $TOOLCHAIN_FILE must define both channel and components" >&2
    exit 1
fi
read -r -a RUST_COMPONENTS <<< "$(printf '%s' "$components" | tr -d '"' | tr ',' ' ')"

# cargo-nextest release to install. Pinned so repeated runs converge on the
# same binary; bump deliberately. The SHA-256 of each release tarball is the
# one GitHub publishes alongside it (cargo-nextest-<ver>-<target>.sha256);
# update both hashes together with the version.
NEXTEST_VERSION="0.9.144"
NEXTEST_SHA256_X86_64="8a4f726272b0a1c499bd87ca3978bfbb1a8c20bb08ccf075b9996e2081bd1e1e"
NEXTEST_SHA256_AARCH64="7fecfd431b810c05c589d800524286b8f80ce2fe9fb1fef05caf16d095cea407"
NEXTEST_BIN="/usr/local/bin/cargo-nextest"

# cargo-deny release to install, pinned the same way: the SHA-256 of each
# release tarball is the one GitHub publishes alongside it
# (cargo-deny-<ver>-<target>.tar.gz.sha256).
DENY_VERSION="0.20.2"
DENY_SHA256_X86_64="9f12ed4c49936e09b48bf862b595cde2fe64fcbd9d74dfacac6131ca824c8d5f"
DENY_SHA256_AARCH64="995c82be0defc7a025cae49a2aa2644ce8245c9a3318fc4103907c6a285e8c7d"
DENY_BIN="/usr/local/bin/cargo-deny"

# ---------------------------------------------------------------------------
# Package list
# ---------------------------------------------------------------------------
# Installed with --no-install-recommends: the recommends of the texlive
# metapackages drag in doc/source trees worth several extra GB that the build
# never reads.
APT_PACKAGES=(
    # --- LaTeX engine and build driver -------------------------------------
    latexmk                     # build driver used by docs/latexmkrc
    texlive-latex-base          # pdflatex, report class, array, calc, ifthen,
                                #   fontenc, hyperref
    texlive-latex-recommended   # geometry, caption/subcaption, xcolor,
                                #   colortbl, tabularx, float, xparse, pdfpages
    texlive-latex-extra         # adjustbox, enumitem, titlesec, xltabular
                                #   (needed by reqman.sty and the chapters)
    texlive-pictures            # pgf/TikZ + the arrows.meta, matrix, fit,
                                #   backgrounds, calc, shapes, shadows and
                                #   decorations.markings libraries, and
                                #   pgf-umlsd for the sequence diagrams
    texlive-fonts-recommended   # Computer Modern / AMS fonts (amssymb)
    texlive-plain-generic       # generic-format helpers pulled in by the above

    # --- Diagram layout ----------------------------------------------------
    graphviz                    # `dot -Tplain`, called by reqman-report.py

    # --- Python side -------------------------------------------------------
    python3                     # runs tools/reqman-report.py and the
                                #   .claude/skills/reqman-context helper
    python3-jinja2              # only non-stdlib import in the toolchain

    # --- Rust workspace ----------------------------------------------------
    rustup                      # toolchain manager; conflicts with (and so
                                #   replaces) Debian's cargo/rustc packages
    build-essential             # cc/ld: rustc's linker, cc-built -sys crates
    pkg-config                  # openssl-sys locates OpenSSL through it
    libssl-dev                  # OpenSSL headers for the `openssl` feature
    cmake                       # libz-ng-sys builds zlib-ng with CMake
    git                         # third_party/cache-tests is a git submodule
    curl                        # fetches the cargo-nextest release
    ca-certificates             #   ... over HTTPS

    # --- RFC 9111 compliance suite (cargo xtask cache-tests) ---------------
    nodejs                      # runs cache-tests' origin server and CLI
    npm                         # `npm install` / `npm run` in cache-tests
)

# Print each package from "$@" that dpkg does not report as installed.
missing_packages() {
    local pkg status
    for pkg in "$@"; do
        # shellcheck disable=SC2016  # ${...} is dpkg-query syntax, not shell
        status="$(dpkg-query -W -f='${db:Status-Status}' "$pkg" 2>/dev/null || true)"
        [ "$status" = "installed" ] || printf '%s\n' "$pkg"
    done
}

# Succeeds when TARGET_USER has RUST_CHANNEL installed with every component.
rust_toolchain_ready() {
    local installed component
    as_target_user rustup toolchain list 2>/dev/null \
        | grep -q "^${RUST_CHANNEL}-" || return 1
    installed="$(as_target_user rustup component list --installed \
        --toolchain "$RUST_CHANNEL" 2>/dev/null)" || return 1
    for component in "${RUST_COMPONENTS[@]}"; do
        grep -q "^${component}-" <<< "$installed" || return 1
    done
}

# Print the version of the cargo-nextest at NEXTEST_BIN, or nothing.
nextest_installed_version() {
    [ -x "$NEXTEST_BIN" ] || return 0
    "$NEXTEST_BIN" nextest --version 2>/dev/null | awk 'NR == 1 { print $2 }'
}

# Print the version of the cargo-deny at DENY_BIN, or nothing.
deny_installed_version() {
    [ -x "$DENY_BIN" ] || return 0
    "$DENY_BIN" --version 2>/dev/null | awk 'NR == 1 { print $2 }'
}

# ---------------------------------------------------------------------------
# Install
# ---------------------------------------------------------------------------
if [ "$DO_INSTALL" -eq 1 ]; then
    if [ "$(id -u)" -ne 0 ]; then
        echo "ERROR: installation needs root. Re-run as: sudo $0 ${1:-}" >&2
        exit 1
    fi

    export DEBIAN_FRONTEND=noninteractive

    # --- apt packages -------------------------------------------------------
    mapfile -t MISSING < <(missing_packages "${APT_PACKAGES[@]}")
    if [ "${#MISSING[@]}" -eq 0 ]; then
        echo "==> apt: all ${#APT_PACKAGES[@]} packages already installed"
    else
        echo "==> apt: installing ${#MISSING[@]} missing package(s): ${MISSING[*]}"
        apt-get update
        apt-get install -y --no-install-recommends "${MISSING[@]}"
    fi

    # texlive's postinst normally does this, but re-running is cheap and makes
    # a partially-configured filesystem-database failure obvious here rather
    # than as a mysterious "File `tikz.sty' not found" later.
    echo "==> refreshing the TeX filename database"
    mktexlsr

    # --- Rust toolchain (per user) -----------------------------------------
    if rust_toolchain_ready; then
        echo "==> rust: $RUST_CHANNEL (${RUST_COMPONENTS[*]}) already installed for $TARGET_USER"
    elif as_target_user rustup toolchain list 2>/dev/null | grep -q "^${RUST_CHANNEL}-"; then
        echo "==> rust: adding components (${RUST_COMPONENTS[*]}) to $RUST_CHANNEL for $TARGET_USER"
        as_target_user rustup component add --toolchain "$RUST_CHANNEL" "${RUST_COMPONENTS[@]}"
    else
        echo "==> rust: installing $RUST_CHANNEL (${RUST_COMPONENTS[*]}) for $TARGET_USER"
        component_args=()
        for component in "${RUST_COMPONENTS[@]}"; do
            component_args+=(--component "$component")
        done
        as_target_user rustup toolchain install "$RUST_CHANNEL" --profile minimal "${component_args[@]}"
    fi

    # --- cargo-nextest (system-wide) ---------------------------------------
    current_nextest="$(nextest_installed_version)"
    if [ "$current_nextest" = "$NEXTEST_VERSION" ]; then
        echo "==> nextest: $NEXTEST_VERSION already installed at $NEXTEST_BIN"
    else
        case "$(dpkg --print-architecture)" in
            amd64) nextest_target="x86_64-unknown-linux-gnu";  nextest_sha256="$NEXTEST_SHA256_X86_64" ;;
            arm64) nextest_target="aarch64-unknown-linux-gnu"; nextest_sha256="$NEXTEST_SHA256_AARCH64" ;;
            *)
                echo "ERROR: no prebuilt cargo-nextest for $(dpkg --print-architecture)" >&2
                exit 1
                ;;
        esac
        echo "==> nextest: installing $NEXTEST_VERSION to $NEXTEST_BIN${current_nextest:+ (replacing $current_nextest)}"
        TMP_DIR="$(mktemp -d)"
        trap 'rm -rf "$TMP_DIR"' EXIT
        # Download from GitHub releases directly rather than through the
        # get.nexte.st redirector (one less service to be down), retrying
        # transient failures such as 5xx gateway errors. The tarball is saved
        # and checksum-verified before extraction, so a truncated or
        # substituted download fails loudly instead of reaching tar.
        nextest_tarball="$TMP_DIR/cargo-nextest.tar.gz"
        curl --proto '=https' --tlsv1.2 -fsSL \
            --retry 5 --retry-all-errors --retry-delay 3 \
            -o "$nextest_tarball" \
            "https://github.com/nextest-rs/nextest/releases/download/cargo-nextest-$NEXTEST_VERSION/cargo-nextest-$NEXTEST_VERSION-$nextest_target.tar.gz"
        echo "$nextest_sha256  $nextest_tarball" | sha256sum --check --quiet -
        tar -xzf "$nextest_tarball" -C "$TMP_DIR" cargo-nextest
        install -m 0755 "$TMP_DIR/cargo-nextest" "$NEXTEST_BIN"
    fi

    # --- cargo-deny (system-wide) ------------------------------------------
    current_deny="$(deny_installed_version)"
    if [ "$current_deny" = "$DENY_VERSION" ]; then
        echo "==> cargo-deny: $DENY_VERSION already installed at $DENY_BIN"
    else
        case "$(dpkg --print-architecture)" in
            amd64) deny_target="x86_64-unknown-linux-musl";  deny_sha256="$DENY_SHA256_X86_64" ;;
            arm64) deny_target="aarch64-unknown-linux-musl"; deny_sha256="$DENY_SHA256_AARCH64" ;;
            *)
                echo "ERROR: no prebuilt cargo-deny for $(dpkg --print-architecture)" >&2
                exit 1
                ;;
        esac
        echo "==> cargo-deny: installing $DENY_VERSION to $DENY_BIN${current_deny:+ (replacing $current_deny)}"
        if [ -z "${TMP_DIR:-}" ]; then
            TMP_DIR="$(mktemp -d)"
            trap 'rm -rf "$TMP_DIR"' EXIT
        fi
        deny_dir="cargo-deny-$DENY_VERSION-$deny_target"
        deny_tarball="$TMP_DIR/cargo-deny.tar.gz"
        curl --proto '=https' --tlsv1.2 -fsSL \
            --retry 5 --retry-all-errors --retry-delay 3 \
            -o "$deny_tarball" \
            "https://github.com/EmbarkStudios/cargo-deny/releases/download/$DENY_VERSION/$deny_dir.tar.gz"
        echo "$deny_sha256  $deny_tarball" | sha256sum --check --quiet -
        tar -xzf "$deny_tarball" -C "$TMP_DIR" "$deny_dir/cargo-deny"
        install -m 0755 "$TMP_DIR/$deny_dir/cargo-deny" "$DENY_BIN"
    fi
fi

# ---------------------------------------------------------------------------
# Verify
# ---------------------------------------------------------------------------
failures=0

check_cmd() {
    local cmd="$1" why="$2"
    if command -v "$cmd" >/dev/null 2>&1; then
        printf '  ok      %-14s %s\n' "$cmd" "$(command -v "$cmd")"
    else
        printf '  MISSING %-14s (%s)\n' "$cmd" "$why"
        failures=$((failures + 1))
    fi
}

check_sty() {
    local sty="$1"
    if kpsewhich "$sty.sty" >/dev/null 2>&1; then
        printf '  ok      %s\n' "$sty"
    else
        printf '  MISSING %s.sty\n' "$sty"
        failures=$((failures + 1))
    fi
}

echo
echo "==> checking executables"
check_cmd pdflatex      "texlive-latex-base"
check_cmd latexmk       "latexmk"
check_cmd kpsewhich     "texlive-latex-base"
check_cmd mktexlsr      "texlive-latex-base"
check_cmd dot           "graphviz"
check_cmd perl          "latexmkrc is a perl file"
check_cmd python3       "python3"
check_cmd rustup        "rustup"
check_cmd cargo         "rustup (proxy in /usr/bin)"
check_cmd cc            "build-essential"
check_cmd pkg-config    "pkg-config"
check_cmd cmake         "cmake"
check_cmd git           "git"
check_cmd curl          "curl"
check_cmd node          "nodejs"
check_cmd npm           "npm"

if command -v kpsewhich >/dev/null 2>&1; then
    echo
    echo "==> checking LaTeX packages required by docs/*.tex and tools/reqman.sty"
    for sty in \
        adjustbox amssymb array caption colortbl enumitem float fontenc \
        geometry hyperref pdfpages subcaption tabularx tikz titlesec \
        xcolor xltabular xparse \
        ifthen pgf-umlsd                   # the sequence diagrams
    do
        check_sty "$sty"
    done
fi

echo
echo "==> checking Python imports"
if python3 -c 'import jinja2, argparse, csv, glob, json, os, re, subprocess, sys' 2>/dev/null; then
    printf '  ok      jinja2 %s (plus stdlib)\n' \
        "$(python3 -c 'import jinja2; print(jinja2.__version__)')"
else
    printf '  MISSING python3-jinja2\n'
    failures=$((failures + 1))
fi

echo
echo "==> checking the Rust build environment"
if command -v rustup >/dev/null 2>&1 && rust_toolchain_ready; then
    printf '  ok      toolchain %s (%s) for %s: %s\n' "$RUST_CHANNEL" \
        "${RUST_COMPONENTS[*]}" "$TARGET_USER" \
        "$(as_target_user rustup run "$RUST_CHANNEL" rustc --version)"
else
    printf '  MISSING toolchain %s with %s for %s\n' "$RUST_CHANNEL" \
        "${RUST_COMPONENTS[*]}" "$TARGET_USER"
    failures=$((failures + 1))
fi
if command -v pkg-config >/dev/null 2>&1 && pkg-config --exists openssl; then
    printf '  ok      openssl %s (pkg-config)\n' "$(pkg-config --modversion openssl)"
else
    printf '  MISSING openssl development files (libssl-dev)\n'
    failures=$((failures + 1))
fi
nextest_version="$(nextest_installed_version)"
if [ "$nextest_version" = "$NEXTEST_VERSION" ]; then
    printf '  ok      cargo-nextest %s\n' "$nextest_version"
else
    printf '  MISSING cargo-nextest %s at %s (found: %s)\n' "$NEXTEST_VERSION" \
        "$NEXTEST_BIN" "${nextest_version:-none}"
    failures=$((failures + 1))
fi
deny_version="$(deny_installed_version)"
if [ "$deny_version" = "$DENY_VERSION" ]; then
    printf '  ok      cargo-deny %s\n' "$deny_version"
else
    printf '  MISSING cargo-deny %s at %s (found: %s)\n' "$DENY_VERSION" \
        "$DENY_BIN" "${deny_version:-none}"
    failures=$((failures + 1))
fi

echo
echo "==> checking Node.js for cache-tests"
if command -v node >/dev/null 2>&1; then
    node_version="$(node --version | sed 's/^v//')"
    if [ "$(printf '%s\n' "14.8.0" "$node_version" | sort -V | head -n1)" = "14.8.0" ]; then
        printf '  ok      node %s (>= 14.8.0)\n' "$node_version"
    else
        printf '  MISSING node >= 14.8.0 (found %s)\n' "$node_version"
        failures=$((failures + 1))
    fi
else
    printf '  skipped (node itself is missing, reported above)\n'
fi

# reqman.sty, the project's own package, ships in the repo under docs/tools/;
# latexmkrc points TEXINPUTS at that directory.  Flag it here so a missing
# checkout is not mistaken for a missing distribution package.
echo
echo "==> checking in-repo LaTeX packages"
for f in "$DOC_DIR/tools/reqman.sty" \
         "$DOC_DIR/tools/reqman-report.py" "$DOC_DIR/latexmkrc"; do
    if [ -f "$f" ]; then
        printf '  ok      %s\n' "${f#"$REPO_ROOT"/}"
    else
        printf '  MISSING %s\n' "${f#"$REPO_ROOT"/}"
        failures=$((failures + 1))
    fi
done

echo
if [ "$failures" -ne 0 ]; then
    echo "FAILED: $failures check(s) did not pass."
    exit 1
fi
echo "All checks passed."

# ---------------------------------------------------------------------------
# Optional smoke build
# ---------------------------------------------------------------------------
if [ "$DO_BUILD" -eq 1 ]; then
    echo
    echo "==> smoke build: latexmk -pdf $JOBNAME (in docs/)"
    # Drop back to the invoking user so the build does not leave root-owned
    # artefacts in docs/build/ and docs/*.pdf.
    as_target_user sh -c "cd '$DOC_DIR' && latexmk -pdf '$JOBNAME'"
    echo "==> built $DOC_DIR/$JOBNAME.pdf"
fi

echo
cat <<EOF
Next steps (as your normal user, not root):

    cd $DOC_DIR
    latexmk -pdf $JOBNAME      # build; reruns passes and reqman-report.py
    latexmk -c   $JOBNAME      # clean, keeping the PDF and requirements.json
    latexmk -C   $JOBNAME      # clean everything

  In the Rust workspace (the repository root):

    cargo nextest run -p durable-cache   # Storage-level tests
    cargo nextest run -p compat                  # end-to-end via fixture-proxy
    cargo xtask trace-report                     # requirement coverage
    git submodule update --init                  # once, for the RFC suite
    cargo xtask cache-tests                      # RFC 9111 suite (Node)

EOF
