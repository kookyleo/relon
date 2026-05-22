//! v3++ b-4 Unicode combining-mark range table embedded into the
//! wasm-AOT `title` / `upper` / `lower` stdlib bodies.
//!
//! The table lists the inclusive `(start, end)` codepoint ranges of
//! the Unicode `Mark` general category (M = Mn + Mc + Me). Marks have
//! no case (Unicode treats them as non-cased), so the case-folding
//! bodies emit them verbatim. For the `title` body the additional
//! contract is that a Mark **does not** flip the `at_word_start`
//! flag — a mark belongs to its base codepoint's grapheme cluster,
//! so the next codepoint after the base+mark sequence should still
//! be treated as "after the first letter of the word" rather than
//! "first letter of a new word".
//!
//! Why hand-maintained:
//!   * `std` does not expose Unicode general-category data.
//!   * Pulling `icu_properties` (or `unicode-properties`) as a
//!     build-dep just to enumerate Marks adds a multi-MB build-tree
//!     dependency for a few hundred bytes of data.
//!   * Mark ranges are stable across Unicode revisions — new ranges
//!     get appended over time, but existing ranges never shrink, so a
//!     hand-maintained table only needs an additive update per
//!     Unicode release.
//!
//! Coverage version: **Unicode 14.0.0** (matches the same Unicode
//! revision the case-folding tables were derived against in v3+ a-4).
//! When the host toolchain bumps its bundled UCD, append any new
//! ranges to `COMBINING_MARK_RANGES` and bump the comment below.
//!
//! Layout invariants (the wasm runtime helper relies on these):
//!   * Sorted ascending by `start`.
//!   * Non-overlapping (binary search assumes `prev.end < next.start`).
//!   * `start <= end` for every range.
//!
//! The encoded byte layout the wasm body binary-searches mirrors the
//! case-folding table: a leading u32 LE count followed by
//! `count * (u32 LE start, u32 LE end)` pairs.

