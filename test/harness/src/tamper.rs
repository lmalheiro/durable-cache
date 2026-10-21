// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Direct filesystem tamper helpers shared by Group C and Group D tests:
//! byte-flip/truncate an object file at exact offsets from the record
//! layout (`durable_cache::test_support`).

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use anyhow::Result;
use durable_cache::test_support::HEADER_LEN;

/// Flip one byte at `offset` within the fixed record header.
pub fn flip_header_byte(path: &Path, offset: usize) -> Result<()> {
    assert!(offset < HEADER_LEN, "offset must be inside the header");
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut byte = [0u8; 1];
    file.seek(SeekFrom::Start(offset as u64))?;
    std::io::Read::read_exact(&mut file, &mut byte)?;
    byte[0] ^= 0xFF;
    file.seek(SeekFrom::Start(offset as u64))?;
    file.write_all(&byte)?;
    Ok(())
}

/// Truncate a file to `new_len` bytes, shorter than its recorded extent —
/// simulating a torn write or a crash mid-flush.
pub fn truncate_to(path: &Path, new_len: u64) -> Result<()> {
    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(new_len)?;
    Ok(())
}
