//! Master ordered list of bundled stdlib functions.
//!
//! The order this file declares is part of the wasm wire format — see
//! the module-level doc comment in [`super`] for the contract.
//!
//! Body builders for each entry live in:
//!   * [`super::defs`] — length / math / is_empty / concat / substring
//!     / starts_with / contains / list_* methods.
//!   * [`super::case_fold`] — upper / lower / title / locale variants
//!     plus the internal `__casefold_lookup`, `__is_combining_mark`,
//!     `__is_whitespace`, `__full_casefold_lookup`,
//!     `__final_sigma_check` helpers.
//!   * [`super::normalization`] — nfd / nfkd / nfc / nfkc plus the
//!     internal `__decomp_lookup`, `__ccc_lookup`, `__compose_lookup`
//!     helpers.

use std::sync::OnceLock;

use super::case_fold::{
    casefold_lookup_helper, final_sigma_check_helper, full_casefold_lookup_helper,
    is_combining_mark_helper, is_whitespace_helper, lower_locale_string, lower_string,
    title_locale_string, title_string, upper_locale_string, upper_string,
};
use super::defs::{
    abs_int, concat_string_string, contains_string, is_empty_string, length_string_to_int,
    list_bool_length, list_float_length, list_int_filter, list_int_fold, list_int_length_to_int,
    list_int_map, list_int_max, list_int_sum, list_schema_length, list_string_length, max_int,
    min_int, starts_with_string, substring_string,
};
use super::normalization::{
    ccc_lookup_helper, compose_lookup_helper, decomp_lookup_helper, nfc_string, nfd_string,
    nfkc_string, nfkd_string,
};
use super::signatures::StdlibFunction;