/// Unicode 14.0.0 Mark category ranges (Mn + Mc + Me), sorted
/// ascending by `start`, inclusive on both ends.
///
/// The list is hand-curated against the published UCD blocks. Marks
/// for scripts that ship after Unicode 14 (e.g. Tangsa post-15)
/// should be appended on the next UCD bump.
#[rustfmt::skip]
pub const COMBINING_MARK_RANGES: &[(u32, u32)] = &[
    // Combining Diacritical Marks (Mn).
    (0x0300, 0x036F),
    // Cyrillic Supplement / Cyrillic Extended-A (Mn).
    (0x0483, 0x0489),
    // Hebrew (Mn).
    (0x0591, 0x05BD),
    (0x05BF, 0x05BF),
    (0x05C1, 0x05C2),
    (0x05C4, 0x05C5),
    (0x05C7, 0x05C7),
    // Arabic (Mn + Mc).
    (0x0610, 0x061A),
    (0x064B, 0x065F),
    (0x0670, 0x0670),
    (0x06D6, 0x06DC),
    (0x06DF, 0x06E4),
    (0x06E7, 0x06E8),
    (0x06EA, 0x06ED),
    // Syriac (Mn).
    (0x0711, 0x0711),
    (0x0730, 0x074A),
    // Thaana (Mn).
    (0x07A6, 0x07B0),
    // NKo (Mn).
    (0x07EB, 0x07F3),
    (0x07FD, 0x07FD),
    // Samaritan (Mn + Mc).
    (0x0816, 0x0819),
    (0x081B, 0x0823),
    (0x0825, 0x0827),
    (0x0829, 0x082D),
    // Mandaic (Mn).
    (0x0859, 0x085B),
    // Arabic Extended-A (Mn).
    (0x08D3, 0x08E1),
    (0x08E3, 0x0902),
    // Devanagari (Mn + Mc + Me).
    (0x0903, 0x0903),
    (0x093A, 0x093A),
    (0x093B, 0x093B),
    (0x093C, 0x093C),
    (0x093E, 0x094F),
    (0x0951, 0x0957),
    (0x0962, 0x0963),
    // Bengali.
    (0x0981, 0x0983),
    (0x09BC, 0x09BC),
    (0x09BE, 0x09C4),
    (0x09C7, 0x09C8),
    (0x09CB, 0x09CD),
    (0x09D7, 0x09D7),
    (0x09E2, 0x09E3),
    (0x09FE, 0x09FE),
    // Gurmukhi.
    (0x0A01, 0x0A03),
    (0x0A3C, 0x0A3C),
    (0x0A3E, 0x0A42),
    (0x0A47, 0x0A48),
    (0x0A4B, 0x0A4D),
    (0x0A51, 0x0A51),
    (0x0A70, 0x0A71),
    (0x0A75, 0x0A75),
    // Gujarati.
    (0x0A81, 0x0A83),
    (0x0ABC, 0x0ABC),
    (0x0ABE, 0x0AC5),
    (0x0AC7, 0x0AC9),
    (0x0ACB, 0x0ACD),
    (0x0AE2, 0x0AE3),
    (0x0AFA, 0x0AFF),
    // Oriya.
    (0x0B01, 0x0B03),
    (0x0B3C, 0x0B3C),
    (0x0B3E, 0x0B44),
    (0x0B47, 0x0B48),
    (0x0B4B, 0x0B4D),
    (0x0B55, 0x0B57),
    (0x0B62, 0x0B63),
    // Tamil.
    (0x0B82, 0x0B82),
    (0x0BBE, 0x0BC2),
    (0x0BC6, 0x0BC8),
    (0x0BCA, 0x0BCD),
    (0x0BD7, 0x0BD7),
    // Telugu.
    (0x0C00, 0x0C04),
    (0x0C3C, 0x0C3C),
    (0x0C3E, 0x0C44),
    (0x0C46, 0x0C48),
    (0x0C4A, 0x0C4D),
    (0x0C55, 0x0C56),
    (0x0C62, 0x0C63),
    (0x0C81, 0x0C83),
    // Kannada.
    (0x0CBC, 0x0CBC),
    (0x0CBE, 0x0CC4),
    (0x0CC6, 0x0CC8),
    (0x0CCA, 0x0CCD),
    (0x0CD5, 0x0CD6),
    (0x0CE2, 0x0CE3),
    // Malayalam.
    (0x0D00, 0x0D03),
    (0x0D3B, 0x0D3C),
    (0x0D3E, 0x0D44),
    (0x0D46, 0x0D48),
    (0x0D4A, 0x0D4D),
    (0x0D57, 0x0D57),
    (0x0D62, 0x0D63),
    (0x0D81, 0x0D83),
    // Sinhala.
    (0x0DCA, 0x0DCA),
    (0x0DCF, 0x0DD4),
    (0x0DD6, 0x0DD6),
    (0x0DD8, 0x0DDF),
    (0x0DF2, 0x0DF3),
    // Thai.
    (0x0E31, 0x0E31),
    (0x0E34, 0x0E3A),
    (0x0E47, 0x0E4E),
    // Lao.
    (0x0EB1, 0x0EB1),
    (0x0EB4, 0x0EBC),
    (0x0EC8, 0x0ECD),
    // Tibetan.
    (0x0F18, 0x0F19),
    (0x0F35, 0x0F35),
    (0x0F37, 0x0F37),
    (0x0F39, 0x0F39),
    (0x0F3E, 0x0F3F),
    (0x0F71, 0x0F84),
    (0x0F86, 0x0F87),
    (0x0F8D, 0x0F97),
    (0x0F99, 0x0FBC),
    (0x0FC6, 0x0FC6),
    // Myanmar.
    (0x102B, 0x103E),
    (0x1056, 0x1059),
    (0x105E, 0x1060),
    (0x1062, 0x1064),
    (0x1067, 0x106D),
    (0x1071, 0x1074),
    (0x1082, 0x108D),
    (0x108F, 0x108F),
    (0x109A, 0x109D),
    // Ethiopic.
    (0x135D, 0x135F),
    // Tagalog.
    (0x1712, 0x1715),
    (0x1732, 0x1734),
    (0x1752, 0x1753),
    (0x1772, 0x1773),
    // Khmer.
    (0x17B4, 0x17D3),
    (0x17DD, 0x17DD),
    // Mongolian.
    (0x180B, 0x180D),
    (0x1885, 0x1886),
    (0x18A9, 0x18A9),
    // Limbu.
    (0x1920, 0x192B),
    (0x1930, 0x193B),
    // Buginese.
    (0x1A17, 0x1A1B),
    // Tai Tham.
    (0x1A55, 0x1A5E),
    (0x1A60, 0x1A7C),
    (0x1A7F, 0x1A7F),
    (0x1AB0, 0x1ACE),
    // Balinese.
    (0x1B00, 0x1B04),
    (0x1B34, 0x1B44),
    (0x1B6B, 0x1B73),
    (0x1B80, 0x1B82),
    (0x1BA1, 0x1BAD),
    (0x1BE6, 0x1BF3),
    // Batak / Lepcha / others.
    (0x1C24, 0x1C37),
    (0x1CD0, 0x1CD2),
    (0x1CD4, 0x1CE8),
    (0x1CED, 0x1CED),
    (0x1CF4, 0x1CF4),
    (0x1CF7, 0x1CF9),
    (0x1DC0, 0x1DFF),
    // Combining Diacritical Marks for Symbols.
    (0x20D0, 0x20F0),
    // Coptic.
    (0x2CEF, 0x2CF1),
    // Tifinagh.
    (0x2D7F, 0x2D7F),
    // Combining Half Marks.
    (0x2DE0, 0x2DFF),
    // CJK ideographic combining marks.
    (0x302A, 0x302F),
    (0x3099, 0x309A),
    // Combining Cyrillic Letter ranges (Cyrillic Extended-B).
    (0xA66F, 0xA672),
    (0xA674, 0xA67D),
    (0xA69E, 0xA69F),
    // Bamum / Syloti.
    (0xA6F0, 0xA6F1),
    (0xA802, 0xA802),
    (0xA806, 0xA806),
    (0xA80B, 0xA80B),
    (0xA823, 0xA827),
    (0xA82C, 0xA82C),
    // Saurashtra / Devanagari Extended.
    (0xA880, 0xA881),
    (0xA8B4, 0xA8C5),
    (0xA8E0, 0xA8F1),
    (0xA8FF, 0xA8FF),
    (0xA926, 0xA92D),
    (0xA947, 0xA953),
    (0xA980, 0xA983),
    (0xA9B3, 0xA9C0),
    (0xA9E5, 0xA9E5),
    (0xAA29, 0xAA36),
    (0xAA43, 0xAA43),
    (0xAA4C, 0xAA4D),
    (0xAA7B, 0xAA7D),
    (0xAAB0, 0xAAB0),
    (0xAAB2, 0xAAB4),
    (0xAAB7, 0xAAB8),
    (0xAABE, 0xAABF),
    (0xAAC1, 0xAAC1),
    (0xAAEB, 0xAAEF),
    (0xAAF5, 0xAAF6),
    (0xABE3, 0xABEA),
    (0xABEC, 0xABED),
    // Hebrew presentation forms.
    (0xFB1E, 0xFB1E),
    // Variation Selectors.
    (0xFE00, 0xFE0F),
    (0xFE20, 0xFE2F),
    // Supplementary planes (selected — common scripts that ship with
    // marks). The wasm body skips marks in the entire table; the
    // listing below covers Phoenician / Brahmic / South Asian scripts
    // that real-world inputs hit. Future Unicode bumps just append
    // here.
    (0x101FD, 0x101FD),
    (0x102E0, 0x102E0),
    (0x10376, 0x1037A),
    (0x10A01, 0x10A03),
    (0x10A05, 0x10A06),
    (0x10A0C, 0x10A0F),
    (0x10A38, 0x10A3A),
    (0x10A3F, 0x10A3F),
    (0x10AE5, 0x10AE6),
    (0x10D24, 0x10D27),
    (0x10EAB, 0x10EAC),
    (0x10F46, 0x10F50),
    (0x10F82, 0x10F85),
    (0x11000, 0x11002),
    (0x11038, 0x11046),
    (0x11070, 0x11070),
    (0x11073, 0x11074),
    (0x1107F, 0x11082),
    (0x110B0, 0x110BA),
    (0x110C2, 0x110C2),
    (0x11100, 0x11102),
    (0x11127, 0x11134),
    (0x11145, 0x11146),
    (0x11173, 0x11173),
    (0x11180, 0x11182),
    (0x111B3, 0x111C0),
    (0x111C9, 0x111CC),
    (0x111CE, 0x111CF),
    (0x1122C, 0x11237),
    (0x1123E, 0x1123E),
    (0x112DF, 0x112EA),
    (0x11300, 0x11303),
    (0x1133B, 0x1133C),
    (0x1133E, 0x11344),
    (0x11347, 0x11348),
    (0x1134B, 0x1134D),
    (0x11357, 0x11357),
    (0x11362, 0x11363),
    (0x11366, 0x1136C),
    (0x11370, 0x11374),
    (0x11435, 0x11446),
    (0x1145E, 0x1145E),
    (0x114B0, 0x114C3),
    (0x115AF, 0x115B5),
    (0x115B8, 0x115C0),
    (0x115DC, 0x115DD),
    (0x11630, 0x11640),
    (0x116AB, 0x116B7),
    (0x1171D, 0x1172B),
    (0x1182C, 0x1183A),
    (0x11930, 0x11935),
    (0x11937, 0x11938),
    (0x1193B, 0x1193E),
    (0x11940, 0x11940),
    (0x11942, 0x11943),
    (0x119D1, 0x119D7),
    (0x119DA, 0x119E0),
    (0x119E4, 0x119E4),
    (0x11A01, 0x11A0A),
    (0x11A33, 0x11A39),
    (0x11A3B, 0x11A3E),
    (0x11A47, 0x11A47),
    (0x11A51, 0x11A5B),
    (0x11A8A, 0x11A99),
    (0x11C2F, 0x11C36),
    (0x11C38, 0x11C3F),
    (0x11C92, 0x11CA7),
    (0x11CA9, 0x11CB6),
    (0x11D31, 0x11D36),
    (0x11D3A, 0x11D3A),
    (0x11D3C, 0x11D3D),
    (0x11D3F, 0x11D45),
    (0x11D47, 0x11D47),
    (0x11D8A, 0x11D8E),
    (0x11D90, 0x11D91),
    (0x11D93, 0x11D97),
    (0x11EF3, 0x11EF6),
    (0x16AF0, 0x16AF4),
    (0x16B30, 0x16B36),
    (0x16F4F, 0x16F4F),
    (0x16F51, 0x16F87),
    (0x16F8F, 0x16F92),
    (0x16FE4, 0x16FE4),
    (0x16FF0, 0x16FF1),
    (0x1BC9D, 0x1BC9E),
    (0x1CF00, 0x1CF2D),
    (0x1CF30, 0x1CF46),
    (0x1D165, 0x1D169),
    (0x1D16D, 0x1D172),
    (0x1D17B, 0x1D182),
    (0x1D185, 0x1D18B),
    (0x1D1AA, 0x1D1AD),
    (0x1D242, 0x1D244),
    (0x1DA00, 0x1DA36),
    (0x1DA3B, 0x1DA6C),
    (0x1DA75, 0x1DA75),
    (0x1DA84, 0x1DA84),
    (0x1DA9B, 0x1DA9F),
    (0x1DAA1, 0x1DAAF),
    (0x1E000, 0x1E006),
    (0x1E008, 0x1E018),
    (0x1E01B, 0x1E021),
    (0x1E023, 0x1E024),
    (0x1E026, 0x1E02A),
    (0x1E130, 0x1E136),
    (0x1E2AE, 0x1E2AE),
    (0x1E2EC, 0x1E2EF),
    (0x1E8D0, 0x1E8D6),
    (0x1E944, 0x1E94A),
    // Variation Selectors Supplement.
    (0xE0100, 0xE01EF),
];

