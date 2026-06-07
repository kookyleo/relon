//! Phase 4.a/4.b bundled stdlib registry.
//!
//! v1 stdlib is **bundled**: every compiled module prepends the
//! builtin stdlib function bodies into its wasm function table before
//! any user-defined function. The codegen pass turns each
//! [`StdlibFunction`] into a wasm `func` (params + locals + body) at
//! a fixed index — `0..N` for the N builtin functions, then user
//! functions at `N..N + user_fn_count`.
//!
//! The lowering pass uses [`stdlib_function_index`] to look up the
//! wasm-level callee slot when it lowers stdlib free calls like
//! `length(s)` / `abs(x)` / `min(a, b)` into [`crate::ir::Op::Call`].
//! Method-call form (`s.length()`, `xs.length()`, `s.is_empty()`)
//! resolves through [`stdlib_method_index`], which maps the
//! `(receiver_ir_type, method_name)` pair to a registry index so the
//! same surface name can dispatch to different bodies based on the
//! receiver's IR type (e.g. `String::length` vs `List<Int>::length`).
//!
//! Indices are **stable** across compiles for a given Relon version
//! because [`builtin_stdlib`] returns the list in fixed order;
//! reordering breaks the wire format for any pre-compiled module, so
//! future phases that add functions must always **append**.
//!
//! Phase 4.a scope:
//!   * `length(s: String) -> Int` — byte length of a String record.
//!
//! Phase 4.b scope (this phase):
//!   * `list_int_length(xs: List<Int>) -> Int` — element count of a
//!     `List<Int>` record (record layout shares the `u32 LE` length
//!     prefix with `String`).
//!   * `abs(x: Int) -> Int` — absolute value via wasm `select`.
//!   * `min(a: Int, b: Int) -> Int` / `max(a: Int, b: Int) -> Int` —
//!     two-arg numeric min/max via wasm `select`.
//!   * `is_empty(s: String) -> Bool` — zero-length predicate, reusing
//!     [`crate::ir::Op::ReadStringLen`] + [`crate::ir::Op::Eq`].
//!
//! Phase 4.c-2 scope (this phase):
//!   * `concat(a: String, b: String) -> String` — allocate scratch,
//!     write the combined record, return the new pointer.
//!   * `upper(s: String) -> String` / `lower(s: String) -> String` —
//!     ASCII-only case fold; multi-byte UTF-8 sequences pass through
//!     untouched (a fuller Unicode pass is on the v3+ roadmap).
//!   * `substring(s: String, start: Int, len: Int) -> String` —
//!     bounds-checked slice; out-of-range bounds trap as
//!     `IndexOutOfBounds`.
//!   * `starts_with(s: String, prefix: String) -> Bool` — short-
//!     circuit prefix predicate.
//!   * `list_int_sum(xs: List<Int>) -> Int` — count + iterate +
//!     accumulate.
//!   * `list_int_max(xs: List<Int>) -> Int` — same shape; empty list
//!     traps as `EmptyList` (call-site protected; surfaces a
//!     diagnostic instead of a meaningless i64 minimum).
//!
//! Out of scope (deferred to Phase 10-a closure work):
//!   * `fold(xs, init, f)` / `map(xs, f)` / `filter(xs, p)` — require
//!     first-class closures on the wasm side.
//!   * Multi-byte UTF-8 aware `upper` / `lower` — needs a
//!     codepoint-level walker; the ASCII fast path covers the v1
//!     surface.
//!
//! See `docs/internal/wasm-backend-design-draft.md` Section 4 for the
//! bundling rationale.
//!
//! ## File layout (review-improvement P3, 2026-05-21)
//!
//! The bundled stdlib was a single 8.7k-line file at
//! `relon-ir/src/stdlib.rs` through the F-D7-D landing. It has since
//! been split into the following sub-modules; the top-level surface
//! ([`StdlibFunction`], [`builtin_stdlib`], [`stdlib_function_index`],
//! [`stdlib_function_count`], [`stdlib_method_index`],
//! [`stdlib_closure_arg_signature`]) is re-exported here so downstream
//! consumers see no API change.
//!
//! * `signatures` — [`StdlibFunction`] entry type + the stable
//!   `*_INDEX` constants pinning internal helper slots.
//! * `registry` — [`builtin_stdlib`], the ordered list whose
//!   declaration order is part of the wasm wire format.
//! * `index` — name / receiver / closure-arg lookup helpers.
//! * `defs` — non-Unicode body builders (length / arithmetic /
//!   is_empty / concat / substring / starts_with / contains /
//!   list_int_*) plus the shared `tt` op-tag helper.
//! * `case_fold` — case-fold body builders (`upper` / `lower` /
//!   `title` and locale variants) plus the `__casefold_lookup`,
//!   `__is_combining_mark`, `__is_whitespace`,
//!   `__full_casefold_lookup`, `__final_sigma_check` internal helpers.
//! * `normalization` — UAX #15 NFD / NFKD / NFC / NFKC bodies plus
//!   the `__decomp_lookup`, `__ccc_lookup`, `__compose_lookup`
//!   helpers.

