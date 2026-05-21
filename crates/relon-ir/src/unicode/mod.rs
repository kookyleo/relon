//! Unicode-aware tables and helpers shared by the tree-walk
//! evaluator and the wasm-AOT / native codegen backends.
//!
//! This submodule consolidates every Unicode dataset and the SIMD
//! ASCII fast path that previously lived flat under `relon-ir/src/`.
//! Moving them under a single `unicode/` parent keeps the IR crate
//! root focused on the IR surface itself (`ir`, `lowering`,
//! `op_visitor`, `stdlib*`, `shape_hash`, `error`) and gives Unicode
//! contributors one place to land table regenerations + algorithm
//! tweaks.
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
//!
//! UCD version: Unicode 14.0.0 across every regeneration script.
//! When a future Unicode bump lands, regenerate the four data-bearing
//! siblings in one commit so the wasm-AOT data section and the
//! tree-walk algorithm stay consistent.

pub mod ascii_fold_simd;
pub mod case_folding;
pub mod combining_marks;
pub mod full_case_folding;
pub mod normalization;
pub mod normalization_data;
pub mod whitespace;
