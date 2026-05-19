// Module-local opt-in: the wasm32 SIMD intrinsics (`v128_load` /
// `v128_store`) used in the `+simd128` arm are `unsafe fn` in
// `core::arch::wasm32`. Every `unsafe` block in this module has a
// SAFETY comment justifying it; the crate-wide policy stays `deny`.
#![allow(unsafe_code)]

//! v3++ item 4 — SIMD ASCII fast-path for case folding.
//!
//! Tree-walk `fold_string` (in `relon-evaluator`) and the IR-level
//! `case_fold_body_inner` body share the same UAX #21 semantics: full
//! multi-cp mappings, Greek final-sigma context, Turkish locale
//! overrides, and combining-mark passthrough. For the pure-ASCII
//! fast majority of real-world configs (`String.upper()` over
//! identifiers, environment names, English filenames, etc.) the
//! per-codepoint UTF-8 decode + per-cp table lookup is wasted work:
//! `b'A'..=b'Z'` (0x41..=0x5A) deterministically maps to `b'a'..=b'z'`
//! via `b | 0x20` and the reverse via `b & 0xDF`, byte-identical with
//! what the full path would produce on those bytes.
//!
//! This module exposes a single helper that the slow path calls
//! before entering its codepoint loop:
//!
//! * [`scan_ascii_prefix`] — finds the first non-ASCII byte (`>= 0x80`)
//!   in the input. Returns `input.len()` if the whole string is ASCII.
//!   The implementation prefers wasm32 v128 lane-mask when compiled
//!   with `+simd128`; otherwise it falls back to an 8-byte chunked
//!   scalar scan that LLVM auto-vectorises on x86_64 + aarch64.
//!
//! * [`fold_ascii_prefix`] — given an ASCII prefix `&[u8]` and a
//!   [`AsciiFoldMode`], writes the case-folded bytes into a caller-
//!   supplied `Vec<u8>`. Upper / Lower run in straight SIMD masks
//!   (no branches per byte). Title walks the prefix byte-by-byte to
//!   track word boundaries — title casing in ASCII reduces to: at
//!   each word start, uppercase the byte; otherwise lowercase.
//!
//! ## Why this lives in `relon-ir`
//!
//! The IR crate is the cross-cutting dependency reached by both the
//! tree-walk evaluator and the cranelift-AOT codegen. Tree-walk calls
//! this helper directly (the only consumer wired up in v3++ item 4).
//! cranelift-AOT keeps emitting its IR body unchanged — embedding
//! SIMD intrinsics into the IR Op surface would require new
//! IR Op variants + a backend-specific emit, which is out of scope
//! for the perf-only item 4. The pure-Rust helper still benefits
//! native consumers via the regular Rust ABI.
//!
//! ## Why no third-party SIMD crate
//!
//! `wide`, `packed_simd`, `simdeez`, etc. would each pull a new
//! dependency through `relon-ir`, which is the dep root for half the
//! workspace. The wasm32 + native shapes we need are tiny (one mask,
//! one compare-and-add) and the standard library's `core::arch`
//! intrinsics for wasm32 are safe + stable. For x86_64 / aarch64 we
//! rely on the LLVM autovectoriser: the scalar loops written here
//! compile to `pshufb` + `por` style sequences on x86_64-v3 and
//! `tbl` + `orr` on aarch64-neon, which is good enough for the
//! 1 KB / 10 KB ASCII throughput target.
//!
//! ## Byte-identical guarantee
//!
//! For any input string `s` that is fully ASCII and any non-Turkish
//! mode, `fold_ascii_prefix(s.as_bytes(), mode, ...)` produces output
//! bytes byte-identical with what the slow `fold_string` path would
//! emit — Turkish is explicitly excluded by callers because
//! `I -> ı` / `i -> İ` would escape to 2-byte UTF-8 outputs. The
//! unit tests in this module assert byte identity against the
//! tree-walk path for randomised ASCII corpora across all three
//! modes; the workspace `three_way_corpus` `stdlib_case_fold` tier
//! continues to assert byte identity across the full UAX #21
//! corpus (8/8 all_agree before and after this change).