mod case_fold;
mod defs;
mod index;
mod normalization;
mod registry;
mod signatures;
mod string_ops;
mod validators;

pub use index::{
    stdlib_closure_arg_signature, stdlib_function_count, stdlib_function_index, stdlib_method_index,
};
pub use registry::builtin_stdlib;
pub use signatures::{
    StdlibFunction, CONCAT_INDEX, CONTAINS_INDEX, GLOB_MATCH_INDEX, IS_EMPTY_INDEX, LENGTH_INDEX,
    SUBSTRING_INDEX,
};

#[cfg(test)]
mod tests {
    use super::signatures::{CASEFOLD_LOOKUP_INDEX, COMBINING_MARK_INDEX, IS_WHITESPACE_INDEX};
    use super::*;
    use crate::ir::IrType;

    #[test]
    fn length_index_is_zero() {
        assert_eq!(stdlib_function_index("length"), Some(0));
    }

    #[test]
    fn unknown_returns_none() {
        assert_eq!(stdlib_function_index("definitely_not_real"), None);
    }

    #[test]
    fn count_matches_list() {
        assert_eq!(stdlib_function_count() as usize, builtin_stdlib().len());
    }

    #[test]
    fn phase4b_indices_are_stable() {
        // The order is part of the wire format — these indices must
        // not shift across releases. Future stdlib additions must
        // **append** so existing pre-compiled modules keep resolving
        // the correct callee.
        assert_eq!(stdlib_function_index("length"), Some(0));
        assert_eq!(stdlib_function_index("list_int_length"), Some(1));
        assert_eq!(stdlib_function_index("abs"), Some(2));
        assert_eq!(stdlib_function_index("min"), Some(3));
        assert_eq!(stdlib_function_index("max"), Some(4));
        assert_eq!(stdlib_function_index("is_empty"), Some(5));
    }

    #[test]
    fn method_dispatch_resolves_length_per_receiver() {
        assert_eq!(stdlib_method_index(IrType::String, "length"), Some(0));
        assert_eq!(stdlib_method_index(IrType::ListInt, "length"), Some(1));
        assert_eq!(stdlib_method_index(IrType::String, "is_empty"), Some(5));
        // Unsupported receiver shapes surface as `None` so lowering
        // emits its own diagnostic.
        assert_eq!(stdlib_method_index(IrType::I64, "length"), None);
        assert_eq!(stdlib_method_index(IrType::String, "abs"), None);
    }

    /// v3+ a-4: the hardcoded `CASEFOLD_LOOKUP_INDEX` constant must
    /// stay in lock-step with the actual registry slot. Breaking this
    /// invariant would make `upper` / `lower` call the wrong stdlib
    /// body — the wasm verifier would catch the type mismatch but
    /// the diagnostic would be opaque. The unit test pins the slot.
    #[test]
    fn casefold_lookup_index_is_stable() {
        assert_eq!(
            stdlib_function_index("__casefold_lookup"),
            Some(CASEFOLD_LOOKUP_INDEX)
        );
    }

    /// v3++ b-4: `__is_combining_mark` lives at the hardcoded
    /// [`COMBINING_MARK_INDEX`]. Same cycle-breaking rationale as
    /// [`CASEFOLD_LOOKUP_INDEX`] — if this constant ever drifts the
    /// `title` / `upper` / `lower` bodies will call the wrong helper.
    #[test]
    fn combining_mark_index_is_stable() {
        assert_eq!(
            stdlib_function_index("__is_combining_mark"),
            Some(COMBINING_MARK_INDEX)
        );
    }

    /// v3++ b-4: `__is_whitespace` lives at the hardcoded
    /// [`IS_WHITESPACE_INDEX`].
    #[test]
    fn is_whitespace_index_is_stable() {
        assert_eq!(
            stdlib_function_index("__is_whitespace"),
            Some(IS_WHITESPACE_INDEX)
        );
    }

    /// v3++ b-4: `title(String) -> String` registers as a String
    /// method. The free-call form `title(s)` and method form
    /// `s.title()` route through the same stdlib body.
    #[test]
    fn title_string_method_dispatch_resolves() {
        let title_idx = stdlib_function_index("title").expect("title stdlib slot");
        assert_eq!(
            stdlib_method_index(IrType::String, "title"),
            Some(title_idx)
        );
    }

