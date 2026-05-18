//! v3++ b-4 Unicode whitespace range table embedded into the wasm-AOT
//! `title` stdlib body.
//!
//! Mirrors the contract of [`crate::combining_marks`] but for the
//! Unicode `White_Space` property — the set of codepoints
//! [`char::is_whitespace`] returns `true` for. The `title` body uses
//! this table to decide whether a decoded codepoint resets the
//! `at_word_start` flag.
//!
//! ASCII whitespace (`0x09..=0x0D` + `0x20`) is special-cased on the
//! wasm fast path so the common case stays branch-light; only the
//! non-ASCII residue is binary-searched here.
//!
//! Layout invariants:
//!   * Sorted ascending by `start`.
//!   * Non-overlapping (`prev.end < next.start`).
//!   * `start <= end`.
//!
//! Encoded byte layout: same shape as the case-folding / combining-
//! mark tables — a leading u32 LE count followed by `(start, end)`
//! pairs as `(u32 LE, u32 LE)`. The runtime helper reuses the same
//! `(table_addr + 4 + mid * 8)` rebase arithmetic.

/// Unicode 14.0.0 non-ASCII whitespace codepoint ranges, sorted
/// ascending. ASCII whitespace is intentionally excluded — the wasm
/// fast path covers it via a direct comparison and never hits this
/// table.
#[rustfmt::skip]
pub const NON_ASCII_WHITESPACE_RANGES: &[(u32, u32)] = &[
    // Next Line (NEL) — White_Space per Unicode.
    (0x0085, 0x0085),
    // No-Break Space.
    (0x00A0, 0x00A0),
    // Ogham Space Mark.
    (0x1680, 0x1680),
    // En Quad through Hair Space.
    (0x2000, 0x200A),
    // Line Separator.
    (0x2028, 0x2028),
    // Paragraph Separator.
    (0x2029, 0x2029),
    // Narrow No-Break Space.
    (0x202F, 0x202F),
    // Medium Mathematical Space.
    (0x205F, 0x205F),
    // Ideographic Space.
    (0x3000, 0x3000),
];

/// Public view of the non-ASCII whitespace ranges. Sorted ascending
/// by the start of each range; the wasm runtime helper depends on
/// the sort order for its binary search.
pub fn non_ascii_whitespace_ranges() -> &'static [(u32, u32)] {
    NON_ASCII_WHITESPACE_RANGES
}

/// Encode the whitespace range table into the raw byte layout the
/// wasm data section expects. See [`crate::combining_marks::encode_ranges_bytes`]
/// for the wire format — both helpers emit the same `[count: u32][(start, end) × N]`
/// shape so the runtime can binary-search them with one shared op
/// stream.
pub fn encode_ranges_bytes(table: &[(u32, u32)]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + table.len() * 8);
    bytes.extend_from_slice(&(table.len() as u32).to_le_bytes());
    for (s, e) in table {
        bytes.extend_from_slice(&s.to_le_bytes());
        bytes.extend_from_slice(&e.to_le_bytes());
    }
    bytes
}

/// Byte size of the encoded whitespace ranges. Mirrors
/// [`crate::combining_marks::encoded_ranges_size`].
pub fn encoded_ranges_size(table: &[(u32, u32)]) -> usize {
    4 + table.len() * 8
}

/// Compile-time check used by the tree-walk evaluator (the wasm body
/// performs the equivalent decision via the binary-searched helper).
/// Returns `true` when `cp` is in the Unicode `White_Space`
/// property set — ASCII fast path plus non-ASCII ranges.
pub fn is_unicode_whitespace(cp: u32) -> bool {
    // ASCII fast path: 0x09..=0x0D (HT/LF/VT/FF/CR) and 0x20 (SPACE).
    if (0x09..=0x0D).contains(&cp) || cp == 0x20 {
        return true;
    }
    NON_ASCII_WHITESPACE_RANGES
        .binary_search_by(|(s, e)| {
            if cp < *s {
                std::cmp::Ordering::Greater
            } else if cp > *e {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_sorted_non_overlapping() {
        for win in NON_ASCII_WHITESPACE_RANGES.windows(2) {
            let (_, prev_end) = win[0];
            let (next_start, next_end) = win[1];
            assert!(prev_end < next_start);
            assert!(next_start <= next_end);
        }
    }

    #[test]
    fn ascii_whitespace_detected() {
        for cp in [0x09u32, 0x0A, 0x0B, 0x0C, 0x0D, 0x20] {
            assert!(is_unicode_whitespace(cp));
        }
    }

    #[test]
    fn non_ascii_whitespace_detected() {
        for cp in [
            0x00A0u32, 0x1680, 0x2000, 0x200A, 0x2028, 0x2029, 0x202F, 0x205F, 0x3000,
        ] {
            assert!(is_unicode_whitespace(cp), "cp {cp:#x} should be whitespace");
        }
    }

    #[test]
    fn letters_not_whitespace() {
        for cp in [b'a' as u32, b'Z' as u32, 0x00E9, 0x4F60, 0x1F30D] {
            assert!(
                !is_unicode_whitespace(cp),
                "cp {cp:#x} must not be whitespace"
            );
        }
    }

    #[test]
    fn matches_rust_char_is_whitespace() {
        // The wasm body promises to mirror char::is_whitespace for
        // every BMP codepoint. Sanity-check the non-BMP boundary
        // separately (no astral whitespace exists in Unicode 14).
        for cp in 0u32..=0xFFFF {
            if let Some(ch) = char::from_u32(cp) {
                assert_eq!(
                    is_unicode_whitespace(cp),
                    ch.is_whitespace(),
                    "mismatch at cp {cp:#x}"
                );
            }
        }
    }
}
