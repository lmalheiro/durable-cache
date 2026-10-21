# SPDX-FileCopyrightText: 2026 The durable-cache authors
# SPDX-License-Identifier: Apache-2.0

{
  description = "durable-cache dev/CI environment";

  # Every input is pinned to a commit rather than a branch, so an upstream
  # change (such as nixpkgs dropping a Node.js release) cannot break a build
  # that worked yesterday. Bump deliberately: rust-overlay must be recent
  # enough to carry the channel in rust-toolchain.toml.
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/b1b875982b17dabde9b4a37f3e229e74913e6db3";
    rust-overlay = {
      url = "github:oxalica/rust-overlay/a1d497a9415999853809ba22a7959b1c36dc6554";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-utils.url = "github:numtide/flake-utils/11707dc2f618dd54ca8739b309ec4fc024de578b";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ rust-overlay.overlays.default ];
        };

        # Pinned via rust-toolchain.toml at the repo root; rust-overlay
        # reads it directly so the flake and `rustup`-based local builds
        # can't drift apart.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # pingora-cache/pingora-core both default to *no* TLS backend
        # (`default = []`), so durable-cache itself never touches
        # a TLS backend at all. Only fixture-proxy actually builds a
        # pingora_core::server::Server and needs one — we standardize on
        # the `openssl` feature everywhere it's requested (see the root
        # Cargo.toml's [workspace.dependencies]) specifically to avoid
        # BoringSSL's cmake+go build-from-source path, which is a real
        # Nix/Darwin reproducibility risk. Confirm the exact feature name
        # still matches if the pingora pin is ever bumped past 0.9.0.
      in
      {
        devShells.default = pkgs.mkShell {
          buildInputs = [
            rustToolchain
            pkgs.cargo-nextest
            pkgs.cargo-deny # dependency checks; see deny.toml
            pkgs.pkg-config
            pkgs.openssl
            pkgs.nodejs_24 # cache-tests' CLI (test-engine/cli.mjs); its
                           # package.json only requires node >=14.8.0.
            pkgs.git
          ];

          # openssl-sys needs this on some platforms even with pkg-config
          # present; harmless to set unconditionally.
          OPENSSL_NO_VENDOR = "1";
        };

        # Sandbox-safe, network-free checks only — the actual test suite
        # binds ports, spawns processes, and sends signals (Group A/D),
        # which fight `nix flake check`'s build sandbox. CI runs
        # `nix develop -c cargo nextest run ...` explicitly instead; see
        # .github/workflows/{ci,compliance}.yml.
        checks.fmt = pkgs.runCommand "cargo-fmt-check" { buildInputs = [ rustToolchain ]; } ''
          cd ${self}
          cargo fmt --all -- --check
          touch $out
        '';
      });
}
