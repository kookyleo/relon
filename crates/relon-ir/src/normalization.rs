//! Unicode normalization (UAX #15).
//!
//! v3++ b-5: implements the four standard normalization forms - NFC,
//! NFD, NFKC, NFKD - directly against the embedded UCD 14.0.0 tables
//! in [`crate::normalization_data`]. The implementation is intentionally
//! third-party-free so:
//!
//!   * Both the tree-walk evaluator and the wasm-AOT backend share
//!     **one** dataset and one algorithm, avoiding silent drift
//!     between executors.
//!   * Bumping the Unicode version is a single regenerate-and-commit
//!     step (see `tools/gen_normalization_tables.py`).
//!
//! The four entry points ([`to_nfd`], [`to_nfkd`], [`to_nfc`],
//! [`to_nfkc`]) all return owned [`String`]s. Hangul syllables are
//! decomposed / composed algorithmically per UAX #15 section 16 -
//! keeping them in the data tables would cost ~88 KB for the syllable
//! block alone with no performance gain.
//!
//! ### Algorithm sketch
//!
//! * **NFD**:  decode each `char` -> recursive canonical decomposition
//!   (data table + Hangul algorithm) -> canonical reorder (stable sort
//!   on CCC within each non-starter run) -> re-encode.
//! * **NFKD**: same as NFD but using the compatibility table.
//! * **NFC**:  run NFD, then a single left-to-right composition pass
//!   that pairs each starter with subsequent characters via
//!   `COMPOSITION_PAIRS` plus the algorithmic Hangul composer.
//! * **NFKC**: run NFKD, then the same composition pass.
//!
//! Excluded composites (`Full_Composition_Exclusion` plus the explicit
//! `CompositionExclusions.txt` list) are absent from
//! `COMPOSITION_PAIRS` at generation time, so the composition pass
//! never needs to consult an exclusion table at runtime.

use crate::normalization_data::{
    CCC_TABLE, COMPOSITION_PAIRS, NFD_INDEX, NFD_POOL, NFKD_INDEX, NFKD_POOL,
};

// Hangul syllable algorithm constants (UAX #15 section 16).
/// First precomposed Hangul syllable (U+AC00).
pub const HANGUL_S_BASE: u32 = 0xAC00;
/// First Hangul leading consonant jamo (U+1100).
pub const HANGUL_L_BASE: u32 = 0x1100;
/// First Hangul vowel jamo (U+1161).
pub const HANGUL_V_BASE: u32 = 0x1161;
/// Hangul trailing-consonant filler (T_BASE itself never composes; the
/// real trailing jamo range is `T_BASE + 1 ..= T_BASE + T_COUNT - 1`).
pub const HANGUL_T_BASE: u32 = 0x11A7;
/// Count of leading-consonant jamos.
pub const HANGUL_L_COUNT: u32 = 19;
/// Count of vowel jamos.
pub const HANGUL_V_COUNT: u32 = 21;
/// Count of trailing-consonant jamos (including the filler at offset 0).
pub const HANGUL_T_COUNT: u32 = 28;
/// `HANGUL_V_COUNT * HANGUL_T_COUNT` — block size per leading jamo.
pub const HANGUL_N_COUNT: u32 = HANGUL_V_COUNT * HANGUL_T_COUNT; // 588
/// Total count of precomposed Hangul syllables.
pub const HANGUL_S_COUNT: u32 = HANGUL_L_COUNT * HANGUL_N_COUNT; // 11172

/// Canonical_Combining_Class for `cp`. Returns 0 for any code point
/// not present in [`CCC_TABLE`] - the table only stores non-zero
/// classes (Not_Reordered is the default).
#[inline]
pub fn ccc(cp: u32) -> u8 {
    match CCC_TABLE.binary_search_by_key(&cp, |entry| entry.0) {
        Ok(idx) => CCC_TABLE[idx].1,
        Err(_) => 0,
    }
}