/// Public view of the combining-mark ranges. Sorted ascending by
/// the start of each range; the wasm runtime helper depends on this
/// invariant for its binary search.
pub fn combining_mark_ranges() -> &'static [(u32, u32)] {
    COMBINING_MARK_RANGES
}

/// Encode the combining-mark range table into the wasm data-section
/// layout. Delegates to [`super::encode_u32_pair_table`].
pub fn encode_ranges_bytes(table: &[(u32, u32)]) -> Vec<u8> {
    super::encode_u32_pair_table(table)
}

/// Byte size of the encoded ranges table.
pub fn encoded_ranges_size(table: &[(u32, u32)]) -> usize {
    super::encoded_u32_pair_table_size(table.len())
}

/// Compile-time check (mirrors the runtime contract). Returns true
/// when `cp` falls inside any of the ranges in
/// [`COMBINING_MARK_RANGES`]. The wasm body uses a binary-search
/// loop instead so the per-codepoint cost stays O(log N) — this
/// helper is only used by the unit tests and the tree-walk
/// evaluator's title implementation.
pub fn is_combining_mark(cp: u32) -> bool {
    super::cp_in_ranges(cp, COMBINING_MARK_RANGES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_sorted_non_overlapping() {
        let table = COMBINING_MARK_RANGES;
        for win in table.windows(2) {
            let (_, prev_end) = win[0];
            let (next_start, next_end) = win[1];
            assert!(
                prev_end < next_start,
                "combining-mark ranges must be sorted + non-overlapping; \
                 prev_end={prev_end:#x} >= next_start={next_start:#x}"
            );
            assert!(
                next_start <= next_end,
                "range start must be <= end; got {next_start:#x}..={next_end:#x}"
            );
        }
    }

    #[test]
    fn common_combining_marks_present() {
        // U+0301 (Combining Acute Accent) is the canonical Mn example.
        assert!(is_combining_mark(0x0301));
        // U+0302 (Combining Circumflex).
        assert!(is_combining_mark(0x0302));
        // U+0308 (Combining Diaeresis).
        assert!(is_combining_mark(0x0308));
        // U+FE0F (Variation Selector-16, emoji presentation).
        assert!(is_combining_mark(0xFE0F));
        // U+200D (Zero-Width Joiner) is NOT a mark (it's a Format
        // char). Confirms the ZWJ path stays orthogonal to the
        // mark detection.
        assert!(!is_combining_mark(0x200D));
    }

    #[test]
    fn ascii_letters_not_marks() {
        for cp in 0x20u32..0x7F {
            assert!(
                !is_combining_mark(cp),
                "ascii cp {cp:#x} must not be detected as a Mark"
            );
        }
    }

    #[test]
    fn encode_ranges_layout() {
        let toy: &[(u32, u32)] = &[(0x300, 0x36F), (0x483, 0x489)];
        let bytes = encode_ranges_bytes(toy);
        assert_eq!(bytes.len(), 4 + 16);
        assert_eq!(&bytes[0..4], &2u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &0x300u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &0x36Fu32.to_le_bytes());
        assert_eq!(&bytes[12..16], &0x483u32.to_le_bytes());
        assert_eq!(&bytes[16..20], &0x489u32.to_le_bytes());
    }
}