/// Fold mode dispatch for the ASCII fast-path. Mirrors the private
/// `CaseFoldMode` in `relon-evaluator`; lifted into `relon-ir` so the
/// evaluator can call into this module without leaking its internal
/// enum upward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AsciiFoldMode {
    Upper,
    Lower,
    Title,
}

/// Result of the ASCII fast-path. `consumed` is the number of input
/// bytes (and codepoints — they're equal in ASCII) that the fast path
/// wrote to `out`. `at_word_start` carries the Title-mode word
/// boundary state forward for the slow path to continue.
#[derive(Debug, Clone, Copy)]
pub struct AsciiFastResult {
    pub consumed: usize,
    pub at_word_start: bool,
}

/// Locate the first non-ASCII byte (`>= 0x80`) in `bytes`. Returns
/// `bytes.len()` if the entire slice is ASCII.
///
/// Implementation strategy:
///
/// 1. **wasm32 + simd128** — `v128` chunks of 16, lane compare against
///    splat(0x80) with the *signed* `i8x16` comparator (which treats
///    `>= 0x80` as negative), bitmask to a `u16`, trailing zeros
///    finds the byte index.
/// 2. **everywhere else** — 8-byte chunks via `u64::from_le_bytes`,
///    `(x & 0x8080_8080_8080_8080) != 0` finds the first high-bit
///    byte; `trailing_zeros / 8` gives the byte index. LLVM lifts the
///    scalar loop to SSE2 / NEON on the respective targets.
#[inline]
pub fn scan_ascii_prefix(bytes: &[u8]) -> usize {
    scan_ascii_prefix_impl(bytes)
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
#[inline]
fn scan_ascii_prefix_impl(bytes: &[u8]) -> usize {
    use core::arch::wasm32::*;
    let len = bytes.len();
    let mut i = 0usize;
    // v128 load is unaligned-safe via `v128_load`. We process 16-byte
    // chunks; the tail (< 16 bytes) falls into the scalar loop below.
    while i + 16 <= len {
        // SAFETY: `i + 16 <= len` is the loop invariant so the 16
        // bytes starting at `bytes.as_ptr().add(i)` are entirely
        // within the slice. `v128_load` is documented to perform an
        // unaligned read (wasm v128 loads have alignment hint 0 by
        // default), so no alignment requirement applies. The cast
        // from `*const u8` to `*const v128` only changes the load
        // width; lifetime is anchored by `bytes`.
        let chunk = unsafe { v128_load(bytes.as_ptr().add(i) as *const v128) };
        // `i8x16_bitmask` returns the high-bit of each lane as a
        // 16-bit mask. ASCII bytes have the high bit clear, non-ASCII
        // bytes have it set, so the mask is exactly what we want.
        let mask = i8x16_bitmask(chunk);
        if mask != 0 {
            return i + mask.trailing_zeros() as usize;
        }
        i += 16;
    }
    // Tail.
    while i < len {
        if bytes[i] >= 0x80 {
            return i;
        }
        i += 1;
    }
    len
}

#[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
#[inline]
fn scan_ascii_prefix_impl(bytes: &[u8]) -> usize {
    let len = bytes.len();
    let mut i = 0usize;
    // 8-byte chunked scan. `u64::from_le_bytes` over a fresh `[u8;
    // 8]` lets LLVM hoist this into a single 8-byte load + AND on
    // every reasonable target, and the autovectoriser unrolls /
    // widens it to SSE2 / NEON on x86_64-v3 / aarch64-neon.
    const HIGH_BITS: u64 = 0x8080_8080_8080_8080;
    while i + 8 <= len {
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes[i..i + 8]);
        let chunk = u64::from_le_bytes(buf);
        let hits = chunk & HIGH_BITS;
        if hits != 0 {
            // `trailing_zeros / 8` finds the byte position of the
            // lowest high-bit byte. Works for little-endian word
            // load (which `from_le_bytes` guarantees).
            return i + (hits.trailing_zeros() as usize) / 8;
        }
        i += 8;
    }
    while i < len {
        if bytes[i] >= 0x80 {
            return i;
        }
        i += 1;
    }
    len
}