/// Look up the canonical decomposition of `cp` in [`NFD_INDEX`] /
/// [`NFD_POOL`]. Returns `None` if `cp` has no canonical
/// decomposition.
#[inline]
pub fn nfd_lookup(cp: u32) -> Option<&'static [u32]> {
    let idx = NFD_INDEX.binary_search_by_key(&cp, |entry| entry.0).ok()?;
    let (_, off, len) = NFD_INDEX[idx];
    let start = off as usize;
    let end = start + len as usize;
    Some(&NFD_POOL[start..end])
}

/// Compatibility analog of [`nfd_lookup`]. Falls back to the canonical
/// entry when no compatibility mapping exists (the generator script
/// duplicates canonical-only entries into NFKD as well).
#[inline]
pub fn nfkd_lookup(cp: u32) -> Option<&'static [u32]> {
    let idx = NFKD_INDEX.binary_search_by_key(&cp, |entry| entry.0).ok()?;
    let (_, off, len) = NFKD_INDEX[idx];
    let start = off as usize;
    let end = start + len as usize;
    Some(&NFKD_POOL[start..end])
}

/// Composition pair lookup: `(first, second) -> composed`. Returns
/// `None` when no canonical composition exists or when the composite
/// is on the exclusion list (filtered out at table-generation time,
/// so the runtime never re-checks).
#[inline]
pub fn compose_pair(first: u32, second: u32) -> Option<u32> {
    let idx = COMPOSITION_PAIRS
        .binary_search_by(|entry| (entry.0, entry.1).cmp(&(first, second)))
        .ok()?;
    Some(COMPOSITION_PAIRS[idx].2)
}

/// Algorithmic Hangul decomposition. Returns the L / V (/ optional T)
/// jamo sequence in `out` when `cp` is in the syllable block, or
/// `false` if `cp` is not a precomposed Hangul syllable.
#[inline]
pub fn hangul_decompose_into(cp: u32, out: &mut Vec<u32>) -> bool {
    if !(HANGUL_S_BASE..HANGUL_S_BASE + HANGUL_S_COUNT).contains(&cp) {
        return false;
    }
    let s_index = cp - HANGUL_S_BASE;
    let l = HANGUL_L_BASE + s_index / HANGUL_N_COUNT;
    let v = HANGUL_V_BASE + (s_index % HANGUL_N_COUNT) / HANGUL_T_COUNT;
    let t_offset = s_index % HANGUL_T_COUNT;
    out.push(l);
    out.push(v);
    if t_offset != 0 {
        out.push(HANGUL_T_BASE + t_offset);
    }
    true
}

/// Algorithmic Hangul composition. Tries L + V (and optionally + T)
/// -> precomposed syllable. Returns `None` when the pair is not a
/// valid jamo pairing.
#[inline]
pub fn hangul_compose(first: u32, second: u32) -> Option<u32> {
    // L + V -> LV syllable.
    if (HANGUL_L_BASE..HANGUL_L_BASE + HANGUL_L_COUNT).contains(&first)
        && (HANGUL_V_BASE..HANGUL_V_BASE + HANGUL_V_COUNT).contains(&second)
    {
        let l_index = first - HANGUL_L_BASE;
        let v_index = second - HANGUL_V_BASE;
        return Some(HANGUL_S_BASE + (l_index * HANGUL_V_COUNT + v_index) * HANGUL_T_COUNT);
    }
    // LV + T -> LVT syllable. We detect "LV-shaped" by checking
    // `(cp - S_BASE) % T_COUNT == 0` - that's exactly the precomposed
    // LV syllables. T_BASE itself is the filler; skip it.
    if (HANGUL_S_BASE..HANGUL_S_BASE + HANGUL_S_COUNT).contains(&first) {
        let s_index = first - HANGUL_S_BASE;
        if s_index.is_multiple_of(HANGUL_T_COUNT)
            && (HANGUL_T_BASE + 1..HANGUL_T_BASE + HANGUL_T_COUNT).contains(&second)
        {
            return Some(first + (second - HANGUL_T_BASE));
        }
    }
    None
}

/// Mode flag for [`decompose_to_buffer`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecompKind {
    /// Canonical decomposition (NFD / NFC source pass).
    Canonical,
    /// Compatibility decomposition (NFKD / NFKC source pass).
    Compatibility,
}

