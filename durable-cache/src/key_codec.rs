// SPDX-FileCopyrightText: 2026 The durable-cache authors
// SPDX-License-Identifier: Apache-2.0

//! The key segment's encoding (SDS-LAYOUT.KEYSEGMENT-01): `primary`, a
//! variance flag (0 or 1), `variance` when the flag is 1, then `user_tag`
//! running to the end of the segment. No length prefix on `user_tag` — the
//! header's `key_len` already delimits the whole segment, so whatever
//! remains after the fixed part *is* the tag. Each key has exactly one
//! encoding, which is what lets a lookup compare a stored key byte for byte.

use std::mem::size_of;

use pingora_cache::key::{CompactCacheKey, HashBinary};

// IMP-LAYOUT.KEYSEGMENT-01: satisfy SDS-LAYOUT.KEYSEGMENT-01
/// The length of each hash in the segment, the primary and the variance.
pub const HASH_LEN: usize = size_of::<HashBinary>();

// The hash length is part of the on-disk format, and the one size here that
// something outside this crate could change: a `pingora-cache` release that
// resized `HashBinary` would fail the build here rather than silently change
// what is stored (FRE-COMPAT.FORMATUPGRADE-01).
const _: () = assert!(HASH_LEN == 16);

/// Write the fields in segment order, each copied once from the key into the
/// segment.
pub fn encode(key: &CompactCacheKey) -> Vec<u8> {
    let variance = key.variance.as_deref();
    // The primary hash and the one-byte variance flag, then the variance
    // hash if there is one, then the tag.
    let len = HASH_LEN + 1 + variance.map_or(0, |v| v.len()) + key.user_tag.len();
    let mut buf = Vec::with_capacity(len);
    buf.extend_from_slice(&key.primary);
    buf.push(variance.is_some().into());
    if let Some(variance) = variance {
        buf.extend_from_slice(variance);
    }
    buf.extend_from_slice(key.user_tag.as_bytes());
    buf
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum KeyDecodeError {
    #[error("key segment too short ({0} bytes)")]
    TooShort(usize),
    #[error("key segment's variance flag is {0}, neither 0 nor 1")]
    BadVarianceFlag(u8),
    #[error("stored user_tag is not valid UTF-8")]
    InvalidUserTag,
}

/// Split the segment into its fields, borrowing each from the segment, and
/// copy each once into the key it returns.
pub fn decode(buf: &[u8]) -> Result<CompactCacheKey, KeyDecodeError> {
    let too_short = || KeyDecodeError::TooShort(buf.len());
    let (primary, rest) = buf.split_first_chunk::<HASH_LEN>().ok_or_else(too_short)?;
    let (&flag, rest) = rest.split_first().ok_or_else(too_short)?;
    let (variance, tag) = match flag {
        0 => (None, rest),
        1 => {
            let (variance, tag) = rest.split_first_chunk::<HASH_LEN>().ok_or_else(too_short)?;
            (Some(Box::new(*variance)), tag)
        }
        flag => return Err(KeyDecodeError::BadVarianceFlag(flag)),
    };
    let user_tag = user_tag(tag).ok_or(KeyDecodeError::InvalidUserTag)?.into();
    Ok(CompactCacheKey {
        primary: *primary,
        variance,
        user_tag,
    })
}

/// The user tag `bytes` hold, if they are UTF-8: one function, so that the
/// Kani checks below can stand in for it.
fn user_tag(bytes: &[u8]) -> Option<&str> {
    std::str::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIMARY: HashBinary = [7; HASH_LEN];
    const VARIANCE: HashBinary = [9; HASH_LEN];
    /// Where the variance flag lies: right after the primary hash.
    const FLAG_AT: usize = HASH_LEN;
    /// Right after the one-byte flag: where the variance hash starts when
    /// the flag is 1, and the tag when it is 0.
    const AFTER_FLAG: usize = FLAG_AT + 1;

    fn key(variance: Option<HashBinary>, user_tag: &str) -> CompactCacheKey {
        CompactCacheKey {
            primary: PRIMARY,
            variance: variance.map(Box::new),
            user_tag: user_tag.into(),
        }
    }

    /// Step 1: with and without a variance hash, with an empty and a
    /// non-empty tag, a key decodes back to itself.
    #[test]
    fn tc_layout_keysegment_01_keys_round_trip() {
        for k in [
            key(None, ""),
            key(None, "tenant-42"),
            key(Some(VARIANCE), ""),
            key(Some(VARIANCE), "tenant-42"),
        ] {
            assert_eq!(decode(&encode(&k)), Ok(k));
        }
    }

    /// Step 1: each field lies where SDS-LAYOUT.KEYSEGMENT-01 puts it. Every
    /// offset follows from `HASH_LEN`, which the build pins at the SDS's 16
    /// bytes.
    #[test]
    fn tc_layout_keysegment_01_fields_lie_where_specified() {
        let with = encode(&key(Some(VARIANCE), "abc"));
        let tag_at = AFTER_FLAG + HASH_LEN;
        assert_eq!(&with[..FLAG_AT], &PRIMARY, "primary");
        assert_eq!(with[FLAG_AT], 1, "variance flag");
        assert_eq!(&with[AFTER_FLAG..tag_at], &VARIANCE, "variance");
        assert_eq!(&with[tag_at..], b"abc", "user_tag, to the end");

        let without = encode(&key(None, "abc"));
        assert_eq!(&without[..FLAG_AT], &PRIMARY, "primary");
        assert_eq!(without[FLAG_AT], 0, "variance flag");
        assert_eq!(&without[AFTER_FLAG..], b"abc", "user_tag, to the end");
    }

    /// Step 2: a segment too short for the fields its flag announces: one
    /// that ends before the flag, and one whose flag is 1 that ends a byte
    /// before the variance hash would.
    #[test]
    fn tc_layout_keysegment_01_a_short_segment_is_rejected() {
        let without = encode(&key(None, ""));
        assert_eq!(
            decode(&without[..FLAG_AT]),
            Err(KeyDecodeError::TooShort(FLAG_AT))
        );
        let with = encode(&key(Some(VARIANCE), ""));
        let cut = AFTER_FLAG + HASH_LEN - 1;
        assert_eq!(decode(&with[..cut]), Err(KeyDecodeError::TooShort(cut)));
    }

    /// Step 2: a variance flag other than 0 or 1.
    #[test]
    fn tc_layout_keysegment_01_a_flag_other_than_0_or_1_is_rejected() {
        let mut bytes = encode(&key(None, "abc"));
        bytes[FLAG_AT] = 2;
        assert_eq!(decode(&bytes), Err(KeyDecodeError::BadVarianceFlag(2)));
    }

    /// Step 2: a tag that is not UTF-8.
    #[test]
    fn tc_layout_keysegment_01_a_tag_that_is_not_utf8_is_rejected() {
        let mut bytes = encode(&key(None, ""));
        bytes.extend_from_slice(&[0xFF, 0xFE]);
        assert_eq!(decode(&bytes), Err(KeyDecodeError::InvalidUserTag));
    }
}

/// The key segment for every input up to a bound, with Kani
/// (TC-LAYOUT.FORMATS-01). Built only by Kani, which sets `cfg(kani)`.
#[cfg(kani)]
mod kani_checks {
    use super::*;

    // IMP-TC.LAYOUT.FORMATS-01: satisfy TC-LAYOUT.FORMATS-01

    /// The longest segment the checks try: a variance and a user tag of four
    /// bytes.
    const MAX_SEGMENT: usize = HASH_LEN + 1 + HASH_LEN + 4;

    /// A stand-in for [`user_tag`]: it accepts the ASCII tags, as
    /// `std::str::from_utf8` does, and rejects every other. Kani cannot
    /// follow `from_utf8` over bytes whose values or addresses it does not
    /// know, so the checks cover the ASCII tags, and the rest of UTF-8 is
    /// the standard library's.
    fn ascii_user_tag(bytes: &[u8]) -> Option<&str> {
        if bytes.iter().all(|byte| *byte < 0x80) {
            // SAFETY: ASCII bytes are UTF-8.
            Some(unsafe { std::str::from_utf8_unchecked(bytes) })
        } else {
            None
        }
    }

    /// Decoding the encoding of a key with every primary hash, `variance`
    /// and `user_tag` gives the key back.
    fn round_trips(variance: Option<HashBinary>, user_tag: &str) {
        let key = CompactCacheKey {
            primary: kani::any(),
            variance: variance.map(Box::new),
            user_tag: String::from(user_tag).into_boxed_str(),
        };
        let decoded = decode(&encode(&key));
        assert!(decoded == Ok(key));
    }

    // The round trip, for every primary hash, without a variance and with
    // every one, one case to a harness, with a user tag of one character.
    // Kani follows `std::str::from_utf8` over one known byte, but not over
    // many, nor over bytes it does not know, nor in a harness that has
    // freed and allocated memory before, as two cases in one harness would;
    // and it does not finish with an empty tag. The empty tag's decoding is
    // in the check of every segment below, and in the tests.

    /// No variance.
    #[kani::proof]
    fn tc_layout_formats_01_a_key_round_trips() {
        round_trips(None, "a");
    }

    /// Every variance.
    #[kani::proof]
    fn tc_layout_formats_01_a_key_with_a_variance_round_trips() {
        round_trips(Some(kani::any()), "a");
    }

    /// Any segment of up to [`MAX_SEGMENT`] bytes decodes or fails, without
    /// a panic, and one that decodes is the encoding of the key it decodes
    /// to: each key has exactly one encoding.
    #[kani::proof]
    #[kani::stub(user_tag, ascii_user_tag)]
    #[kani::unwind(40)]
    fn tc_layout_formats_01_any_segment_decodes_or_fails() {
        let bytes: [u8; MAX_SEGMENT] = kani::any();
        let len: usize = kani::any_where(|len| *len <= MAX_SEGMENT);
        let segment = &bytes[..len];
        if let Ok(key) = decode(segment) {
            assert!(encode(&key) == segment);
        }
    }
}
