// SPDX-FileCopyrightText: 2026 The HttpCacheStorage authors
// SPDX-License-Identifier: Apache-2.0

//! `httpcachestorage-driver OUT_DIR [SEEDS] [CALLS]`: drive `MemCache`, the
//! in-memory storage of the `pingora-cache` crate, through one workload for
//! each seed from 0 to SEEDS (8 by default), and write each trace to
//! `OUT_DIR/memcache-<seed>.jsonl` for `check-trace`.

use std::fs::File;
use std::io::BufWriter;
use std::path::PathBuf;

use httpcachestorage_driver::{run, Allow, Options};
use pingora_cache::MemCache;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(
        args.next()
            .expect("usage: httpcachestorage-driver OUT_DIR [SEEDS] [CALLS]"),
    );
    let seeds: u64 = args
        .next()
        .map_or(8, |s| s.parse().expect("SEEDS is a number"));
    let calls: Option<usize> = args.next().map(|s| s.parse().expect("CALLS is a number"));
    std::fs::create_dir_all(&out)?;
    for seed in 0..seeds {
        // `Storage` takes `&'static self`, so each run's storage lives on.
        let storage: &'static MemCache = Box::leak(Box::new(MemCache::new()));
        let mut opts = Options::new(seed, Allow::MEMCACHE);
        if let Some(calls) = calls {
            opts.calls = calls;
        }
        let path = out.join(format!("memcache-{seed}.jsonl"));
        let summary = run(storage, &opts, BufWriter::new(File::create(&path)?)).await?;
        println!(
            "{}: {} calls, {} hits, {} stores",
            path.display(),
            summary.calls,
            summary.hits,
            summary.stored
        );
    }
    Ok(())
}