    /// v3++ b-4: the post-b-4 stdlib list appends `__is_combining_mark`,
    /// `__is_whitespace`, and `title` after the existing v3+ a-4
    /// `__casefold_lookup` slot. Pin the indices so future inserts go
    /// to the tail rather than shifting these.
    #[test]
    fn b4_indices_are_stable() {
        assert_eq!(stdlib_function_index("__casefold_lookup"), Some(20));
        assert_eq!(stdlib_function_index("__is_combining_mark"), Some(21));
        assert_eq!(stdlib_function_index("__is_whitespace"), Some(22));
        assert_eq!(stdlib_function_index("title"), Some(23));
    }
}

#[cfg(test)]
mod b5_index_tests {
    use super::*;
    #[test]
    fn b5_indices_are_stable() {
        assert_eq!(stdlib_function_index("__decomp_lookup"), Some(24));
        assert_eq!(stdlib_function_index("__ccc_lookup"), Some(25));
        assert_eq!(stdlib_function_index("__compose_lookup"), Some(26));
        assert_eq!(stdlib_function_index("nfd"), Some(27));
        assert_eq!(stdlib_function_index("nfkd"), Some(28));
        assert_eq!(stdlib_function_index("nfc"), Some(29));
        assert_eq!(stdlib_function_index("nfkc"), Some(30));
    }
}

#[cfg(test)]
mod b6_b7_index_tests {
    use super::signatures::{FINAL_SIGMA_CHECK_INDEX, FULL_CASEFOLD_LOOKUP_INDEX};
    use super::*;

    /// v3++ b-6 locale-aware bodies retain their `31..=33` slots once
    /// the b-7 helpers are appended.
    #[test]
    fn b6_indices_are_stable() {
        assert_eq!(stdlib_function_index("upper_locale"), Some(31));
        assert_eq!(stdlib_function_index("lower_locale"), Some(32));
        assert_eq!(stdlib_function_index("title_locale"), Some(33));
    }

    /// v3++ b-7 reframed FULL / final-sigma helpers live at `34` /
    /// `35`. Pinning these via assertion guards against accidental
    /// reordering that would silently break the per-cp dispatch from
    /// `case_fold_body_inner`.
    #[test]
    fn b7_indices_are_stable() {
        assert_eq!(
            stdlib_function_index("__full_casefold_lookup"),
            Some(FULL_CASEFOLD_LOOKUP_INDEX)
        );
        assert_eq!(stdlib_function_index("__full_casefold_lookup"), Some(34));
        assert_eq!(
            stdlib_function_index("__final_sigma_check"),
            Some(FINAL_SIGMA_CHECK_INDEX)
        );
        assert_eq!(stdlib_function_index("__final_sigma_check"), Some(35));
    }
}

#[cfg(test)]
mod d7d_index_tests {
    use super::*;
    use crate::ir::IrType;

    /// F-D7-D: `contains(String, String) -> Bool` lands at slot 36 —
    /// the slot the trace recorder pins via
    /// [`relon_trace_recorder::lowering::STDLIB_IDX_CONTAINS`]. Any
    /// reordering that drifts the slot would either silently route a
    /// different op through `TraceOp::StrContains` or break the
    /// recorder's drift guard. The `stdlib_index_consistency` test in
    /// `relon-trace-recorder` cross-checks this against the recorder's
    /// hardcoded constant.
    #[test]
    fn contains_index_is_36() {
        assert_eq!(stdlib_function_index("contains"), Some(36));
    }

    /// F-D7-D: method-form dispatch `s.contains(needle)` resolves to
    /// the same slot as the free-call form `contains(s, needle)`.
    #[test]
    fn contains_method_dispatch_resolves() {
        let idx = stdlib_function_index("contains").expect("contains stdlib slot");
        assert_eq!(stdlib_method_index(IrType::String, "contains"), Some(idx));
    }
}

#[cfg(test)]
mod glob_match_index_tests {
    use super::signatures::GLOB_MATCH_INDEX;
    use super::*;
    use crate::ir::IrType;

    /// 2026-05-21: `glob_match(String, String) -> Bool` lands at slot
    /// 37 — the slot cranelift's `emit_call_stdlib` intercepts to
    /// route through the `RelonGlobMatch` vtable indirection.
    /// Reordering the bundle without bumping the constant would
    /// silently route a different body through the host helper,
    /// almost certainly with a fatal ABI mismatch on the first call.
    #[test]
    fn glob_match_index_is_37() {
        assert_eq!(stdlib_function_index("glob_match"), Some(GLOB_MATCH_INDEX));
        assert_eq!(stdlib_function_index("glob_match"), Some(37));
    }

    /// Method-form dispatch `s.glob_match(pattern)` resolves to the
    /// same slot as the free-call form `glob_match(s, pattern)`.
    #[test]
    fn glob_match_method_dispatch_resolves() {
        let idx = stdlib_function_index("glob_match").expect("glob_match stdlib slot");
        assert_eq!(stdlib_method_index(IrType::String, "glob_match"), Some(idx));
    }

