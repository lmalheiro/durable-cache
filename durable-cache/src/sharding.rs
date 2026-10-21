// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! Two-level shard directory tree (SDS-LAYOUT.SHARDING-01).
//!
//! An object's file lives at `<root>/<name[0..2]>/<name[2..4]>/<name>`,
//! where `name` is [`object_name`]: the hex-encoded combined hash of its
//! cache key (`CacheHashKey::combined_bin()`), or, for a key with a user
//! tag, a hash of that hash and the tag. Both `CacheKey` (lookup/store) and
//! `CompactCacheKey` (purge) carry the hash and the tag, so the two key
//! forms resolve to one path — TC-LAYOUT.SHARDING-01 requires them to agree.

use std::mem::size_of;
use std::path::{Path, PathBuf};

use blake2::digest::consts::U16;
use blake2::{Blake2b, Digest};
use pingora_cache::key::HashBinary;

// IMP-LAYOUT.SHARDING-01: satisfy SDS-LAYOUT.SHARDING-01

/// Hex digits naming one directory level.
const LEVEL_DIGITS: usize = 2;
/// The leading digits of a name that pick its shard: the first level's,
/// then the second level's.
const PREFIX_LEN: usize = LEVEL_DIGITS + LEVEL_DIGITS;
/// The base names are written in: hex.
pub const RADIX: u32 = 16;
/// A name's length: its 128-bit hash in hex, two digits per byte.
const NAME_LEN: usize = 2 * size_of::<HashBinary>();

/// The possible names of a shard path's first-level directory.
pub const FIRST_LEVEL_COUNT: usize = (RADIX as usize).pow(LEVEL_DIGITS as u32);
/// The possible names of a shard path's second-level directory, within one
/// first-level directory.
pub const SECOND_LEVEL_COUNT: usize = (RADIX as usize).pow(LEVEL_DIGITS as u32);
/// Total number of leaf shard directories.
pub const SHARD_COUNT: usize = FIRST_LEVEL_COUNT * SECOND_LEVEL_COUNT;

// The scheme's numbers as SDS-LAYOUT.SHARDING-01 states them: a change to
// the constants above that moved one fails the build.
const _: () = {
    assert!(FIRST_LEVEL_COUNT == 256);
    assert!(SECOND_LEVEL_COUNT == 256);
    assert!(SHARD_COUNT == 65_536);
    assert!(NAME_LEN == 32);
};

/// Blake2b with a 128-bit digest, the hash `pingora-cache` names keys by.
type Blake2b128 = Blake2b<U16>;

/// The name of a key's files, given the key's hash and its user tag.
///
/// A user tag is part of an object's identity, as the Pingora eviction
/// managers count it, but Pingora hashes only a key's primary bytes. So an
/// untagged key is named by its hash in hex, and a tagged key by Blake2b-128
/// over the hash followed by the tag, in hex: keys that differ only in their
/// tag name different files. The hash has a fixed length and comes first,
/// so the hashed input can be read only one way.
pub fn object_name(hash: HashBinary, user_tag: &str) -> String {
    if user_tag.is_empty() {
        return hex::encode(hash);
    }
    hex::encode(
        Blake2b128::new_with_prefix(hash)
            .chain_update(user_tag)
            .finalize(),
    )
}

/// A name that cannot be split into a shard path.
///
/// Every name this backend derives is 32 hex characters, so neither variant
/// is reachable through this crate's own call paths today. They exist so
/// that a future caller passing something else -- or a shard constant above
/// drifting -- surfaces as an ordinary error the backend reports as a cache
/// miss, rather than as a panic that would take the hosting proxy down with
/// it.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ShardKeyError {
    #[error("name must be at least {} hex characters, got {:?}", PREFIX_LEN, .0)]
    TooShort(String),
    #[error("name must start with {} hex characters, got {:?}", PREFIX_LEN, .0)]
    NotHex(String),
}