/// Decompose `input` into `out` using the requested table. The payload
/// tables are already fully expanded (the generator script flattens
/// nested decompositions), so a single lookup per code point is
/// sufficient - no recursion needed at runtime.
pub fn decompose_to_buffer(input: &str, kind: DecompKind, out: &mut Vec<u32>) {
    // Worst-case expansion factor across all Unicode 14.0 mappings is
    // 18 (U+FDFA -> 18 cps). Reserve roughly that to keep reallocs
    // off the hot path for compatibility decomposition; canonical
    // expansion stays bounded around 4.
    out.reserve(input.len() * 2);
    for ch in input.chars() {
        let cp = ch as u32;
        if hangul_decompose_into(cp, out) {
            continue;
        }
        let mapping = match kind {
            DecompKind::Canonical => nfd_lookup(cp),
            DecompKind::Compatibility => nfkd_lookup(cp),
        };
        match mapping {
            Some(slice) => out.extend_from_slice(slice),
            None => out.push(cp),
        }
    }
}

/// Canonical reorder pass (UAX #15 D109): within every run of
/// non-starters (CCC > 0) sort code points by CCC ascending, stably.
/// Starters (CCC == 0) are anchors that break runs.
pub fn canonical_reorder(buf: &mut [u32]) {
    let len = buf.len();
    let mut i = 0;
    while i < len {
        if ccc(buf[i]) == 0 {
            i += 1;
            continue;
        }
        let start = i;
        while i < len && ccc(buf[i]) != 0 {
            i += 1;
        }
        // `sort_by_key` is stable in std, which matters: same-CCC code
        // points must keep their original order or Quick_Check
        // round-trips break.
        buf[start..i].sort_by_key(|&cp| ccc(cp));
    }
}

/// Common scaffold: decompose into a `Vec<u32>` then canonical-reorder.
pub fn decompose_and_reorder(input: &str, kind: DecompKind) -> Vec<u32> {
    let mut buf = Vec::with_capacity(input.len() + 4);
    decompose_to_buffer(input, kind, &mut buf);
    canonical_reorder(&mut buf);
    buf
}

/// Re-encode a `Vec<u32>` to a `String`. Any code point that does not
/// round-trip through `char::from_u32` (surrogates, > U+10FFFF) is
/// silently dropped - they cannot appear in our tables, but defensive
/// coding keeps `from_u32_unchecked` out of the picture.
pub fn encode(cps: &[u32]) -> String {
    let mut out = String::with_capacity(cps.len());
    for &cp in cps {
        if let Some(c) = char::from_u32(cp) {
            out.push(c);
        }
    }
    out
}

/// Public: NFD.
pub fn to_nfd(input: &str) -> String {
    encode(&decompose_and_reorder(input, DecompKind::Canonical))
}

/// Public: NFKD.
pub fn to_nfkd(input: &str) -> String {
    encode(&decompose_and_reorder(input, DecompKind::Compatibility))
}

/// Canonical composition pass (UAX #15 section 16). Operates on a
/// `Vec<u32>` that has already been decomposed and reordered.
pub fn compose(buf: Vec<u32>) -> Vec<u32> {
    if buf.is_empty() {
        return buf;
    }
    let mut out: Vec<u32> = Vec::with_capacity(buf.len());
    // Index in `out` of the most recent starter that can still absorb
    // following non-starters. `usize::MAX` means "no live starter yet".
    let mut last_starter: usize = usize::MAX;
    // CCC of the last non-starter we've emitted since `last_starter`.
    let mut last_ccc: u8 = 0;

    for cp in buf {
        let cur_ccc = ccc(cp);
        if last_starter != usize::MAX {
            let starter_cp = out[last_starter];
            // Try Hangul composition first - pure algorithm, no table
            // hit at all.
            let composed =
                hangul_compose(starter_cp, cp).or_else(|| compose_pair(starter_cp, cp));
            if let Some(comp) = composed {
                // The composition is only valid if `cp` is not
                // "blocked" by a preceding non-starter of equal or
                // higher CCC. Starters (cur_ccc == 0) are never blocked
                // but they also don't have last_ccc semantics until
                // they become the new starter.
                let blocked = cur_ccc != 0 && last_ccc >= cur_ccc;
                if !blocked {
                    out[last_starter] = comp;
                    continue;
                }
            }
        }
        out.push(cp);
        if cur_ccc == 0 {
            last_starter = out.len() - 1;
            last_ccc = 0;
        } else {
            last_ccc = cur_ccc;
        }
    }
    out
}