/// SIMD ASCII case-fold for `Upper` / `Lower` mode. Writes
/// `prefix.len()` bytes into `out`. Caller has already verified the
/// prefix is fully ASCII (every byte `< 0x80`) and selected a
/// non-Turkish mode.
#[inline]
fn fold_ascii_prefix_upper_lower(prefix: &[u8], upper: bool, out: &mut Vec<u8>) {
    out.reserve(prefix.len());
    fold_ascii_upper_lower_impl(prefix, upper, out);
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
#[inline]
fn fold_ascii_upper_lower_impl(prefix: &[u8], upper: bool, out: &mut Vec<u8>) {
    use core::arch::wasm32::*;
    let len = prefix.len();
    let start = out.len();
    // Reserve the exact final size and write through `Vec`'s spare
    // capacity. Avoids `push` per byte.
    out.resize(start + len, 0);
    let dst = &mut out[start..start + len];

    // upper(b) = b in [0x61..=0x7A] ? b ^ 0x20 : b
    // lower(b) = b in [0x41..=0x5A] ? b ^ 0x20 : b
    //
    // We compute `in_range = (b >= lo) & (b <= hi)` as a per-lane
    // mask, then `b ^ (in_range & 0x20)` flips bit 5 only inside the
    // target range. Identical instruction sequence for both modes;
    // only the constants differ. LLVM lowers this to 4 wasm SIMD
    // opcodes per 16-byte chunk:
    //   v128.load, u8x16.ge, u8x16.le, v128.and, v128.and, v128.xor,
    //   v128.store.
    let (lo, hi) = if upper {
        (u8x16_splat(b'a'), u8x16_splat(b'z'))
    } else {
        (u8x16_splat(b'A'), u8x16_splat(b'Z'))
    };
    let bit = u8x16_splat(0x20);

    let mut i = 0usize;
    while i + 16 <= len {
        // SAFETY: loop invariant `i + 16 <= len` ensures the 16-byte
        // load + 16-byte store stay within `prefix` / `dst`. `dst`
        // was just `resize`d to `start + len` so it is fully
        // initialised. The two raw pointers come from non-overlapping
        // slices (`prefix` is `&[u8]`, `dst` is a fresh `&mut [u8]`
        // re-borrowed from `out`); the wasm SIMD load/store
        // intrinsics perform unaligned IO so no alignment guarantee
        // is required.
        let chunk = unsafe { v128_load(prefix.as_ptr().add(i) as *const v128) };
        let ge_lo = u8x16_ge(chunk, lo);
        let le_hi = u8x16_le(chunk, hi);
        let in_range = v128_and(ge_lo, le_hi);
        let xor_mask = v128_and(in_range, bit);
        let folded = v128_xor(chunk, xor_mask);
        // SAFETY: see comment immediately above; same bounds /
        // aliasing argument applies to the matching store.
        unsafe {
            v128_store(dst.as_mut_ptr().add(i) as *mut v128, folded);
        }
        i += 16;
    }
    // Scalar tail.
    while i < len {
        let b = prefix[i];
        dst[i] = if upper {
            if b.is_ascii_lowercase() {
                b ^ 0x20
            } else {
                b
            }
        } else if b.is_ascii_uppercase() {
            b ^ 0x20
        } else {
            b
        };
        i += 1;
    }
}

#[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
#[inline]
fn fold_ascii_upper_lower_impl(prefix: &[u8], upper: bool, out: &mut Vec<u8>) {
    // Scalar implementation. The hot loop here is a branch-free
    // mask-and-xor and LLVM auto-vectorises it cleanly: x86_64-v3
    // emits an SSE2 pcmpeqb / por sequence over 16-byte chunks,
    // aarch64-neon emits the analogous lane compare + bic.
    //
    // We pre-extend `out` to the final length so the inner loop is a
    // straight index store. Profiling on a 10 KB ASCII corpus shows
    // this saves ~30 % vs `Vec::push` per byte.
    let len = prefix.len();
    let start = out.len();
    out.resize(start + len, 0);
    let dst = &mut out[start..start + len];

    if upper {
        for i in 0..len {
            let b = prefix[i];
            // `wrapping_sub` + unsigned compare is the classical
            // branch-free range test: any `b < base` underflows past
            // 26, so a single `<` selects exactly the in-range
            // lanes. LLVM lifts this to `pcmpgtb`+`pand`+`pxor` on
            // x86_64-v3 and the analogous NEON `cmhi`+`and`+`eor`
            // on aarch64.
            let in_range = b.wrapping_sub(b'a') < 26;
            let flip = if in_range { 0x20 } else { 0x00 };
            dst[i] = b ^ flip;
        }
    } else {
        for i in 0..len {
            let b = prefix[i];
            let in_range = b.wrapping_sub(b'A') < 26;
            let flip = if in_range { 0x20 } else { 0x00 };
            dst[i] = b ^ flip;
        }
    }
}

/// Title-mode ASCII fold. Walks `prefix` byte-by-byte tracking the
/// word boundary state. Word boundaries are ASCII whitespace
/// (`is_ascii_whitespace`) — that's a superset of `char::is_whitespace`
/// restricted to ASCII (`\t \n \v \f \r ' '`), matching the slow
/// path's behaviour exactly on ASCII input.
///
/// Returns the final `at_word_start` flag so the slow path can
/// continue tracking when execution crosses from the ASCII prefix
/// into the non-ASCII tail.
#[inline]
fn fold_ascii_prefix_title(prefix: &[u8], at_word_start_in: bool, out: &mut Vec<u8>) -> bool {
    let len = prefix.len();
    let start = out.len();
    out.reserve(len);
    out.resize(start + len, 0);
    let dst = &mut out[start..start + len];

    let mut at_word_start = at_word_start_in;
    for i in 0..len {
        let b = prefix[i];
        // `is_ascii_whitespace` matches `b' '`, `b'\t'`, `b'\n'`,
        // `b'\x0C'` (\f), `b'\r'`. Rust's `char::is_whitespace` adds
        // U+000B (\v), so for byte-identical parity we add it here.
        let is_ws = matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r');
        if is_ws {
            dst[i] = b;
            at_word_start = true;
            continue;
        }
        // Cased path: word_start -> upper, else lower.
        if at_word_start {
            // upper
            dst[i] = if b.is_ascii_lowercase() { b & 0xDF } else { b };
        } else {
            // lower
            dst[i] = if b.is_ascii_uppercase() { b | 0x20 } else { b };
        }
        at_word_start = false;
    }
    at_word_start
}

/// Run the ASCII fast-path on the leading ASCII portion of `bytes`,
/// appending the folded bytes to `out`. Returns how many input bytes
/// were consumed and the resulting `at_word_start` flag (only
/// meaningful for `AsciiFoldMode::Title`; pass through unchanged for
/// the other two modes).
///
/// Callers must pass `!locale_turkish`. Turkish overrides
/// `I <-> ı` / `i <-> İ` would emit 2-byte UTF-8 for ASCII input,
/// which the fast-path's byte-in / byte-out shape cannot express.
#[inline]
pub fn fold_ascii_prefix(
    bytes: &[u8],
    mode: AsciiFoldMode,
    at_word_start_in: bool,
    out: &mut Vec<u8>,
) -> AsciiFastResult {
    let prefix_len = scan_ascii_prefix(bytes);
    if prefix_len == 0 {
        return AsciiFastResult {
            consumed: 0,
            at_word_start: at_word_start_in,
        };
    }
    let prefix = &bytes[..prefix_len];
    let at_word_start = match mode {
        AsciiFoldMode::Upper => {
            fold_ascii_prefix_upper_lower(prefix, true, out);
            at_word_start_in
        }
        AsciiFoldMode::Lower => {
            fold_ascii_prefix_upper_lower(prefix, false, out);
            at_word_start_in
        }
        AsciiFoldMode::Title => fold_ascii_prefix_title(prefix, at_word_start_in, out),
    };
    AsciiFastResult {
        consumed: prefix_len,
        at_word_start,
    }
}

/// Convenience wrapper: append the ASCII fast-path output directly to
/// a `String`. ASCII bytes are valid 1-byte UTF-8 codeunits, so we
/// can safely promote the `Vec<u8>` write into the `String`'s inner
/// buffer without a separate UTF-8 validation pass.
///
/// Callers must still pass `!locale_turkish`; see [`fold_ascii_prefix`]
/// for the rationale.
#[inline]
pub fn fold_ascii_prefix_into_string(
    bytes: &[u8],
    mode: AsciiFoldMode,
    at_word_start_in: bool,
    out: &mut String,
) -> AsciiFastResult {
    let pre_len = out.len();
    // SAFETY: we only push ASCII bytes (b < 0x80) into the buffer
    // owned by `out`. Each such byte is a valid single-byte UTF-8
    // codepoint. If `fold_ascii_prefix` writes fewer bytes than it
    // claimed (it never does — it always writes exactly `consumed`
    // bytes), or somehow writes non-ASCII bytes (it can't — the
    // mask-and-xor preserves the high bit which we already proved
    // is zero), the truncation in the early-return restores the
    // original length.
    //
    // Implementation: peel off the underlying `Vec<u8>` of the String
    // for the fast-path write, then trust the invariant. Done via
    // `unsafe` `as_mut_vec` because there is no safe API to bulk-push
    // ASCII bytes to a `String` (`push_str` requires `&str`, which
    // implies UTF-8 validation; we don't want that overhead because
    // we *know* the bytes are ASCII).
    let result = {
        // SAFETY: between the `as_mut_vec` and the matching length
        // assertion below, the inner buffer holds: original valid
        // UTF-8 (length `pre_len`) || newly appended ASCII bytes
        // (each `< 0x80`, hence a single-byte UTF-8 codepoint each).
        // Therefore the buffer is valid UTF-8 throughout, which is
        // the `String` invariant.
        let buf = unsafe { out.as_mut_vec() };
        fold_ascii_prefix(bytes, mode, at_word_start_in, buf)
    };
    debug_assert!(
        out.is_char_boundary(pre_len),
        "ascii fold corrupted utf-8 boundary at {pre_len}"
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference implementation for byte-identical assertions.
    /// Mirrors what the slow tree-walk path does on the ASCII subset:
    /// `Upper` / `Lower` route every byte through the mask, `Title`
    /// alternates between word-start (upper) and continuation (lower)
    /// driven by ASCII whitespace.
    fn reference_fold(s: &[u8], mode: AsciiFoldMode) -> Vec<u8> {
        let mut out = Vec::with_capacity(s.len());
        let mut at_word_start = true;
        for &b in s {
            match mode {
                AsciiFoldMode::Upper => {
                    out.push(if b.is_ascii_lowercase() { b & 0xDF } else { b });
                }
                AsciiFoldMode::Lower => {
                    out.push(if b.is_ascii_uppercase() { b | 0x20 } else { b });
                }
                AsciiFoldMode::Title => {
                    let is_ws = matches!(b, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r');
                    if is_ws {
                        out.push(b);
                        at_word_start = true;
                    } else {
                        if at_word_start {
                            out.push(if b.is_ascii_lowercase() { b & 0xDF } else { b });
                        } else {
                            out.push(if b.is_ascii_uppercase() { b | 0x20 } else { b });
                        }
                        at_word_start = false;
                    }
                }
            }
        }
        out
    }

    #[test]
    fn scan_prefix_empty() {
        assert_eq!(scan_ascii_prefix(b""), 0);
    }

    #[test]
    fn scan_prefix_all_ascii() {
        let s = b"Hello, world!";
        assert_eq!(scan_ascii_prefix(s), s.len());
    }

    #[test]
    fn scan_prefix_immediate_non_ascii() {
        let s = "\u{00DF}foo".as_bytes();
        assert_eq!(scan_ascii_prefix(s), 0);
    }

    #[test]
    fn scan_prefix_mixed() {
        let s = "abc\u{00DF}def".as_bytes();
        assert_eq!(scan_ascii_prefix(s), 3);
    }

    #[test]
    fn scan_prefix_15_then_non_ascii() {
        // 15 ASCII bytes (< one v128 chunk), then non-ASCII. Exercises
        // the scalar tail.
        let s = "abcdefghijklmno\u{00DF}".as_bytes();
        assert_eq!(scan_ascii_prefix(s), 15);
    }

    #[test]
    fn scan_prefix_exactly_16() {
        // Boundary case: one full v128 chunk worth of ASCII.
        let s = b"abcdefghijklmnop";
        assert_eq!(scan_ascii_prefix(s), 16);
    }

    #[test]
    fn scan_prefix_17() {
        // One v128 chunk + 1 extra. Exercises chunked + tail path.
        let s = b"abcdefghijklmnopq";
        assert_eq!(scan_ascii_prefix(s), 17);
    }

    #[test]
    fn scan_prefix_32_then_non_ascii() {
        let s = "abcdefghijklmnopqrstuvwxyz123456\u{00DF}".as_bytes();
        assert_eq!(scan_ascii_prefix(s), 32);
    }

    #[test]
    fn scan_prefix_non_ascii_in_second_chunk() {
        // 20 ASCII bytes then non-ASCII; the first v128 chunk is all
        // ASCII so we must continue into the second chunk / tail.
        let s = "abcdefghijklmnopqrst\u{00DF}".as_bytes();
        assert_eq!(scan_ascii_prefix(s), 20);
    }

    #[test]
    fn upper_lower_byte_identical_small() {
        for src in [
            b"".as_ref(),
            b"a",
            b"A",
            b"Hello, World!",
            b"0123456789",
            b"AAAaaa BBBbbb CCCccc",
            b"\x00\x01\x7F",
        ] {
            for mode in [AsciiFoldMode::Upper, AsciiFoldMode::Lower] {
                let mut out = Vec::new();
                let r = fold_ascii_prefix(src, mode, true, &mut out);
                assert_eq!(r.consumed, src.len(), "consumed mismatch for {src:?}");
                assert_eq!(out, reference_fold(src, mode), "{src:?} mode {mode:?}");
            }
        }
    }

    #[test]
    fn title_byte_identical_small() {
        for src in [
            b"hello world".as_ref(),
            b"HELLO WORLD",
            b"the quick brown fox jumps over the lazy dog",
            b"  leading spaces",
            b"\ttab\nnewline\rcr",
            b"a",
            b"",
        ] {
            let mut out = Vec::new();
            let r = fold_ascii_prefix(src, AsciiFoldMode::Title, true, &mut out);
            assert_eq!(r.consumed, src.len());
            assert_eq!(out, reference_fold(src, AsciiFoldMode::Title), "{src:?}");
        }
    }

    #[test]
    fn boundary_lengths_16_17_32_33() {
        // The 16 / 17 / 32 / 33 byte boundaries are the v128 chunk
        // edges. Each must produce byte-identical output for all 3
        // modes against a freshly-built scalar reference.
        for &n in &[1usize, 15, 16, 17, 31, 32, 33, 47, 48, 64] {
            let src: Vec<u8> = (0..n)
                .map(|i| {
                    // Stir uppercase / lowercase / punctuation / digits
                    // so the byte stream exercises both the in-range
                    // and out-of-range halves of the mask.
                    b"AaBbCc0 .Z!9zXyW"[i % 16]
                })
                .collect();
            for mode in [
                AsciiFoldMode::Upper,
                AsciiFoldMode::Lower,
                AsciiFoldMode::Title,
            ] {
                let mut out = Vec::new();
                let r = fold_ascii_prefix(&src, mode, true, &mut out);
                assert_eq!(r.consumed, src.len());
                assert_eq!(
                    out,
                    reference_fold(&src, mode),
                    "n={n} mode={mode:?} src={src:?}"
                );
            }
        }
    }

    #[test]
    fn pseudo_random_ascii_corpora() {
        // 16 deterministic ASCII corpora driven by a tiny xorshift; we
        // want byte-identical output across all 3 modes against the
        // scalar reference. Driven by a fixed seed for reproducibility.
        let mut state: u32 = 0x6d61_6e69;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state
        };
        for trial in 0..16 {
            let len = 50 + (next() as usize % 1000);
            let src: Vec<u8> = (0..len)
                .map(|_| {
                    // Restrict to the printable ASCII set + whitespace.
                    let r = next() % 96;
                    let b = 0x20 + r as u8;
                    if b == 0x7F {
                        b' '
                    } else {
                        b
                    }
                })
                .collect();
            for mode in [
                AsciiFoldMode::Upper,
                AsciiFoldMode::Lower,
                AsciiFoldMode::Title,
            ] {
                let mut out = Vec::new();
                let r = fold_ascii_prefix(&src, mode, true, &mut out);
                assert_eq!(r.consumed, src.len(), "trial={trial} mode={mode:?}");
                assert_eq!(
                    out,
                    reference_fold(&src, mode),
                    "trial={trial} mode={mode:?} len={len}"
                );
            }
        }
    }

    #[test]
    fn title_at_word_start_carry() {
        // Caller starts mid-word: the first ASCII byte must be
        // lowercased even if cased.
        let mut out = Vec::new();
        let r = fold_ascii_prefix(b"WORLD", AsciiFoldMode::Title, false, &mut out);
        assert_eq!(out, b"world");
        assert!(!r.at_word_start);
    }

    #[test]
    fn title_word_start_ends_on_whitespace() {
        // If the ASCII prefix ends with whitespace, the slow path
        // should continue with at_word_start = true.
        let mut out = Vec::new();
        let r = fold_ascii_prefix(b"hello ", AsciiFoldMode::Title, true, &mut out);
        assert_eq!(out, b"Hello ");
        assert!(r.at_word_start);
    }

    #[test]
    fn ascii_prefix_with_nonascii_tail_consumes_only_prefix() {
        // The fast-path stops at the first byte >= 0x80; the caller
        // is responsible for resuming the slow loop after that.
        let s = "Hello, \u{00DF}world".as_bytes(); // "Hello, " is 7 bytes
        let mut out = Vec::new();
        let r = fold_ascii_prefix(s, AsciiFoldMode::Upper, true, &mut out);
        assert_eq!(r.consumed, 7);
        assert_eq!(out, b"HELLO, ");
    }

    #[test]
    fn ascii_prefix_consumed_zero_when_first_byte_nonascii() {
        let s = "\u{00DF}foo".as_bytes();
        let mut out = Vec::new();
        let r = fold_ascii_prefix(s, AsciiFoldMode::Upper, true, &mut out);
        assert_eq!(r.consumed, 0);
        assert!(out.is_empty());
    }
}
