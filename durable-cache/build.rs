// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Names the builds in which stores can write through an `io_uring` ring
//! (FDS-STORAGE.IOURING-01): `cfg(io_uring)` holds on Linux, on the
//! architectures the `io-uring` crate has bindings for, all 64-bit. That
//! crate refuses to build elsewhere, so the manifest's dependency on it names
//! the same platforms, and `crate::uring` is a stand-in there, without a
//! ring.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(io_uring)");
    // Set by the checkers of ADR-VERIFICATION.FORMAL-01, not by this script:
    // `cfg(kani)` by Kani, and `cfg(durable_cache_loom)` by the loom checks,
    // which build the crate's tests with it (crate::sync).
    println!("cargo::rustc-check-cfg=cfg(kani)");
    println!("cargo::rustc-check-cfg=cfg(durable_cache_loom)");
    println!("cargo::rerun-if-changed=build.rs");
    let os = std::env::var("CARGO_CFG_TARGET_OS")
        .expect("cargo sets CARGO_CFG_TARGET_OS for a build script");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH")
        .expect("cargo sets CARGO_CFG_TARGET_ARCH for a build script");
    let bindings = matches!(
        arch.as_str(),
        "x86_64" | "aarch64" | "riscv64" | "loongarch64" | "powerpc64"
    );
    if os == "linux" && bindings {
        println!("cargo::rustc-cfg=io_uring");
    }
}