/// Return the ordered list of builtin stdlib functions. The order is
/// part of the wire format — new entries must be **appended** so the
/// indices of earlier entries remain stable across compiler versions.
///
/// Index assignments:
///   * `0` — `length(String) -> Int` (Phase 4.a).
///   * `1` — `list_int_length(List<Int>) -> Int` (Phase 4.b).
///   * `2` — `abs(Int) -> Int` (Phase 4.b).
///   * `3` — `min(Int, Int) -> Int` (Phase 4.b).
///   * `4` — `max(Int, Int) -> Int` (Phase 4.b).
///   * `5` — `is_empty(String) -> Bool` (Phase 4.b).
///   * `6` — `concat(String, String) -> String` (Phase 4.c-2).
///   * `7` — `upper(String) -> String` (Phase 4.c-2, ASCII fast path).
///   * `8` — `lower(String) -> String` (Phase 4.c-2, ASCII fast path).
///   * `9` — `substring(String, Int, Int) -> String` (Phase 4.c-2;
///     traps as `IndexOutOfBounds` when the slice walks past the
///     receiver).
///   * `10` — `starts_with(String, String) -> Bool` (Phase 4.c-2).
///   * `11` — `list_int_sum(List<Int>) -> Int` (Phase 4.c-2).
///   * `12` — `list_int_max(List<Int>) -> Int` (Phase 4.c-2; traps
///     as `EmptyList` on a zero-length receiver).
///   * `13` — `list_int_map(List<Int>, Closure<Int -> Int>) -> List<Int>`
///     (Phase 10-a).
///   * `14` — `list_int_filter(List<Int>, Closure<Int -> Bool>) -> List<Int>`
///     (Phase 10-a).
///   * `15` — `list_int_fold(List<Int>, Int, Closure<(Int, Int) -> Int>) -> Int`
///     (Phase 10-a).
///   * `16` — `list_float_length(List<Float>) -> Int` (Phase 10-c).
///   * `17` — `list_bool_length(List<Bool>) -> Int` (Phase 10-c).
///   * `18` — `list_string_length(List<String>) -> Int` (Phase 10-c).
///   * `19` — `list_schema_length(List<Schema>) -> Int` (Phase 10-c).
///   * `20` — `__casefold_lookup(cp: I32, table_addr: I32) -> I32`
///     (v3+ a-4 internal helper; binary-searches the simple Unicode
///     case-folding table — see [`casefold_lookup_helper`] for the
///     body. Not surfaced through `stdlib_method_index`; only
///     reachable as a `Op::Call` target from the rewritten
///     `upper` / `lower` bodies).
///   * `21` — `__is_combining_mark(cp: I32, table_addr: I32) -> I32`
///     (v3++ b-4 internal helper; binary-searches the Unicode Mark
///     (Mn + Mc + Me) range table embedded by codegen. Returns `1`
///     when `cp` falls inside any range, else `0`. Same DCE-and-cycle
///     contract as `__casefold_lookup`).
///   * `22` — `__is_whitespace(cp: I32, table_addr: I32) -> I32`
///     (v3++ b-4 internal helper; ASCII fast path plus binary search
///     of the non-ASCII Unicode whitespace ranges. Returns `1` when
///     `cp` is whitespace, else `0`. Called only from the rewritten
///     `title` body).
///   * `23` — `title(String) -> String` (v3++ b-4 word-boundary case
///     fold; per-word first codepoint goes through the upper table,
///     subsequent codepoints through the lower table, combining
///     marks stay un-flipped per the grapheme-cluster contract).
///   * `24` — `__decomp_lookup(cp, table_addr) -> i32` (v3++ b-5
///     internal helper; binary-searches the canonical (NFD) or
///     compatibility (NFKD) decomposition index. Returns
///     `(pool_off << 8) | pool_len` packed, or `0` on miss. Not
///     surfaced through `stdlib_method_index`).
///   * `25` — `__ccc_lookup(cp, table_addr) -> i32` (v3++ b-5
///     internal helper; binary-searches the Canonical_Combining_Class
///     table. Returns `0` on miss to match the UCD default).
///   * `26` — `__compose_lookup(first, second, table_addr) -> i32`
///     (v3++ b-5 internal helper; binary-searches the canonical
///     composition pair table. Returns the composed codepoint, or
///     `-1` (sentinel) on miss).
///   * `27` — `nfd(String) -> String` (v3++ b-5; canonical
///     decomposition + canonical reorder).
///   * `28` — `nfkd(String) -> String` (v3++ b-5; compatibility
///     decomposition + canonical reorder).
///   * `29` — `nfc(String) -> String` (v3++ b-5; canonical
///     decomposition + reorder + composition pass).
///   * `30` — `nfkc(String) -> String` (v3++ b-5; compatibility
///     decomposition + reorder + composition pass).
///   * `31` — `upper_locale(String, String) -> String` (v3++ b-6;
///     locale-aware uppercase via Turkish / Azerbaijani override
///     table).
///   * `32` — `lower_locale(String, String) -> String` (v3++ b-6;
///     locale-aware lowercase).
///   * `33` — `title_locale(String, String) -> String` (v3++ b-6;
///     locale-aware title case).
///   * `34` — `__full_casefold_lookup(cp, table_addr) -> i32` (v3++
///     b-7 reframed; binary-searches the FULL 20-byte-stride table.
///     Returns the entry address on hit, `0` on miss).
///   * `35` — `__final_sigma_check(s_ptr, byte_offset, cased_addr,
///     ignorable_addr) -> i32` (v3++ b-7 reframed; UAX #21
///     Final_Sigma scan — UTF-8 reverse + forward decode, skipping
///     case-ignorables. Returns `1` for word-final, `0` otherwise).
pub fn builtin_stdlib() -> &'static [StdlibFunction] {
    static REGISTRY: OnceLock<Vec<StdlibFunction>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        vec![
            length_string_to_int(),
            list_int_length_to_int(),
            abs_int(),
            min_int(),
            max_int(),
            is_empty_string(),
            concat_string_string(),
            upper_string(),
            lower_string(),
            substring_string(),
            starts_with_string(),
            list_int_sum(),
            list_int_max(),
            list_int_map(),
            list_int_filter(),
            list_int_fold(),
            list_float_length(),
            list_bool_length(),
            list_string_length(),
            list_schema_length(),
            casefold_lookup_helper(),
            is_combining_mark_helper(),
            is_whitespace_helper(),
            title_string(),
            decomp_lookup_helper(),
            ccc_lookup_helper(),
            compose_lookup_helper(),
            nfd_string(),
            nfkd_string(),
            nfc_string(),
            nfkc_string(),
            // v3++ b-6: locale-aware case folding bodies. Each accepts
            // `(String s, String locale) -> String`; the body parses the
            // leading two ASCII letters of `locale` and dispatches to the
            // Turkish / Azerbaijani override table when they match
            // `tr` / `az` (case-insensitive, BCP-47-ish boundary check).
            upper_locale_string(),
            lower_locale_string(),
            title_locale_string(),
            // v3++ b-7 reframed: FULL multi-codepoint + final-sigma helpers
            // shared by every (locale-aware or default) case-fold body.
            full_casefold_lookup_helper(),
            final_sigma_check_helper(),
            // F-D7-D (2026-05-20): `contains(haystack, needle) -> Bool`.
            // Lives at index 36 — the slot the trace recorder pins via
            // [`relon_trace_recorder::lowering::STDLIB_IDX_CONTAINS`].
            // Body is naive O(s_len * p_len); the JIT side has the F-D7-C
            // inline lowering for short const needles, so the body cost is
            // only seen on the cold / tree-walk path.
            contains_string(),
        ]
    })
}
