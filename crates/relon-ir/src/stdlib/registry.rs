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
    abs_float, abs_int, ceil_float_to_int, concat_string_string, contains_string,
    floor_float_to_int, glob_match_string, is_empty_string, length_string_to_int, list_bool_length,
    list_float_every, list_float_filter, list_float_fold, list_float_length, list_float_map,
    list_float_map_to_int, list_float_map_to_string, list_float_map_to_variant_list,
    list_float_some, list_float_unique, list_int_every, list_int_filter, list_int_fold,
    list_int_length_to_int, list_int_map, list_int_map_to_float, list_int_map_to_string,
    list_int_map_to_variant_list, list_int_max, list_int_some, list_int_sum, list_int_unique,
    list_list_filter, list_list_length, list_schema_length, list_string_length, list_string_map,
    list_string_map_to_variant_list, max_int, min_int, pow_float, round_float_to_int, sqrt_float,
    starts_with_string, substring_string,
};
use super::normalization::{
    ccc_lookup_helper, compose_lookup_helper, decomp_lookup_helper, nfc_string, nfd_string,
    nfkc_string, nfkd_string,
};
use super::signatures::StdlibFunction;
use super::string_ops::{
    ends_with_string, len_string_to_int, replace_string, split_string, trim_end_string,
    trim_start_string, trim_string,
};
use super::validators::{
    dict_size_in_range, in_range_float, is_email_string, is_iso_date_string, is_uri_string,
    is_uuid_string, multiple_of_int, size_in_range_list,
};

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
///     case-folding table — see `casefold_lookup_helper` for the
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
            // 2026-05-21: `glob_match(s, pattern) -> Bool`.
            // Lives at index 37 (pinned via `GLOB_MATCH_INDEX`). Tier-2
            // LuaJIT-pattern-subset matcher — see `crate::glob::glob_match`
            // for the algorithm and `super::defs::glob_match_string` for
            // the backend-dispatch matrix (tree-walker native impl,
            // cranelift vtable indirection, wasm body trap).
            glob_match_string(),
            // Nested `List<List<…>>.length()`. Appended at the tail
            // (index 38) so the position-pinned internal helper / method
            // indices above (`__casefold_lookup` = 20, `contains` = 36,
            // `glob_match` = 37, …) stay put. Shares the
            // `list_length_record_header_body` op-stream with the other
            // `list_<T>_length` shapes — only the `ListList` param tag
            // differs.
            list_list_length(),
            // Wave R3b: typed list higher-order ops over `List<Float>`
            // plus the element-type-changing numeric `map` shapes.
            // Appended at the tail (indices 39+) so every position-pinned
            // index above stays put — existing-construct cranelift bytes
            // are unchanged, so GENERATOR_VERSION does not move.
            //   * `39` — `list_float_map(List<Float>, Closure<F64 -> F64>)
            //             -> List<Float>`.
            //   * `40` — `list_float_filter(List<Float>,
            //             Closure<F64 -> Bool>) -> List<Float>`.
            //   * `41` — `list_float_fold(List<Float>, Float,
            //             Closure<(F64, F64) -> F64>) -> Float`.
            //   * `42` — `list_int_map_to_float(List<Int>,
            //             Closure<I64 -> F64>) -> List<Float>`.
            //   * `43` — `list_float_map_to_int(List<Float>,
            //             Closure<F64 -> I64>) -> List<Int>`.
            list_float_map(),
            list_float_filter(),
            list_float_fold(),
            list_int_map_to_float(),
            list_float_map_to_int(),
            // Wave R3c: String-result list higher-order ops. Appended at
            // the tail (indices 44+) so every position-pinned index above
            // stays put — existing-construct cranelift bytes are
            // unchanged, so GENERATOR_VERSION does not move. Each result
            // is a `List<String>` pointer-array record (`[count][off_i]…`,
            // 4-byte slots) byte-identical to `write_list_string`, so the
            // return ABI / verifier walk it unchanged.
            //   * `44` — `list_string_map(List<String>,
            //             Closure<String -> String>) -> List<String>`.
            //   * `45` — `list_int_map_to_string(List<Int>,
            //             Closure<Int -> String>) -> List<String>`.
            //   * `46` — `list_float_map_to_string(List<Float>,
            //             Closure<Float -> String>) -> List<String>`.
            // `list_string_filter` is intentionally NOT registered: no
            // `String -> Bool` predicate currently lowers four-way, so the
            // shape stays capped (the body would be unreachable / unverified
            // — see the cap note in `lowering::peephole::emit_list_hof_call`).
            list_string_map(),
            list_int_map_to_string(),
            list_float_map_to_string(),
            // Wave R7: scalar-returning Float math stdlib. Appended at the
            // tail (indices 47+) so every position-pinned index above stays
            // put — existing-construct cranelift/llvm bytes are unchanged,
            // so GENERATOR_VERSION does not move. Each body is a tiny
            // direct-op stream over the new `Op::F64Unary` / `Op::F64ToI64Sat`
            // float intrinsics (NOT a libcall), four-way byte-equal with the
            // tree-walk oracle.
            //   * `47` — `abs_float(Float) -> Float`  (`f64::abs`).
            //   * `48` — `floor(Float) -> Int`        (`f64::floor as i64`).
            //   * `49` — `ceil(Float) -> Int`         (`f64::ceil as i64`).
            //   * `50` — `round(Float) -> Int`        (`f64::round_ties_even as i64`).
            //   * `51` — `sqrt(Float) -> Float`       (`f64::sqrt`; NaN on neg).
            // `pow` joined later (index 77): the `pow` libcall is bridged
            // per backend — cranelift external symbol, LLVM `llvm.pow.f64`,
            // wasm `env` import — so four-way byte-equality holds.
            abs_float(),
            floor_float_to_int(),
            ceil_float_to_int(),
            round_float_to_int(),
            sqrt_float(),
            // Wave R8: scalar / Bool / String-returning string stdlib.
            // Appended at the tail (indices 52+) so every position-pinned
            // index above stays put — existing-construct cranelift/llvm
            // bytes are unchanged, so GENERATOR_VERSION does not move.
            // Each body is purely byte-level (record header read, byte
            // loads/stores, scratch alloc + memcpy, `BitAnd` char-boundary
            // test) — no UTF-8 decode or `Op::Trap` — so it lowers four-way
            // (tree-walk == cranelift == llvm-native == llvm-wasm),
            // byte-exact with the tree-walk oracle.
            //   * `52` — `len(String) -> Int` (free-call byte length;
            //             same op-stream as `length`).
            //   * `53` — `ends_with(String, String) -> Bool` (suffix
            //             byte compare; sibling to `starts_with`).
            //   * `54` — `replace(String, String, String) -> String`
            //             (non-overlapping byte substring replace-all,
            //             empty-`from` inserts at every char boundary).
            // `trim` / `trim_start` / `trim_end` stay capped: a
            // `char::is_whitespace()`-exact trim needs the UTF-8 decoder +
            // `__is_whitespace` helper + `Op::Trap { InvalidUtf8 }` seam
            // the LLVM-native / wasm backends do not lower (same seam that
            // keeps `upper` / `title` / `nfd` at tree-walk + cranelift —
            // see `relon-codegen-llvm/tests/phase0b_unicode.rs`).
            // `matches` stays capped (needs the full `regex` engine, no
            // wasm-portable body). `split` is lowered four-way as of Wave
            // R15 (index 56 below) for a non-empty string-literal separator;
            // an empty separator stays capped (the tree-walk oracle errors on
            // it rather than producing a value — see `string_ops::split_string`
            // and the `lower_fn_call.split_empty_separator` cap).
            len_string_to_int(),
            ends_with_string(),
            replace_string(),
            // Wave R9: Bool-returning `is_*` validator stdlib. Appended at
            // the tail (index 55) so every position-pinned index above
            // stays put — existing-construct cranelift/llvm bytes are
            // unchanged, so GENERATOR_VERSION does not move. The body is
            // purely byte-level (record-header read, byte loads, integer
            // compares, `BitAnd` / `Add` / `Sub` / `Mul`) — no UTF-8
            // decode, no `Op::Trap`, no integer division/remainder — so it
            // lowers four-way (tree-walk == cranelift == llvm-native ==
            // llvm-wasm), byte-exact with the tree-walk `is_uuid_str`
            // oracle.
            //   * `55` — `is_uuid(String) -> Bool` (RFC 4122 canonical
            //             text form, case-insensitive).
            // Sibling validators stay capped: `is_email` / `is_uri` walk
            // `s.chars()` (UTF-8 decode seam — LLVM/wasm segfault),
            // `is_ipv4` / `is_ipv6` route through `core::net` parsers (no
            // wasm-portable body), and `is_iso_date` needs integer
            // division / remainder (leap-year `% 4 / % 100 / % 400`) for
            // which the IR exposes no `DivS` / `RemS` op.
            is_uuid_string(),
            // Wave R15: `split(String, String) -> List<String>`. Appended at
            // the tail (index 56) so every position-pinned index above stays
            // put — existing-construct cranelift/llvm bytes are unchanged, so
            // GENERATOR_VERSION does not move (the body is a brand-new bundled
            // entry, not a re-emit of an existing construct). The body is
            // purely byte-level (record-header read, byte loads/stores,
            // `Memcpy`, integer compares / `BitAnd`) — no UTF-8 decode, no
            // `Op::Trap` — building a self-contained scratch `List<String>`
            // pointer-array record byte-identical to `write_list_string`, so
            // it lowers four-way (tree-walk == cranelift == llvm-native ==
            // llvm-wasm) and returns in place through the shared
            // `inplace_return` decoder (Wave R13 wired the wasm in-place
            // `List<String>` decode for scratch-built results).
            //   * `56` — `split(String, sep: String) -> List<String>`
            //             (non-empty substring separator; N+1 segments,
            //             data-dependent count — leading / trailing /
            //             consecutive cuts each yield an empty segment,
            //             empty input yields `[""]`, no-match yields the whole
            //             string). The empty-separator case stays capped: the
            //             tree-walk oracle rejects it with a loud
            //             `UnsupportedOperator` rather than producing a value,
            //             so there is no value to be byte-equal to.
            split_string(),
            // Enum-like list-producing maps. Appended at indices 57..59:
            // each body returns a 4-byte pointer-array ListList whose slots
            // point at variant records produced by the closure.
            list_int_map_to_variant_list(),
            list_float_map_to_variant_list(),
            list_string_map_to_variant_list(),
            // Pointer-array list filter. Appended at index 60.
            list_list_filter(),
            // JSON-Schema numeric / size predicates lowered four-way for
            // their statically-decidable arms. Appended at the tail
            // (indices 61..64) so every position-pinned index above stays
            // put — existing-construct cranelift/llvm bytes are unchanged,
            // so GENERATOR_VERSION does not move. The lowering peepholes
            // (`try_lower_predicate_math` / `try_lower_size_in_range`)
            // inspect the operand IR types and route to these bodies;
            // unsupported arms cap loudly (no silent wrong value):
            //   * `61` — `multiple_of(Int, Int) -> Bool` (`d == 0 ? false :
            //             n % d == 0`; the `d == 0` guard gates the
            //             `Op::Mod(I64)` so a zero divisor never traps).
            //             Float arms stay capped: `Op::Mod(F64)` has no
            //             native cranelift / wasm remainder and the
            //             oracle's `fract().abs() < 1e-9` tolerance has no
            //             four-way body.
            //   * `62` — `in_range(n, lo, hi) -> Bool` (all-`F64`; the
            //             oracle widens every arg to f64, so the peephole
            //             widens any Int arg with `ConvertI64ToF64` first).
            //   * `63` — `size_in_range(List<_>, lo, hi) -> Bool`
            //             (`minItems` / `maxItems`; element count from the
            //             shared `[len: u32 LE]` record header).
            //   * `64` — `dict_size_in_range(Dict, lo, hi) -> Bool`
            //             (`minProperties` / `maxProperties`; entry count
            //             from the same header — shares the op-stream with
            //             index 63). The `size_in_range` String arm stays
            //             capped: the oracle counts Unicode code points
            //             (`chars().count()`), which needs the UTF-8 decode
            //             seam LLVM-native / wasm do not lower.
            multiple_of_int(),
            in_range_float(),
            size_in_range_list(),
            dict_size_in_range(),
            // Whitespace-stripping String builders + ASCII-structured
            // validators, lowered four-way now that the UTF-8 decode seam
            // (R14) is in place. Appended at the tail (indices 65..69) so
            // every position-pinned index above stays put — GENERATOR_VERSION
            // does not move:
            //   * `65` — `trim(s) -> String` (Rust `str::trim`).
            //   * `66` — `trim_start(s) -> String` (Rust `str::trim_start`).
            //   * `67` — `trim_end(s) -> String` (Rust `str::trim_end`).
            //     All three forward-decode the input (trapping `InvalidUtf8`)
            //     and use the `__is_whitespace` helper (Unicode `White_Space`,
            //     i.e. `char::is_whitespace`), then memcpy the surviving
            //     slice into a fresh record.
            //   * `68` — `is_email(s) -> Bool` (byte-level ASCII structure;
            //     non-ASCII bytes fail the local / label char class exactly
            //     as the codepoint-level oracle rejects them).
            //   * `69` — `is_uri(s) -> Bool` (scheme `:` non-empty-rest,
            //     ASCII scheme grammar). Both are purely byte-level (no UTF-8
            //     decode, no trap) — the same discipline `is_uuid` uses.
            // Appended at the tail (index 70) so every position-pinned index
            // above stays put — GENERATOR_VERSION does not move:
            //   * `70` — `is_iso_date(s) -> Bool` (RFC 3339 `YYYY-MM-DD`:
            //     byte-level shape + integer date arithmetic; the leap-year
            //     test uses `Op::Mod(I32)` against non-zero constant divisors,
            //     so a byte-exact four-way body is constructible).
            trim_string(),
            trim_start_string(),
            trim_end_string(),
            is_email_string(),
            is_uri_string(),
            is_iso_date_string(),
            // Stdlib tail wave: the last five tree-walk-only stdlib
            // functions (`every` / `some` / `unique` / `pow`; `count` is a
            // pure peephole over the shared `[len: u32 LE]` record header
            // and needs no bundled body). Appended at the tail (indices
            // 71..77) so every position-pinned index above stays put. The
            // bodies reuse existing constructs (`CallClosure`, typed
            // loads, `Eq`, nested `Block`/`Loop`) — but this wave also
            // introduces `Op::F64Pow` for index 77, which DOES move
            // GENERATOR_VERSION (see
            // `relon-codegen-cranelift::object_cache_integration`).
            //   * `71` — `list_int_every(List<Int>, Closure<I64 -> Bool>)
            //             -> Bool` (short-circuit on first `false`;
            //             empty → `true`).
            //   * `72` — `list_int_some(List<Int>, Closure<I64 -> Bool>)
            //             -> Bool` (short-circuit on first `true`;
            //             empty → `false`).
            //   * `73` — `list_float_every(List<Float>, ...) -> Bool`.
            //   * `74` — `list_float_some(List<Float>, ...) -> Bool`.
            //   * `75` — `list_int_unique(List<Int>) -> Bool` (JSON Schema
            //             `uniqueItems`: O(N²) pairwise scan, `false` on
            //             the first `i < j` equal pair).
            //   * `76` — `list_float_unique(List<Float>) -> Bool` (Float
            //             equality is OrderedFloat on every backend —
            //             `NaN == NaN` true, `-0.0 == 0.0` true — matching
            //             the oracle's `Value::Float` `PartialEq`).
            //   * `77` — `pow(Float, Float) -> Float` (`f64::powf` via
            //             `Op::F64Pow`; the peephole widens Int args with
            //             `ConvertI64ToF64`, matching the oracle's
            //             per-operand `to_f64_val`). `List<String>` /
            //             `List<Bool>` / pointer-array `every` / `some` /
            //             `unique` shapes stay capped (no four-way String
            //             `Eq` / no `String -> Bool` predicate surface).
            list_int_every(),
            list_int_some(),
            list_float_every(),
            list_float_some(),
            list_int_unique(),
            list_float_unique(),
            pow_float(),
        ]
    })
}