/// Public: NFC.
pub fn to_nfc(input: &str) -> String {
    let decomposed = decompose_and_reorder(input, DecompKind::Canonical);
    encode(&compose(decomposed))
}

/// Public: NFKC.
pub fn to_nfkc(input: &str) -> String {
    let decomposed = decompose_and_reorder(input, DecompKind::Compatibility);
    encode(&compose(decomposed))
}

// -------------------------------------------------------------------
// Table encoding helpers for the wasm-AOT backend.
//
// The wasm-AOT bodies embed the four normalization tables into the
// const data section so the runtime can binary-search them via raw
// memory loads. The helpers below produce the matching byte layouts.
// -------------------------------------------------------------------

/// Encode [`NFD_INDEX`] + [`NFD_POOL`] into the byte layout the wasm
/// runtime expects.
///
/// Layout: `[index_count: u32 LE]` followed by `index_count` records
/// of `(cp: u32, pool_off: u32, pool_len: u32)` - 12 bytes per record
/// so the runtime helper can rebase as `table_addr + 4 + mid * 12`.
/// Then `[pool_count: u32 LE]` and `pool_count * u32 LE` payload
/// entries.
///
/// `pool_len` is widened from `u8` to `u32` on the wire so every entry
/// stays on a 4-byte stride; the wasm body has no narrow load opcodes
/// it would prefer over `i32.load`.
pub fn encode_decomp_table_bytes(index: &[(u32, u32, u8)], pool: &[u32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + index.len() * 12 + 4 + pool.len() * 4);
    bytes.extend_from_slice(&(index.len() as u32).to_le_bytes());
    for (cp, off, len) in index {
        bytes.extend_from_slice(&cp.to_le_bytes());
        bytes.extend_from_slice(&off.to_le_bytes());
        bytes.extend_from_slice(&u32::from(*len).to_le_bytes());
    }
    bytes.extend_from_slice(&(pool.len() as u32).to_le_bytes());
    for cp in pool {
        bytes.extend_from_slice(&cp.to_le_bytes());
    }
    bytes
}

/// Encode the canonical-combining-class table.
///
/// Layout: `[count: u32 LE]` followed by `count` records of
/// `(cp: u32 LE, ccc: u32 LE)` - 8 bytes per record so the runtime
/// helper can reuse the same `(table_addr + 4 + mid * 8)` rebase
/// arithmetic as the existing case-folding helper. The CCC value is
/// widened from `u8` to `u32` for the same alignment reason as the
/// decomposition `pool_len`.
pub fn encode_ccc_table_bytes(table: &[(u32, u8)]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + table.len() * 8);
    bytes.extend_from_slice(&(table.len() as u32).to_le_bytes());
    for (cp, ccc) in table {
        bytes.extend_from_slice(&cp.to_le_bytes());
        bytes.extend_from_slice(&u32::from(*ccc).to_le_bytes());
    }
    bytes
}