/// The leading digits of a name, which pick its shard, validated as ASCII
/// hex. This is the single place a name is sliced, so every other function
/// here is total by construction.
fn shard_prefix(name: &str) -> Result<&str, ShardKeyError> {
    if name.len() < PREFIX_LEN {
        return Err(ShardKeyError::TooShort(name.to_owned()));
    }
    // `str::get` gives `None` when a multi-byte character straddles the
    // prefix's end, where a `&name[..PREFIX_LEN]` slice would panic.
    match name.get(..PREFIX_LEN) {
        Some(prefix) if prefix.bytes().all(|b| b.is_ascii_hexdigit()) => Ok(prefix),
        _ => Err(ShardKeyError::NotHex(name.to_owned())),
    }
}

/// Split a name into its two shard-directory components.
fn split_shard(name: &str) -> Result<(&str, &str), ShardKeyError> {
    Ok(shard_prefix(name)?.split_at(LEVEL_DIGITS))
}

/// The second-level shard *directory* holding an object and any of its
/// per-writer temporaries: `<root>/<name[0..2]>/<name[2..4]>`.
pub fn shard_dir(root: &Path, name: &str) -> Result<PathBuf, ShardKeyError> {
    let (first, second) = split_shard(name)?;
    Ok(root.join(first).join(second))
}

/// Build the path of the object named `name`.
pub fn shard_path(root: &Path, name: &str) -> Result<PathBuf, ShardKeyError> {
    Ok(shard_dir(root, name)?.join(name))
}

/// Build a per-writer temporary's path in the same shard directory as its
/// eventual canonical name (FDS-STORAGE.PUBLISH-01): the temporary is never
/// staged elsewhere, so the publishing rename is atomic by construction.
///
/// Built from [`shard_dir`] directly rather than from the canonical path's
/// `file_name()`, so there is no `Option` to unwrap on the way.
pub fn temp_path(
    root: &Path,
    name: &str,
    writer: &crate::writerid::WriterId,
    counter: u64,
) -> Result<PathBuf, ShardKeyError> {
    let temp_name = crate::writerid::temp_name(name, writer, counter);
    Ok(shard_dir(root, name)?.join(temp_name))
}

/// The first-level shard directory numbered `level` (`0..FIRST_LEVEL_COUNT`):
/// `<root>/<level, in LEVEL_DIGITS hex digits>`.
pub fn first_level_dir(root: &Path, level: usize) -> PathBuf {
    root.join(format!("{level:0LEVEL_DIGITS$x}"))
}

/// Every first-level shard directory under `root`, in order, whether it
/// exists or not: where the restart scan looks.
pub fn first_level_dirs(root: &Path) -> impl Iterator<Item = PathBuf> + '_ {
    (0..FIRST_LEVEL_COUNT).map(move |level| first_level_dir(root, level))
}

/// The number a shard directory's name stands for, or `None` if the name is
/// not one: exactly `LEVEL_DIGITS` lowercase hex digits, as this module
/// writes them. Uppercase and a leading sign, which `from_str_radix` alone
/// would accept, are not.
pub fn level_index(name: &str) -> Option<usize> {
    if name.len() != LEVEL_DIGITS || !name.bytes().all(is_name_digit) {
        return None;
    }
    usize::from_str_radix(name, RADIX).ok()
}

/// The flat shard index (`0..SHARD_COUNT`) of the second-level directory
/// numbered `second` inside the first-level one numbered `first`: the index
/// [`shard_index`] gives every name in that directory.
pub fn shard_index_of(first: usize, second: usize) -> usize {
    first * SECOND_LEVEL_COUNT + second
}

/// Whether a directory entry's name is a canonical object name: exactly the
/// lowercase hex of a name this backend derives, and nothing else.
/// Temporaries carry further dot-separated components, and anything else in
/// the tree is not this backend's, so the restart scan leaves it alone
/// (FDS-STORAGE.RECLAIM-01 classifies by name first).
pub fn is_canonical_name(name: &str) -> bool {
    name.len() == NAME_LEN && name.bytes().all(is_name_digit)
}

/// A character of the names this module writes: a lowercase hex digit.
fn is_name_digit(b: u8) -> bool {
    matches!(b, b'0'..=b'9' | b'a'..=b'f')
}