    /// The bundled stdlib count. `glob_match` (37) and `contains` (36)
    /// keep their pinned slots; `list_list_length` was appended at index
    /// 38. Wave R3b appended the typed `List<Float>` / cross-type HOF
    /// bodies at indices 39..43; Wave R3c appended the String-result list
    /// `map` family at indices 44..46; Wave R7 appended the scalar Float
    /// math bodies at indices 47..51; Wave R8 appended the byte-level
    /// string ops (`len` / `ends_with` / `replace`) at indices 52..54;
    /// Wave R9 appended the Bool `is_uuid` validator at index 55; Wave R15
    /// appended `split` -> List<String> at index 56 — all at the tail so
    /// every position-pinned index above stays put.
    /// Pinning the count catches accidental double-registrations.
    #[test]
    fn bundle_has_57_entries() {
        assert_eq!(stdlib_function_count(), 57);
        assert_eq!(stdlib_function_index("glob_match"), Some(37));
        assert_eq!(stdlib_function_index("list_list_length"), Some(38));
        // Wave R3b tail appends (order-pinned wire format).
        assert_eq!(stdlib_function_index("list_float_map"), Some(39));
        assert_eq!(stdlib_function_index("list_float_filter"), Some(40));
        assert_eq!(stdlib_function_index("list_float_fold"), Some(41));
        assert_eq!(stdlib_function_index("list_int_map_to_float"), Some(42));
        assert_eq!(stdlib_function_index("list_float_map_to_int"), Some(43));
        // Wave R3c tail appends.
        assert_eq!(stdlib_function_index("list_string_map"), Some(44));
        assert_eq!(stdlib_function_index("list_int_map_to_string"), Some(45));
        assert_eq!(stdlib_function_index("list_float_map_to_string"), Some(46));
        // Wave R7 tail appends (scalar Float math).
        assert_eq!(stdlib_function_index("abs_float"), Some(47));
        assert_eq!(stdlib_function_index("floor"), Some(48));
        assert_eq!(stdlib_function_index("ceil"), Some(49));
        assert_eq!(stdlib_function_index("round"), Some(50));
        assert_eq!(stdlib_function_index("sqrt"), Some(51));
        // Wave R8 tail appends (byte-level string ops).
        assert_eq!(stdlib_function_index("len"), Some(52));
        assert_eq!(stdlib_function_index("ends_with"), Some(53));
        assert_eq!(stdlib_function_index("replace"), Some(54));
        // Wave R9 tail append (Bool validator).
        assert_eq!(stdlib_function_index("is_uuid"), Some(55));
        // Wave R15 tail append (`split` -> List<String>).
        assert_eq!(stdlib_function_index("split"), Some(56));
    }
}

/// Bytecode-coverage-expansion B-1: drift guard for the public
/// `CONCAT_INDEX` / `SUBSTRING_INDEX` / `CONTAINS_INDEX` constants the
/// bytecode VM short-circuits against. The trace recorder mirrors the
/// same slots via its own `STDLIB_IDX_*` constants — see
/// `relon-trace-recorder` for the parallel guard. Any reordering of
/// the bundle that drifts these slots silently re-routes a different
/// op through `BcOp::Str*` and almost certainly produces a wrong
/// answer on the first call.
#[cfg(test)]
mod str_stdlib_index_consistency_tests {
    use super::*;
    use signatures::{CONCAT_INDEX, CONTAINS_INDEX, IS_EMPTY_INDEX, LENGTH_INDEX, SUBSTRING_INDEX};

    #[test]
    fn length_index_matches_registry() {
        assert_eq!(stdlib_function_index("length"), Some(LENGTH_INDEX));
        assert_eq!(LENGTH_INDEX, 0);
    }

    #[test]
    fn is_empty_index_matches_registry() {
        assert_eq!(stdlib_function_index("is_empty"), Some(IS_EMPTY_INDEX));
        assert_eq!(IS_EMPTY_INDEX, 5);
    }

    #[test]
    fn concat_index_matches_registry() {
        assert_eq!(stdlib_function_index("concat"), Some(CONCAT_INDEX));
        assert_eq!(CONCAT_INDEX, 6);
    }

    #[test]
    fn substring_index_matches_registry() {
        assert_eq!(stdlib_function_index("substring"), Some(SUBSTRING_INDEX));
        assert_eq!(SUBSTRING_INDEX, 9);
    }

    #[test]
    fn contains_index_matches_registry() {
        assert_eq!(stdlib_function_index("contains"), Some(CONTAINS_INDEX));
        assert_eq!(CONTAINS_INDEX, 36);
    }
}
