// Relaxed from `forbid` to `deny` so the v3++ item 4 SIMD ASCII fast
// path (`ascii_fold_simd`) can use wasm32 `v128_load` / `v128_store`
// intrinsics, both of which are `unsafe fn` in `core::arch::wasm32`.
// The `unsafe` blocks are confined to that single module behind a
// `#[allow(unsafe_code)]` and each has a SAFETY comment; the rest of
// the crate stays unsafe-free.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

//! Unicode-aware tables, algorithms, and the glob matcher shared by
//! the tree-walk evaluator and the wasm-AOT / native codegen
//! backends.
//!
//! This crate is a **leaf**: it depends on no other `relon-*` crate
//! (matching `relon-util` / `relon-cap`), so it sits at the very
//! bottom of the workspace dep graph. It consolidates every Unicode
//! dataset, the SIMD ASCII fast path, and the linear-time glob
//! matcher that previously lived under `relon-ir/src/unicode/` and
//! `relon-ir/src/glob.rs`. Pulling them into a standalone crate lets
//! `relon-evaluator` consume the shared tables without an edge to
//! `relon-ir` (the evaluator is a tree-walk engine and never touches
//! the IR surface), keeping the dep graph honest.
//!
//! `relon-ir` keeps same-named re-exports so the codegen backends
//! that reach for `relon_ir::ascii_fold_simd` / `relon_ir::glob` /
//! etc. compile unchanged.
//!
//! ### Module map
//!
//! * [`case_folding`] — UCD simple (1:1) upper / lower folding tables,
//!   generated at build time from `char::to_uppercase` /
//!   `char::to_lowercase`. Drives the wasm-AOT `__casefold_lookup`
//!   helper.
//! * [`full_case_folding`] — UAX #21 full case folding (multi-codepoint
//!   mappings, Greek final sigma, Turkish / Azerbaijani locale
//!   overrides). Generated from `data/SpecialCasing.txt` via
//!   `tools/gen_full_case_folding.py`.
//! * `full_case_folding_data` — raw generated tables for
//!   `full_case_folding`. Pulled in via `include!()` from
//!   `full_case_folding.rs` rather than declared as a sibling
//!   module, matching the pre-split layout so the generated symbols
//!   stay in a single namespace.
//! * [`combining_marks`] — Mn + Mc + Me range table used by every
//!   case-fold body to decide whether a codepoint resets the word
//!   boundary.
//! * [`whitespace`] — non-ASCII `White_Space` ranges (the ASCII subset
//!   is special-cased on the wasm fast path).
//! * [`normalization`] — UAX #15 NFD / NFKD / NFC / NFKC algorithms
//!   on top of the [`normalization_data`] tables. UCD version pinned
//!   at 14.0.0; regenerate via `tools/gen_normalization_tables.py`.
//! * [`normalization_data`] — generated UCD 14.0.0 decomposition,
//!   canonical-combining-class, and composition-pair tables.
//! * [`ascii_fold_simd`] — v3++ item 4 SIMD ASCII fast path for the
//!   tree-walk `upper` / `lower` / `title` bodies. Only the wasm32
//!   arm uses `unsafe` v128 intrinsics; other targets stay on the
//!   chunked scalar fallback.
//! * [`glob`] — linear-time Unicode-aware glob matcher backing the
//!   `glob_match(s, pattern) -> Bool` stdlib function.
//!
//! UCD version: Unicode 14.0.0 across every regeneration script.
//! When a future Unicode bump lands, regenerate the four data-bearing
//! siblings in one commit so the wasm-AOT data section and the
//! tree-walk algorithm stay consistent.

pub mod ascii_fold_simd;
pub mod case_folding;
pub mod combining_marks;
pub mod full_case_folding;
pub mod glob;
pub mod normalization;
pub mod normalization_data;
pub mod whitespace;

/// Encode a `(u32, u32)` table into the wasm data-section layout
/// shared by case-folding, combining-mark, whitespace, and full-fold
/// range tables: `[count: u32 LE][(a: u32 LE, b: u32 LE) × N]`. The
/// runtime helpers all binary-search with the same `(addr + 4 + mid *
/// 8)` rebase arithmetic, so the byte format is identical regardless
/// of whether the pair encodes `(input_cp, output_cp)` or `(start,
/// end)`.
pub fn encode_u32_pair_table(table: &[(u32, u32)]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(encoded_u32_pair_table_size(table.len()));
    bytes.extend_from_slice(&(table.len() as u32).to_le_bytes());
    for (a, b) in table {
        bytes.extend_from_slice(&a.to_le_bytes());
        bytes.extend_from_slice(&b.to_le_bytes());
    }
    bytes
}

/// Byte size of [`encode_u32_pair_table`]'s output — header + 8 bytes
/// per entry. Codegen calls this to pre-size data sections.
pub fn encoded_u32_pair_table_size(len: usize) -> usize {
    4 + len * 8
}

/// Binary-search a sorted `(start, end)` range table for `cp` — used
/// by every compile-time membership predicate (whitespace,
/// combining-marks, full-fold locale ranges). The wasm body emits the
/// same comparison via a hand-unrolled loop instead so the per-cp cost
/// stays O(log N) on both sides.
pub fn cp_in_ranges(cp: u32, ranges: &[(u32, u32)]) -> bool {
    ranges
        .binary_search_by(|&(lo, hi)| {
            if cp < lo {
                std::cmp::Ordering::Greater
            } else if cp > hi {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}