/// Compute the flat shard index (`0..SHARD_COUNT`) for a name, used by
/// [`crate::dircache::ShardDirCache`]'s bitset. Each level is
/// `LEVEL_DIGITS` hex digits, so the shard prefix read as one number is
/// already `first * SECOND_LEVEL_COUNT + second`.
pub fn shard_index(name: &str) -> Result<usize, ShardKeyError> {
    usize::from_str_radix(shard_prefix(name)?, RADIX)
        .map_err(|_| ShardKeyError::NotHex(name.to_owned()))
}

#[cfg(test)]
mod tests {
    use pingora_cache::key::{CacheHashKey, CacheKey};

    use super::*;

    /// A name's path: its first two hex characters name the first-level
    /// directory, the next two the second-level one, and the file keeps the
    /// whole name (SDS-LAYOUT.SHARDING-01).
    #[test]
    fn splits_on_first_four_hex_chars() {
        let root = Path::new("/cache");
        let path = shard_path(root, "a1b2c3d4e5f6").unwrap();
        assert_eq!(path, Path::new("/cache/a1/b2/a1b2c3d4e5f6"));
    }

    /// A name's shard index identifies the directory `shard_dir` gives it:
    /// the index divided by the second level's count is the first-level
    /// directory, and the remainder the second-level one. `dircache.rs`
    /// relies on this, since its bitset records a directory's existence
    /// under the index. The first and last names take the first and last
    /// index, and in `0aa0` the two levels differ, so a mix-up of them would
    /// show.
    #[test]
    fn shard_index_numbers_the_directory_shard_dir_names() {
        let root = Path::new("/c");
        let first_name = "0000ffffffffffffffffffffffffffff";
        let last_name = "ffff0000000000000000000000000000";
        let names = [
            first_name,
            "0aa05555555555555555555555555555",
            "a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6",
            last_name,
        ];
        for name in names {
            assert!(is_canonical_name(name), "{name} is shaped like a real name");
            let index = shard_index(name).unwrap();
            let (first, second) = (index / SECOND_LEVEL_COUNT, index % SECOND_LEVEL_COUNT);
            assert_eq!(
                shard_dir(root, name).unwrap(),
                root.join(format!("{first:0LEVEL_DIGITS$x}"))
                    .join(format!("{second:0LEVEL_DIGITS$x}")),
                "{name}"
            );
            assert_eq!(shard_index_of(first, second), index, "{name}");
        }
        assert_eq!(shard_index(first_name).unwrap(), 0);
        assert_eq!(shard_index(last_name).unwrap(), SHARD_COUNT - 1);
    }

    /// A name whose first four characters are missing or not hex is an
    /// error, never a panic: the backend reports it as a miss rather than
    /// take the proxy down. The cases include two inputs a naive version
    /// would get wrong: a sign, which `from_str_radix` accepts, and a
    /// multi-byte character across the prefix's end, where a `str` slice
    /// panics.
    #[test]
    fn a_prefix_that_is_not_four_hex_digits_is_an_error_never_a_panic() {
        let too_short = |h: &str| ShardKeyError::TooShort(h.to_owned());
        let not_hex = |h: &str| ShardKeyError::NotHex(h.to_owned());
        assert_eq!(shard_index("a1b"), Err(too_short("a1b")));
        assert_eq!(shard_index("a1g2c3"), Err(not_hex("a1g2c3")));
        // A sign `from_str_radix` alone would accept.
        assert_eq!(shard_index("+a1bc3"), Err(not_hex("+a1bc3")));
        // A multi-byte character across the prefix's end: a `str` slice
        // there would panic.
        assert_eq!(shard_index("a1bé"), Err(not_hex("a1bé")));
        assert_eq!(shard_path(Path::new("/c"), "a1b"), Err(too_short("a1b")));
    }