/// Encode the canonical composition pair table.
///
/// Layout: `[count: u32 LE]` followed by `count` records of
/// `(first: u32 LE, second: u32 LE, composed: u32 LE)` - 12 bytes per
/// record. Sorted by `(first, second)` lexicographic so the runtime
/// helper can binary-search by the combined 64-bit key.
pub fn encode_composition_table_bytes(table: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + table.len() * 12);
    bytes.extend_from_slice(&(table.len() as u32).to_le_bytes());
    for (first, second, composed) in table {
        bytes.extend_from_slice(&first.to_le_bytes());
        bytes.extend_from_slice(&second.to_le_bytes());
        bytes.extend_from_slice(&composed.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_roundtrips_unchanged() {
        for s in ["", "hello", "ABC 123", "the quick brown fox"] {
            assert_eq!(to_nfc(s), s);
            assert_eq!(to_nfd(s), s);
            assert_eq!(to_nfkc(s), s);
            assert_eq!(to_nfkd(s), s);
        }
    }

    #[test]
    fn nfc_composes_combining_acute() {
        // "café" written as 'e' + U+0301 should compose to the
        // precomposed e-acute (U+00E9).
        let decomposed = "cafe\u{0301}";
        let composed = "caf\u{00E9}";
        assert_eq!(to_nfc(decomposed), composed);
        assert_eq!(to_nfc(composed), composed);
    }

    #[test]
    fn nfd_decomposes_precomposed_acute() {
        let composed = "caf\u{00E9}";
        let decomposed = "cafe\u{0301}";
        assert_eq!(to_nfd(composed), decomposed);
        assert_eq!(to_nfd(decomposed), decomposed);
    }

    #[test]
    fn hangul_nfd_uses_algorithmic_decomposition() {
        // U+D55C -> U+1112 U+1161 U+11AB
        let composed = "\u{D55C}";
        let decomposed = "\u{1112}\u{1161}\u{11AB}";
        assert_eq!(to_nfd(composed), decomposed);
    }

    #[test]
    fn hangul_nfc_recomposes_jamos() {
        let composed = "\u{D55C}";
        let decomposed = "\u{1112}\u{1161}\u{11AB}";
        assert_eq!(to_nfc(decomposed), composed);
    }

    #[test]
    fn nfkd_expands_compatibility_form() {
        // U+00BD (1/2 fraction) -> "1" + U+2044 + "2"
        let input = "\u{00BD}";
        let expected = "1\u{2044}2";
        assert_eq!(to_nfkd(input), expected);
        // NFD leaves U+00BD untouched.
        assert_eq!(to_nfd(input), input);
    }

    #[test]
    fn nfkc_does_not_recompose_compatibility_fraction() {
        assert_eq!(to_nfkc("\u{00BD}"), "1\u{2044}2");
    }

    #[test]
    fn canonical_reorder_sorts_combining_marks_by_ccc() {
        // U+0307 (CCC 230) followed by U+0323 (CCC 220) reorders to
        // (U+0323, U+0307) under NFD.
        let input = "a\u{0307}\u{0323}";
        let expected = "a\u{0323}\u{0307}";
        assert_eq!(to_nfd(input), expected);
        assert_eq!(to_nfd(expected), expected);
    }

    #[test]
    fn nfc_idempotence() {
        for s in [
            "",
            "caf\u{00E9}",
            "\u{D55C}\u{AD6D}\u{C5B4}",
            "1\u{2044}2",
            "a\u{0307}\u{0323}b",
        ] {
            let once = to_nfc(s);
            assert_eq!(to_nfc(&once), once, "NFC idempotence fail on {s:?}");
        }
    }

    #[test]
    fn nfd_idempotence() {
        for s in ["", "caf\u{00E9}", "\u{D55C}\u{AD6D}\u{C5B4}", "a\u{0307}\u{0323}b"] {
            let once = to_nfd(s);
            assert_eq!(to_nfd(&once), once, "NFD idempotence fail on {s:?}");
        }
    }

    #[test]
    fn nfkc_idempotence() {
        for s in ["", "caf\u{00E9}", "\u{D55C}\u{AD6D}\u{C5B4}", "\u{00BD}", "\u{FB01}le"] {
            let once = to_nfkc(s);
            assert_eq!(to_nfkc(&once), once, "NFKC idempotence fail on {s:?}");
        }
    }

    #[test]
    fn nfkd_idempotence() {
        for s in ["", "caf\u{00E9}", "\u{D55C}\u{AD6D}\u{C5B4}", "\u{00BD}", "\u{FB01}le"] {
            let once = to_nfkd(s);
            assert_eq!(to_nfkd(&once), once, "NFKD idempotence fail on {s:?}");
        }
    }

    #[test]
    fn nfc_skips_full_composition_exclusion() {
        // U+212A (KELVIN SIGN) decomposes canonically to U+004B ('K').
        // Full_Composition_Exclusion = True, so NFC must NOT recompose
        // 'K' back to U+212A. The generator filters U+212A out of
        // COMPOSITION_PAIRS, making the exclusion automatic at runtime.
        assert_eq!(to_nfc("K"), "K");
        assert_eq!(to_nfc("\u{212A}"), "K");
    }

    #[test]
    fn nfd_decomposes_kelvin_to_ascii_k() {
        assert_eq!(to_nfd("\u{212A}"), "K");
    }

    #[test]
    fn ligature_nfkc_splits_into_components() {
        assert_eq!(to_nfkd("\u{FB01}"), "fi");
        assert_eq!(to_nfkc("\u{FB01}"), "fi");
    }

    #[test]
    fn nfc_starter_blocking_prevents_invalid_composition() {
        // Per UAX #15: in `a U+0308 U+0301`, the U+0308 (CCC 230) is a
        // non-starter that blocks composition between `a` and U+0301
        // (CCC 230). NFC composes `a + U+0308` -> U+00E4, then U+0301
        // follows unaltered.
        assert_eq!(to_nfc("a\u{0308}\u{0301}"), "\u{00E4}\u{0301}");
    }

    #[test]
    fn encode_decomp_table_layout() {
        let index: &[(u32, u32, u8)] = &[(0x00C0, 0, 2), (0x00C1, 2, 2)];
        let pool: &[u32] = &[0x0041, 0x0300, 0x0041, 0x0301];
        let bytes = encode_decomp_table_bytes(index, pool);
        assert_eq!(bytes.len(), 4 + 2 * 12 + 4 + 4 * 4);
        assert_eq!(&bytes[0..4], &2u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &0x00C0u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &0u32.to_le_bytes());
        assert_eq!(&bytes[12..16], &2u32.to_le_bytes());
        // pool header sits after the index
        assert_eq!(&bytes[28..32], &4u32.to_le_bytes());
        assert_eq!(&bytes[32..36], &0x0041u32.to_le_bytes());
    }

    #[test]
    fn encode_ccc_table_layout() {
        let table: &[(u32, u8)] = &[(0x0300, 230), (0x0301, 230)];
        let bytes = encode_ccc_table_bytes(table);
        assert_eq!(bytes.len(), 4 + 2 * 8);
        assert_eq!(&bytes[0..4], &2u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &0x0300u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &230u32.to_le_bytes());
    }

    #[test]
    fn encode_composition_table_layout() {
        let table: &[(u32, u32, u32)] = &[(0x0041, 0x0300, 0x00C0)];
        let bytes = encode_composition_table_bytes(table);
        assert_eq!(bytes.len(), 4 + 12);
        assert_eq!(&bytes[0..4], &1u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &0x0041u32.to_le_bytes());
        assert_eq!(&bytes[8..12], &0x0300u32.to_le_bytes());
        assert_eq!(&bytes[12..16], &0x00C0u32.to_le_bytes());
    }

    #[test]
    fn ccc_table_contains_combining_acute() {
        assert_eq!(ccc(0x0301), 230);
        assert_eq!(ccc(0x0041), 0);
    }

    #[test]
    fn composition_table_sorted_and_excludes_kelvin() {
        // Sanity: sorted by (first, second).
        for w in COMPOSITION_PAIRS.windows(2) {
            let a = (w[0].0, w[0].1);
            let b = (w[1].0, w[1].1);
            assert!(a < b, "COMPOSITION_PAIRS must be sorted: {a:?} >= {b:?}");
        }
        // Sanity: U+212A is excluded.
        let kelvin_idx =
            COMPOSITION_PAIRS.binary_search_by(|t| (t.0, t.1).cmp(&(0x004B, 0)));
        assert!(kelvin_idx.is_err(), "U+212A should be excluded from COMPOSITION_PAIRS");
    }
}