    /// The restart scan's walk: `first_level_dirs` yields every first-level
    /// directory, from `00` to `ff`, in order.
    #[test]
    fn first_level_dirs_name_every_first_level_directory() {
        let dirs: Vec<PathBuf> = first_level_dirs(Path::new("/c")).collect();
        assert_eq!(dirs.len(), FIRST_LEVEL_COUNT);
        assert_eq!(dirs[0], Path::new("/c/00"));
        assert_eq!(dirs[0xa1], Path::new("/c/a1"));
        assert_eq!(dirs[FIRST_LEVEL_COUNT - 1], Path::new("/c/ff"));
    }

    /// A shard directory's name stands for its number only if it is
    /// exactly `LEVEL_DIGITS` lowercase hex digits, as `first_level_dir` and
    /// `shard_dir` write it. Anything else is not a shard directory: too
    /// short or too long, uppercase, not hex, or with a sign that
    /// `from_str_radix` alone would accept.
    #[test]
    fn a_level_name_is_exactly_its_hex_digits() {
        assert_eq!(level_index("00"), Some(0));
        assert_eq!(level_index("a1"), Some(0xa1));
        assert_eq!(level_index("ff"), Some(FIRST_LEVEL_COUNT - 1));
        for name in ["a", "a1b", "B2", "g1", "+f", "-1", "é"] {
            assert_eq!(level_index(name), None, "{name:?}");
        }
        assert_eq!(first_level_dir(Path::new("/c"), 0xa1), Path::new("/c/a1"));
    }

    /// The restart scan reads a file as a record only if its name has the
    /// exact shape of the names the backend derives: 32 lowercase hex
    /// characters. Anything else is left alone, whether one character short
    /// or long, uppercase, a short hex name, or a temporary's name.
    #[test]
    fn a_canonical_name_is_exactly_a_derived_name() {
        let name = object_name([0xab; size_of::<HashBinary>()], "");
        assert!(is_canonical_name(&name));
        assert!(!is_canonical_name(&name[..NAME_LEN - 1]), "one short");
        assert!(!is_canonical_name(&format!("{name}0")), "one long");
        assert!(!is_canonical_name(&name.to_uppercase()), "uppercase");
        assert!(!is_canonical_name("beef"), "a short hex name");
        let temp = format!("{name}.4711-6fd5a44b00000001.17.tmp");
        assert!(!is_canonical_name(&temp), "a temporary");
    }

    /// An untagged key is named by the combined hash Pingora computes: a
    /// user tag changes the name only when there is one.
    #[test]
    fn an_untagged_key_is_named_by_pingoras_combined_hash() {
        let key = CacheKey::new("/one-url", "");
        assert_eq!(
            object_name(key.combined_bin(), key.user_tag()),
            key.combined()
        );
    }

    /// Pins the naming rule to exact values from an independent
    /// implementation. File names are part of the on-disk format: if a
    /// change to `object_name`, or to how Pingora hashes keys, gave a key a
    /// different name, every object already stored would sit under a name
    /// no lookup asks for. The expected values were computed with Python's
    /// `hashlib`: `blake2b(b"/one-url", digest_size=16)` for the untagged
    /// name, then `blake2b(<that hash> + b"tenant-a", digest_size=16)` for
    /// the tagged one.
    #[test]
    fn names_match_independently_computed_values() {
        let hash = CacheKey::new("/one-url", "").combined_bin();
        assert_eq!(object_name(hash, ""), "94aece64d92034cb8e198ffa39d92234");
        assert_eq!(
            object_name(hash, "tenant-a"),
            "bc229d520f782410d82ce3d205919cf2"
        );
    }

    /// A tagged key's name differs from the untagged key's and from another
    /// tag's, is the same every time, and has the shape of any other name
    /// (SDS-LAYOUT.SHARDING-01, "User tags").
    #[test]
    fn a_user_tag_gives_a_name_of_its_own() {
        let hash = CacheKey::new("/one-url", "").combined_bin();
        let a = object_name(hash, "tenant-a");
        assert_ne!(a, object_name(hash, ""), "differs from the untagged name");
        assert_ne!(a, object_name(hash, "tenant-b"), "differs per tag");
        assert_eq!(a, object_name(hash, "tenant-a"), "is stable");
        assert!(is_canonical_name(&a), "is shaped like any other name");
    }
}
