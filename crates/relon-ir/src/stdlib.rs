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
//!     [`Op::ReadStringLen`] + [`Op::Eq`].
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

use crate::ir::{IrType, Op, TaggedOp, TrapKind};
use relon_parser::TokenRange;

/// v3+ a-4: stable slot of the `__casefold_lookup` internal helper in
/// the [`builtin_stdlib`] registry. Hardcoded so the `upper` / `lower`
/// body builders can emit the matching `Op::Call { fn_index }` without
/// recursing back into [`builtin_stdlib`] (which would infinite-loop
/// because those builders are called *from* [`builtin_stdlib`]). The
/// constant is sanity-checked at unit-test time in
/// `casefold_lookup_index_is_stable`; future additions to the
/// registry must `append` (per the module doc-comment) so this
/// constant never needs to change once it has shipped.
pub(crate) const CASEFOLD_LOOKUP_INDEX: u32 = 20;

/// v3++ b-4: stable slot of the `__is_combining_mark` internal helper
/// in the [`builtin_stdlib`] registry. Same cycle-breaking rationale
/// as [`CASEFOLD_LOOKUP_INDEX`] — the rewritten `title` / `upper` /
/// `lower` body builders emit `Op::Call { fn_index = COMBINING_MARK_INDEX }`
/// without re-entering [`builtin_stdlib`]. Unit-tested by
/// `combining_mark_index_is_stable`.
pub(crate) const COMBINING_MARK_INDEX: u32 = 21;

/// v3++ b-4: stable slot of the `__is_whitespace` internal helper in
/// the [`builtin_stdlib`] registry. Only the `title` body calls it;
/// `upper` / `lower` do not need word-boundary detection. Same
/// cycle-breaking rationale as [`CASEFOLD_LOOKUP_INDEX`].
pub(crate) const IS_WHITESPACE_INDEX: u32 = 22;

/// v3++ b-5: stable slot of the `__decomp_lookup(cp, table_addr) -> i32`
/// internal helper in the [`builtin_stdlib`] registry. Returns the
/// 32-bit-packed `(pool_off << 8) | pool_len` lookup result, or `0`
/// when `cp` is not in the table (pool_len of `0` is a sentinel - the
/// table never has a zero-length mapping). The four normalization
/// bodies share this helper across both NFD and NFKD table families;
/// the table address is the discriminator.
pub(crate) const DECOMP_LOOKUP_INDEX: u32 = 24;

/// v3++ b-5: stable slot of the `__ccc_lookup(cp, table_addr) -> i32`
/// internal helper. Returns the Canonical_Combining_Class of `cp`, or
/// `0` when `cp` is not in the table (matches the UCD convention that
/// absent entries default to Not_Reordered).
pub(crate) const CCC_LOOKUP_INDEX: u32 = 25;

/// v3++ b-5: stable slot of the `__compose_lookup(first, second,
/// table_addr) -> i32` internal helper. Returns the composed code
/// point when the `(first, second)` pair is present in the canonical
/// composition table, or `-1` when no composition is defined.
/// `-1` (a u32-as-i32 of `0xFFFF_FFFF`) is safe as a sentinel because
/// Unicode caps codepoints at `U+10FFFF`.
pub(crate) const COMPOSE_LOOKUP_INDEX: u32 = 26;

/// v3++ b-7 reframed: stable slot of the
/// `__full_casefold_lookup(cp, table_addr) -> i32` internal helper.
///
/// Binary-searches the FULL multi-codepoint folding table (20-byte
/// stride: `(in: u32, out0: u32, out1: u32, out2: u32, out_len: u32)`)
/// and returns the absolute address of the matched entry (i.e.
/// `table_addr + 4 + idx * 20`), or `0` on miss. Callers load `out_len`
/// from `entry + 16` and the up-to-three output codepoints from
/// `entry + 4 / 8 / 12`.
///
/// The address-return ABI keeps the helper signature at a single i32
/// while letting callers fetch every output slot without a second
/// helper round-trip — matches the shape of `__decomp_lookup` (which
/// also returns a packed integer rather than a scratch handle).
pub(crate) const FULL_CASEFOLD_LOOKUP_INDEX: u32 = 34;

/// v3++ b-7 reframed: stable slot of the
/// `__final_sigma_check(s_ptr, byte_offset, cased_addr, ignorable_addr) -> i32`
/// helper. Returns `1` when `Σ` at `byte_offset` in the input UTF-8
/// string `s_ptr` is at the end of a word per UAX #21 Final_Sigma —
/// i.e. preceded by at least one cased codepoint (skipping case-
/// ignorables), and either followed by only case-ignorables until end
/// of string or followed by a non-cased non-ignorable codepoint.
/// Returns `0` otherwise.
///
/// `s_ptr` is a String record pointer (the leading `u32 LE` length
/// header lives at `s_ptr + 0`; the payload bytes start at
/// `s_ptr + 4`). The helper does its own UTF-8 reverse / forward
/// decoding so callers don't need to materialise a codepoint array.
pub(crate) const FINAL_SIGMA_CHECK_INDEX: u32 = 35;

/// v3++ b-4 case-folding mode driving [`case_fold_body`].
///
/// `Upper` and `Lower` are straight per-codepoint folds against the
/// matching simple-folding table. `Title` extends the pipeline with a
/// per-word boundary tracker plus combining-mark detection — the
/// first cased codepoint of each word maps through the upper table,
/// every subsequent codepoint maps through the lower table, and
/// Unicode combining marks (Mn + Mc + Me) never reset the boundary
/// because they belong to their base codepoint's grapheme cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CaseFoldMode {
    /// `upper(s)` — every codepoint goes through the upper table.
    Upper,
    /// `lower(s)` — every codepoint goes through the lower table.
    Lower,
    /// `title(s)` — first cased codepoint of each whitespace-separated
    /// word maps through the upper table; the rest map through the
    /// lower table; combining marks pass through unchanged and do
    /// **not** flip the word-boundary flag.
    Title,
}

/// One bundled stdlib function — name, signature, and IR body.
///
/// Body uses the same op stream the lowering pass would produce for a
/// user-defined function: `LocalGet` indices refer to the function's
/// declared `params` slots in declaration order; the body must end
/// with a value on top of the virtual stack and an `Op::Return`. The
/// stdlib bodies are hand-written so they sidestep the lowering pass
/// entirely.
#[derive(Debug, Clone)]
pub struct StdlibFunction {
    /// Surface-level name the lowering pass looks up via
    /// [`stdlib_function_index`].
    pub name: &'static str,
    /// Parameter types in declaration order. Each maps to a wasm-
    /// level function-parameter slot consumed via `Op::LocalGet`.
    pub params: Vec<IrType>,
    /// Return type. Each stdlib function returns exactly one value.
    pub ret: IrType,
    /// IR op stream forming the function body.
    pub body: Vec<TaggedOp>,
}

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
pub fn builtin_stdlib() -> Vec<StdlibFunction> {
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
}

/// Hand-written body for `length(s: String) -> Int`.
///
/// Equivalent wasm:
/// ```text
/// (func (param i32) (result i64)
///   local.get 0      ;; the String pointer (absolute wasm memory address)
///   i32.load offset=0 align=2
///   i64.extend_i32_u
/// )
/// ```
fn length_string_to_int() -> StdlibFunction {
    let range = TokenRange::default();
    StdlibFunction {
        name: "length",
        params: vec![IrType::String],
        ret: IrType::I64,
        body: vec![
            // Push the param slot (the String pointer).
            TaggedOp {
                op: Op::LocalGet(0),
                range,
            },
            // Pop the pointer, push the u32 length widened to i64.
            TaggedOp {
                op: Op::ReadStringLen,
                range,
            },
            // End-of-function marker (codegen will translate the
            // implicit value-on-stack into the wasm `end`).
            TaggedOp {
                op: Op::Return,
                range,
            },
        ],
    }
}

/// Hand-written body for `list_int_length(xs: List<Int>) -> Int`.
///
/// `List<Int>` shares the leading `[len: u32 LE]` record header with
/// `String` (the record continues with a 4-byte pad and the i64
/// elements), so the body is byte-identical to the `length` String
/// body — just typed against the `ListInt` slot at the IR level so
/// lowering can dispatch on the receiver's IR type.
fn list_int_length_to_int() -> StdlibFunction {
    let range = TokenRange::default();
    StdlibFunction {
        name: "list_int_length",
        params: vec![IrType::ListInt],
        ret: IrType::I64,
        body: vec![
            TaggedOp {
                op: Op::LocalGet(0),
                range,
            },
            TaggedOp {
                op: Op::ReadStringLen,
                range,
            },
            TaggedOp {
                op: Op::Return,
                range,
            },
        ],
    }
}

/// Phase 10-c length bodies for the new list types.
///
/// Every list record carries the same `[len: u32 LE]` prefix at offset
/// 0 (the trailing payload differs by element type but the header
/// shape is uniform). So the body is byte-identical to
/// [`list_int_length_to_int`] — only the param type tag changes, which
/// drives the IR-level dispatch in [`stdlib_method_index`].
fn list_float_length() -> StdlibFunction {
    let range = TokenRange::default();
    StdlibFunction {
        name: "list_float_length",
        params: vec![IrType::ListFloat],
        ret: IrType::I64,
        body: vec![
            TaggedOp {
                op: Op::LocalGet(0),
                range,
            },
            TaggedOp {
                op: Op::ReadStringLen,
                range,
            },
            TaggedOp {
                op: Op::Return,
                range,
            },
        ],
    }
}

fn list_bool_length() -> StdlibFunction {
    let range = TokenRange::default();
    StdlibFunction {
        name: "list_bool_length",
        params: vec![IrType::ListBool],
        ret: IrType::I64,
        body: vec![
            TaggedOp {
                op: Op::LocalGet(0),
                range,
            },
            TaggedOp {
                op: Op::ReadStringLen,
                range,
            },
            TaggedOp {
                op: Op::Return,
                range,
            },
        ],
    }
}

fn list_string_length() -> StdlibFunction {
    let range = TokenRange::default();
    StdlibFunction {
        name: "list_string_length",
        params: vec![IrType::ListString],
        ret: IrType::I64,
        body: vec![
            TaggedOp {
                op: Op::LocalGet(0),
                range,
            },
            TaggedOp {
                op: Op::ReadStringLen,
                range,
            },
            TaggedOp {
                op: Op::Return,
                range,
            },
        ],
    }
}

fn list_schema_length() -> StdlibFunction {
    let range = TokenRange::default();
    StdlibFunction {
        name: "list_schema_length",
        params: vec![IrType::ListSchema],
        ret: IrType::I64,
        body: vec![
            TaggedOp {
                op: Op::LocalGet(0),
                range,
            },
            TaggedOp {
                op: Op::ReadStringLen,
                range,
            },
            TaggedOp {
                op: Op::Return,
                range,
            },
        ],
    }
}

/// Hand-written body for `abs(x: Int) -> Int`.
///
/// Equivalent wasm:
/// ```text
/// (func (param i64) (result i64)
///   local.get 0          ;; push x  (this becomes the "true" arm of select)
///   i64.const 0
///   local.get 0
///   i64.sub              ;; push -x (the "false" arm of select)
///   local.get 0
///   i64.const 0
///   i64.lt_s             ;; cond: x < 0
///   select               ;; if (x < 0) -x else x
/// )
/// ```
///
/// Stack ordering follows wasm's `select` convention: `[val_true,
/// val_false, cond] -> [result]`, picking `val_true` when `cond` is
/// non-zero. We arrange `[-x, x, x < 0]` so a negative `x` selects
/// `-x` while a non-negative `x` selects `x`.
fn abs_int() -> StdlibFunction {
    let range = TokenRange::default();
    StdlibFunction {
        name: "abs",
        params: vec![IrType::I64],
        ret: IrType::I64,
        body: vec![
            // val_true: -x  (computed as 0 - x).
            TaggedOp {
                op: Op::ConstI64(0),
                range,
            },
            TaggedOp {
                op: Op::LocalGet(0),
                range,
            },
            TaggedOp {
                op: Op::Sub(IrType::I64),
                range,
            },
            // val_false: x.
            TaggedOp {
                op: Op::LocalGet(0),
                range,
            },
            // cond: x < 0.
            TaggedOp {
                op: Op::LocalGet(0),
                range,
            },
            TaggedOp {
                op: Op::ConstI64(0),
                range,
            },
            TaggedOp {
                op: Op::Lt(IrType::I64),
                range,
            },
            // select.
            TaggedOp {
                op: Op::Select { ty: IrType::I64 },
                range,
            },
            TaggedOp {
                op: Op::Return,
                range,
            },
        ],
    }
}

/// Hand-written body for `min(a: Int, b: Int) -> Int`.
///
/// Stack arrangement: push `[a, b, a < b]` so wasm `select` returns
/// `a` when `a < b` and `b` otherwise.
fn min_int() -> StdlibFunction {
    let range = TokenRange::default();
    StdlibFunction {
        name: "min",
        params: vec![IrType::I64, IrType::I64],
        ret: IrType::I64,
        body: vec![
            // val_true: a.
            TaggedOp {
                op: Op::LocalGet(0),
                range,
            },
            // val_false: b.
            TaggedOp {
                op: Op::LocalGet(1),
                range,
            },
            // cond: a < b.
            TaggedOp {
                op: Op::LocalGet(0),
                range,
            },
            TaggedOp {
                op: Op::LocalGet(1),
                range,
            },
            TaggedOp {
                op: Op::Lt(IrType::I64),
                range,
            },
            TaggedOp {
                op: Op::Select { ty: IrType::I64 },
                range,
            },
            TaggedOp {
                op: Op::Return,
                range,
            },
        ],
    }
}

/// Hand-written body for `max(a: Int, b: Int) -> Int`.
///
/// Mirror of [`min_int`] with the comparison flipped.
fn max_int() -> StdlibFunction {
    let range = TokenRange::default();
    StdlibFunction {
        name: "max",
        params: vec![IrType::I64, IrType::I64],
        ret: IrType::I64,
        body: vec![
            TaggedOp {
                op: Op::LocalGet(0),
                range,
            },
            TaggedOp {
                op: Op::LocalGet(1),
                range,
            },
            TaggedOp {
                op: Op::LocalGet(0),
                range,
            },
            TaggedOp {
                op: Op::LocalGet(1),
                range,
            },
            TaggedOp {
                op: Op::Gt(IrType::I64),
                range,
            },
            TaggedOp {
                op: Op::Select { ty: IrType::I64 },
                range,
            },
            TaggedOp {
                op: Op::Return,
                range,
            },
        ],
    }
}

/// Hand-written body for `is_empty(s: String) -> Bool`.
///
/// Reads the record's `u32 LE` length prefix via [`Op::ReadStringLen`]
/// (which widens to `I64`), compares against the i64 constant zero,
/// and returns the `Bool` result of the equality op directly.
fn is_empty_string() -> StdlibFunction {
    let range = TokenRange::default();
    StdlibFunction {
        name: "is_empty",
        params: vec![IrType::String],
        ret: IrType::Bool,
        body: vec![
            TaggedOp {
                op: Op::LocalGet(0),
                range,
            },
            TaggedOp {
                op: Op::ReadStringLen,
                range,
            },
            TaggedOp {
                op: Op::ConstI64(0),
                range,
            },
            TaggedOp {
                op: Op::Eq(IrType::I64),
                range,
            },
            TaggedOp {
                op: Op::Return,
                range,
            },
        ],
    }
}

// ---------------------------------------------------------------------------
// Phase 4.c-2 stdlib bodies.
//
// Conventions shared by every body below:
//
// * `t(op)` shorthand pairs an [`Op`] with a synthetic
//   [`TokenRange::default()`]. The codegen srcmap collapses the entries
//   to a `(0, 0)` source position; stdlib traps surface as
//   `range: TokenRange::default()` at the trap-translate site (the
//   call-site srcmap lookup falls through to the call op's user range
//   in the caller).
// * String record layout: `[len: u32 LE at +0][utf8 bytes from +4]`.
// * List<Int> record layout: `[len: u32 LE at +0][pad: u32 at +4]
//   [i64 elements from +8]`.
// * Bodies that build a new record use `Op::AllocScratchDyn` to
//   reserve space in the module-internal bump heap; the returned
//   address is the absolute wasm-memory base of the new record. No
//   tail-cursor protocol — scratch addresses live outside the
//   `out_buf` and the caller-side `EmitTailRecordFromAbsoluteAddr`
//   handles eventual memcpy into `out_buf` if a Phase 3.b dict
//   literal stores the returned String / List pointer.
// ---------------------------------------------------------------------------

/// Phase 4.c-2 helper: pair an [`Op`] with a synthetic
/// [`TokenRange`]. Keeps the hand-written bodies readable.
fn tt(op: Op) -> TaggedOp {
    TaggedOp {
        op,
        range: TokenRange::default(),
    }
}

/// Hand-written body for `concat(a: String, b: String) -> String`.
///
/// Algorithm:
///   1. Load `len_a = i32.load(a + 0)` and `len_b = i32.load(b + 0)`.
///   2. `total_payload = len_a + len_b`; `record_size = total_payload + 4`.
///   3. `base = alloc_scratch_dyn(record_size)`.
///   4. Write the header: `i32.store(base + 0, total_payload)`.
///   5. `memory.copy(base + 4, a + 4, len_a)`.
///   6. `memory.copy(base + 4 + len_a, b + 4, len_b)`.
///   7. Return `base`.
///
/// Locals (indices relative to the stdlib body's let-area):
///   * 0 — `len_a: I32`
///   * 1 — `len_b: I32`
///   * 2 — `base:  I32`
fn concat_string_string() -> StdlibFunction {
    const LEN_A: u32 = 0;
    const LEN_B: u32 = 1;
    const BASE: u32 = 2;
    StdlibFunction {
        name: "concat",
        params: vec![IrType::String, IrType::String],
        ret: IrType::String,
        body: vec![
            // len_a = load_i32(a, 0)
            tt(Op::LocalGet(0)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: LEN_A,
                ty: IrType::I32,
            }),
            // len_b = load_i32(b, 0)
            tt(Op::LocalGet(1)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: LEN_B,
                ty: IrType::I32,
            }),
            // record_size = len_a + len_b + 4
            tt(Op::LetGet {
                idx: LEN_A,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: LEN_B,
                ty: IrType::I32,
            }),
            tt(Op::Add(IrType::I32)),
            tt(Op::ConstI32(4)),
            tt(Op::Add(IrType::I32)),
            // base = alloc_scratch_dyn(record_size)
            tt(Op::AllocScratchDyn),
            tt(Op::LetSet {
                idx: BASE,
                ty: IrType::I32,
            }),
            // store header: i32.store(base + 0, len_a + len_b)
            tt(Op::LetGet {
                idx: BASE,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: LEN_A,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: LEN_B,
                ty: IrType::I32,
            }),
            tt(Op::Add(IrType::I32)),
            tt(Op::StoreI32AtAbsolute { offset: 0 }),
            // memcpy(base + 4, a + 4, len_a)
            tt(Op::LetGet {
                idx: BASE,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(4)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LetGet {
                idx: LEN_A,
                ty: IrType::I32,
            }),
            tt(Op::MemcpyAtAbsolute),
            // memcpy(base + 4 + len_a, b + 4, len_b)
            tt(Op::LetGet {
                idx: BASE,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(4)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LetGet {
                idx: LEN_A,
                ty: IrType::I32,
            }),
            tt(Op::Add(IrType::I32)),
            tt(Op::LocalGet(1)),
            tt(Op::ConstI32(4)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LetGet {
                idx: LEN_B,
                ty: IrType::I32,
            }),
            tt(Op::MemcpyAtAbsolute),
            // return base
            tt(Op::LetGet {
                idx: BASE,
                ty: IrType::I32,
            }),
            tt(Op::Return),
        ],
    }
}

/// v3+ a-4 Unicode-aware body for `upper(s: String) -> String`.
///
/// Replaces the Phase 4.c-2 ASCII fast-path. The body now decodes
/// each input codepoint from its UTF-8 byte sequence, looks the
/// codepoint up in the simple upper case-folding table embedded in
/// the wasm data section, and re-encodes the (possibly different)
/// output codepoint back into UTF-8. The fold table is keyed off
/// [`Op::CaseFoldTableAddr { upper: true }`]; only mappings that
/// produce a **single** replacement codepoint are honoured (German
/// `ß` -> `SS` and other multi-codepoint cases stay un-folded, a
/// v3++ item).
///
/// v3++ b-4: Unicode combining marks (Mn + Mc + Me) skip the case-
/// fold lookup and pass through verbatim. Marks are non-cased in
/// Unicode, so the lookup happened to be identity in v3+ a-4; the
/// explicit skip keeps the fold table semantically honest and lays
/// groundwork for the v3++ b-6 full-folding pass that may map cased
/// marks through context-sensitive rules.
fn upper_string() -> StdlibFunction {
    case_fold_body("upper", CaseFoldMode::Upper)
}

/// v3+ a-4 mirror of [`upper_string`] looking up the simple lower
/// case-folding table. Same decode/encode pipeline, different table
/// address — driven by the `upper: false` arm of
/// [`Op::CaseFoldTableAddr`].
///
/// v3++ b-4: same combining-mark skip as [`upper_string`].
fn lower_string() -> StdlibFunction {
    case_fold_body("lower", CaseFoldMode::Lower)
}

/// v3++ b-6 locale-aware variant: `upper_locale(s, locale) -> String`.
///
/// Body wraps [`case_fold_body`] with a runtime locale dispatch. When
/// the second parameter (a `String` holding the locale tag) starts
/// with `tr` / `TR` / `az` / `AZ`, the per-codepoint fold consults
/// the Turkish / Azerbaijani override table before falling back to
/// the default simple folding chain. All other locale values
/// degenerate to the default UAX #21 behaviour.
///
/// The full multi-codepoint Unicode mappings (`ß` -> `SS`, ligatures,
/// `İ` -> `i\u{0307}`) and the Greek final-sigma context are handled
/// on the tree-walk evaluator side; the wasm-AOT body keeps the v3+
/// a-4 simple-folding contract for the moment, with the locale
/// extension layered on top. See the `crates/relon-evaluator/src/stdlib.rs`
/// `fold_string` helper for the reference behaviour the wasm-AOT
/// future-pass aligns with.
fn upper_locale_string() -> StdlibFunction {
    locale_aware_case_fold_body("upper_locale", CaseFoldMode::Upper)
}

/// Mirror of [`upper_locale_string`] for lowercasing.
fn lower_locale_string() -> StdlibFunction {
    locale_aware_case_fold_body("lower_locale", CaseFoldMode::Lower)
}

/// Mirror of [`upper_locale_string`] for title-casing.
fn title_locale_string() -> StdlibFunction {
    locale_aware_case_fold_body("title_locale", CaseFoldMode::Title)
}

/// v3++ b-4 word-boundary aware body for `title(s: String) -> String`.
///
/// Splits the input on Unicode whitespace; the first codepoint of
/// each word maps through the simple **upper** fold table while every
/// subsequent codepoint maps through the simple **lower** fold table.
/// Whitespace codepoints themselves pass through unchanged and reset
/// the per-word boundary state.
///
/// Grapheme-cluster contract: Unicode combining marks (Mn + Mc + Me)
/// belong to their base codepoint's cluster. The body emits each mark
/// verbatim and does **not** flip the boundary flag — so e.g.
/// `cafe\u{0301} bar` still produces `Cafe\u{0301} Bar` rather than
/// `Cafe\u{0301} bar` or `Cafe\u{0301} bAr` (which a naive
/// codepoint-level walk would produce by treating the combining
/// acute as "the second character of cafe-acute").
///
/// Limitations (deferred):
///   * Only simple whitespace-based word boundaries. UAX #29 Extended
///     Grapheme Cluster + Word Boundary (handling punctuation as
///     boundaries, joiner / extender rules, etc.) is on v3++ b-6.
///   * Combining marks following a non-cased base (whitespace +
///     mark — invalid Unicode but possible in malformed input) reset
///     the boundary; matches Rust's `to_titlecase` behaviour.
fn title_string() -> StdlibFunction {
    case_fold_body("title", CaseFoldMode::Title)
}

/// Shared body generator for `upper` / `lower` / `title` with
/// Unicode-aware case folding.
///
/// Algorithm:
///   1. Read `s_len` (byte count of the input UTF-8 record).
///   2. Allocate a scratch String record sized `4 + s_len * 4`. The
///      worst-case output growth is 4x (a single-byte ASCII char
///      mapping to a 4-byte UTF-8 codepoint), but practically the
///      ratio is closer to 1x — we over-provision to keep the body
///      branchless on the resize side.
///   3. Walk the input byte-by-byte. For each codepoint: (a) decode
///      the codepoint and its byte count from the UTF-8 leading
///      byte's bit pattern — truncated tails or invalid leading bytes
///      trap as `InvalidUtf8`; (b) decide the per-codepoint mapping
///      based on the [`CaseFoldMode`]:
///         * `Upper` / `Lower` — if the codepoint is a combining
///           mark, pass it through unchanged; otherwise call the
///           bundled `__casefold_lookup` helper with the matching
///           `Op::CaseFoldTableAddr` address.
///         * `Title` — if the codepoint is a combining mark, pass it
///           through unchanged and leave `at_word_start` alone (the
///           grapheme-cluster contract — see v3++ b-4 notes). Else
///           if the codepoint is Unicode whitespace, pass it through
///           and set `at_word_start = 1`. Otherwise, look the cp up
///           in the upper table when `at_word_start == 1` (first
///           cased cp of the word) or the lower table otherwise; then
///           clear `at_word_start`.
///
///      (c) encode the resolved (possibly folded) codepoint back into
///      UTF-8 starting at `base + 4 + j` and advance `j` by the
///      byte count.
///   4. Once `i == s_len`, store the final `j` into the record's
///      length prefix and return the record pointer.
///
/// Locals:
///   * 0  — `s_len:    I32`  total input byte count
///   * 1  — `base:     I32`  pointer to the scratch result record
///   * 2  — `i:        I32`  read cursor (byte offset into payload)
///   * 3  — `j:        I32`  write cursor (byte offset into payload)
///   * 4  — `cp:       I32`  decoded codepoint
///   * 5  — `cp_bytes: I32`  byte count of the decoded codepoint
///   * 6  — `folded:   I32`  case-folded codepoint
///   * 7  — `b0:       I32`  leading byte buffer
///   * 8  — `b_tmp:    I32`  continuation byte buffer
///   * 9  — `sink:     I32`  drop slot for If-arm placeholders
///   * 10 — `at_word_start: I32`  Title-mode word boundary flag
///   * 11 — `is_mark:  I32`  Title/Upper/Lower combining-mark cache
// The body builder is structurally a streaming push-sequence; clippy's
// `vec_init_then_push` lint would force every intermediate sequence
// into a literal `vec![..]` macro, which loses the per-line comment
// alignment that makes the UTF-8 decode/encode pipeline reviewable.
// The lint stays off across the function — every helper closure here
// produces a `Vec<TaggedOp>` it appends to by-line.
#[allow(clippy::vec_init_then_push)]
pub(crate) fn case_fold_body(name: &'static str, mode: CaseFoldMode) -> StdlibFunction {
    case_fold_body_inner(name, mode, false)
}

/// v3++ b-6 locale-aware variant of [`case_fold_body`]. Same single-
/// codepoint fold pipeline, but the body takes a second `String`
/// parameter (the locale tag); when the first two bytes match
/// `tr` / `TR` / `az` / `AZ`, the per-cp lookup first consults the
/// Turkish / Azerbaijani override table before falling back to the
/// default simple folding table.
///
/// Limitations vs the tree-walk reference (`fold_string` in
/// `crates/relon-evaluator/src/stdlib.rs`):
///
///   * Multi-codepoint outputs (e.g. `ß` -> `SS`, `ﬁ` -> `FI`, the
///     Turkish `I` -> `i\u{0307}` form) are not yet emitted by the
///     wasm-AOT body — those cases pass through identity for now.
///   * Greek final-sigma context (`Σ` -> `ς` vs `σ`) is similarly
///     deferred on the wasm-AOT side.
///
/// Both follow-ups are tracked as v3++ b-6 wasm-AOT items; the
/// tree-walk evaluator handles them correctly today so functional
/// tests that exercise those paths pass when run through the host
/// evaluator.
#[allow(clippy::vec_init_then_push)]
pub(crate) fn locale_aware_case_fold_body(
    name: &'static str,
    mode: CaseFoldMode,
) -> StdlibFunction {
    case_fold_body_inner(name, mode, true)
}

#[allow(clippy::vec_init_then_push)]
fn case_fold_body_inner(
    name: &'static str,
    mode: CaseFoldMode,
    locale_aware: bool,
) -> StdlibFunction {
    const S_LEN: u32 = 0;
    const BASE: u32 = 1;
    const I: u32 = 2;
    const J: u32 = 3;
    const CP: u32 = 4;
    const CP_BYTES: u32 = 5;
    const FOLDED: u32 = 6;
    const B0: u32 = 7;
    const B_TMP: u32 = 8;
    // Sink slot for the i32 placeholder each `Op::If { result_ty: I32 }`
    // branch leaves on the operand stack. Keeping the sink separate
    // from CP_BYTES avoids the trap-arm trick clobbering the
    // legitimate `cp_bytes` value the branch just wrote.
    const SINK: u32 = 9;
    // v3++ b-4: word-boundary flag for `Title` mode. Initialised to 1
    // before the first codepoint so the very first cased cp goes
    // through the upper table. Unused (but still allocated) in
    // `Upper` / `Lower` mode — the wasm verifier doesn't penalise
    // unused locals.
    const AT_WORD_START: u32 = 10;
    // v3++ b-4: cached `__is_combining_mark(cp)` result for the
    // current codepoint. Kept in a let-local so the encode step can
    // re-use the same i32 without re-running the binary search.
    const IS_MARK: u32 = 11;
    // v3++ b-6: locale-aware dispatch flag. `1` when the caller-
    // supplied locale tag is `tr` / `TR` / `az` / `AZ` (case-
    // insensitive two-letter prefix matching, with optional `-` /
    // `_` subtag separator). The per-cp fold consults the Turkish
    // override table first when this flag is set; otherwise it
    // shortcircuits straight to the default simple folding table.
    const IS_TURKISH: u32 = 12;
    // Scratch for the Turkish-lookup hit detection: we call the
    // standard `__casefold_lookup` helper against the Turkish table
    // and stash the result; if the result differs from the input cp
    // we know we hit the override, otherwise we fall back to the
    // default table.
    const TURKISH_RESULT: u32 = 13;
    // v3++ b-7 reframed: `1` when fold_seq actually consulted a
    // case-folding table (i.e. not a combining mark and not a
    // Title-mode whitespace pass-through). The multi-cp / final-sigma
    // overlay only runs when this flag is set; otherwise the existing
    // single-cp emit path runs unchanged.
    const FOLD_TABLE_FIRED: u32 = 14;
    // v3++ b-7 reframed: `1` when the effective per-codepoint mode is
    // Upper, `0` when it is Lower. For `Upper` / `Lower` bodies this
    // is a compile-time constant; for `Title` it captures the value of
    // `at_word_start` **before** the cased-cp consumption clears it.
    // Used by the FULL multi-cp lookup to pick between the upper and
    // lower FULL tables.
    const EFFECTIVE_UPPER: u32 = 15;
    // v3++ b-7 reframed: matched FULL-table entry address from
    // `__full_casefold_lookup`. `0` means "no FULL entry" — fall back
    // to the simple-table FOLDED single-cp emit. On a hit the multi-cp
    // emit loop reads `out_len` from `FULL_ENTRY + 16` and the three
    // output codepoints from `FULL_ENTRY + 4 / 8 / 12`.
    const FULL_ENTRY: u32 = 16;
    // v3++ b-7 reframed: scratch for the multi-cp emit loop index
    // (0 .. FULL_OUT_LEN). The loop body writes the indexed output
    // codepoint into FOLDED and re-runs the existing UTF-8 encode
    // sequence per slot.
    const FULL_SLOT_IDX: u32 = 17;
    // v3++ b-7 reframed: `out_len` (1..=3) read once from the matched
    // entry so the multi-cp loop's bound stays loop-invariant.
    const FULL_OUT_LEN: u32 = 18;

    // Helper: load the `i`-th byte of the input payload. Pushes one
    // i32 onto the stack.
    let load_input_byte = |off: i32| {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4 + off)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LetGet {
                idx: I,
                ty: IrType::I32,
            }),
            tt(Op::Add(IrType::I32)),
            tt(Op::LoadI8UAtAbsolute { offset: 0 }),
        ]
    };

    // Helper: trap as InvalidUtf8 from within an if/else that needs
    // an i32 placeholder on each arm. The `Trap` op marks downstream
    // code unreachable; the placeholder satisfies the wasm verifier's
    // both-arms-typed contract.
    let trap_invalid_utf8 = || {
        vec![
            tt(Op::Trap {
                kind: TrapKind::InvalidUtf8,
            }),
            tt(Op::ConstI32(0)),
        ]
    };

    // Helper: decode a continuation byte at offset `+n` into B_TMP
    // (masked with 0x3F so callers can directly OR via `+`).
    let load_continuation = |n: i32| {
        let mut out = load_input_byte(n);
        out.push(tt(Op::ConstI32(0x3F)));
        out.push(tt(Op::BitAnd(IrType::I32)));
        out.push(tt(Op::LetSet {
            idx: B_TMP,
            ty: IrType::I32,
        }));
        out
    };

    // ----- UTF-8 decode step -----
    // Stack precondition: empty. Postcondition: empty. Sets CP and
    // CP_BYTES from the byte at offset `i` (and possibly more).
    let mut decode_seq: Vec<TaggedOp> = Vec::new();
    decode_seq.extend(load_input_byte(0));
    decode_seq.push(tt(Op::LetSet {
        idx: B0,
        ty: IrType::I32,
    }));
    // outer If: 1-byte vs multi-byte
    decode_seq.push(tt(Op::LetGet {
        idx: B0,
        ty: IrType::I32,
    }));
    decode_seq.push(tt(Op::ConstI32(0x80)));
    decode_seq.push(tt(Op::Lt(IrType::I32)));
    decode_seq.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: {
            // 1-byte ASCII: cp = b0, cp_bytes = 1
            let mut v = Vec::new();
            v.push(tt(Op::LetGet {
                idx: B0,
                ty: IrType::I32,
            }));
            v.push(tt(Op::LetSet {
                idx: CP,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(1)));
            v.push(tt(Op::LetSet {
                idx: CP_BYTES,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0)));
            v
        },
        else_body: {
            // multi-byte path. Trap when b0 is a continuation byte
            // (`0x80..=0xBF`) or beyond the 4-byte range
            // (`0xF8..=0xFF`).
            let mut v = Vec::new();
            // Reject lone continuation byte / overlong: b0 < 0xC2.
            v.push(tt(Op::LetGet {
                idx: B0,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0xC2)));
            v.push(tt(Op::Lt(IrType::I32)));
            v.push(tt(Op::If {
                result_ty: IrType::I32,
                then_body: trap_invalid_utf8(),
                else_body: {
                    let mut v2 = Vec::new();
                    // 2-byte: 0xC2..=0xDF
                    v2.push(tt(Op::LetGet {
                        idx: B0,
                        ty: IrType::I32,
                    }));
                    v2.push(tt(Op::ConstI32(0xE0)));
                    v2.push(tt(Op::Lt(IrType::I32)));
                    v2.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            let mut t = Vec::new();
                            // require i + 1 < s_len
                            t.push(tt(Op::LetGet {
                                idx: I,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(1)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetGet {
                                idx: S_LEN,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::Ge(IrType::I32)));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: trap_invalid_utf8(),
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            t.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            // cp = (b0 & 0x1F) * 64 + (b1 & 0x3F)
                            t.extend(load_continuation(1));
                            t.push(tt(Op::LetGet {
                                idx: B0,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0x1F)));
                            t.push(tt(Op::BitAnd(IrType::I32)));
                            t.push(tt(Op::ConstI32(64)));
                            t.push(tt(Op::Mul(IrType::I32)));
                            t.push(tt(Op::LetGet {
                                idx: B_TMP,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetSet {
                                idx: CP,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(2)));
                            t.push(tt(Op::LetSet {
                                idx: CP_BYTES,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                        else_body: {
                            let mut e = Vec::new();
                            // 3-byte vs 4-byte: split on 0xF0
                            e.push(tt(Op::LetGet {
                                idx: B0,
                                ty: IrType::I32,
                            }));
                            e.push(tt(Op::ConstI32(0xF0)));
                            e.push(tt(Op::Lt(IrType::I32)));
                            e.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: {
                                    let mut t = Vec::new();
                                    // require i + 2 < s_len
                                    t.push(tt(Op::LetGet {
                                        idx: I,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(2)));
                                    t.push(tt(Op::Add(IrType::I32)));
                                    t.push(tt(Op::LetGet {
                                        idx: S_LEN,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::Ge(IrType::I32)));
                                    t.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: trap_invalid_utf8(),
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    t.push(tt(Op::LetSet {
                                        idx: SINK,
                                        ty: IrType::I32,
                                    }));
                                    // cp = (b0 & 0x0F) * 4096
                                    t.push(tt(Op::LetGet {
                                        idx: B0,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(0x0F)));
                                    t.push(tt(Op::BitAnd(IrType::I32)));
                                    t.push(tt(Op::ConstI32(4096)));
                                    t.push(tt(Op::Mul(IrType::I32)));
                                    // + (b1 & 0x3F) * 64
                                    t.extend(load_continuation(1));
                                    t.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(64)));
                                    t.push(tt(Op::Mul(IrType::I32)));
                                    t.push(tt(Op::Add(IrType::I32)));
                                    // + (b2 & 0x3F)
                                    t.extend(load_continuation(2));
                                    t.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::Add(IrType::I32)));
                                    t.push(tt(Op::LetSet {
                                        idx: CP,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(3)));
                                    t.push(tt(Op::LetSet {
                                        idx: CP_BYTES,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(0)));
                                    t
                                },
                                else_body: {
                                    let mut e2 = Vec::new();
                                    // 4-byte (0xF0..=0xF7) — reject 0xF8+
                                    e2.push(tt(Op::LetGet {
                                        idx: B0,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(0xF8)));
                                    e2.push(tt(Op::Ge(IrType::I32)));
                                    e2.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: trap_invalid_utf8(),
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    e2.push(tt(Op::LetSet {
                                        idx: SINK,
                                        ty: IrType::I32,
                                    }));
                                    // require i + 3 < s_len
                                    e2.push(tt(Op::LetGet {
                                        idx: I,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(3)));
                                    e2.push(tt(Op::Add(IrType::I32)));
                                    e2.push(tt(Op::LetGet {
                                        idx: S_LEN,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::Ge(IrType::I32)));
                                    e2.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: trap_invalid_utf8(),
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    e2.push(tt(Op::LetSet {
                                        idx: SINK,
                                        ty: IrType::I32,
                                    }));
                                    // cp = (b0 & 0x07) * 262144
                                    e2.push(tt(Op::LetGet {
                                        idx: B0,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(0x07)));
                                    e2.push(tt(Op::BitAnd(IrType::I32)));
                                    e2.push(tt(Op::ConstI32(262144)));
                                    e2.push(tt(Op::Mul(IrType::I32)));
                                    // + (b1 & 0x3F) * 4096
                                    e2.extend(load_continuation(1));
                                    e2.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(4096)));
                                    e2.push(tt(Op::Mul(IrType::I32)));
                                    e2.push(tt(Op::Add(IrType::I32)));
                                    // + (b2 & 0x3F) * 64
                                    e2.extend(load_continuation(2));
                                    e2.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(64)));
                                    e2.push(tt(Op::Mul(IrType::I32)));
                                    e2.push(tt(Op::Add(IrType::I32)));
                                    // + (b3 & 0x3F)
                                    e2.extend(load_continuation(3));
                                    e2.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::Add(IrType::I32)));
                                    e2.push(tt(Op::LetSet {
                                        idx: CP,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(4)));
                                    e2.push(tt(Op::LetSet {
                                        idx: CP_BYTES,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(0)));
                                    e2
                                },
                            }));
                            e
                        },
                    }));
                    v2
                },
            }));
            v
        },
    }));
    decode_seq.push(tt(Op::LetSet {
        idx: SINK,
        ty: IrType::I32,
    }));

    // ----- grapheme-aware fold lookup -----
    // Stack precondition: empty. Postcondition: empty. Writes into
    // FOLDED + IS_MARK, and (for `Title` mode) updates AT_WORD_START.
    //
    // Cross-mode shape:
    //   * Compute `is_mark = __is_combining_mark(cp, ranges_addr)`,
    //     stash it in IS_MARK.
    //   * If `is_mark == 1`: FOLDED = cp (identity passthrough).
    //   * Else: per-mode logic decides FOLDED + AT_WORD_START update.
    //
    // Indices are hardcoded — see [`CASEFOLD_LOOKUP_INDEX`] /
    // [`COMBINING_MARK_INDEX`] / [`IS_WHITESPACE_INDEX`] for the
    // cycle-breaking rationale. Unit-tested by the three
    // `*_index_is_stable` checks.
    let casefold_idx = CASEFOLD_LOOKUP_INDEX;
    let combining_idx = COMBINING_MARK_INDEX;
    let whitespace_idx = IS_WHITESPACE_INDEX;
    let mut fold_seq: Vec<TaggedOp> = Vec::new();

    // Default state for the multi-cp overlay: no table fired, lower
    // mode. Each non-passthrough branch flips these as appropriate so
    // the post-fold stage can dispatch FULL lookup correctly.
    fold_seq.push(tt(Op::ConstI32(0)));
    fold_seq.push(tt(Op::LetSet {
        idx: FOLD_TABLE_FIRED,
        ty: IrType::I32,
    }));
    fold_seq.push(tt(Op::ConstI32(0)));
    fold_seq.push(tt(Op::LetSet {
        idx: EFFECTIVE_UPPER,
        ty: IrType::I32,
    }));

    // is_mark = __is_combining_mark(cp, combining_marks_addr)
    fold_seq.push(tt(Op::LetGet {
        idx: CP,
        ty: IrType::I32,
    }));
    fold_seq.push(tt(Op::CombiningMarkRangesAddr));
    fold_seq.push(tt(Op::Call {
        fn_index: combining_idx,
        arg_count: 2,
        param_tys: vec![IrType::I32, IrType::I32],
        ret_ty: IrType::I32,
    }));
    fold_seq.push(tt(Op::LetSet {
        idx: IS_MARK,
        ty: IrType::I32,
    }));

    // Per-mode FOLDED + AT_WORD_START decision.
    //
    // Build helper closures that produce the inner i32-typed if-arms
    // — every arm leaves a placeholder i32 on the operand stack so
    // the outer `Op::If { result_ty: I32 }` typechecks; the trailing
    // `LetSet SINK` drops it.
    let cp_as_folded = || -> Vec<TaggedOp> {
        // FOLDED = cp
        vec![
            tt(Op::LetGet {
                idx: CP,
                ty: IrType::I32,
            }),
            tt(Op::LetSet {
                idx: FOLDED,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
        ]
    };

    let lookup_through_table = |upper: bool| -> Vec<TaggedOp> {
        // Default path: FOLDED = __casefold_lookup(cp, default-table-addr).
        // Also flip the multi-cp overlay's gate: the table fired, and
        // EFFECTIVE_UPPER captures the per-cp mode so the FULL-table
        // fall-through can pick the right table family.
        let default_seq: Vec<TaggedOp> = vec![
            tt(Op::LetGet {
                idx: CP,
                ty: IrType::I32,
            }),
            tt(Op::CaseFoldTableAddr { upper }),
            tt(Op::Call {
                fn_index: casefold_idx,
                arg_count: 2,
                param_tys: vec![IrType::I32, IrType::I32],
                ret_ty: IrType::I32,
            }),
            tt(Op::LetSet {
                idx: FOLDED,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(1)),
            tt(Op::LetSet {
                idx: FOLD_TABLE_FIRED,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(if upper { 1 } else { 0 })),
            tt(Op::LetSet {
                idx: EFFECTIVE_UPPER,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
        ];

        if !locale_aware {
            return default_seq;
        }

        // Locale-aware path: when IS_TURKISH == 1, consult the
        // Turkish / Azerbaijani override table first; on a miss the
        // helper returns the input cp unchanged, in which case we
        // fall back to the default table. The detection is purely
        // arithmetic — the override table never maps a codepoint to
        // itself, so `result != cp` exactly distinguishes hit from
        // miss without needing a separate found flag.
        let mut v: Vec<TaggedOp> = Vec::new();
        v.push(tt(Op::LetGet {
            idx: IS_TURKISH,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(1)));
        v.push(tt(Op::Eq(IrType::I32)));
        v.push(tt(Op::If {
            result_ty: IrType::I32,
            then_body: {
                let mut t: Vec<TaggedOp> = Vec::new();
                // turkish_result = __casefold_lookup(cp, turkish_addr)
                t.push(tt(Op::LetGet {
                    idx: CP,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::TurkishCaseFoldTableAddr { upper }));
                t.push(tt(Op::Call {
                    fn_index: casefold_idx,
                    arg_count: 2,
                    param_tys: vec![IrType::I32, IrType::I32],
                    ret_ty: IrType::I32,
                }));
                t.push(tt(Op::LetSet {
                    idx: TURKISH_RESULT,
                    ty: IrType::I32,
                }));
                // if turkish_result != cp { FOLDED = turkish_result }
                // else { fall through to default lookup }
                t.push(tt(Op::LetGet {
                    idx: TURKISH_RESULT,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::LetGet {
                    idx: CP,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: default_seq.clone(),
                    else_body: {
                        // Turkish hit — `default_seq` is skipped so we
                        // must mirror its FOLD_TABLE_FIRED /
                        // EFFECTIVE_UPPER bookkeeping here. Otherwise
                        // the post-fold FULL / final-sigma overlay
                        // would see "table did not fire" and emit the
                        // wrong cp.
                        let mut inner = Vec::new();
                        inner.push(tt(Op::LetGet {
                            idx: TURKISH_RESULT,
                            ty: IrType::I32,
                        }));
                        inner.push(tt(Op::LetSet {
                            idx: FOLDED,
                            ty: IrType::I32,
                        }));
                        inner.push(tt(Op::ConstI32(1)));
                        inner.push(tt(Op::LetSet {
                            idx: FOLD_TABLE_FIRED,
                            ty: IrType::I32,
                        }));
                        inner.push(tt(Op::ConstI32(if upper { 1 } else { 0 })));
                        inner.push(tt(Op::LetSet {
                            idx: EFFECTIVE_UPPER,
                            ty: IrType::I32,
                        }));
                        inner.push(tt(Op::ConstI32(0)));
                        inner
                    },
                }));
                t.push(tt(Op::LetSet {
                    idx: SINK,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(0)));
                t
            },
            else_body: default_seq,
        }));
        v.push(tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(0)));
        v
    };

    // The `if is_mark == 1 { FOLDED = cp } else { <mode-specific> }`
    // outer wrapper — common to every mode.
    let non_mark_branch: Vec<TaggedOp> = match mode {
        CaseFoldMode::Upper => {
            // All non-mark codepoints map through the upper table.
            lookup_through_table(true)
        }
        CaseFoldMode::Lower => {
            // All non-mark codepoints map through the lower table.
            lookup_through_table(false)
        }
        CaseFoldMode::Title => {
            // is_ws = __is_whitespace(cp, whitespace_ranges_addr)
            //   if 1: FOLDED = cp; at_word_start = 1
            //   else: lookup against upper or lower table depending
            //         on at_word_start; then clear at_word_start.
            let mut v: Vec<TaggedOp> = Vec::new();
            // tmp = __is_whitespace(cp, ws_addr)
            v.push(tt(Op::LetGet {
                idx: CP,
                ty: IrType::I32,
            }));
            v.push(tt(Op::WhitespaceRangesAddr));
            v.push(tt(Op::Call {
                fn_index: whitespace_idx,
                arg_count: 2,
                param_tys: vec![IrType::I32, IrType::I32],
                ret_ty: IrType::I32,
            }));
            // if is_ws == 1 { FOLDED = cp; at_word_start = 1 }
            // else { fold + clear }
            v.push(tt(Op::ConstI32(1)));
            v.push(tt(Op::Eq(IrType::I32)));
            v.push(tt(Op::If {
                result_ty: IrType::I32,
                then_body: {
                    let mut t = Vec::new();
                    // FOLDED = cp
                    t.push(tt(Op::LetGet {
                        idx: CP,
                        ty: IrType::I32,
                    }));
                    t.push(tt(Op::LetSet {
                        idx: FOLDED,
                        ty: IrType::I32,
                    }));
                    // at_word_start = 1
                    t.push(tt(Op::ConstI32(1)));
                    t.push(tt(Op::LetSet {
                        idx: AT_WORD_START,
                        ty: IrType::I32,
                    }));
                    t.push(tt(Op::ConstI32(0)));
                    t
                },
                else_body: {
                    // if at_word_start == 1 { upper } else { lower }
                    let mut e = Vec::new();
                    e.push(tt(Op::LetGet {
                        idx: AT_WORD_START,
                        ty: IrType::I32,
                    }));
                    e.push(tt(Op::ConstI32(1)));
                    e.push(tt(Op::Eq(IrType::I32)));
                    e.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: lookup_through_table(true),
                        else_body: lookup_through_table(false),
                    }));
                    // Drop the placeholder i32 produced by the inner If.
                    e.push(tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }));
                    // at_word_start = 0 (cased cp consumed)
                    e.push(tt(Op::ConstI32(0)));
                    e.push(tt(Op::LetSet {
                        idx: AT_WORD_START,
                        ty: IrType::I32,
                    }));
                    e.push(tt(Op::ConstI32(0)));
                    e
                },
            }));
            // Drop the placeholder i32 produced by the whitespace If.
            v.push(tt(Op::LetSet {
                idx: SINK,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0)));
            v
        }
    };

    // if is_mark == 1 { FOLDED = cp } else { <non_mark_branch> }
    fold_seq.push(tt(Op::LetGet {
        idx: IS_MARK,
        ty: IrType::I32,
    }));
    fold_seq.push(tt(Op::ConstI32(1)));
    fold_seq.push(tt(Op::Eq(IrType::I32)));
    fold_seq.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: cp_as_folded(),
        else_body: non_mark_branch,
    }));
    fold_seq.push(tt(Op::LetSet {
        idx: SINK,
        ty: IrType::I32,
    }));

    // ----- UTF-8 encode step -----
    // Stack precondition: empty. Postcondition: empty. Writes the
    // FOLDED codepoint starting at `base + 4 + j` and advances `j`
    // by 1..=4. Mirrors the 4-arm split on output codepoint range.
    let store_byte = |off: i32, byte_expr: Vec<TaggedOp>| {
        let mut v = Vec::new();
        // addr = base + 4 + j + off
        v.push(tt(Op::LetGet {
            idx: BASE,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(4 + off)));
        v.push(tt(Op::Add(IrType::I32)));
        v.push(tt(Op::LetGet {
            idx: J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::Add(IrType::I32)));
        v.extend(byte_expr);
        v.push(tt(Op::StoreI8AtAbsolute { offset: 0 }));
        v
    };
    // Convenience: build the i32 byte expression for `prefix |
    // (FOLDED >> shift_div_pow2 & 0x3F)`. `shift_div_pow2` is the
    // divisor that emulates the wasm-missing `i32.shr_u` op via
    // signed `i32.div_s` — safe because FOLDED is always
    // non-negative (it's a Unicode codepoint <= 0x10FFFF).
    let prefix_plus_shifted = |prefix: i32, shift_div: i32, mask: bool| {
        let mut v = vec![tt(Op::ConstI32(prefix))];
        // shifted = (FOLDED / shift_div)
        v.push(tt(Op::LetGet {
            idx: FOLDED,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(shift_div)));
        v.push(tt(Op::Div(IrType::I32)));
        if mask {
            v.push(tt(Op::ConstI32(0x3F)));
            v.push(tt(Op::BitAnd(IrType::I32)));
        }
        v.push(tt(Op::Add(IrType::I32)));
        v
    };

    let mut encode_seq: Vec<TaggedOp> = Vec::new();
    // outer If: FOLDED < 0x80
    encode_seq.push(tt(Op::LetGet {
        idx: FOLDED,
        ty: IrType::I32,
    }));
    encode_seq.push(tt(Op::ConstI32(0x80)));
    encode_seq.push(tt(Op::Lt(IrType::I32)));
    encode_seq.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: {
            // 1-byte: store FOLDED at base + 4 + j; j += 1
            let mut v = Vec::new();
            v.extend(store_byte(
                0,
                vec![tt(Op::LetGet {
                    idx: FOLDED,
                    ty: IrType::I32,
                })],
            ));
            v.push(tt(Op::LetGet {
                idx: J,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(1)));
            v.push(tt(Op::Add(IrType::I32)));
            v.push(tt(Op::LetSet {
                idx: J,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0)));
            v
        },
        else_body: {
            let mut v = Vec::new();
            // FOLDED < 0x800?
            v.push(tt(Op::LetGet {
                idx: FOLDED,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0x800)));
            v.push(tt(Op::Lt(IrType::I32)));
            v.push(tt(Op::If {
                result_ty: IrType::I32,
                then_body: {
                    // 2-byte: 0xC0 | (cp >> 6), 0x80 | (cp & 0x3F)
                    let mut t = Vec::new();
                    t.extend(store_byte(0, prefix_plus_shifted(0xC0, 64, false)));
                    t.extend(store_byte(1, {
                        let mut e = vec![tt(Op::ConstI32(0x80))];
                        e.push(tt(Op::LetGet {
                            idx: FOLDED,
                            ty: IrType::I32,
                        }));
                        e.push(tt(Op::ConstI32(0x3F)));
                        e.push(tt(Op::BitAnd(IrType::I32)));
                        e.push(tt(Op::Add(IrType::I32)));
                        e
                    }));
                    t.push(tt(Op::LetGet {
                        idx: J,
                        ty: IrType::I32,
                    }));
                    t.push(tt(Op::ConstI32(2)));
                    t.push(tt(Op::Add(IrType::I32)));
                    t.push(tt(Op::LetSet {
                        idx: J,
                        ty: IrType::I32,
                    }));
                    t.push(tt(Op::ConstI32(0)));
                    t
                },
                else_body: {
                    let mut e = Vec::new();
                    // FOLDED < 0x10000?
                    e.push(tt(Op::LetGet {
                        idx: FOLDED,
                        ty: IrType::I32,
                    }));
                    e.push(tt(Op::ConstI32(0x10000)));
                    e.push(tt(Op::Lt(IrType::I32)));
                    e.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            // 3-byte
                            let mut t = Vec::new();
                            t.extend(store_byte(0, prefix_plus_shifted(0xE0, 4096, false)));
                            t.extend(store_byte(1, prefix_plus_shifted(0x80, 64, true)));
                            t.extend(store_byte(2, {
                                let mut ee = vec![tt(Op::ConstI32(0x80))];
                                ee.push(tt(Op::LetGet {
                                    idx: FOLDED,
                                    ty: IrType::I32,
                                }));
                                ee.push(tt(Op::ConstI32(0x3F)));
                                ee.push(tt(Op::BitAnd(IrType::I32)));
                                ee.push(tt(Op::Add(IrType::I32)));
                                ee
                            }));
                            t.push(tt(Op::LetGet {
                                idx: J,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(3)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetSet {
                                idx: J,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                        else_body: {
                            // 4-byte (FOLDED < 0x110000 guaranteed by
                            // table contents; we don't re-check)
                            let mut t = Vec::new();
                            t.extend(store_byte(0, prefix_plus_shifted(0xF0, 262144, false)));
                            t.extend(store_byte(1, prefix_plus_shifted(0x80, 4096, true)));
                            t.extend(store_byte(2, prefix_plus_shifted(0x80, 64, true)));
                            t.extend(store_byte(3, {
                                let mut ee = vec![tt(Op::ConstI32(0x80))];
                                ee.push(tt(Op::LetGet {
                                    idx: FOLDED,
                                    ty: IrType::I32,
                                }));
                                ee.push(tt(Op::ConstI32(0x3F)));
                                ee.push(tt(Op::BitAnd(IrType::I32)));
                                ee.push(tt(Op::Add(IrType::I32)));
                                ee
                            }));
                            t.push(tt(Op::LetGet {
                                idx: J,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(4)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetSet {
                                idx: J,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                    }));
                    e
                },
            }));
            v
        },
    }));
    encode_seq.push(tt(Op::LetSet {
        idx: SINK,
        ty: IrType::I32,
    }));

    // ----- v3++ b-7 reframed multi-cp overlay -----
    //
    // Bridges the gap between the simple-table FOLDED single-cp emit
    // (existing) and the UAX #21 expansions that map a single input
    // codepoint to up to 3 output codepoints (`ß` -> `SS`, `ﬁ` -> `FI`,
    // `İ` -> `i` + combining-dot-above, …) plus the Greek final-sigma
    // context rule (`Σ` -> `ς` at end of word, `σ` otherwise).
    //
    // Shape:
    //   if FOLD_TABLE_FIRED == 0 { <encode FOLDED once> }
    //   else if cp == 0x03A3 && EFFECTIVE_UPPER == 0 {
    //       // final-sigma override; never multi-cp
    //       if __final_sigma_check(s, i, cased, ignorable) == 1 {
    //           FOLDED = 0x03C2  (ς)
    //       }
    //       <encode FOLDED once>
    //   } else {
    //       FULL_ENTRY = __full_casefold_lookup(cp,
    //           FullCaseFoldTableAddr{ upper: EFFECTIVE_UPPER })
    //       if FULL_ENTRY != 0 {
    //           FULL_OUT_LEN = load(FULL_ENTRY + 16)
    //           for slot in 0..FULL_OUT_LEN {
    //               FOLDED = load(FULL_ENTRY + 4 + 4*slot)
    //               <encode FOLDED once>
    //           }
    //       } else {
    //           <encode FOLDED once>
    //       }
    //   }
    //
    // The "Turkish FULL" path is *not* needed today — every entry in
    // the Turkish override table is 1:1, so the simple-table path
    // (already wired through `lookup_through_table`) already handles
    // them via `encode_simple_view_bytes`. If a future UCD revision
    // adds a multi-cp Turkish override, this overlay is the place to
    // dispatch the alternate FULL table.

    // Helper: emit FOLDED via the existing encode_seq, cloned once
    // per call site (FOLDED is mutated between emits in the multi-cp
    // loop). The clone is cheap — the IR tree is small.
    let emit_folded_once = || encode_seq.clone();

    // Helper: load `FULL_ENTRY + lit_off` as i32. Used to fetch
    // out_len and the three output codepoints.
    let load_full_entry_off = |lit_off: u32| -> Vec<TaggedOp> {
        vec![
            tt(Op::LetGet {
                idx: FULL_ENTRY,
                ty: IrType::I32,
            }),
            tt(Op::LoadI32AtAbsolute { offset: lit_off }),
        ]
    };

    // ----- FULL lookup helpers -----
    // Compile the FULL-table lookup once per `EFFECTIVE_UPPER` value.
    // For Title mode both compile-time options are reachable; for
    // Upper / Lower only one is. We always emit both branches for
    // simplicity — the dead arm is pruned by cranelift / wasm DCE.
    let full_lookup_for = |upper: bool| -> Vec<TaggedOp> {
        vec![
            tt(Op::LetGet {
                idx: CP,
                ty: IrType::I32,
            }),
            tt(Op::FullCaseFoldTableAddr { upper }),
            tt(Op::Call {
                fn_index: FULL_CASEFOLD_LOOKUP_INDEX,
                arg_count: 2,
                param_tys: vec![IrType::I32, IrType::I32],
                ret_ty: IrType::I32,
            }),
            tt(Op::LetSet {
                idx: FULL_ENTRY,
                ty: IrType::I32,
            }),
        ]
    };

    // FULL_ENTRY = upper ? full_lookup(true) : full_lookup(false).
    // Branches on EFFECTIVE_UPPER at runtime so Title mode picks the
    // right table per codepoint.
    let full_lookup_dispatch: Vec<TaggedOp> = vec![
        tt(Op::LetGet {
            idx: EFFECTIVE_UPPER,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(1)),
        tt(Op::Eq(IrType::I32)),
        tt(Op::If {
            result_ty: IrType::I32,
            then_body: {
                let mut t = full_lookup_for(true);
                t.push(tt(Op::ConstI32(0)));
                t
            },
            else_body: {
                let mut e = full_lookup_for(false);
                e.push(tt(Op::ConstI32(0)));
                e
            },
        }),
        tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }),
    ];

    // Multi-cp emit loop: read FULL_OUT_LEN once, then iterate
    // FULL_SLOT_IDX from 0 to FULL_OUT_LEN; each iteration loads the
    // slot's codepoint into FOLDED and runs the existing encode_seq.
    let multi_emit_loop: Vec<TaggedOp> = {
        let mut v: Vec<TaggedOp> = Vec::new();
        // FULL_OUT_LEN = load(FULL_ENTRY + 16)
        v.extend(load_full_entry_off(16));
        v.push(tt(Op::LetSet {
            idx: FULL_OUT_LEN,
            ty: IrType::I32,
        }));
        // FULL_SLOT_IDX = 0
        v.push(tt(Op::ConstI32(0)));
        v.push(tt(Op::LetSet {
            idx: FULL_SLOT_IDX,
            ty: IrType::I32,
        }));
        v.push(tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: {
                    let mut l: Vec<TaggedOp> = Vec::new();
                    // exit when FULL_SLOT_IDX >= FULL_OUT_LEN
                    l.push(tt(Op::LetGet {
                        idx: FULL_SLOT_IDX,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::LetGet {
                        idx: FULL_OUT_LEN,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::Ge(IrType::I32)));
                    l.push(tt(Op::BrIf { label_depth: 1 }));
                    // FOLDED = load(FULL_ENTRY + 4 + 4 * FULL_SLOT_IDX)
                    l.push(tt(Op::LetGet {
                        idx: FULL_ENTRY,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::ConstI32(4)));
                    l.push(tt(Op::Add(IrType::I32)));
                    l.push(tt(Op::LetGet {
                        idx: FULL_SLOT_IDX,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::ConstI32(4)));
                    l.push(tt(Op::Mul(IrType::I32)));
                    l.push(tt(Op::Add(IrType::I32)));
                    l.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                    l.push(tt(Op::LetSet {
                        idx: FOLDED,
                        ty: IrType::I32,
                    }));
                    // <encode FOLDED once>
                    l.extend(emit_folded_once());
                    // FULL_SLOT_IDX += 1
                    l.push(tt(Op::LetGet {
                        idx: FULL_SLOT_IDX,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::ConstI32(1)));
                    l.push(tt(Op::Add(IrType::I32)));
                    l.push(tt(Op::LetSet {
                        idx: FULL_SLOT_IDX,
                        ty: IrType::I32,
                    }));
                    l.push(tt(Op::Br { label_depth: 0 }));
                    l
                },
            })],
        }));
        v
    };

    // Final-sigma override: only for the Lower effective mode at cp =
    // U+03A3. The simple table already maps Σ -> σ (U+03C3); we
    // override to ς (U+03C2) when the context-scan helper returns 1.
    let final_sigma_override: Vec<TaggedOp> = vec![
        tt(Op::LocalGet(0)),
        tt(Op::LetGet {
            idx: I,
            ty: IrType::I32,
        }),
        tt(Op::CasedRangesAddr),
        tt(Op::CaseIgnorableRangesAddr),
        tt(Op::Call {
            fn_index: FINAL_SIGMA_CHECK_INDEX,
            arg_count: 4,
            param_tys: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
            ret_ty: IrType::I32,
        }),
        tt(Op::ConstI32(1)),
        tt(Op::Eq(IrType::I32)),
        tt(Op::If {
            result_ty: IrType::I32,
            then_body: vec![
                tt(Op::ConstI32(0x03C2)),
                tt(Op::LetSet {
                    idx: FOLDED,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(0)),
            ],
            else_body: vec![tt(Op::ConstI32(0))],
        }),
        tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }),
    ];

    // Sigma-detection guard: cp == 0x03A3 && EFFECTIVE_UPPER == 0.
    // Two separate Eq results sum to 2 exactly when both hold, then
    // an `== 2` collapses to the boolean outcome.
    let is_sigma_lower: Vec<TaggedOp> = vec![
        tt(Op::LetGet {
            idx: CP,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(0x03A3)),
        tt(Op::Eq(IrType::I32)),
        tt(Op::LetGet {
            idx: EFFECTIVE_UPPER,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(0)),
        tt(Op::Eq(IrType::I32)),
        tt(Op::Add(IrType::I32)),
        tt(Op::ConstI32(2)),
        tt(Op::Eq(IrType::I32)),
    ];

    let multi_cp_overlay: Vec<TaggedOp> = vec![
        tt(Op::LetGet {
            idx: FOLD_TABLE_FIRED,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(0)),
        tt(Op::Eq(IrType::I32)),
        tt(Op::If {
            result_ty: IrType::I32,
            then_body: {
                // No table fired (combining mark, Title-mode
                // whitespace) — single-cp passthrough.
                let mut t = emit_folded_once();
                t.push(tt(Op::ConstI32(0)));
                t
            },
            else_body: {
                let mut e: Vec<TaggedOp> = Vec::new();
                // Inner If: sigma-lower?
                e.extend(is_sigma_lower);
                e.push(tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: {
                        let mut t = final_sigma_override;
                        t.extend(emit_folded_once());
                        t.push(tt(Op::ConstI32(0)));
                        t
                    },
                    else_body: {
                        // Try FULL lookup.
                        let mut e2 = full_lookup_dispatch;
                        // if FULL_ENTRY != 0 { multi-emit } else
                        // { single-emit }
                        e2.push(tt(Op::LetGet {
                            idx: FULL_ENTRY,
                            ty: IrType::I32,
                        }));
                        e2.push(tt(Op::ConstI32(0)));
                        e2.push(tt(Op::Ne(IrType::I32)));
                        e2.push(tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: {
                                let mut t = multi_emit_loop;
                                t.push(tt(Op::ConstI32(0)));
                                t
                            },
                            else_body: {
                                let mut t = emit_folded_once();
                                t.push(tt(Op::ConstI32(0)));
                                t
                            },
                        }));
                        e2.push(tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }));
                        e2.push(tt(Op::ConstI32(0)));
                        e2
                    },
                }));
                e.push(tt(Op::LetSet {
                    idx: SINK,
                    ty: IrType::I32,
                }));
                e.push(tt(Op::ConstI32(0)));
                e
            },
        }),
        tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }),
    ];

    // ----- assemble the full body -----
    let mut body: Vec<TaggedOp> = Vec::new();
    // s_len = i32.load(s, 0)
    body.push(tt(Op::LocalGet(0)));
    body.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
    body.push(tt(Op::LetSet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    // base = alloc_scratch_dyn(4 + s_len * 4)
    body.push(tt(Op::ConstI32(4)));
    body.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(4)));
    body.push(tt(Op::Mul(IrType::I32)));
    body.push(tt(Op::Add(IrType::I32)));
    body.push(tt(Op::AllocScratchDyn));
    body.push(tt(Op::LetSet {
        idx: BASE,
        ty: IrType::I32,
    }));
    // i = 0; j = 0; at_word_start = 1
    //
    // `at_word_start` is initialised to 1 in every mode (Upper / Lower
    // ignore it; Title relies on it). The choice keeps the loop body
    // mode-agnostic — no branchy "if Title" guards around the
    // initialisation.
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: J,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(1)));
    body.push(tt(Op::LetSet {
        idx: AT_WORD_START,
        ty: IrType::I32,
    }));
    // v3++ b-6: when the body is locale-aware, decode the leading two
    // ASCII letters of the locale parameter and set IS_TURKISH to 1
    // iff they spell `tr`/`TR`/`az`/`AZ` (with optional `-` / `_`
    // subtag separator after position 2). Empty / single-byte / non-
    // ASCII locales fall through to IS_TURKISH = 0, matching the
    // tree-walk evaluator's `is_turkish_locale` helper.
    if locale_aware {
        // Default IS_TURKISH = 0; the conditionals below flip to 1
        // when the locale tag matches.
        body.push(tt(Op::ConstI32(0)));
        body.push(tt(Op::LetSet {
            idx: IS_TURKISH,
            ty: IrType::I32,
        }));
        // locale_len = i32.load(locale_ptr + 0)
        body.push(tt(Op::LocalGet(1)));
        body.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
        body.push(tt(Op::LetSet {
            idx: TURKISH_RESULT, /* scratch: locale_len */
            ty: IrType::I32,
        }));
        // if locale_len >= 2 { check first two bytes }
        body.push(tt(Op::LetGet {
            idx: TURKISH_RESULT,
            ty: IrType::I32,
        }));
        body.push(tt(Op::ConstI32(2)));
        body.push(tt(Op::Ge(IrType::I32)));
        body.push(tt(Op::If {
            result_ty: IrType::I32,
            then_body: {
                let mut t: Vec<TaggedOp> = Vec::new();
                // Boundary check: when locale_len > 2 the third byte
                // must be `-` (0x2D) or `_` (0x5F), otherwise the
                // two-letter prefix is just a prefix of a longer
                // language code (e.g. `tron`).
                // boundary_ok = (locale_len == 2) || (byte2 == '-') || (byte2 == '_')
                t.push(tt(Op::LetGet {
                    idx: TURKISH_RESULT,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(2)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: vec![tt(Op::ConstI32(1))],
                    else_body: {
                        let mut e: Vec<TaggedOp> = Vec::new();
                        // byte2 = i32.load_u8(locale_ptr + 4 + 2)
                        e.push(tt(Op::LocalGet(1)));
                        e.push(tt(Op::ConstI32(6)));
                        e.push(tt(Op::Add(IrType::I32)));
                        e.push(tt(Op::LoadI8UAtAbsolute { offset: 0 }));
                        e.push(tt(Op::LetSet {
                            idx: B0,
                            ty: IrType::I32,
                        }));
                        // (byte2 == 0x2D) || (byte2 == 0x5F)
                        // Encoded as (a + b) where a, b ∈ {0, 1} are
                        // disjoint Eq results — the sum is the OR.
                        e.push(tt(Op::LetGet {
                            idx: B0,
                            ty: IrType::I32,
                        }));
                        e.push(tt(Op::ConstI32(0x2D)));
                        e.push(tt(Op::Eq(IrType::I32)));
                        e.push(tt(Op::LetGet {
                            idx: B0,
                            ty: IrType::I32,
                        }));
                        e.push(tt(Op::ConstI32(0x5F)));
                        e.push(tt(Op::Eq(IrType::I32)));
                        e.push(tt(Op::Add(IrType::I32)));
                        e
                    },
                }));
                t.push(tt(Op::LetSet {
                    idx: B_TMP, /* scratch for boundary_ok */
                    ty: IrType::I32,
                }));
                // Load byte0 and byte1 raw. We compare each against
                // both lowercase and uppercase forms directly (rather
                // than normalising via `| 0x20`, since the IR has no
                // BitOr op today).
                t.push(tt(Op::LocalGet(1)));
                t.push(tt(Op::ConstI32(4)));
                t.push(tt(Op::Add(IrType::I32)));
                t.push(tt(Op::LoadI8UAtAbsolute { offset: 0 }));
                t.push(tt(Op::LetSet {
                    idx: B0,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::LocalGet(1)));
                t.push(tt(Op::ConstI32(5)));
                t.push(tt(Op::Add(IrType::I32)));
                t.push(tt(Op::LoadI8UAtAbsolute { offset: 0 }));
                t.push(tt(Op::LetSet {
                    idx: CP,
                    ty: IrType::I32,
                }));
                // is_tr = (b0 == 't' || b0 == 'T') && (b1 == 'r' || b1 == 'R')
                //
                // Each clause is an Eq returning 0/1, summed via Add to
                // emulate OR over disjoint booleans. Then BitAnd
                // multiplies the two clause results into the final
                // 0/1 outcome.
                t.push(tt(Op::LetGet {
                    idx: B0,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b't' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::LetGet {
                    idx: B0,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b'T' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::Add(IrType::I32)));
                t.push(tt(Op::LetGet {
                    idx: CP,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b'r' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::LetGet {
                    idx: CP,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b'R' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::Add(IrType::I32)));
                t.push(tt(Op::BitAnd(IrType::I32)));
                // is_az = (b0 == 'a' || b0 == 'A') && (b1 == 'z' || b1 == 'Z')
                t.push(tt(Op::LetGet {
                    idx: B0,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b'a' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::LetGet {
                    idx: B0,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b'A' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::Add(IrType::I32)));
                t.push(tt(Op::LetGet {
                    idx: CP,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b'z' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::LetGet {
                    idx: CP,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(b'Z' as i32)));
                t.push(tt(Op::Eq(IrType::I32)));
                t.push(tt(Op::Add(IrType::I32)));
                t.push(tt(Op::BitAnd(IrType::I32)));
                // is_tr OR is_az — both are {0,1} so Add gives the OR.
                t.push(tt(Op::Add(IrType::I32)));
                // (is_tr || is_az) && boundary_ok
                t.push(tt(Op::LetGet {
                    idx: B_TMP,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::BitAnd(IrType::I32)));
                t.push(tt(Op::LetSet {
                    idx: IS_TURKISH,
                    ty: IrType::I32,
                }));
                t.push(tt(Op::ConstI32(0)));
                t
            },
            else_body: vec![tt(Op::ConstI32(0))],
        }));
        body.push(tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }));
    }
    // block { loop { ... } }
    let mut loop_body: Vec<TaggedOp> = Vec::new();
    // exit when i >= s_len
    loop_body.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::Ge(IrType::I32)));
    loop_body.push(tt(Op::BrIf { label_depth: 1 }));
    // decode -> fold -> (b-7 reframed) multi-cp / final-sigma overlay
    // wraps the original encode_seq so the loop covers FULL and Σ
    // expansions in addition to the simple 1:1 case-fold map.
    loop_body.extend(decode_seq);
    loop_body.extend(fold_seq);
    loop_body.extend(multi_cp_overlay);
    // i += cp_bytes
    loop_body.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::LetGet {
        idx: CP_BYTES,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::Add(IrType::I32)));
    loop_body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    loop_body.push(tt(Op::Br { label_depth: 0 }));
    body.push(tt(Op::Block {
        result_ty: None,
        body: vec![tt(Op::Loop {
            result_ty: None,
            body: loop_body,
        })],
    }));
    // write header: i32.store(base + 0, j)
    body.push(tt(Op::LetGet {
        idx: BASE,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetGet {
        idx: J,
        ty: IrType::I32,
    }));
    body.push(tt(Op::StoreI32AtAbsolute { offset: 0 }));
    // return base
    body.push(tt(Op::LetGet {
        idx: BASE,
        ty: IrType::I32,
    }));
    body.push(tt(Op::Return));

    let params = if locale_aware {
        vec![IrType::String, IrType::String]
    } else {
        vec![IrType::String]
    };
    StdlibFunction {
        name,
        params,
        ret: IrType::String,
        body,
    }
}

/// v3+ a-4 internal helper: `__casefold_lookup(cp: I32, table_addr: I32) -> I32`.
///
/// Binary-searches the simple case-folding table embedded in the
/// wasm data section and returns the mapped codepoint, or the input
/// codepoint unchanged when no entry matches. The table layout is
/// `[count: u32 LE][(input_cp: u32, output_cp: u32) × count]`; the
/// helper rebases each midpoint against `table_addr + 4 + mid * 8`
/// to pick the entry's input/output pair.
///
/// The helper is **not** part of the user-facing surface — it is
/// only invoked by the rewritten `upper` / `lower` bodies and never
/// surfaces through [`stdlib_method_index`] or
/// [`stdlib_function_index`] (the name leads with `__` to make that
/// intent visible to anyone scanning the registry). DCE keeps it
/// alive iff at least one reachable function reaches `upper` or
/// `lower`, since those are the only callers.
///
/// Locals:
///   * 0 — `count: I32`  table entry count (loaded from header)
///   * 1 — `lo:    I32`  inclusive low bound of the search window
///   * 2 — `hi:    I32`  exclusive high bound of the search window
///   * 3 — `mid:   I32`  midpoint of the current window
///   * 4 — `entry: I32`  absolute address of the entry's input slot
///   * 5 — `key:   I32`  decoded input codepoint of the midpoint
fn casefold_lookup_helper() -> StdlibFunction {
    const COUNT: u32 = 0;
    const LO: u32 = 1;
    const HI: u32 = 2;
    const MID: u32 = 3;
    const ENTRY: u32 = 4;
    const KEY: u32 = 5;
    /// Sink slot for the i32 sentinel each `Op::If { result_ty: I32 }`
    /// arm leaves on the operand stack. Kept distinct from MID /
    /// LO / HI so the placeholder doesn't trample the binary-search
    /// state between the "match?" check and the window narrowing.
    const SINK: u32 = 6;
    /// Search result — initialised to the input codepoint so a miss
    /// falls through to "identity" without needing a separate "found"
    /// flag. On a hit the search-loop body writes the mapped
    /// codepoint into this slot and `br 1`s out of the surrounding
    /// block; the function tail simply pushes RESULT and returns.
    const RESULT: u32 = 7;
    StdlibFunction {
        name: "__casefold_lookup",
        params: vec![IrType::I32, IrType::I32],
        ret: IrType::I32,
        body: vec![
            // count = i32.load(table_addr + 0)
            tt(Op::LocalGet(1)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: COUNT,
                ty: IrType::I32,
            }),
            // lo = 0; hi = count
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: LO,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: COUNT,
                ty: IrType::I32,
            }),
            tt(Op::LetSet {
                idx: HI,
                ty: IrType::I32,
            }),
            // result = cp (identity-on-miss).
            tt(Op::LocalGet(0)),
            tt(Op::LetSet {
                idx: RESULT,
                ty: IrType::I32,
            }),
            // block { loop { ... } } — search loop. `Br 1` (outer
            // block) exits the search; the body writes the mapped
            // codepoint into RESULT before the `Br 1` on a match.
            // `Op::Return` cannot be used for early-out — the IR
            // codegen treats it as the function-end marker only and
            // does not emit a wasm `return` instruction at the call
            // site (see codegen `Op::Return` arm). The block-and-flag
            // shape gives the same control flow.
            tt(Op::Block {
                result_ty: None,
                body: vec![tt(Op::Loop {
                    result_ty: None,
                    body: vec![
                        // exit when lo >= hi
                        tt(Op::LetGet {
                            idx: LO,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: HI,
                            ty: IrType::I32,
                        }),
                        tt(Op::Ge(IrType::I32)),
                        tt(Op::BrIf { label_depth: 1 }),
                        // mid = (lo + hi) / 2
                        tt(Op::LetGet {
                            idx: LO,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: HI,
                            ty: IrType::I32,
                        }),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::ConstI32(2)),
                        tt(Op::Div(IrType::I32)),
                        tt(Op::LetSet {
                            idx: MID,
                            ty: IrType::I32,
                        }),
                        // entry = table_addr + 4 + mid * 8
                        tt(Op::LocalGet(1)),
                        tt(Op::ConstI32(4)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetGet {
                            idx: MID,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(8)),
                        tt(Op::Mul(IrType::I32)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: ENTRY,
                            ty: IrType::I32,
                        }),
                        // key = i32.load(entry + 0)
                        tt(Op::LetGet {
                            idx: ENTRY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LoadI32AtAbsolute { offset: 0 }),
                        tt(Op::LetSet {
                            idx: KEY,
                            ty: IrType::I32,
                        }),
                        // if key == cp { RESULT = load(entry+4); br 1 }
                        tt(Op::LetGet {
                            idx: KEY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LocalGet(0)),
                        tt(Op::Eq(IrType::I32)),
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                // RESULT = i32.load(entry + 4)
                                tt(Op::LetGet {
                                    idx: ENTRY,
                                    ty: IrType::I32,
                                }),
                                tt(Op::LoadI32AtAbsolute { offset: 4 }),
                                tt(Op::LetSet {
                                    idx: RESULT,
                                    ty: IrType::I32,
                                }),
                                // br 2 exits the enclosing block
                                // (depth 0 = If, 1 = Loop, 2 = Block)
                                tt(Op::Br { label_depth: 2 }),
                                tt(Op::ConstI32(0)),
                            ],
                            else_body: vec![tt(Op::ConstI32(0))],
                        }),
                        // sink for the placeholder i32 from the
                        // match-check above. MID stays untouched so
                        // the narrowing arms below can still read the
                        // current midpoint.
                        tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }),
                        // narrow the window: if key < cp { lo = mid + 1 } else { hi = mid }
                        tt(Op::LetGet {
                            idx: KEY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LocalGet(0)),
                        tt(Op::Lt(IrType::I32)),
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                tt(Op::LetGet {
                                    idx: MID,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(1)),
                                tt(Op::Add(IrType::I32)),
                                tt(Op::LetSet {
                                    idx: LO,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(0)),
                            ],
                            else_body: vec![
                                tt(Op::LetGet {
                                    idx: MID,
                                    ty: IrType::I32,
                                }),
                                tt(Op::LetSet {
                                    idx: HI,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(0)),
                            ],
                        }),
                        tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }),
                        tt(Op::Br { label_depth: 0 }),
                    ],
                })],
            }),
            // RESULT either holds the matched codepoint or the input
            // codepoint (identity-on-miss). Return it.
            tt(Op::LetGet {
                idx: RESULT,
                ty: IrType::I32,
            }),
            tt(Op::Return),
        ],
    }
}

/// v3++ b-4 internal helper:
/// `__is_combining_mark(cp: I32, table_addr: I32) -> I32`.
///
/// Binary-searches the embedded Unicode Mark range table (`Mn + Mc +
/// Me`) for the codepoint `cp`. Returns `1` when `cp` falls inside
/// any `(start, end)` range (inclusive), else `0`.
///
/// Table layout: `[count: u32 LE][(start: u32 LE, end: u32 LE) × count]`.
/// The helper rebases each midpoint against `table_addr + 4 + mid * 8`
/// to load the `(start, end)` pair — same arithmetic shape as
/// [`casefold_lookup_helper`].
///
/// Not surfaced through user-facing dispatch. Only called from the
/// rewritten `title` / `upper` / `lower` bodies to honour the
/// grapheme-cluster contract (see the v3++ b-4 notes on `title` for
/// the rationale).
///
/// Locals:
///   * 0 — `count: I32`  range entry count (loaded from header)
///   * 1 — `lo:    I32`  inclusive low bound
///   * 2 — `hi:    I32`  exclusive high bound
///   * 3 — `mid:   I32`  midpoint
///   * 4 — `entry: I32`  absolute address of the current range
///   * 5 — `start: I32`  inclusive range start
///   * 6 — `end:   I32`  inclusive range end
///   * 7 — `sink:  I32`  drop slot for If-arm placeholders
///   * 8 — `result: I32` 0 by default, 1 on a hit
fn is_combining_mark_helper() -> StdlibFunction {
    range_membership_helper("__is_combining_mark")
}

/// v3++ b-4 internal helper:
/// `__is_whitespace(cp: I32, table_addr: I32) -> I32`.
///
/// ASCII fast path then range-membership search. Returns `1` when
/// `cp` is in the Unicode `White_Space` property set, else `0`. The
/// ASCII fast path covers `0x09..=0x0D` (HT/LF/VT/FF/CR) and `0x20`
/// (SPACE); non-ASCII codepoints fall through to a binary search of
/// the embedded non-ASCII whitespace ranges (`U+00A0`, `U+1680`,
/// `U+2000..=U+200A`, `U+2028..=U+2029`, `U+202F`, `U+205F`,
/// `U+3000`).
///
/// Implementation reuses [`range_membership_helper`] for the binary
/// search core — the helper builder accepts the surface name and
/// emits the same range-membership shape. The ASCII fast path is
/// stitched in front so the common case never touches the table.
fn is_whitespace_helper() -> StdlibFunction {
    // Locals mirror `range_membership_helper`.
    const COUNT: u32 = 0;
    const LO: u32 = 1;
    const HI: u32 = 2;
    const MID: u32 = 3;
    const ENTRY: u32 = 4;
    const START: u32 = 5;
    const END: u32 = 6;
    const SINK: u32 = 7;
    const RESULT: u32 = 8;
    let body = vec![
        // result = 0
        tt(Op::ConstI32(0)),
        tt(Op::LetSet {
            idx: RESULT,
            ty: IrType::I32,
        }),
        // ASCII fast path:
        //   if 0x09 <= cp <= 0x0D { result = 1; goto end }
        //   if cp == 0x20         { result = 1; goto end }
        // Implemented as `if cp >= 0x09 && cp <= 0x0D` via Lt/Ge.
        // Use an outer Block + Br to short-circuit on any hit so the
        // non-ASCII search stays untouched.
        tt(Op::Block {
            result_ty: None,
            body: vec![
                // if cp >= 0x09 { ... }
                tt(Op::LocalGet(0)),
                tt(Op::ConstI32(0x09)),
                tt(Op::Ge(IrType::I32)),
                tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: vec![
                        // if cp <= 0x0D { result = 1; br 2 }
                        tt(Op::LocalGet(0)),
                        tt(Op::ConstI32(0x0D)),
                        tt(Op::Le(IrType::I32)),
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                tt(Op::ConstI32(1)),
                                tt(Op::LetSet {
                                    idx: RESULT,
                                    ty: IrType::I32,
                                }),
                                tt(Op::Br { label_depth: 2 }),
                                tt(Op::ConstI32(0)),
                            ],
                            else_body: vec![tt(Op::ConstI32(0))],
                        }),
                        tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(0)),
                    ],
                    else_body: vec![tt(Op::ConstI32(0))],
                }),
                tt(Op::LetSet {
                    idx: SINK,
                    ty: IrType::I32,
                }),
                // if cp == 0x20 { result = 1; br 1 }
                tt(Op::LocalGet(0)),
                tt(Op::ConstI32(0x20)),
                tt(Op::Eq(IrType::I32)),
                tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: vec![
                        tt(Op::ConstI32(1)),
                        tt(Op::LetSet {
                            idx: RESULT,
                            ty: IrType::I32,
                        }),
                        tt(Op::Br { label_depth: 1 }),
                        tt(Op::ConstI32(0)),
                    ],
                    else_body: vec![tt(Op::ConstI32(0))],
                }),
                tt(Op::LetSet {
                    idx: SINK,
                    ty: IrType::I32,
                }),
                // non-ASCII search — only entered when ASCII fast
                // path missed. Mirrors `range_membership_search`.
                // count = i32.load(table_addr + 0)
                tt(Op::LocalGet(1)),
                tt(Op::LoadI32AtAbsolute { offset: 0 }),
                tt(Op::LetSet {
                    idx: COUNT,
                    ty: IrType::I32,
                }),
                // lo = 0; hi = count
                tt(Op::ConstI32(0)),
                tt(Op::LetSet {
                    idx: LO,
                    ty: IrType::I32,
                }),
                tt(Op::LetGet {
                    idx: COUNT,
                    ty: IrType::I32,
                }),
                tt(Op::LetSet {
                    idx: HI,
                    ty: IrType::I32,
                }),
                tt(Op::Block {
                    result_ty: None,
                    body: vec![tt(Op::Loop {
                        result_ty: None,
                        body: range_search_loop_body(LO, HI, MID, ENTRY, START, END, SINK, RESULT),
                    })],
                }),
            ],
        }),
        // Return result.
        tt(Op::LetGet {
            idx: RESULT,
            ty: IrType::I32,
        }),
        tt(Op::Return),
    ];
    StdlibFunction {
        name: "__is_whitespace",
        params: vec![IrType::I32, IrType::I32],
        ret: IrType::I32,
        body,
    }
}

/// Shared body generator for the range-membership helpers
/// `__is_combining_mark`. The same binary-search shape services the
/// whitespace helper through [`range_search_loop_body`] — kept as a
/// standalone builder so the wasm body's local layout stays
/// hand-auditable instead of buried inside a higher-order helper.
fn range_membership_helper(name: &'static str) -> StdlibFunction {
    const COUNT: u32 = 0;
    const LO: u32 = 1;
    const HI: u32 = 2;
    const MID: u32 = 3;
    const ENTRY: u32 = 4;
    const START: u32 = 5;
    const END: u32 = 6;
    const SINK: u32 = 7;
    const RESULT: u32 = 8;
    StdlibFunction {
        name,
        params: vec![IrType::I32, IrType::I32],
        ret: IrType::I32,
        body: vec![
            // count = i32.load(table_addr + 0)
            tt(Op::LocalGet(1)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: COUNT,
                ty: IrType::I32,
            }),
            // lo = 0; hi = count
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: LO,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: COUNT,
                ty: IrType::I32,
            }),
            tt(Op::LetSet {
                idx: HI,
                ty: IrType::I32,
            }),
            // result = 0 (default miss)
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: RESULT,
                ty: IrType::I32,
            }),
            tt(Op::Block {
                result_ty: None,
                body: vec![tt(Op::Loop {
                    result_ty: None,
                    body: range_search_loop_body(LO, HI, MID, ENTRY, START, END, SINK, RESULT),
                })],
            }),
            tt(Op::LetGet {
                idx: RESULT,
                ty: IrType::I32,
            }),
            tt(Op::Return),
        ],
    }
}

/// Inner loop body for the range-membership binary search. Encoded
/// as a free-standing helper so both `__is_combining_mark` and the
/// `__is_whitespace` non-ASCII path share the exact same control
/// shape — easier to reason about than two parallel sites that drift
/// over time.
///
/// Each entry is 8 bytes: `(start: u32 LE, end: u32 LE)`. The loop
/// narrows the window by comparing `cp` against `start..=end` —
/// `cp < start` shifts `hi`, `cp > end` shifts `lo`, an in-range hit
/// stores `1` into RESULT and `br 2`s out of the enclosing block.
///
/// Caller invariants:
///   * `lo`, `hi`, `result` are initialised before the surrounding
///     block.
///   * Block/loop nesting is `block { loop { <this body> } }` — the
///     `br 1` exits the loop on `lo >= hi`; the `br 2` on a match
///     exits the enclosing block.
#[allow(clippy::too_many_arguments)]
fn range_search_loop_body(
    lo: u32,
    hi: u32,
    mid: u32,
    entry: u32,
    start: u32,
    end: u32,
    sink: u32,
    result: u32,
) -> Vec<TaggedOp> {
    vec![
        // exit when lo >= hi
        tt(Op::LetGet {
            idx: lo,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: hi,
            ty: IrType::I32,
        }),
        tt(Op::Ge(IrType::I32)),
        tt(Op::BrIf { label_depth: 1 }),
        // mid = (lo + hi) / 2
        tt(Op::LetGet {
            idx: lo,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: hi,
            ty: IrType::I32,
        }),
        tt(Op::Add(IrType::I32)),
        tt(Op::ConstI32(2)),
        tt(Op::Div(IrType::I32)),
        tt(Op::LetSet {
            idx: mid,
            ty: IrType::I32,
        }),
        // entry = table_addr + 4 + mid * 8
        tt(Op::LocalGet(1)),
        tt(Op::ConstI32(4)),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetGet {
            idx: mid,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(8)),
        tt(Op::Mul(IrType::I32)),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetSet {
            idx: entry,
            ty: IrType::I32,
        }),
        // start = i32.load(entry + 0); end = i32.load(entry + 4)
        tt(Op::LetGet {
            idx: entry,
            ty: IrType::I32,
        }),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: start,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: entry,
            ty: IrType::I32,
        }),
        tt(Op::LoadI32AtAbsolute { offset: 4 }),
        tt(Op::LetSet {
            idx: end,
            ty: IrType::I32,
        }),
        // if cp < start { hi = mid }
        tt(Op::LocalGet(0)),
        tt(Op::LetGet {
            idx: start,
            ty: IrType::I32,
        }),
        tt(Op::Lt(IrType::I32)),
        tt(Op::If {
            result_ty: IrType::I32,
            then_body: vec![
                tt(Op::LetGet {
                    idx: mid,
                    ty: IrType::I32,
                }),
                tt(Op::LetSet {
                    idx: hi,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(0)),
            ],
            else_body: vec![
                // else if cp > end { lo = mid + 1 } else { match }
                tt(Op::LocalGet(0)),
                tt(Op::LetGet {
                    idx: end,
                    ty: IrType::I32,
                }),
                tt(Op::Gt(IrType::I32)),
                tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: vec![
                        tt(Op::LetGet {
                            idx: mid,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(1)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: lo,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(0)),
                    ],
                    else_body: vec![
                        // match — result = 1; br 3 exits the
                        // enclosing block (depth 0 = inner If, 1 =
                        // outer If, 2 = Loop, 3 = Block).
                        tt(Op::ConstI32(1)),
                        tt(Op::LetSet {
                            idx: result,
                            ty: IrType::I32,
                        }),
                        tt(Op::Br { label_depth: 3 }),
                        tt(Op::ConstI32(0)),
                    ],
                }),
                tt(Op::LetSet {
                    idx: sink,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(0)),
            ],
        }),
        tt(Op::LetSet {
            idx: sink,
            ty: IrType::I32,
        }),
        tt(Op::Br { label_depth: 0 }),
    ]
}
/// v3++ b-5 internal helper: `__decomp_lookup(cp, table_addr) -> i32`.
///
/// Binary-searches the canonical (NFD) or compatibility (NFKD)
/// decomposition index embedded in the wasm data section. The table
/// layout is `[count: u32 LE][(cp: u32, off: u32, len: u32) × count]`
/// (12-byte stride). Returns `(off << 8) | len` on a hit so the
/// caller can decode both in one i32 — len is bounded by 18 (UCD 14
/// worst case is U+FDFA at 18 cps) and off fits in 24 bits since the
/// pool is well under 16M entries. Returns `0` on miss; len of `0` is
/// a valid "no decomposition" sentinel because the table never stores
/// zero-length entries.
fn decomp_lookup_helper() -> StdlibFunction {
    const COUNT: u32 = 0;
    const LO: u32 = 1;
    const HI: u32 = 2;
    const MID: u32 = 3;
    const ENTRY: u32 = 4;
    const KEY: u32 = 5;
    const SINK: u32 = 6;
    const RESULT: u32 = 7;
    StdlibFunction {
        name: "__decomp_lookup",
        params: vec![IrType::I32, IrType::I32],
        ret: IrType::I32,
        body: vec![
            tt(Op::LocalGet(1)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: COUNT,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: LO,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: COUNT,
                ty: IrType::I32,
            }),
            tt(Op::LetSet {
                idx: HI,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: RESULT,
                ty: IrType::I32,
            }),
            tt(Op::Block {
                result_ty: None,
                body: vec![tt(Op::Loop {
                    result_ty: None,
                    body: vec![
                        tt(Op::LetGet {
                            idx: LO,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: HI,
                            ty: IrType::I32,
                        }),
                        tt(Op::Ge(IrType::I32)),
                        tt(Op::BrIf { label_depth: 1 }),
                        tt(Op::LetGet {
                            idx: LO,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: HI,
                            ty: IrType::I32,
                        }),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::ConstI32(2)),
                        tt(Op::Div(IrType::I32)),
                        tt(Op::LetSet {
                            idx: MID,
                            ty: IrType::I32,
                        }),
                        // entry = table_addr + 4 + mid * 12
                        tt(Op::LocalGet(1)),
                        tt(Op::ConstI32(4)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetGet {
                            idx: MID,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(12)),
                        tt(Op::Mul(IrType::I32)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: ENTRY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: ENTRY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LoadI32AtAbsolute { offset: 0 }),
                        tt(Op::LetSet {
                            idx: KEY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: KEY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LocalGet(0)),
                        tt(Op::Eq(IrType::I32)),
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                // result = (load(entry+4) << 8) | load(entry+8)
                                tt(Op::LetGet {
                                    idx: ENTRY,
                                    ty: IrType::I32,
                                }),
                                tt(Op::LoadI32AtAbsolute { offset: 4 }),
                                tt(Op::ConstI32(256)),
                                tt(Op::Mul(IrType::I32)),
                                tt(Op::LetGet {
                                    idx: ENTRY,
                                    ty: IrType::I32,
                                }),
                                tt(Op::LoadI32AtAbsolute { offset: 8 }),
                                tt(Op::Add(IrType::I32)),
                                tt(Op::LetSet {
                                    idx: RESULT,
                                    ty: IrType::I32,
                                }),
                                tt(Op::Br { label_depth: 2 }),
                                tt(Op::ConstI32(0)),
                            ],
                            else_body: vec![tt(Op::ConstI32(0))],
                        }),
                        tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: KEY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LocalGet(0)),
                        tt(Op::Lt(IrType::I32)),
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                tt(Op::LetGet {
                                    idx: MID,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(1)),
                                tt(Op::Add(IrType::I32)),
                                tt(Op::LetSet {
                                    idx: LO,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(0)),
                            ],
                            else_body: vec![
                                tt(Op::LetGet {
                                    idx: MID,
                                    ty: IrType::I32,
                                }),
                                tt(Op::LetSet {
                                    idx: HI,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(0)),
                            ],
                        }),
                        tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }),
                        tt(Op::Br { label_depth: 0 }),
                    ],
                })],
            }),
            tt(Op::LetGet {
                idx: RESULT,
                ty: IrType::I32,
            }),
            tt(Op::Return),
        ],
    }
}

/// v3++ b-5 internal helper: `__ccc_lookup(cp, table_addr) -> i32`.
///
/// Binary-searches the Canonical_Combining_Class table for `cp`.
/// Returns the CCC value on a hit, or `0` on a miss (the UCD
/// convention: absent entries default to Not_Reordered).
fn ccc_lookup_helper() -> StdlibFunction {
    const COUNT: u32 = 0;
    const LO: u32 = 1;
    const HI: u32 = 2;
    const MID: u32 = 3;
    const ENTRY: u32 = 4;
    const KEY: u32 = 5;
    const SINK: u32 = 6;
    const RESULT: u32 = 7;
    StdlibFunction {
        name: "__ccc_lookup",
        params: vec![IrType::I32, IrType::I32],
        ret: IrType::I32,
        body: vec![
            tt(Op::LocalGet(1)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: COUNT,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: LO,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: COUNT,
                ty: IrType::I32,
            }),
            tt(Op::LetSet {
                idx: HI,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: RESULT,
                ty: IrType::I32,
            }),
            tt(Op::Block {
                result_ty: None,
                body: vec![tt(Op::Loop {
                    result_ty: None,
                    body: vec![
                        tt(Op::LetGet {
                            idx: LO,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: HI,
                            ty: IrType::I32,
                        }),
                        tt(Op::Ge(IrType::I32)),
                        tt(Op::BrIf { label_depth: 1 }),
                        tt(Op::LetGet {
                            idx: LO,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: HI,
                            ty: IrType::I32,
                        }),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::ConstI32(2)),
                        tt(Op::Div(IrType::I32)),
                        tt(Op::LetSet {
                            idx: MID,
                            ty: IrType::I32,
                        }),
                        // entry = table_addr + 4 + mid * 8
                        tt(Op::LocalGet(1)),
                        tt(Op::ConstI32(4)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetGet {
                            idx: MID,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(8)),
                        tt(Op::Mul(IrType::I32)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: ENTRY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: ENTRY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LoadI32AtAbsolute { offset: 0 }),
                        tt(Op::LetSet {
                            idx: KEY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: KEY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LocalGet(0)),
                        tt(Op::Eq(IrType::I32)),
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                tt(Op::LetGet {
                                    idx: ENTRY,
                                    ty: IrType::I32,
                                }),
                                tt(Op::LoadI32AtAbsolute { offset: 4 }),
                                tt(Op::LetSet {
                                    idx: RESULT,
                                    ty: IrType::I32,
                                }),
                                tt(Op::Br { label_depth: 2 }),
                                tt(Op::ConstI32(0)),
                            ],
                            else_body: vec![tt(Op::ConstI32(0))],
                        }),
                        tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: KEY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LocalGet(0)),
                        tt(Op::Lt(IrType::I32)),
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                tt(Op::LetGet {
                                    idx: MID,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(1)),
                                tt(Op::Add(IrType::I32)),
                                tt(Op::LetSet {
                                    idx: LO,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(0)),
                            ],
                            else_body: vec![
                                tt(Op::LetGet {
                                    idx: MID,
                                    ty: IrType::I32,
                                }),
                                tt(Op::LetSet {
                                    idx: HI,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(0)),
                            ],
                        }),
                        tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }),
                        tt(Op::Br { label_depth: 0 }),
                    ],
                })],
            }),
            tt(Op::LetGet {
                idx: RESULT,
                ty: IrType::I32,
            }),
            tt(Op::Return),
        ],
    }
}

/// v3++ b-5 internal helper:
/// `__compose_lookup(first, second, table_addr) -> i32`.
///
/// Binary-searches the canonical composition pair table sorted
/// lexicographically by `(first, second)`. Returns the composed
/// codepoint on a hit, or `-1` on a miss. Composition exclusions are
/// filtered at generation time so the runtime needs no extra check.
fn compose_lookup_helper() -> StdlibFunction {
    const COUNT: u32 = 0;
    const LO: u32 = 1;
    const HI: u32 = 2;
    const MID: u32 = 3;
    const ENTRY: u32 = 4;
    const KEY_A: u32 = 5;
    const KEY_B: u32 = 6;
    const SINK: u32 = 7;
    const RESULT: u32 = 8;
    StdlibFunction {
        name: "__compose_lookup",
        params: vec![IrType::I32, IrType::I32, IrType::I32],
        ret: IrType::I32,
        body: vec![
            tt(Op::LocalGet(2)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: COUNT,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: LO,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: COUNT,
                ty: IrType::I32,
            }),
            tt(Op::LetSet {
                idx: HI,
                ty: IrType::I32,
            }),
            // sentinel -1: codegen represents as 0xFFFFFFFF i32
            tt(Op::ConstI32(-1)),
            tt(Op::LetSet {
                idx: RESULT,
                ty: IrType::I32,
            }),
            tt(Op::Block {
                result_ty: None,
                body: vec![tt(Op::Loop {
                    result_ty: None,
                    body: vec![
                        tt(Op::LetGet {
                            idx: LO,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: HI,
                            ty: IrType::I32,
                        }),
                        tt(Op::Ge(IrType::I32)),
                        tt(Op::BrIf { label_depth: 1 }),
                        tt(Op::LetGet {
                            idx: LO,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: HI,
                            ty: IrType::I32,
                        }),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::ConstI32(2)),
                        tt(Op::Div(IrType::I32)),
                        tt(Op::LetSet {
                            idx: MID,
                            ty: IrType::I32,
                        }),
                        // entry = table_addr + 4 + mid * 12
                        tt(Op::LocalGet(2)),
                        tt(Op::ConstI32(4)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetGet {
                            idx: MID,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(12)),
                        tt(Op::Mul(IrType::I32)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: ENTRY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: ENTRY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LoadI32AtAbsolute { offset: 0 }),
                        tt(Op::LetSet {
                            idx: KEY_A,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: ENTRY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LoadI32AtAbsolute { offset: 4 }),
                        tt(Op::LetSet {
                            idx: KEY_B,
                            ty: IrType::I32,
                        }),
                        // if key_a == first && key_b == second { match }
                        tt(Op::LetGet {
                            idx: KEY_A,
                            ty: IrType::I32,
                        }),
                        tt(Op::LocalGet(0)),
                        tt(Op::Eq(IrType::I32)),
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                tt(Op::LetGet {
                                    idx: KEY_B,
                                    ty: IrType::I32,
                                }),
                                tt(Op::LocalGet(1)),
                                tt(Op::Eq(IrType::I32)),
                                tt(Op::If {
                                    result_ty: IrType::I32,
                                    then_body: vec![
                                        tt(Op::LetGet {
                                            idx: ENTRY,
                                            ty: IrType::I32,
                                        }),
                                        tt(Op::LoadI32AtAbsolute { offset: 8 }),
                                        tt(Op::LetSet {
                                            idx: RESULT,
                                            ty: IrType::I32,
                                        }),
                                        // Depth 0 = inner If, 1 = outer If,
                                        // 2 = Loop, 3 = Block. Jump out of
                                        // the search Block on a hit.
                                        tt(Op::Br { label_depth: 3 }),
                                        tt(Op::ConstI32(0)),
                                    ],
                                    else_body: vec![tt(Op::ConstI32(0))],
                                }),
                            ],
                            else_body: vec![tt(Op::ConstI32(0))],
                        }),
                        tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }),
                        // Compare (key_a, key_b) < (first, second)?
                        // if key_a < first OR (key_a == first AND
                        //                       key_b < second): lo = mid + 1
                        // else: hi = mid
                        tt(Op::LetGet {
                            idx: KEY_A,
                            ty: IrType::I32,
                        }),
                        tt(Op::LocalGet(0)),
                        tt(Op::Lt(IrType::I32)),
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![tt(Op::ConstI32(1))],
                            else_body: vec![
                                // key_a == first?
                                tt(Op::LetGet {
                                    idx: KEY_A,
                                    ty: IrType::I32,
                                }),
                                tt(Op::LocalGet(0)),
                                tt(Op::Eq(IrType::I32)),
                                tt(Op::If {
                                    result_ty: IrType::I32,
                                    then_body: vec![
                                        // key_b < second?
                                        tt(Op::LetGet {
                                            idx: KEY_B,
                                            ty: IrType::I32,
                                        }),
                                        tt(Op::LocalGet(1)),
                                        tt(Op::Lt(IrType::I32)),
                                    ],
                                    else_body: vec![tt(Op::ConstI32(0))],
                                }),
                            ],
                        }),
                        // top of stack: 1 if (key_a, key_b) < (first, second)
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                tt(Op::LetGet {
                                    idx: MID,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(1)),
                                tt(Op::Add(IrType::I32)),
                                tt(Op::LetSet {
                                    idx: LO,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(0)),
                            ],
                            else_body: vec![
                                tt(Op::LetGet {
                                    idx: MID,
                                    ty: IrType::I32,
                                }),
                                tt(Op::LetSet {
                                    idx: HI,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(0)),
                            ],
                        }),
                        tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }),
                        tt(Op::Br { label_depth: 0 }),
                    ],
                })],
            }),
            tt(Op::LetGet {
                idx: RESULT,
                ty: IrType::I32,
            }),
            tt(Op::Return),
        ],
    }
}

/// v3++ b-5 normalization form discriminator for [`normalize_body`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NormForm {
    /// NFD - canonical decomposition + canonical reorder.
    Nfd,
    /// NFKD - compatibility decomposition + canonical reorder.
    Nfkd,
    /// NFC - canonical decomposition + reorder + canonical composition.
    Nfc,
    /// NFKC - compatibility decomposition + reorder + canonical composition.
    Nfkc,
}

impl NormForm {
    fn name(self) -> &'static str {
        match self {
            NormForm::Nfd => "nfd",
            NormForm::Nfkd => "nfkd",
            NormForm::Nfc => "nfc",
            NormForm::Nfkc => "nfkc",
        }
    }

    /// `true` when the form uses the compatibility (NFKD) decomposition
    /// table; `false` for canonical (NFD).
    fn use_compatibility(self) -> bool {
        matches!(self, NormForm::Nfkd | NormForm::Nfkc)
    }

    /// `true` when the form runs a composition pass after decomposition
    /// + reorder.
    fn composes(self) -> bool {
        matches!(self, NormForm::Nfc | NormForm::Nfkc)
    }
}

fn nfd_string() -> StdlibFunction {
    normalize_body(NormForm::Nfd)
}

fn nfkd_string() -> StdlibFunction {
    normalize_body(NormForm::Nfkd)
}

fn nfc_string() -> StdlibFunction {
    normalize_body(NormForm::Nfc)
}

fn nfkc_string() -> StdlibFunction {
    normalize_body(NormForm::Nfkc)
}

/// Shared body builder for `nfd` / `nfkd` / `nfc` / `nfkc`.
///
/// ## v3++ b-5 scope
///
/// The body implements the full UAX #15 pipeline against the embedded
/// UCD 14.0.0 tables: decompose -> canonical reorder -> (optional)
/// canonical composition -> re-encode. Hangul syllables are decomposed
/// and composed algorithmically per UAX #15 section 16 so the table
/// data section does not have to embed the ~11K-entry syllable block.
///
/// ### Layout
///
/// Two scratch buffers are bump-allocated up front:
///
///   * `cp_buf`: `[len: u32 LE][u32 entries x len]` - the working
///     codepoint buffer, sized worst-case `4 + s.len() * 18 * 4` bytes
///     (UCD 14's longest decomposition is U+FDFA at 18 cps).
///   * `out_buf`: `[len: u32 LE][u8 utf8 x len]` - the encoded result,
///     sized worst-case `4 + s.len() * 4` bytes (a 1-byte ASCII cp
///     mapping to a 4-byte UTF-8 sequence). The body returns this
///     buffer's address.
///
/// ### Phases
///
///   1. Decompose: walk the input byte-by-byte, decode each UTF-8 cp,
///      and append the cp (or its decomposition / Hangul jamo
///      sequence) to `cp_buf`.
///   2. Canonical reorder: walk `cp_buf` and run an in-place insertion
///      sort on each maximal run of non-starters, ordered by CCC.
///   3. Composition (NFC / NFKC only): scan `cp_buf` left-to-right
///      maintaining a "last starter index" and "last CCC" pair; for
///      each cp try Hangul composition then table composition with
///      the live starter, honouring the UAX #15 blocking rule, and
///      collapse the pair into the starter slot when a match is found.
///   4. Encode: re-emit each `cp_buf` entry as UTF-8 into `out_buf`.
///
/// ### Locals
///
///   * 0  `s_len:   I32` - input byte count.
///   * 1  `cp_base: I32` - cp_buf pointer.
///   * 2  `out_base: I32` - out_buf pointer.
///   * 3  `cp_len:  I32` - cp_buf logical length.
///   * 4  `i:       I32` - input read cursor.
///   * 5  `j:       I32` - cp_buf write cursor (in bytes).
///   * 6  `cp:      I32` - decoded codepoint.
///   * 7  `cp_bytes: I32` - byte count of the decoded codepoint.
///   * 8  `b0:      I32` - leading byte buffer.
///   * 9  `b_tmp:   I32` - continuation byte buffer.
///   * 10 `sink:    I32` - drop slot for If-arm i32 placeholders.
///   * 11 `tmp:     I32` - general-purpose loop scratch.
///   * 12 `lookup:  I32` - packed decomp lookup result.
///   * 13 `tmp2:    I32` - second general-purpose loop scratch.
///   * 14 `out_j:   I32` - out_buf write cursor (in bytes).
///   * 15 `folded:  I32` - cp slot being encoded.
///   * 16 `k:       I32` - reorder pass cursor.
///   * 17 `run_end: I32` - end of current non-starter run during reorder.
#[allow(clippy::vec_init_then_push, clippy::too_many_lines)]
fn normalize_body(form: NormForm) -> StdlibFunction {
    const S_LEN: u32 = 0;
    const CP_BASE: u32 = 1;
    const OUT_BASE: u32 = 2;
    const CP_LEN: u32 = 3;
    const I: u32 = 4;
    const J: u32 = 5;
    const CP: u32 = 6;
    const CP_BYTES: u32 = 7;
    const B0: u32 = 8;
    const B_TMP: u32 = 9;
    const SINK: u32 = 10;
    const TMP: u32 = 11;
    const LOOKUP: u32 = 12;
    const TMP2: u32 = 13;
    const OUT_J: u32 = 14;
    const FOLDED: u32 = 15;
    const K: u32 = 16;
    const RUN_END: u32 = 17;
    // Dedicated slot for `append_cp_from_stack` so the helper doesn't
    // stomp on TMP / TMP2 between Hangul-decompose phases.
    const APPEND_TMP: u32 = 18;

    let decomp_idx = DECOMP_LOOKUP_INDEX;
    let ccc_idx = CCC_LOOKUP_INDEX;
    let compose_idx = COMPOSE_LOOKUP_INDEX;
    let compat = form.use_compatibility();

    // Helper: load the `i + off`-th input byte. Pushes one i32.
    let load_input_byte = |off: i32| -> Vec<TaggedOp> {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4 + off)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LetGet {
                idx: I,
                ty: IrType::I32,
            }),
            tt(Op::Add(IrType::I32)),
            tt(Op::LoadI8UAtAbsolute { offset: 0 }),
        ]
    };

    let trap_invalid_utf8 = || -> Vec<TaggedOp> {
        vec![
            tt(Op::Trap {
                kind: TrapKind::InvalidUtf8,
            }),
            tt(Op::ConstI32(0)),
        ]
    };

    let load_continuation = |n: i32| -> Vec<TaggedOp> {
        let mut out = load_input_byte(n);
        out.push(tt(Op::ConstI32(0x3F)));
        out.push(tt(Op::BitAnd(IrType::I32)));
        out.push(tt(Op::LetSet {
            idx: B_TMP,
            ty: IrType::I32,
        }));
        out
    };

    // Helper: append `cp` (i32 on stack top) as a u32 LE at
    // `cp_base + 4 + j`; advance j by 4. Uses APPEND_TMP so the helper
    // never stomps on TMP / TMP2 / other state slots.
    let append_cp_from_stack = || -> Vec<TaggedOp> {
        let mut v = Vec::new();
        v.push(tt(Op::LetSet {
            idx: APPEND_TMP,
            ty: IrType::I32,
        }));
        v.push(tt(Op::LetGet {
            idx: CP_BASE,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(4)));
        v.push(tt(Op::Add(IrType::I32)));
        v.push(tt(Op::LetGet {
            idx: J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::Add(IrType::I32)));
        v.push(tt(Op::LetGet {
            idx: APPEND_TMP,
            ty: IrType::I32,
        }));
        v.push(tt(Op::StoreI32AtAbsolute { offset: 0 }));
        v.push(tt(Op::LetGet {
            idx: J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(4)));
        v.push(tt(Op::Add(IrType::I32)));
        v.push(tt(Op::LetSet {
            idx: J,
            ty: IrType::I32,
        }));
        v
    };

    // ----- UTF-8 decode (CP, CP_BYTES) -----
    let mut decode_seq: Vec<TaggedOp> = Vec::new();
    decode_seq.extend(load_input_byte(0));
    decode_seq.push(tt(Op::LetSet {
        idx: B0,
        ty: IrType::I32,
    }));
    decode_seq.push(tt(Op::LetGet {
        idx: B0,
        ty: IrType::I32,
    }));
    decode_seq.push(tt(Op::ConstI32(0x80)));
    decode_seq.push(tt(Op::Lt(IrType::I32)));
    decode_seq.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: {
            let mut v = Vec::new();
            v.push(tt(Op::LetGet {
                idx: B0,
                ty: IrType::I32,
            }));
            v.push(tt(Op::LetSet {
                idx: CP,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(1)));
            v.push(tt(Op::LetSet {
                idx: CP_BYTES,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0)));
            v
        },
        else_body: {
            let mut v = Vec::new();
            v.push(tt(Op::LetGet {
                idx: B0,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0xC2)));
            v.push(tt(Op::Lt(IrType::I32)));
            v.push(tt(Op::If {
                result_ty: IrType::I32,
                then_body: trap_invalid_utf8(),
                else_body: {
                    let mut v2 = Vec::new();
                    v2.push(tt(Op::LetGet {
                        idx: B0,
                        ty: IrType::I32,
                    }));
                    v2.push(tt(Op::ConstI32(0xE0)));
                    v2.push(tt(Op::Lt(IrType::I32)));
                    v2.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            let mut t = Vec::new();
                            t.push(tt(Op::LetGet {
                                idx: I,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(1)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetGet {
                                idx: S_LEN,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::Ge(IrType::I32)));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: trap_invalid_utf8(),
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            t.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            t.extend(load_continuation(1));
                            t.push(tt(Op::LetGet {
                                idx: B0,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0x1F)));
                            t.push(tt(Op::BitAnd(IrType::I32)));
                            t.push(tt(Op::ConstI32(64)));
                            t.push(tt(Op::Mul(IrType::I32)));
                            t.push(tt(Op::LetGet {
                                idx: B_TMP,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetSet {
                                idx: CP,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(2)));
                            t.push(tt(Op::LetSet {
                                idx: CP_BYTES,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                        else_body: {
                            let mut e = Vec::new();
                            e.push(tt(Op::LetGet {
                                idx: B0,
                                ty: IrType::I32,
                            }));
                            e.push(tt(Op::ConstI32(0xF0)));
                            e.push(tt(Op::Lt(IrType::I32)));
                            e.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: {
                                    let mut t = Vec::new();
                                    t.push(tt(Op::LetGet {
                                        idx: I,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(2)));
                                    t.push(tt(Op::Add(IrType::I32)));
                                    t.push(tt(Op::LetGet {
                                        idx: S_LEN,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::Ge(IrType::I32)));
                                    t.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: trap_invalid_utf8(),
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    t.push(tt(Op::LetSet {
                                        idx: SINK,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::LetGet {
                                        idx: B0,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(0x0F)));
                                    t.push(tt(Op::BitAnd(IrType::I32)));
                                    t.push(tt(Op::ConstI32(4096)));
                                    t.push(tt(Op::Mul(IrType::I32)));
                                    t.extend(load_continuation(1));
                                    t.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(64)));
                                    t.push(tt(Op::Mul(IrType::I32)));
                                    t.push(tt(Op::Add(IrType::I32)));
                                    t.extend(load_continuation(2));
                                    t.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::Add(IrType::I32)));
                                    t.push(tt(Op::LetSet {
                                        idx: CP,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(3)));
                                    t.push(tt(Op::LetSet {
                                        idx: CP_BYTES,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(0)));
                                    t
                                },
                                else_body: {
                                    let mut e2 = Vec::new();
                                    e2.push(tt(Op::LetGet {
                                        idx: B0,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(0xF8)));
                                    e2.push(tt(Op::Ge(IrType::I32)));
                                    e2.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: trap_invalid_utf8(),
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    e2.push(tt(Op::LetSet {
                                        idx: SINK,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::LetGet {
                                        idx: I,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(3)));
                                    e2.push(tt(Op::Add(IrType::I32)));
                                    e2.push(tt(Op::LetGet {
                                        idx: S_LEN,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::Ge(IrType::I32)));
                                    e2.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: trap_invalid_utf8(),
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    e2.push(tt(Op::LetSet {
                                        idx: SINK,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::LetGet {
                                        idx: B0,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(0x07)));
                                    e2.push(tt(Op::BitAnd(IrType::I32)));
                                    e2.push(tt(Op::ConstI32(262144)));
                                    e2.push(tt(Op::Mul(IrType::I32)));
                                    e2.extend(load_continuation(1));
                                    e2.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(4096)));
                                    e2.push(tt(Op::Mul(IrType::I32)));
                                    e2.push(tt(Op::Add(IrType::I32)));
                                    e2.extend(load_continuation(2));
                                    e2.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(64)));
                                    e2.push(tt(Op::Mul(IrType::I32)));
                                    e2.push(tt(Op::Add(IrType::I32)));
                                    e2.extend(load_continuation(3));
                                    e2.push(tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::Add(IrType::I32)));
                                    e2.push(tt(Op::LetSet {
                                        idx: CP,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(4)));
                                    e2.push(tt(Op::LetSet {
                                        idx: CP_BYTES,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(0)));
                                    e2
                                },
                            }));
                            e
                        },
                    }));
                    v2
                },
            }));
            v
        },
    }));
    decode_seq.push(tt(Op::LetSet {
        idx: SINK,
        ty: IrType::I32,
    }));

    // ----- decompose dispatch -----
    // Compute decomposition into cp_buf for the current cp:
    //   1. Try Hangul algorithmic decomposition.
    //   2. Otherwise call __decomp_lookup; on hit write the pool slice;
    //      on miss write cp as-is.
    let mut decompose_seq: Vec<TaggedOp> = Vec::new();
    // Hangul fast path: s_index = cp - S_BASE; if 0 <= s_index <
    // S_COUNT, write L / V / (T) and skip table lookup.
    decompose_seq.push(tt(Op::LetGet {
        idx: CP,
        ty: IrType::I32,
    }));
    decompose_seq.push(tt(Op::ConstI32(crate::normalization::HANGUL_S_BASE as i32)));
    decompose_seq.push(tt(Op::Ge(IrType::I32)));
    decompose_seq.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: {
            // cp - S_BASE
            let mut t = Vec::new();
            t.push(tt(Op::LetGet {
                idx: CP,
                ty: IrType::I32,
            }));
            t.push(tt(Op::ConstI32(crate::normalization::HANGUL_S_BASE as i32)));
            t.push(tt(Op::Sub(IrType::I32)));
            t.push(tt(Op::LetSet {
                idx: TMP,
                ty: IrType::I32,
            }));
            // if tmp < S_COUNT { hangul decompose } else { table }
            t.push(tt(Op::LetGet {
                idx: TMP,
                ty: IrType::I32,
            }));
            t.push(tt(Op::ConstI32(
                crate::normalization::HANGUL_S_COUNT as i32,
            )));
            t.push(tt(Op::Lt(IrType::I32)));
            t.push(tt(Op::If {
                result_ty: IrType::I32,
                then_body: {
                    let mut h = Vec::new();
                    // l = L_BASE + tmp / N_COUNT
                    h.push(tt(Op::ConstI32(crate::normalization::HANGUL_L_BASE as i32)));
                    h.push(tt(Op::LetGet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    h.push(tt(Op::ConstI32(
                        crate::normalization::HANGUL_N_COUNT as i32,
                    )));
                    h.push(tt(Op::Div(IrType::I32)));
                    h.push(tt(Op::Add(IrType::I32)));
                    h.extend(append_cp_from_stack());
                    // v = V_BASE + (tmp % N_COUNT) / T_COUNT
                    h.push(tt(Op::ConstI32(crate::normalization::HANGUL_V_BASE as i32)));
                    h.push(tt(Op::LetGet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    h.push(tt(Op::ConstI32(
                        crate::normalization::HANGUL_N_COUNT as i32,
                    )));
                    h.push(tt(Op::Mod(IrType::I32)));
                    h.push(tt(Op::ConstI32(
                        crate::normalization::HANGUL_T_COUNT as i32,
                    )));
                    h.push(tt(Op::Div(IrType::I32)));
                    h.push(tt(Op::Add(IrType::I32)));
                    h.extend(append_cp_from_stack());
                    // t_offset = tmp % T_COUNT; if != 0 write T_BASE + t_offset
                    h.push(tt(Op::LetGet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    h.push(tt(Op::ConstI32(
                        crate::normalization::HANGUL_T_COUNT as i32,
                    )));
                    h.push(tt(Op::Mod(IrType::I32)));
                    h.push(tt(Op::LetSet {
                        idx: TMP2,
                        ty: IrType::I32,
                    }));
                    h.push(tt(Op::LetGet {
                        idx: TMP2,
                        ty: IrType::I32,
                    }));
                    h.push(tt(Op::ConstI32(0)));
                    h.push(tt(Op::Ne(IrType::I32)));
                    h.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            let mut tb = Vec::new();
                            tb.push(tt(Op::ConstI32(crate::normalization::HANGUL_T_BASE as i32)));
                            tb.push(tt(Op::LetGet {
                                idx: TMP2,
                                ty: IrType::I32,
                            }));
                            tb.push(tt(Op::Add(IrType::I32)));
                            tb.extend(append_cp_from_stack());
                            tb.push(tt(Op::ConstI32(0)));
                            tb
                        },
                        else_body: vec![tt(Op::ConstI32(0))],
                    }));
                    h.push(tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }));
                    // 1 = "took hangul path"
                    h.push(tt(Op::ConstI32(1)));
                    h
                },
                else_body: vec![tt(Op::ConstI32(0))],
            }));
            t
        },
        else_body: vec![tt(Op::ConstI32(0))],
    }));
    // top of stack = 1 if Hangul handled, else 0
    decompose_seq.push(tt(Op::LetSet {
        idx: TMP,
        ty: IrType::I32,
    }));
    // if !hangul_handled: try table lookup, else cp passthrough
    decompose_seq.push(tt(Op::LetGet {
        idx: TMP,
        ty: IrType::I32,
    }));
    decompose_seq.push(tt(Op::ConstI32(0)));
    decompose_seq.push(tt(Op::Eq(IrType::I32)));
    decompose_seq.push(tt(Op::If {
        result_ty: IrType::I32,
        then_body: {
            let mut v = Vec::new();
            // lookup = __decomp_lookup(cp, table_addr)
            v.push(tt(Op::LetGet {
                idx: CP,
                ty: IrType::I32,
            }));
            v.push(tt(Op::DecompTableAddr {
                compatibility: compat,
            }));
            v.push(tt(Op::Call {
                fn_index: decomp_idx,
                arg_count: 2,
                param_tys: vec![IrType::I32, IrType::I32],
                ret_ty: IrType::I32,
            }));
            v.push(tt(Op::LetSet {
                idx: LOOKUP,
                ty: IrType::I32,
            }));
            // if lookup == 0: append cp
            v.push(tt(Op::LetGet {
                idx: LOOKUP,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0)));
            v.push(tt(Op::Eq(IrType::I32)));
            v.push(tt(Op::If {
                result_ty: IrType::I32,
                then_body: {
                    let mut t = Vec::new();
                    t.push(tt(Op::LetGet {
                        idx: CP,
                        ty: IrType::I32,
                    }));
                    t.extend(append_cp_from_stack());
                    t.push(tt(Op::ConstI32(0)));
                    t
                },
                else_body: {
                    // off = lookup / 256; len = lookup % 256
                    let mut t = Vec::new();
                    t.push(tt(Op::LetGet {
                        idx: LOOKUP,
                        ty: IrType::I32,
                    }));
                    t.push(tt(Op::ConstI32(256)));
                    t.push(tt(Op::Div(IrType::I32)));
                    t.push(tt(Op::LetSet {
                        idx: TMP,
                        ty: IrType::I32,
                    })); // off
                    t.push(tt(Op::LetGet {
                        idx: LOOKUP,
                        ty: IrType::I32,
                    }));
                    t.push(tt(Op::ConstI32(0xFF)));
                    t.push(tt(Op::BitAnd(IrType::I32)));
                    t.push(tt(Op::LetSet {
                        idx: TMP2,
                        ty: IrType::I32,
                    })); // len
                         // pool_base = table_addr + 4 + index_count * 12 + 4
                         // We compute pool_base on the fly:
                         //   pool_header = table_addr + 4 + index_count * 12
                         //   pool_base   = pool_header + 4
                         // index_count = load(table_addr, 0)
                    t.push(tt(Op::DecompTableAddr {
                        compatibility: compat,
                    }));
                    t.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                    t.push(tt(Op::ConstI32(12)));
                    t.push(tt(Op::Mul(IrType::I32)));
                    t.push(tt(Op::ConstI32(8))); // +4 header +4 pool count
                    t.push(tt(Op::Add(IrType::I32)));
                    t.push(tt(Op::DecompTableAddr {
                        compatibility: compat,
                    }));
                    t.push(tt(Op::Add(IrType::I32)));
                    // pool_base now on stack. Stash.
                    t.push(tt(Op::LetSet {
                        idx: LOOKUP,
                        ty: IrType::I32,
                    })); // repurpose: pool_base
                         // for (k=0; k<len; k++) append load_i32(pool_base + (off+k)*4)
                    t.push(tt(Op::ConstI32(0)));
                    t.push(tt(Op::LetSet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    t.push(tt(Op::Block {
                        result_ty: None,
                        body: vec![tt(Op::Loop {
                            result_ty: None,
                            body: {
                                let mut lb = Vec::new();
                                lb.push(tt(Op::LetGet {
                                    idx: K,
                                    ty: IrType::I32,
                                }));
                                lb.push(tt(Op::LetGet {
                                    idx: TMP2,
                                    ty: IrType::I32,
                                }));
                                lb.push(tt(Op::Ge(IrType::I32)));
                                lb.push(tt(Op::BrIf { label_depth: 1 }));
                                // addr = pool_base + (off + k) * 4
                                lb.push(tt(Op::LetGet {
                                    idx: LOOKUP,
                                    ty: IrType::I32,
                                }));
                                lb.push(tt(Op::LetGet {
                                    idx: TMP,
                                    ty: IrType::I32,
                                }));
                                lb.push(tt(Op::LetGet {
                                    idx: K,
                                    ty: IrType::I32,
                                }));
                                lb.push(tt(Op::Add(IrType::I32)));
                                lb.push(tt(Op::ConstI32(4)));
                                lb.push(tt(Op::Mul(IrType::I32)));
                                lb.push(tt(Op::Add(IrType::I32)));
                                lb.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                                lb.extend(append_cp_from_stack());
                                lb.push(tt(Op::LetGet {
                                    idx: K,
                                    ty: IrType::I32,
                                }));
                                lb.push(tt(Op::ConstI32(1)));
                                lb.push(tt(Op::Add(IrType::I32)));
                                lb.push(tt(Op::LetSet {
                                    idx: K,
                                    ty: IrType::I32,
                                }));
                                lb.push(tt(Op::Br { label_depth: 0 }));
                                lb
                            },
                        })],
                    }));
                    t.push(tt(Op::ConstI32(0)));
                    t
                },
            }));
            v.push(tt(Op::LetSet {
                idx: SINK,
                ty: IrType::I32,
            }));
            v.push(tt(Op::ConstI32(0)));
            v
        },
        else_body: vec![tt(Op::ConstI32(0))],
    }));
    decompose_seq.push(tt(Op::LetSet {
        idx: SINK,
        ty: IrType::I32,
    }));

    // ----- canonical reorder (insertion sort within non-starter runs) -----
    // cp_buf has cp_len u32 entries starting at cp_base + 4. Walk via
    // k. Each non-starter run [start, end) is bubble-sorted by CCC.
    let reorder_seq: Vec<TaggedOp> = {
        let mut v: Vec<TaggedOp> = Vec::new();
        // Compute cp_len = j / 4 (j is byte cursor, 4 bytes per entry)
        v.push(tt(Op::LetGet {
            idx: J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(4)));
        v.push(tt(Op::Div(IrType::I32)));
        v.push(tt(Op::LetSet {
            idx: CP_LEN,
            ty: IrType::I32,
        }));
        // k = 0
        v.push(tt(Op::ConstI32(0)));
        v.push(tt(Op::LetSet {
            idx: K,
            ty: IrType::I32,
        }));
        // Outer block + loop: walk k from 0..cp_len
        v.push(tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: {
                    let mut lb: Vec<TaggedOp> = Vec::new();
                    // exit if k >= cp_len
                    lb.push(tt(Op::LetGet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::LetGet {
                        idx: CP_LEN,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Ge(IrType::I32)));
                    lb.push(tt(Op::BrIf { label_depth: 1 }));
                    // entry_addr = cp_base + 4 + k*4
                    // tmp_cp = load(entry_addr)
                    lb.push(tt(Op::LetGet {
                        idx: CP_BASE,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetGet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Mul(IrType::I32)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                    lb.push(tt(Op::LetSet {
                        idx: CP,
                        ty: IrType::I32,
                    }));
                    // ccc = __ccc_lookup(cp, ccc_table)
                    lb.push(tt(Op::LetGet {
                        idx: CP,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::CccTableAddr));
                    lb.push(tt(Op::Call {
                        fn_index: ccc_idx,
                        arg_count: 2,
                        param_tys: vec![IrType::I32, IrType::I32],
                        ret_ty: IrType::I32,
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    // if ccc == 0: k++; continue
                    lb.push(tt(Op::LetGet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(0)));
                    lb.push(tt(Op::Eq(IrType::I32)));
                    lb.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            let mut t = Vec::new();
                            t.push(tt(Op::LetGet {
                                idx: K,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(1)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetSet {
                                idx: K,
                                ty: IrType::I32,
                            }));
                            // depth 1 = the enclosing reorder Loop -
                            // jumping there continues the outer walk.
                            t.push(tt(Op::Br { label_depth: 1 }));
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                        else_body: vec![tt(Op::ConstI32(0))],
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }));
                    // Find run end: run_end = k; while run_end < cp_len
                    // and ccc(buf[run_end]) != 0: run_end++
                    lb.push(tt(Op::LetGet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: RUN_END,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Block {
                        result_ty: None,
                        body: vec![tt(Op::Loop {
                            result_ty: None,
                            body: {
                                let mut rl: Vec<TaggedOp> = Vec::new();
                                rl.push(tt(Op::LetGet {
                                    idx: RUN_END,
                                    ty: IrType::I32,
                                }));
                                rl.push(tt(Op::LetGet {
                                    idx: CP_LEN,
                                    ty: IrType::I32,
                                }));
                                rl.push(tt(Op::Ge(IrType::I32)));
                                rl.push(tt(Op::BrIf { label_depth: 1 }));
                                // load buf[run_end]
                                rl.push(tt(Op::LetGet {
                                    idx: CP_BASE,
                                    ty: IrType::I32,
                                }));
                                rl.push(tt(Op::ConstI32(4)));
                                rl.push(tt(Op::Add(IrType::I32)));
                                rl.push(tt(Op::LetGet {
                                    idx: RUN_END,
                                    ty: IrType::I32,
                                }));
                                rl.push(tt(Op::ConstI32(4)));
                                rl.push(tt(Op::Mul(IrType::I32)));
                                rl.push(tt(Op::Add(IrType::I32)));
                                rl.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                                rl.push(tt(Op::CccTableAddr));
                                rl.push(tt(Op::Call {
                                    fn_index: ccc_idx,
                                    arg_count: 2,
                                    param_tys: vec![IrType::I32, IrType::I32],
                                    ret_ty: IrType::I32,
                                }));
                                rl.push(tt(Op::ConstI32(0)));
                                rl.push(tt(Op::Eq(IrType::I32)));
                                rl.push(tt(Op::BrIf { label_depth: 1 }));
                                rl.push(tt(Op::LetGet {
                                    idx: RUN_END,
                                    ty: IrType::I32,
                                }));
                                rl.push(tt(Op::ConstI32(1)));
                                rl.push(tt(Op::Add(IrType::I32)));
                                rl.push(tt(Op::LetSet {
                                    idx: RUN_END,
                                    ty: IrType::I32,
                                }));
                                rl.push(tt(Op::Br { label_depth: 0 }));
                                rl
                            },
                        })],
                    }));
                    // Bubble sort the run [k, run_end) by CCC, stably.
                    // Outer i = k+1; while i < run_end:
                    //   j_inner = i; while j_inner > k and ccc(buf[j_inner-1]) > ccc(buf[j_inner]): swap; j_inner--
                    // We reuse I (clobbered by reorder pass, decode is done)
                    // and J (clobbered similarly). Save K so the outer loop
                    // continues. We'll use I for inner i, B0 for j_inner.
                    lb.push(tt(Op::LetGet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(1)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetSet {
                        idx: I,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Block {
                        result_ty: None,
                        body: vec![tt(Op::Loop {
                            result_ty: None,
                            body: {
                                let mut ol: Vec<TaggedOp> = Vec::new();
                                ol.push(tt(Op::LetGet {
                                    idx: I,
                                    ty: IrType::I32,
                                }));
                                ol.push(tt(Op::LetGet {
                                    idx: RUN_END,
                                    ty: IrType::I32,
                                }));
                                ol.push(tt(Op::Ge(IrType::I32)));
                                ol.push(tt(Op::BrIf { label_depth: 1 }));
                                // j_inner = i
                                ol.push(tt(Op::LetGet {
                                    idx: I,
                                    ty: IrType::I32,
                                }));
                                ol.push(tt(Op::LetSet {
                                    idx: B0,
                                    ty: IrType::I32,
                                }));
                                ol.push(tt(Op::Block {
                                    result_ty: None,
                                    body: vec![tt(Op::Loop {
                                        result_ty: None,
                                        body: {
                                            let mut il: Vec<TaggedOp> = Vec::new();
                                            // exit if j_inner <= k
                                            il.push(tt(Op::LetGet {
                                                idx: B0,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::LetGet {
                                                idx: K,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::Le(IrType::I32)));
                                            il.push(tt(Op::BrIf { label_depth: 1 }));
                                            // a_addr = cp_base + 4 + (j_inner-1)*4
                                            il.push(tt(Op::LetGet {
                                                idx: CP_BASE,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::ConstI32(4)));
                                            il.push(tt(Op::Add(IrType::I32)));
                                            il.push(tt(Op::LetGet {
                                                idx: B0,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::ConstI32(1)));
                                            il.push(tt(Op::Sub(IrType::I32)));
                                            il.push(tt(Op::ConstI32(4)));
                                            il.push(tt(Op::Mul(IrType::I32)));
                                            il.push(tt(Op::Add(IrType::I32)));
                                            il.push(tt(Op::LetSet {
                                                idx: B_TMP,
                                                ty: IrType::I32,
                                            }));
                                            // b_addr = cp_base + 4 + j_inner*4
                                            il.push(tt(Op::LetGet {
                                                idx: CP_BASE,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::ConstI32(4)));
                                            il.push(tt(Op::Add(IrType::I32)));
                                            il.push(tt(Op::LetGet {
                                                idx: B0,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::ConstI32(4)));
                                            il.push(tt(Op::Mul(IrType::I32)));
                                            il.push(tt(Op::Add(IrType::I32)));
                                            il.push(tt(Op::LetSet {
                                                idx: LOOKUP,
                                                ty: IrType::I32,
                                            }));
                                            // a = load(a_addr); b = load(b_addr)
                                            il.push(tt(Op::LetGet {
                                                idx: B_TMP,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                                            il.push(tt(Op::LetSet {
                                                idx: TMP,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::LetGet {
                                                idx: LOOKUP,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                                            il.push(tt(Op::LetSet {
                                                idx: TMP2,
                                                ty: IrType::I32,
                                            }));
                                            // ccc_a / ccc_b
                                            il.push(tt(Op::LetGet {
                                                idx: TMP,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::CccTableAddr));
                                            il.push(tt(Op::Call {
                                                fn_index: ccc_idx,
                                                arg_count: 2,
                                                param_tys: vec![IrType::I32, IrType::I32],
                                                ret_ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::LetGet {
                                                idx: TMP2,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::CccTableAddr));
                                            il.push(tt(Op::Call {
                                                fn_index: ccc_idx,
                                                arg_count: 2,
                                                param_tys: vec![IrType::I32, IrType::I32],
                                                ret_ty: IrType::I32,
                                            }));
                                            // if ccc_a > ccc_b: swap
                                            il.push(tt(Op::Gt(IrType::I32)));
                                            il.push(tt(Op::If {
                                                result_ty: IrType::I32,
                                                then_body: {
                                                    let mut sb = Vec::new();
                                                    // store(a_addr, b)
                                                    sb.push(tt(Op::LetGet {
                                                        idx: B_TMP,
                                                        ty: IrType::I32,
                                                    }));
                                                    sb.push(tt(Op::LetGet {
                                                        idx: TMP2,
                                                        ty: IrType::I32,
                                                    }));
                                                    sb.push(tt(Op::StoreI32AtAbsolute {
                                                        offset: 0,
                                                    }));
                                                    // store(b_addr, a)
                                                    sb.push(tt(Op::LetGet {
                                                        idx: LOOKUP,
                                                        ty: IrType::I32,
                                                    }));
                                                    sb.push(tt(Op::LetGet {
                                                        idx: TMP,
                                                        ty: IrType::I32,
                                                    }));
                                                    sb.push(tt(Op::StoreI32AtAbsolute {
                                                        offset: 0,
                                                    }));
                                                    // j_inner--
                                                    sb.push(tt(Op::LetGet {
                                                        idx: B0,
                                                        ty: IrType::I32,
                                                    }));
                                                    sb.push(tt(Op::ConstI32(1)));
                                                    sb.push(tt(Op::Sub(IrType::I32)));
                                                    sb.push(tt(Op::LetSet {
                                                        idx: B0,
                                                        ty: IrType::I32,
                                                    }));
                                                    sb.push(tt(Op::ConstI32(0)));
                                                    sb
                                                },
                                                else_body: vec![
                                                    // break out: br 2 exits the inner block.
                                                    tt(Op::Br { label_depth: 2 }),
                                                    tt(Op::ConstI32(0)),
                                                ],
                                            }));
                                            il.push(tt(Op::LetSet {
                                                idx: SINK,
                                                ty: IrType::I32,
                                            }));
                                            il.push(tt(Op::Br { label_depth: 0 }));
                                            il
                                        },
                                    })],
                                }));
                                ol.push(tt(Op::LetGet {
                                    idx: I,
                                    ty: IrType::I32,
                                }));
                                ol.push(tt(Op::ConstI32(1)));
                                ol.push(tt(Op::Add(IrType::I32)));
                                ol.push(tt(Op::LetSet {
                                    idx: I,
                                    ty: IrType::I32,
                                }));
                                ol.push(tt(Op::Br { label_depth: 0 }));
                                ol
                            },
                        })],
                    }));
                    // k = run_end
                    lb.push(tt(Op::LetGet {
                        idx: RUN_END,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Br { label_depth: 0 }));
                    lb
                },
            })],
        }));
        v
    };

    // ----- canonical composition pass (NFC/NFKC only) -----
    // In-place over cp_buf. last_starter: index in buf (or -1).
    // last_ccc: int. write_idx: cp_buf write position (0..cp_len).
    // For each read_idx in 0..cp_len:
    //   cur = buf[read_idx]; cur_ccc = ccc(cur)
    //   if last_starter != -1:
    //     starter = buf[last_starter]
    //     composed = hangul_compose(starter, cur) or compose_lookup(starter, cur)
    //     if composed != -1 and !(cur_ccc != 0 and last_ccc >= cur_ccc):
    //       buf[last_starter] = composed; continue (do NOT emit cur)
    //   buf[write_idx++] = cur
    //   if cur_ccc == 0: last_starter = write_idx-1; last_ccc = 0
    //   else: last_ccc = cur_ccc
    // After: cp_len = write_idx
    let compose_seq: Vec<TaggedOp> = if form.composes() {
        // Locals reused for compose state:
        //   I        - read_idx
        //   J        - write_idx
        //   B0       - last_starter (-1 sentinel)
        //   B_TMP    - last_ccc
        //   TMP      - cur cp
        //   TMP2     - cur ccc
        //   LOOKUP   - starter cp / composed cp
        //   FOLDED   - hangul/compose result
        let mut v: Vec<TaggedOp> = Vec::new();
        v.push(tt(Op::ConstI32(0)));
        v.push(tt(Op::LetSet {
            idx: I,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(0)));
        v.push(tt(Op::LetSet {
            idx: J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(-1)));
        v.push(tt(Op::LetSet {
            idx: B0,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(0)));
        v.push(tt(Op::LetSet {
            idx: B_TMP,
            ty: IrType::I32,
        }));
        v.push(tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: {
                    let mut lb: Vec<TaggedOp> = Vec::new();
                    lb.push(tt(Op::LetGet {
                        idx: I,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::LetGet {
                        idx: CP_LEN,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Ge(IrType::I32)));
                    lb.push(tt(Op::BrIf { label_depth: 1 }));
                    // cur = buf[i]
                    lb.push(tt(Op::LetGet {
                        idx: CP_BASE,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetGet {
                        idx: I,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Mul(IrType::I32)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                    lb.push(tt(Op::LetSet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    // cur_ccc
                    lb.push(tt(Op::LetGet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::CccTableAddr));
                    lb.push(tt(Op::Call {
                        fn_index: ccc_idx,
                        arg_count: 2,
                        param_tys: vec![IrType::I32, IrType::I32],
                        ret_ty: IrType::I32,
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: TMP2,
                        ty: IrType::I32,
                    }));
                    // if last_starter != -1: try compose
                    lb.push(tt(Op::LetGet {
                        idx: B0,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(-1)));
                    lb.push(tt(Op::Ne(IrType::I32)));
                    lb.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            let mut t: Vec<TaggedOp> = Vec::new();
                            // starter = buf[last_starter]
                            t.push(tt(Op::LetGet {
                                idx: CP_BASE,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(4)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetGet {
                                idx: B0,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(4)));
                            t.push(tt(Op::Mul(IrType::I32)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                            t.push(tt(Op::LetSet {
                                idx: LOOKUP,
                                ty: IrType::I32,
                            }));
                            // hangul_compose: try L+V or LV+T inline.
                            // Initialise FOLDED = -1 (no composition yet).
                            t.push(tt(Op::ConstI32(-1)));
                            t.push(tt(Op::LetSet {
                                idx: FOLDED,
                                ty: IrType::I32,
                            }));
                            // L+V case: if L_BASE <= starter < L_BASE+L_COUNT
                            //           and V_BASE <= cur < V_BASE+V_COUNT:
                            //   FOLDED = S_BASE + (starter - L_BASE) * V_COUNT * T_COUNT + (cur - V_BASE) * T_COUNT
                            let l_base = crate::normalization::HANGUL_L_BASE as i32;
                            let l_count = crate::normalization::HANGUL_L_COUNT as i32;
                            let v_base = crate::normalization::HANGUL_V_BASE as i32;
                            let v_count = crate::normalization::HANGUL_V_COUNT as i32;
                            let t_base = crate::normalization::HANGUL_T_BASE as i32;
                            let t_count = crate::normalization::HANGUL_T_COUNT as i32;
                            let s_base = crate::normalization::HANGUL_S_BASE as i32;
                            let s_count = crate::normalization::HANGUL_S_COUNT as i32;
                            // is_L = starter >= L_BASE && starter < L_BASE+L_COUNT
                            t.push(tt(Op::LetGet {
                                idx: LOOKUP,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(l_base)));
                            t.push(tt(Op::Ge(IrType::I32)));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: vec![
                                    tt(Op::LetGet {
                                        idx: LOOKUP,
                                        ty: IrType::I32,
                                    }),
                                    tt(Op::ConstI32(l_base + l_count)),
                                    tt(Op::Lt(IrType::I32)),
                                ],
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: {
                                    let mut lv = Vec::new();
                                    lv.push(tt(Op::LetGet {
                                        idx: TMP,
                                        ty: IrType::I32,
                                    }));
                                    lv.push(tt(Op::ConstI32(v_base)));
                                    lv.push(tt(Op::Ge(IrType::I32)));
                                    lv.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: vec![
                                            tt(Op::LetGet {
                                                idx: TMP,
                                                ty: IrType::I32,
                                            }),
                                            tt(Op::ConstI32(v_base + v_count)),
                                            tt(Op::Lt(IrType::I32)),
                                        ],
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    lv.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: {
                                            let mut comp = Vec::new();
                                            // FOLDED = S_BASE + (starter - L_BASE) * (V_COUNT*T_COUNT) + (cur - V_BASE) * T_COUNT
                                            comp.push(tt(Op::ConstI32(s_base)));
                                            comp.push(tt(Op::LetGet {
                                                idx: LOOKUP,
                                                ty: IrType::I32,
                                            }));
                                            comp.push(tt(Op::ConstI32(l_base)));
                                            comp.push(tt(Op::Sub(IrType::I32)));
                                            comp.push(tt(Op::ConstI32(v_count * t_count)));
                                            comp.push(tt(Op::Mul(IrType::I32)));
                                            comp.push(tt(Op::Add(IrType::I32)));
                                            comp.push(tt(Op::LetGet {
                                                idx: TMP,
                                                ty: IrType::I32,
                                            }));
                                            comp.push(tt(Op::ConstI32(v_base)));
                                            comp.push(tt(Op::Sub(IrType::I32)));
                                            comp.push(tt(Op::ConstI32(t_count)));
                                            comp.push(tt(Op::Mul(IrType::I32)));
                                            comp.push(tt(Op::Add(IrType::I32)));
                                            comp.push(tt(Op::LetSet {
                                                idx: FOLDED,
                                                ty: IrType::I32,
                                            }));
                                            comp.push(tt(Op::ConstI32(0)));
                                            comp
                                        },
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    lv.push(tt(Op::LetSet {
                                        idx: SINK,
                                        ty: IrType::I32,
                                    }));
                                    lv.push(tt(Op::ConstI32(0)));
                                    lv
                                },
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            t.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            // LV+T: if FOLDED still -1 and starter is LV-shaped + cur is T-jamo > T_BASE
                            t.push(tt(Op::LetGet {
                                idx: FOLDED,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(-1)));
                            t.push(tt(Op::Eq(IrType::I32)));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: {
                                    let mut lv = Vec::new();
                                    // s_index = starter - S_BASE
                                    // if 0 <= s_index < S_COUNT and s_index % T_COUNT == 0
                                    // and T_BASE+1 <= cur < T_BASE+T_COUNT:
                                    //   FOLDED = starter + (cur - T_BASE)
                                    lv.push(tt(Op::LetGet {
                                        idx: LOOKUP,
                                        ty: IrType::I32,
                                    }));
                                    lv.push(tt(Op::ConstI32(s_base)));
                                    lv.push(tt(Op::Ge(IrType::I32)));
                                    lv.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: vec![
                                            tt(Op::LetGet {
                                                idx: LOOKUP,
                                                ty: IrType::I32,
                                            }),
                                            tt(Op::ConstI32(s_base + s_count)),
                                            tt(Op::Lt(IrType::I32)),
                                        ],
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    lv.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: {
                                            let mut g = Vec::new();
                                            // (starter - S_BASE) % T_COUNT == 0?
                                            g.push(tt(Op::LetGet {
                                                idx: LOOKUP,
                                                ty: IrType::I32,
                                            }));
                                            g.push(tt(Op::ConstI32(s_base)));
                                            g.push(tt(Op::Sub(IrType::I32)));
                                            g.push(tt(Op::ConstI32(t_count)));
                                            g.push(tt(Op::Mod(IrType::I32)));
                                            g.push(tt(Op::ConstI32(0)));
                                            g.push(tt(Op::Eq(IrType::I32)));
                                            g.push(tt(Op::If {
                                                result_ty: IrType::I32,
                                                then_body: vec![
                                                    tt(Op::LetGet {
                                                        idx: TMP,
                                                        ty: IrType::I32,
                                                    }),
                                                    tt(Op::ConstI32(t_base + 1)),
                                                    tt(Op::Ge(IrType::I32)),
                                                    tt(Op::If {
                                                        result_ty: IrType::I32,
                                                        then_body: vec![
                                                            tt(Op::LetGet {
                                                                idx: TMP,
                                                                ty: IrType::I32,
                                                            }),
                                                            tt(Op::ConstI32(t_base + t_count)),
                                                            tt(Op::Lt(IrType::I32)),
                                                        ],
                                                        else_body: vec![tt(Op::ConstI32(0))],
                                                    }),
                                                ],
                                                else_body: vec![tt(Op::ConstI32(0))],
                                            }));
                                            g.push(tt(Op::If {
                                                result_ty: IrType::I32,
                                                then_body: {
                                                    let mut comp = Vec::new();
                                                    comp.push(tt(Op::LetGet {
                                                        idx: LOOKUP,
                                                        ty: IrType::I32,
                                                    }));
                                                    comp.push(tt(Op::LetGet {
                                                        idx: TMP,
                                                        ty: IrType::I32,
                                                    }));
                                                    comp.push(tt(Op::ConstI32(t_base)));
                                                    comp.push(tt(Op::Sub(IrType::I32)));
                                                    comp.push(tt(Op::Add(IrType::I32)));
                                                    comp.push(tt(Op::LetSet {
                                                        idx: FOLDED,
                                                        ty: IrType::I32,
                                                    }));
                                                    comp.push(tt(Op::ConstI32(0)));
                                                    comp
                                                },
                                                else_body: vec![tt(Op::ConstI32(0))],
                                            }));
                                            g.push(tt(Op::LetSet {
                                                idx: SINK,
                                                ty: IrType::I32,
                                            }));
                                            g.push(tt(Op::ConstI32(0)));
                                            g
                                        },
                                        else_body: vec![tt(Op::ConstI32(0))],
                                    }));
                                    lv.push(tt(Op::LetSet {
                                        idx: SINK,
                                        ty: IrType::I32,
                                    }));
                                    lv.push(tt(Op::ConstI32(0)));
                                    lv
                                },
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            t.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            // if FOLDED == -1: try table compose
                            t.push(tt(Op::LetGet {
                                idx: FOLDED,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(-1)));
                            t.push(tt(Op::Eq(IrType::I32)));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: {
                                    let mut tc = Vec::new();
                                    tc.push(tt(Op::LetGet {
                                        idx: LOOKUP,
                                        ty: IrType::I32,
                                    }));
                                    tc.push(tt(Op::LetGet {
                                        idx: TMP,
                                        ty: IrType::I32,
                                    }));
                                    tc.push(tt(Op::CompositionTableAddr));
                                    tc.push(tt(Op::Call {
                                        fn_index: compose_idx,
                                        arg_count: 3,
                                        param_tys: vec![IrType::I32, IrType::I32, IrType::I32],
                                        ret_ty: IrType::I32,
                                    }));
                                    tc.push(tt(Op::LetSet {
                                        idx: FOLDED,
                                        ty: IrType::I32,
                                    }));
                                    tc.push(tt(Op::ConstI32(0)));
                                    tc
                                },
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            t.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            // if FOLDED != -1 AND !(cur_ccc != 0 AND last_ccc >= cur_ccc):
                            //   buf[last_starter] = FOLDED; advance i and continue
                            // Compute blocked:
                            //   blocked = (cur_ccc != 0) AND (last_ccc >= cur_ccc)
                            t.push(tt(Op::LetGet {
                                idx: TMP2,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0)));
                            t.push(tt(Op::Ne(IrType::I32)));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: vec![
                                    tt(Op::LetGet {
                                        idx: B_TMP,
                                        ty: IrType::I32,
                                    }),
                                    tt(Op::LetGet {
                                        idx: TMP2,
                                        ty: IrType::I32,
                                    }),
                                    tt(Op::Ge(IrType::I32)),
                                ],
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            // top = blocked
                            t.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            // Now check: FOLDED != -1 AND !blocked
                            t.push(tt(Op::LetGet {
                                idx: FOLDED,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(-1)));
                            t.push(tt(Op::Ne(IrType::I32)));
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: vec![
                                    tt(Op::LetGet {
                                        idx: TMP2,
                                        ty: IrType::I32,
                                    }),
                                    tt(Op::ConstI32(0)),
                                    tt(Op::Ne(IrType::I32)),
                                    tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: vec![
                                            tt(Op::LetGet {
                                                idx: B_TMP,
                                                ty: IrType::I32,
                                            }),
                                            tt(Op::LetGet {
                                                idx: TMP2,
                                                ty: IrType::I32,
                                            }),
                                            tt(Op::Lt(IrType::I32)),
                                        ],
                                        else_body: vec![tt(Op::ConstI32(1))],
                                    }),
                                ],
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            // top = can_compose
                            t.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: {
                                    let mut cc = Vec::new();
                                    // store FOLDED into buf[last_starter]
                                    cc.push(tt(Op::LetGet {
                                        idx: CP_BASE,
                                        ty: IrType::I32,
                                    }));
                                    cc.push(tt(Op::ConstI32(4)));
                                    cc.push(tt(Op::Add(IrType::I32)));
                                    cc.push(tt(Op::LetGet {
                                        idx: B0,
                                        ty: IrType::I32,
                                    }));
                                    cc.push(tt(Op::ConstI32(4)));
                                    cc.push(tt(Op::Mul(IrType::I32)));
                                    cc.push(tt(Op::Add(IrType::I32)));
                                    cc.push(tt(Op::LetGet {
                                        idx: FOLDED,
                                        ty: IrType::I32,
                                    }));
                                    cc.push(tt(Op::StoreI32AtAbsolute { offset: 0 }));
                                    // i++
                                    cc.push(tt(Op::LetGet {
                                        idx: I,
                                        ty: IrType::I32,
                                    }));
                                    cc.push(tt(Op::ConstI32(1)));
                                    cc.push(tt(Op::Add(IrType::I32)));
                                    cc.push(tt(Op::LetSet {
                                        idx: I,
                                        ty: IrType::I32,
                                    }));
                                    // Br 2: depth 0 = inner If, 1 =
                                    // outer If, 2 = Loop. Jumping to
                                    // the Loop label continues the
                                    // outer compose walk without
                                    // writing the current cp.
                                    cc.push(tt(Op::Br { label_depth: 2 }));
                                    cc.push(tt(Op::ConstI32(0)));
                                    cc
                                },
                                else_body: vec![tt(Op::ConstI32(0))],
                            }));
                            t.push(tt(Op::LetSet {
                                idx: SINK,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                        else_body: vec![tt(Op::ConstI32(0))],
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }));
                    // buf[write_idx] = cur
                    lb.push(tt(Op::LetGet {
                        idx: CP_BASE,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetGet {
                        idx: J,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Mul(IrType::I32)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetGet {
                        idx: TMP,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::StoreI32AtAbsolute { offset: 0 }));
                    // if cur_ccc == 0: last_starter = write_idx; last_ccc = 0
                    // else: last_ccc = cur_ccc
                    lb.push(tt(Op::LetGet {
                        idx: TMP2,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(0)));
                    lb.push(tt(Op::Eq(IrType::I32)));
                    lb.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
                            tt(Op::LetGet {
                                idx: J,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetSet {
                                idx: B0,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                            tt(Op::LetSet {
                                idx: B_TMP,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                        ],
                        else_body: vec![
                            tt(Op::LetGet {
                                idx: TMP2,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetSet {
                                idx: B_TMP,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                        ],
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }));
                    // write_idx++
                    lb.push(tt(Op::LetGet {
                        idx: J,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(1)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetSet {
                        idx: J,
                        ty: IrType::I32,
                    }));
                    // i++
                    lb.push(tt(Op::LetGet {
                        idx: I,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(1)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetSet {
                        idx: I,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Br { label_depth: 0 }));
                    lb
                },
            })],
        }));
        // cp_len = write_idx
        v.push(tt(Op::LetGet {
            idx: J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::LetSet {
            idx: CP_LEN,
            ty: IrType::I32,
        }));
        v
    } else {
        Vec::new()
    };

    // ----- encode each cp in cp_buf back to UTF-8 in out_buf -----
    let prefix_plus_shifted = |prefix: i32, shift_div: i32, mask: bool| {
        let mut v = vec![tt(Op::ConstI32(prefix))];
        v.push(tt(Op::LetGet {
            idx: FOLDED,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(shift_div)));
        v.push(tt(Op::Div(IrType::I32)));
        if mask {
            v.push(tt(Op::ConstI32(0x3F)));
            v.push(tt(Op::BitAnd(IrType::I32)));
        }
        v.push(tt(Op::Add(IrType::I32)));
        v
    };
    let store_byte = |off: i32, byte_expr: Vec<TaggedOp>| {
        let mut v = Vec::new();
        v.push(tt(Op::LetGet {
            idx: OUT_BASE,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(4 + off)));
        v.push(tt(Op::Add(IrType::I32)));
        v.push(tt(Op::LetGet {
            idx: OUT_J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::Add(IrType::I32)));
        v.extend(byte_expr);
        v.push(tt(Op::StoreI8AtAbsolute { offset: 0 }));
        v
    };

    let encode_seq: Vec<TaggedOp> = {
        let mut v: Vec<TaggedOp> = Vec::new();
        v.push(tt(Op::ConstI32(0)));
        v.push(tt(Op::LetSet {
            idx: OUT_J,
            ty: IrType::I32,
        }));
        v.push(tt(Op::ConstI32(0)));
        v.push(tt(Op::LetSet {
            idx: K,
            ty: IrType::I32,
        }));
        v.push(tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: {
                    let mut lb: Vec<TaggedOp> = Vec::new();
                    lb.push(tt(Op::LetGet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::LetGet {
                        idx: CP_LEN,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Ge(IrType::I32)));
                    lb.push(tt(Op::BrIf { label_depth: 1 }));
                    // FOLDED = buf[k]
                    lb.push(tt(Op::LetGet {
                        idx: CP_BASE,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetGet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(4)));
                    lb.push(tt(Op::Mul(IrType::I32)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
                    lb.push(tt(Op::LetSet {
                        idx: FOLDED,
                        ty: IrType::I32,
                    }));
                    // Encode FOLDED to UTF-8 (same 4-arm split as case_fold_body)
                    lb.push(tt(Op::LetGet {
                        idx: FOLDED,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(0x80)));
                    lb.push(tt(Op::Lt(IrType::I32)));
                    lb.push(tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: {
                            let mut t = Vec::new();
                            t.extend(store_byte(
                                0,
                                vec![tt(Op::LetGet {
                                    idx: FOLDED,
                                    ty: IrType::I32,
                                })],
                            ));
                            t.push(tt(Op::LetGet {
                                idx: OUT_J,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(1)));
                            t.push(tt(Op::Add(IrType::I32)));
                            t.push(tt(Op::LetSet {
                                idx: OUT_J,
                                ty: IrType::I32,
                            }));
                            t.push(tt(Op::ConstI32(0)));
                            t
                        },
                        else_body: {
                            let mut e = Vec::new();
                            e.push(tt(Op::LetGet {
                                idx: FOLDED,
                                ty: IrType::I32,
                            }));
                            e.push(tt(Op::ConstI32(0x800)));
                            e.push(tt(Op::Lt(IrType::I32)));
                            e.push(tt(Op::If {
                                result_ty: IrType::I32,
                                then_body: {
                                    let mut t = Vec::new();
                                    t.extend(store_byte(0, prefix_plus_shifted(0xC0, 64, false)));
                                    t.extend(store_byte(1, {
                                        let mut x = vec![tt(Op::ConstI32(0x80))];
                                        x.push(tt(Op::LetGet {
                                            idx: FOLDED,
                                            ty: IrType::I32,
                                        }));
                                        x.push(tt(Op::ConstI32(0x3F)));
                                        x.push(tt(Op::BitAnd(IrType::I32)));
                                        x.push(tt(Op::Add(IrType::I32)));
                                        x
                                    }));
                                    t.push(tt(Op::LetGet {
                                        idx: OUT_J,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(2)));
                                    t.push(tt(Op::Add(IrType::I32)));
                                    t.push(tt(Op::LetSet {
                                        idx: OUT_J,
                                        ty: IrType::I32,
                                    }));
                                    t.push(tt(Op::ConstI32(0)));
                                    t
                                },
                                else_body: {
                                    let mut e2 = Vec::new();
                                    e2.push(tt(Op::LetGet {
                                        idx: FOLDED,
                                        ty: IrType::I32,
                                    }));
                                    e2.push(tt(Op::ConstI32(0x10000)));
                                    e2.push(tt(Op::Lt(IrType::I32)));
                                    e2.push(tt(Op::If {
                                        result_ty: IrType::I32,
                                        then_body: {
                                            let mut t = Vec::new();
                                            t.extend(store_byte(
                                                0,
                                                prefix_plus_shifted(0xE0, 4096, false),
                                            ));
                                            t.extend(store_byte(
                                                1,
                                                prefix_plus_shifted(0x80, 64, true),
                                            ));
                                            t.extend(store_byte(2, {
                                                let mut x = vec![tt(Op::ConstI32(0x80))];
                                                x.push(tt(Op::LetGet {
                                                    idx: FOLDED,
                                                    ty: IrType::I32,
                                                }));
                                                x.push(tt(Op::ConstI32(0x3F)));
                                                x.push(tt(Op::BitAnd(IrType::I32)));
                                                x.push(tt(Op::Add(IrType::I32)));
                                                x
                                            }));
                                            t.push(tt(Op::LetGet {
                                                idx: OUT_J,
                                                ty: IrType::I32,
                                            }));
                                            t.push(tt(Op::ConstI32(3)));
                                            t.push(tt(Op::Add(IrType::I32)));
                                            t.push(tt(Op::LetSet {
                                                idx: OUT_J,
                                                ty: IrType::I32,
                                            }));
                                            t.push(tt(Op::ConstI32(0)));
                                            t
                                        },
                                        else_body: {
                                            let mut t = Vec::new();
                                            t.extend(store_byte(
                                                0,
                                                prefix_plus_shifted(0xF0, 262144, false),
                                            ));
                                            t.extend(store_byte(
                                                1,
                                                prefix_plus_shifted(0x80, 4096, true),
                                            ));
                                            t.extend(store_byte(
                                                2,
                                                prefix_plus_shifted(0x80, 64, true),
                                            ));
                                            t.extend(store_byte(3, {
                                                let mut x = vec![tt(Op::ConstI32(0x80))];
                                                x.push(tt(Op::LetGet {
                                                    idx: FOLDED,
                                                    ty: IrType::I32,
                                                }));
                                                x.push(tt(Op::ConstI32(0x3F)));
                                                x.push(tt(Op::BitAnd(IrType::I32)));
                                                x.push(tt(Op::Add(IrType::I32)));
                                                x
                                            }));
                                            t.push(tt(Op::LetGet {
                                                idx: OUT_J,
                                                ty: IrType::I32,
                                            }));
                                            t.push(tt(Op::ConstI32(4)));
                                            t.push(tt(Op::Add(IrType::I32)));
                                            t.push(tt(Op::LetSet {
                                                idx: OUT_J,
                                                ty: IrType::I32,
                                            }));
                                            t.push(tt(Op::ConstI32(0)));
                                            t
                                        },
                                    }));
                                    e2
                                },
                            }));
                            e
                        },
                    }));
                    lb.push(tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }));
                    // k++
                    lb.push(tt(Op::LetGet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::ConstI32(1)));
                    lb.push(tt(Op::Add(IrType::I32)));
                    lb.push(tt(Op::LetSet {
                        idx: K,
                        ty: IrType::I32,
                    }));
                    lb.push(tt(Op::Br { label_depth: 0 }));
                    lb
                },
            })],
        }));
        v
    };

    // ----- assemble the full body -----
    let mut body: Vec<TaggedOp> = Vec::new();
    // s_len = i32.load(s, 0)
    body.push(tt(Op::LocalGet(0)));
    body.push(tt(Op::LoadI32AtAbsolute { offset: 0 }));
    body.push(tt(Op::LetSet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    // cp_base = alloc_scratch_dyn(4 + s_len * 18 * 4)
    body.push(tt(Op::ConstI32(4)));
    body.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(18 * 4)));
    body.push(tt(Op::Mul(IrType::I32)));
    body.push(tt(Op::Add(IrType::I32)));
    body.push(tt(Op::AllocScratchDyn));
    body.push(tt(Op::LetSet {
        idx: CP_BASE,
        ty: IrType::I32,
    }));
    // out_base = alloc_scratch_dyn(4 + s_len * 18 * 4)
    // worst case mirrors cp_buf: each cp is up to 4 bytes UTF-8.
    body.push(tt(Op::ConstI32(4)));
    body.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(18 * 4)));
    body.push(tt(Op::Mul(IrType::I32)));
    body.push(tt(Op::Add(IrType::I32)));
    body.push(tt(Op::AllocScratchDyn));
    body.push(tt(Op::LetSet {
        idx: OUT_BASE,
        ty: IrType::I32,
    }));
    // i = 0; j = 0
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    body.push(tt(Op::ConstI32(0)));
    body.push(tt(Op::LetSet {
        idx: J,
        ty: IrType::I32,
    }));
    // Phase 1: decompose loop
    let mut decompose_loop: Vec<TaggedOp> = Vec::new();
    decompose_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    decompose_loop.push(tt(Op::LetGet {
        idx: S_LEN,
        ty: IrType::I32,
    }));
    decompose_loop.push(tt(Op::Ge(IrType::I32)));
    decompose_loop.push(tt(Op::BrIf { label_depth: 1 }));
    decompose_loop.extend(decode_seq);
    decompose_loop.extend(decompose_seq);
    // i += cp_bytes
    decompose_loop.push(tt(Op::LetGet {
        idx: I,
        ty: IrType::I32,
    }));
    decompose_loop.push(tt(Op::LetGet {
        idx: CP_BYTES,
        ty: IrType::I32,
    }));
    decompose_loop.push(tt(Op::Add(IrType::I32)));
    decompose_loop.push(tt(Op::LetSet {
        idx: I,
        ty: IrType::I32,
    }));
    decompose_loop.push(tt(Op::Br { label_depth: 0 }));
    body.push(tt(Op::Block {
        result_ty: None,
        body: vec![tt(Op::Loop {
            result_ty: None,
            body: decompose_loop,
        })],
    }));
    // Phase 2: canonical reorder
    body.extend(reorder_seq);
    // Phase 3: composition (NFC/NFKC only)
    body.extend(compose_seq);
    // Phase 4: encode
    body.extend(encode_seq);
    // Write out_buf header: store(out_base + 0, out_j)
    body.push(tt(Op::LetGet {
        idx: OUT_BASE,
        ty: IrType::I32,
    }));
    body.push(tt(Op::LetGet {
        idx: OUT_J,
        ty: IrType::I32,
    }));
    body.push(tt(Op::StoreI32AtAbsolute { offset: 0 }));
    // Return out_base
    body.push(tt(Op::LetGet {
        idx: OUT_BASE,
        ty: IrType::I32,
    }));
    body.push(tt(Op::Return));

    StdlibFunction {
        name: form.name(),
        params: vec![IrType::String],
        ret: IrType::String,
        body,
    }
}

///
/// The index is determined by [`builtin_stdlib`]'s declaration order
/// — see the module-level comment for why that order is part of the
/// wire format.
pub fn stdlib_function_index(name: &str) -> Option<u32> {
    builtin_stdlib()
        .iter()
        .position(|f| f.name == name)
        .map(|i| i as u32)
}

/// Number of bundled stdlib functions. Codegen uses this to compute
/// the wasm-level function index offset for user functions
/// (user-fn index = `stdlib_function_count() + ir_user_func_index`).
pub fn stdlib_function_count() -> u32 {
    builtin_stdlib().len() as u32
}

/// Phase 4.b method-dispatch table: resolve `(receiver_ir_type,
/// method_name)` to the registry index of the stdlib function that
/// implements that method on the given receiver type.
///
/// Distinct from [`stdlib_function_index`] because the same surface
/// method name (e.g. `length`) is implemented by different bundled
/// bodies depending on the receiver type — `String::length` goes
/// through the `length` body (index `0`), while `List<Int>::length`
/// goes through `list_int_length` (index `1`). Free-call form
/// (`length(x)`) still resolves through [`stdlib_function_index`];
/// the receiver-typed dispatch only fires when lowering sees an
/// explicit receiver path.
///
/// Returns `None` for unknown `(ty, name)` pairs; lowering surfaces
/// its own diagnostic.
pub fn stdlib_method_index(receiver_ty: IrType, name: &str) -> Option<u32> {
    match (receiver_ty, name) {
        (IrType::String, "length") => stdlib_function_index("length"),
        (IrType::ListInt, "length") => stdlib_function_index("list_int_length"),
        (IrType::String, "is_empty") => stdlib_function_index("is_empty"),
        // Phase 4.c-2: String / List<Int> method-form dispatch.
        // Free-call form (`concat(a, b)` / `list_int_sum(xs)`) still
        // routes through `stdlib_function_index` directly; method
        // form (`a.concat(b)` / `xs.sum()`) goes through this table
        // so the same surface name resolves against the receiver's
        // IR type.
        (IrType::String, "concat") => stdlib_function_index("concat"),
        (IrType::String, "upper") => stdlib_function_index("upper"),
        (IrType::String, "lower") => stdlib_function_index("lower"),
        // v3++ b-4: word-boundary aware case fold. `s.title()` and the
        // free-call `title(s)` both route here.
        (IrType::String, "title") => stdlib_function_index("title"),
        // v3++ b-5: Unicode normalization (UAX #15). `s.nfc()` /
        // `s.nfd()` / `s.nfkc()` / `s.nfkd()` and the matching
        // free-call forms all dispatch to the shared body builders.
        (IrType::String, "nfc") => stdlib_function_index("nfc"),
        (IrType::String, "nfd") => stdlib_function_index("nfd"),
        (IrType::String, "nfkc") => stdlib_function_index("nfkc"),
        (IrType::String, "nfkd") => stdlib_function_index("nfkd"),
        // v3++ b-6: locale-aware case folding. `s.upper_locale("tr")`
        // and the free-call form `upper_locale(s, "tr")` both route
        // through the same stdlib body.
        (IrType::String, "upper_locale") => stdlib_function_index("upper_locale"),
        (IrType::String, "lower_locale") => stdlib_function_index("lower_locale"),
        (IrType::String, "title_locale") => stdlib_function_index("title_locale"),
        (IrType::String, "substring") => stdlib_function_index("substring"),
        (IrType::String, "starts_with") => stdlib_function_index("starts_with"),
        // F-D7-D: `s.contains(needle)` and the free-call form
        // `contains(s, needle)` both resolve to the same body. The
        // trace recorder short-circuits the call onto
        // `TraceOp::StrContains` via `STDLIB_IDX_CONTAINS = 36`; the
        // tree-walk path stays in `Value`-space (see
        // `relon_evaluator::stdlib::call_method`).
        (IrType::String, "contains") => stdlib_function_index("contains"),
        (IrType::ListInt, "sum") => stdlib_function_index("list_int_sum"),
        (IrType::ListInt, "max") => stdlib_function_index("list_int_max"),
        // Phase 10-a higher-order List<Int> methods. Dispatch covers
        // the `xs.map(|x| ...)` / `xs.filter(|x| ...)` /
        // `xs.fold(init, |acc, x| ...)` surfaces.
        (IrType::ListInt, "map") => stdlib_function_index("list_int_map"),
        (IrType::ListInt, "filter") => stdlib_function_index("list_int_filter"),
        (IrType::ListInt, "fold") => stdlib_function_index("list_int_fold"),
        // Phase 10-c length dispatch for the new list types. Each
        // length body just reads the leading `[len: u32 LE]` of the
        // record (all list shapes share the same header), but routes
        // through a distinct stdlib slot so the IR-level param type
        // check stays honest.
        (IrType::ListFloat, "length") => stdlib_function_index("list_float_length"),
        (IrType::ListBool, "length") => stdlib_function_index("list_bool_length"),
        (IrType::ListString, "length") => stdlib_function_index("list_string_length"),
        (IrType::ListSchema, "length") => stdlib_function_index("list_schema_length"),
        _ => None,
    }
}

/// Phase 10-a: side-table describing the expected closure signature
/// for each `Op::Call` arg slot of a stdlib function. Returns `Some`
/// only for entries where slot `arg_idx` is a `Closure` parameter
/// (so the caller can run free-variable analysis + closure
/// conversion against the matching shape).
///
/// Keyed off the stdlib function's surface name; this stays in
/// `stdlib.rs` so the lowering pass has a single source of truth for
/// closure surfaces.
pub fn stdlib_closure_arg_signature(name: &str, arg_idx: u32) -> Option<(Vec<IrType>, IrType)> {
    match (name, arg_idx) {
        // `xs.map(|x| ...)` — closure param at arg index 1.
        ("list_int_map", 1) => Some((vec![IrType::I64], IrType::I64)),
        // `xs.filter(|x| ...)` — closure param at arg index 1.
        ("list_int_filter", 1) => Some((vec![IrType::I64], IrType::Bool)),
        // `xs.fold(init, |acc, x| ...)` — closure param at arg index 2.
        ("list_int_fold", 2) => Some((vec![IrType::I64, IrType::I64], IrType::I64)),
        _ => None,
    }
}

/// Hand-written body for `substring(s: String, start: Int, len: Int) -> String`.
///
/// Bounds check: traps with [`crate::TrapKind::IndexOutOfBounds`]
/// when `start < 0`, `len < 0`, or `start + len > s.len`. The Int
/// params arrive as i64; we narrow to i32 (since the scratch heap
/// can only address i32 offsets) by exploiting the i64-to-i32 wrap
/// via comparison-and-truncate: any value outside `0..=u32::MAX` is
/// caught by the bounds check before the wrap matters in practice.
///
/// Algorithm:
///   1. Read `s_len = i32.load(s, 0)`.
///   2. Compare `start` and `len` (i64) against zero and against
///      `s_len + start` (signed). On failure, trap.
///   3. `record_size = len + 4`; allocate scratch.
///   4. Write header `i32.store(base + 0, len)`.
///   5. `memory.copy(base + 4, s + 4 + start, len)`.
///   6. Return `base`.
///
/// Locals:
///   * 0 — `s_len:     I32`
///   * 1 — `start_i32: I32` (narrowed)
///   * 2 — `len_i32:   I32` (narrowed)
///   * 3 — `base:      I32`
fn substring_string() -> StdlibFunction {
    const S_LEN: u32 = 0;
    const START_I32: u32 = 1;
    const LEN_I32: u32 = 2;
    const BASE: u32 = 3;
    StdlibFunction {
        name: "substring",
        params: vec![IrType::String, IrType::I64, IrType::I64],
        ret: IrType::String,
        body: vec![
            // s_len = load_i32(s, 0)
            tt(Op::LocalGet(0)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: S_LEN,
                ty: IrType::I32,
            }),
            // -----------------------------------------------------
            // Bounds check (all in i64 space against zero and s_len)
            //   if start < 0           => trap
            //   if len < 0             => trap
            //   if start + len > s_len => trap  (signed compare)
            // -----------------------------------------------------
            // if start < 0 { trap }
            tt(Op::LocalGet(1)),
            tt(Op::ConstI64(0)),
            tt(Op::Lt(IrType::I64)),
            tt(Op::If {
                result_ty: IrType::I32,
                then_body: vec![
                    tt(Op::Trap {
                        kind: TrapKind::IndexOutOfBounds,
                    }),
                    // Unreachable but needed to satisfy the wasm
                    // verifier's both-arms-typed contract — push a
                    // placeholder i32 the outer code drops.
                    tt(Op::ConstI32(0)),
                ],
                else_body: vec![tt(Op::ConstI32(0))],
            }),
            tt(Op::LetSet {
                idx: BASE, /* reuse BASE as a scratch sink */
                ty: IrType::I32,
            }),
            // if len < 0 { trap }
            tt(Op::LocalGet(2)),
            tt(Op::ConstI64(0)),
            tt(Op::Lt(IrType::I64)),
            tt(Op::If {
                result_ty: IrType::I32,
                then_body: vec![
                    tt(Op::Trap {
                        kind: TrapKind::IndexOutOfBounds,
                    }),
                    tt(Op::ConstI32(0)),
                ],
                else_body: vec![tt(Op::ConstI32(0))],
            }),
            tt(Op::LetSet {
                idx: BASE,
                ty: IrType::I32,
            }),
            // if start + len > s_len { trap }
            //   compute `start + len` in i64 then compare against
            //   the i64-extended s_len. We extend s_len by going
            //   through a let-set/get into an i32 then promoting via
            //   `i64.extend_i32_u`-style. Without a direct extend
            //   op we go through ReadStringLen (which extends), but
            //   that requires re-loading s. Simpler: compute
            //   `start + len` in i64 then compare against
            //   `ReadStringLen(s)` which already returns i64.
            tt(Op::LocalGet(1)),
            tt(Op::LocalGet(2)),
            tt(Op::Add(IrType::I64)),
            // s.length as i64
            tt(Op::LocalGet(0)),
            tt(Op::ReadStringLen),
            tt(Op::Gt(IrType::I64)),
            tt(Op::If {
                result_ty: IrType::I32,
                then_body: vec![
                    tt(Op::Trap {
                        kind: TrapKind::IndexOutOfBounds,
                    }),
                    tt(Op::ConstI32(0)),
                ],
                else_body: vec![tt(Op::ConstI32(0))],
            }),
            tt(Op::LetSet {
                idx: BASE,
                ty: IrType::I32,
            }),
            // -----------------------------------------------------
            // Bounds-check survived; narrow start / len to i32.
            //   We could go through a dedicated `WrapI64` op; lacking
            //   one, the bounds check above guarantees `0 <= v <= s_len`
            //   and the string layout caps `s_len` at u32, so the
            //   high i32 of each value is zero. We use a wasm `select`
            //   round-trip: `select(v_low, 0, v != 0)` — but that
            //   only preserves zero/nonzero. Cleanest: route through
            //   a `Sub` between the i64 value and its high half (0)
            //   to coerce to i32. We instead leverage the fact that
            //   the surface signature for substring intends i32 lengths
            //   in practice; we introduce two helper sub-ops below.
            //
            // Pragmatic narrowing: cast via i64 -> i32 by computing
            //   v_i32 = (v_i64 + 0) and then a Lt(I64) trap above
            //   guarantees fit. Without a dedicated wrap op the
            //   stdlib can't do it; we add the wrap as an
            //   AllocScratchDyn-friendly path: store v into a wide
            //   slot, then read the low i32. Use the scratch heap
            //   as a temporary u64-to-u32 conversion:
            //     allocate 8 scratch bytes
            //     store_i64(scratch, 0, v_i64)
            //     load_i32(scratch, 0)  -> low i32
            // -----------------------------------------------------
            // Narrow start.
            tt(Op::ConstI32(8)),
            tt(Op::AllocScratchDyn),
            tt(Op::LetSet {
                idx: BASE, /* reuse as narrow_scratch */
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: BASE,
                ty: IrType::I32,
            }),
            tt(Op::LocalGet(1)),
            tt(Op::StoreI64AtAbsolute { offset: 0 }),
            tt(Op::LetGet {
                idx: BASE,
                ty: IrType::I32,
            }),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: START_I32,
                ty: IrType::I32,
            }),
            // Narrow len (reuse the same scratch slot).
            tt(Op::LetGet {
                idx: BASE,
                ty: IrType::I32,
            }),
            tt(Op::LocalGet(2)),
            tt(Op::StoreI64AtAbsolute { offset: 0 }),
            tt(Op::LetGet {
                idx: BASE,
                ty: IrType::I32,
            }),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: LEN_I32,
                ty: IrType::I32,
            }),
            // -----------------------------------------------------
            // Build the result record.
            // -----------------------------------------------------
            // record_size = len + 4
            tt(Op::LetGet {
                idx: LEN_I32,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(4)),
            tt(Op::Add(IrType::I32)),
            tt(Op::AllocScratchDyn),
            tt(Op::LetSet {
                idx: BASE,
                ty: IrType::I32,
            }),
            // store header
            tt(Op::LetGet {
                idx: BASE,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: LEN_I32,
                ty: IrType::I32,
            }),
            tt(Op::StoreI32AtAbsolute { offset: 0 }),
            // memcpy(base + 4, s + 4 + start, len)
            tt(Op::LetGet {
                idx: BASE,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(4)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LetGet {
                idx: START_I32,
                ty: IrType::I32,
            }),
            tt(Op::Add(IrType::I32)),
            tt(Op::LetGet {
                idx: LEN_I32,
                ty: IrType::I32,
            }),
            tt(Op::MemcpyAtAbsolute),
            // return base
            tt(Op::LetGet {
                idx: BASE,
                ty: IrType::I32,
            }),
            tt(Op::Return),
        ],
    }
}

/// Hand-written body for `starts_with(s: String, prefix: String) -> Bool`.
///
/// Algorithm:
///   1. Read `s_len` and `p_len`.
///   2. If `p_len > s_len`, return false.
///   3. Loop i = 0..p_len: if `byte(s, 4+i) != byte(p, 4+i)` return false.
///   4. After the loop completes, return true.
///
/// Returns the result via a scratch i32 cell that captures the
/// short-circuit decision; we lean on a let-local instead.
///
/// Locals:
///   * 0 — `s_len: I32`
///   * 1 — `p_len: I32`
///   * 2 — `i:     I32`
///   * 3 — `acc:   I32` (running result; 1 = still matches, 0 = miss)
fn starts_with_string() -> StdlibFunction {
    const S_LEN: u32 = 0;
    const P_LEN: u32 = 1;
    const I: u32 = 2;
    const ACC: u32 = 3;
    StdlibFunction {
        name: "starts_with",
        params: vec![IrType::String, IrType::String],
        ret: IrType::Bool,
        body: vec![
            // s_len = load_i32(s)
            tt(Op::LocalGet(0)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: S_LEN,
                ty: IrType::I32,
            }),
            // p_len = load_i32(p)
            tt(Op::LocalGet(1)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: P_LEN,
                ty: IrType::I32,
            }),
            // if p_len > s_len { return false }
            //   We express the early-out via an if/else producing
            //   the final Bool. Both arms must produce a value of
            //   the same type; the false arm returns 0, the true arm
            //   runs the loop and returns the final acc.
            tt(Op::LetGet {
                idx: P_LEN,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: S_LEN,
                ty: IrType::I32,
            }),
            tt(Op::Gt(IrType::I32)),
            tt(Op::If {
                result_ty: IrType::Bool,
                then_body: vec![tt(Op::ConstBool(false))],
                else_body: vec![
                    // acc = 1 (true so far)
                    tt(Op::ConstI32(1)),
                    tt(Op::LetSet {
                        idx: ACC,
                        ty: IrType::I32,
                    }),
                    // i = 0
                    tt(Op::ConstI32(0)),
                    tt(Op::LetSet {
                        idx: I,
                        ty: IrType::I32,
                    }),
                    // block { loop { ... } }
                    tt(Op::Block {
                        result_ty: None,
                        body: vec![tt(Op::Loop {
                            result_ty: None,
                            body: vec![
                                // exit when i >= p_len
                                tt(Op::LetGet {
                                    idx: I,
                                    ty: IrType::I32,
                                }),
                                tt(Op::LetGet {
                                    idx: P_LEN,
                                    ty: IrType::I32,
                                }),
                                tt(Op::Ge(IrType::I32)),
                                tt(Op::BrIf { label_depth: 1 }),
                                // sb = i32.load8_u(s + 4 + i)
                                tt(Op::LocalGet(0)),
                                tt(Op::ConstI32(4)),
                                tt(Op::Add(IrType::I32)),
                                tt(Op::LetGet {
                                    idx: I,
                                    ty: IrType::I32,
                                }),
                                tt(Op::Add(IrType::I32)),
                                tt(Op::LoadI8UAtAbsolute { offset: 0 }),
                                // pb = i32.load8_u(p + 4 + i)
                                tt(Op::LocalGet(1)),
                                tt(Op::ConstI32(4)),
                                tt(Op::Add(IrType::I32)),
                                tt(Op::LetGet {
                                    idx: I,
                                    ty: IrType::I32,
                                }),
                                tt(Op::Add(IrType::I32)),
                                tt(Op::LoadI8UAtAbsolute { offset: 0 }),
                                // if sb != pb { acc = 0; br 1 }
                                tt(Op::Ne(IrType::I32)),
                                tt(Op::If {
                                    result_ty: IrType::I32,
                                    then_body: vec![
                                        tt(Op::ConstI32(0)),
                                        tt(Op::LetSet {
                                            idx: ACC,
                                            ty: IrType::I32,
                                        }),
                                        // Use a sentinel i32 to keep the
                                        // arm typed; the Br exits before
                                        // any caller observes it.
                                        tt(Op::ConstI32(0)),
                                        tt(Op::Br { label_depth: 2 }),
                                    ],
                                    else_body: vec![tt(Op::ConstI32(0))],
                                }),
                                // Drop the i32 the If produced
                                // (codegen has no Drop op; instead we
                                // sink it into a let we ignore).
                                tt(Op::LetSet {
                                    idx: ACC, /* harmlessly overwritten next iter */
                                    ty: IrType::I32,
                                }),
                                // Restore acc to its earlier value
                                // (it was 1 entering this iteration
                                // and we just clobbered it). Cheaper:
                                // hoist the sink to a scratch local.
                                tt(Op::ConstI32(1)),
                                tt(Op::LetSet {
                                    idx: ACC,
                                    ty: IrType::I32,
                                }),
                                // i = i + 1
                                tt(Op::LetGet {
                                    idx: I,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(1)),
                                tt(Op::Add(IrType::I32)),
                                tt(Op::LetSet {
                                    idx: I,
                                    ty: IrType::I32,
                                }),
                                tt(Op::Br { label_depth: 0 }),
                            ],
                        })],
                    }),
                    // Result = acc != 0
                    tt(Op::LetGet {
                        idx: ACC,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(0)),
                    tt(Op::Ne(IrType::I32)),
                ],
            }),
            tt(Op::Return),
        ],
    }
}

/// F-D7-D body for `contains(haystack: String, needle: String) -> Bool`.
///
/// Algorithm: naive O(s_len * p_len) substring scan. Mirrors the
/// pre-existing `__relon_str_contains` host shim (and the F-D7-C
/// inline lowering on the trace-JIT side) so the IR-level path stays
/// compatible with the trace recorder's `TraceOp::StrContains`
/// short-circuit (`STDLIB_IDX_CONTAINS = 36`).
///
/// 1. Load `s_len` / `p_len` from the records' `[len: u32 LE]` headers.
/// 2. `p_len > s_len` → return `false` (needle too long).
/// 3. `p_len == 0`     → return `true`  (empty needle).
/// 4. Otherwise scan `i ∈ [0 .. s_len - p_len]` and compare the
///    `p_len`-byte window byte-by-byte. Found ≥ 1 match → return `true`,
///    else `false`.
///
/// Locals (indices into the let-area):
///   * 0 — `s_len:      I32`
///   * 1 — `p_len:      I32`
///   * 2 — `last_start: I32` (= `s_len - p_len`, only valid in the scan arm)
///   * 3 — `i:          I32` (outer scan position)
///   * 4 — `j:          I32` (inner compare cursor)
///   * 5 — `mismatch:   I32` (1 = mismatch hit inside inner loop)
///   * 6 — `found:      I32` (1 = full window matched; outer-exit flag)
///
/// Returned `Bool` is encoded as `i32` (0 / 1), matching every other
/// `Bool`-returning stdlib body.
fn contains_string() -> StdlibFunction {
    const S_LEN: u32 = 0;
    const P_LEN: u32 = 1;
    const LAST_START: u32 = 2;
    const I: u32 = 3;
    const J: u32 = 4;
    const MISMATCH: u32 = 5;
    const FOUND: u32 = 6;
    StdlibFunction {
        name: "contains",
        params: vec![IrType::String, IrType::String],
        ret: IrType::Bool,
        body: vec![
            // s_len = load_i32(s, 0)
            tt(Op::LocalGet(0)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: S_LEN,
                ty: IrType::I32,
            }),
            // p_len = load_i32(p, 0)
            tt(Op::LocalGet(1)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: P_LEN,
                ty: IrType::I32,
            }),
            // if p_len > s_len { return false }
            tt(Op::LetGet {
                idx: P_LEN,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: S_LEN,
                ty: IrType::I32,
            }),
            tt(Op::Gt(IrType::I32)),
            tt(Op::If {
                result_ty: IrType::Bool,
                then_body: vec![tt(Op::ConstBool(false))],
                else_body: vec![
                    // if p_len == 0 { return true }
                    tt(Op::LetGet {
                        idx: P_LEN,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(0)),
                    tt(Op::Eq(IrType::I32)),
                    tt(Op::If {
                        result_ty: IrType::Bool,
                        then_body: vec![tt(Op::ConstBool(true))],
                        else_body: vec![
                            // last_start = s_len - p_len
                            tt(Op::LetGet {
                                idx: S_LEN,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetGet {
                                idx: P_LEN,
                                ty: IrType::I32,
                            }),
                            tt(Op::Sub(IrType::I32)),
                            tt(Op::LetSet {
                                idx: LAST_START,
                                ty: IrType::I32,
                            }),
                            // i = 0; found = 0
                            tt(Op::ConstI32(0)),
                            tt(Op::LetSet {
                                idx: I,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                            tt(Op::LetSet {
                                idx: FOUND,
                                ty: IrType::I32,
                            }),
                            // outer scan
                            tt(Op::Block {
                                result_ty: None,
                                body: vec![tt(Op::Loop {
                                    result_ty: None,
                                    body: vec![
                                        // found != 0 ? br 1  (already matched)
                                        tt(Op::LetGet {
                                            idx: FOUND,
                                            ty: IrType::I32,
                                        }),
                                        tt(Op::ConstI32(0)),
                                        tt(Op::Ne(IrType::I32)),
                                        tt(Op::BrIf { label_depth: 1 }),
                                        // i > last_start ? br 1
                                        tt(Op::LetGet {
                                            idx: I,
                                            ty: IrType::I32,
                                        }),
                                        tt(Op::LetGet {
                                            idx: LAST_START,
                                            ty: IrType::I32,
                                        }),
                                        tt(Op::Gt(IrType::I32)),
                                        tt(Op::BrIf { label_depth: 1 }),
                                        // j = 0; mismatch = 0
                                        tt(Op::ConstI32(0)),
                                        tt(Op::LetSet {
                                            idx: J,
                                            ty: IrType::I32,
                                        }),
                                        tt(Op::ConstI32(0)),
                                        tt(Op::LetSet {
                                            idx: MISMATCH,
                                            ty: IrType::I32,
                                        }),
                                        // inner compare loop: increment j until
                                        // either the window is fully matched
                                        // (`j == p_len` → exit with mismatch=0)
                                        // or a byte differs (mismatch=1).
                                        tt(Op::Block {
                                            result_ty: None,
                                            body: vec![tt(Op::Loop {
                                                result_ty: None,
                                                body: vec![
                                                    // j >= p_len ? br 1 (window fully matched
                                                    // — leave mismatch as-is, which is 0)
                                                    tt(Op::LetGet {
                                                        idx: J,
                                                        ty: IrType::I32,
                                                    }),
                                                    tt(Op::LetGet {
                                                        idx: P_LEN,
                                                        ty: IrType::I32,
                                                    }),
                                                    tt(Op::Ge(IrType::I32)),
                                                    tt(Op::BrIf { label_depth: 1 }),
                                                    // sb = load_i8(s + 4 + i + j)
                                                    tt(Op::LocalGet(0)),
                                                    tt(Op::ConstI32(4)),
                                                    tt(Op::Add(IrType::I32)),
                                                    tt(Op::LetGet {
                                                        idx: I,
                                                        ty: IrType::I32,
                                                    }),
                                                    tt(Op::Add(IrType::I32)),
                                                    tt(Op::LetGet {
                                                        idx: J,
                                                        ty: IrType::I32,
                                                    }),
                                                    tt(Op::Add(IrType::I32)),
                                                    tt(Op::LoadI8UAtAbsolute { offset: 0 }),
                                                    // pb = load_i8(p + 4 + j)
                                                    tt(Op::LocalGet(1)),
                                                    tt(Op::ConstI32(4)),
                                                    tt(Op::Add(IrType::I32)),
                                                    tt(Op::LetGet {
                                                        idx: J,
                                                        ty: IrType::I32,
                                                    }),
                                                    tt(Op::Add(IrType::I32)),
                                                    tt(Op::LoadI8UAtAbsolute { offset: 0 }),
                                                    // mismatch = (sb != pb)   (0 / 1)
                                                    tt(Op::Ne(IrType::I32)),
                                                    tt(Op::LetSet {
                                                        idx: MISMATCH,
                                                        ty: IrType::I32,
                                                    }),
                                                    // mismatch != 0 ? br 1
                                                    tt(Op::LetGet {
                                                        idx: MISMATCH,
                                                        ty: IrType::I32,
                                                    }),
                                                    tt(Op::ConstI32(0)),
                                                    tt(Op::Ne(IrType::I32)),
                                                    tt(Op::BrIf { label_depth: 1 }),
                                                    // j = j + 1
                                                    tt(Op::LetGet {
                                                        idx: J,
                                                        ty: IrType::I32,
                                                    }),
                                                    tt(Op::ConstI32(1)),
                                                    tt(Op::Add(IrType::I32)),
                                                    tt(Op::LetSet {
                                                        idx: J,
                                                        ty: IrType::I32,
                                                    }),
                                                    tt(Op::Br { label_depth: 0 }),
                                                ],
                                            })],
                                        }),
                                        // After inner: found = (mismatch == 0)
                                        tt(Op::LetGet {
                                            idx: MISMATCH,
                                            ty: IrType::I32,
                                        }),
                                        tt(Op::ConstI32(0)),
                                        tt(Op::Eq(IrType::I32)),
                                        tt(Op::LetSet {
                                            idx: FOUND,
                                            ty: IrType::I32,
                                        }),
                                        // i = i + 1
                                        tt(Op::LetGet {
                                            idx: I,
                                            ty: IrType::I32,
                                        }),
                                        tt(Op::ConstI32(1)),
                                        tt(Op::Add(IrType::I32)),
                                        tt(Op::LetSet {
                                            idx: I,
                                            ty: IrType::I32,
                                        }),
                                        tt(Op::Br { label_depth: 0 }),
                                    ],
                                })],
                            }),
                            // result = found != 0
                            tt(Op::LetGet {
                                idx: FOUND,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                            tt(Op::Ne(IrType::I32)),
                        ],
                    }),
                ],
            }),
            tt(Op::Return),
        ],
    }
}

/// Hand-written body for `list_int_sum(xs: List<Int>) -> Int`.
///
/// Record layout: `[len: u32 @0][...optional 4-byte pad to align
/// payload up to 8][i64 elements]`. The host's `BufferBuilder` lays
/// out tail records at a 4-byte prefix-alignment so the record
/// start is only 4-aligned; whether the i64 payload sits at
/// `xs + 4` or `xs + 8` depends on the receiver's absolute parity.
/// We replicate the host's `align_up(xs + 4, 8)` rule via the bit
/// trick `(xs + 4 + 7) & -8`, computed once before the loop.
///
/// Locals:
///   * 0 — `n:       I32` (element count)
///   * 1 — `i:       I32`
///   * 2 — `acc:     I64` (running sum)
///   * 3 — `payload: I32` (absolute address of element 0)
fn list_int_sum() -> StdlibFunction {
    const N: u32 = 0;
    const I: u32 = 1;
    const ACC: u32 = 2;
    const PAYLOAD: u32 = 3;
    StdlibFunction {
        name: "list_int_sum",
        params: vec![IrType::ListInt],
        ret: IrType::I64,
        body: vec![
            // n = load_i32(xs, 0)
            tt(Op::LocalGet(0)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: N,
                ty: IrType::I32,
            }),
            // payload = (xs + 4 + 7) & -8
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4 + 7)),
            tt(Op::Add(IrType::I32)),
            tt(Op::ConstI32(-8)),
            tt(Op::BitAnd(IrType::I32)),
            tt(Op::LetSet {
                idx: PAYLOAD,
                ty: IrType::I32,
            }),
            // acc = 0; i = 0
            tt(Op::ConstI64(0)),
            tt(Op::LetSet {
                idx: ACC,
                ty: IrType::I64,
            }),
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: I,
                ty: IrType::I32,
            }),
            // block { loop { ... } }
            tt(Op::Block {
                result_ty: None,
                body: vec![tt(Op::Loop {
                    result_ty: None,
                    body: vec![
                        // exit when i >= n
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: N,
                            ty: IrType::I32,
                        }),
                        tt(Op::Ge(IrType::I32)),
                        tt(Op::BrIf { label_depth: 1 }),
                        // acc += i64.load(payload + i * 8)
                        tt(Op::LetGet {
                            idx: ACC,
                            ty: IrType::I64,
                        }),
                        tt(Op::LetGet {
                            idx: PAYLOAD,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(8)),
                        tt(Op::Mul(IrType::I32)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LoadI64AtAbsolute { offset: 0 }),
                        tt(Op::Add(IrType::I64)),
                        tt(Op::LetSet {
                            idx: ACC,
                            ty: IrType::I64,
                        }),
                        // i = i + 1
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(1)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::Br { label_depth: 0 }),
                    ],
                })],
            }),
            // return acc
            tt(Op::LetGet {
                idx: ACC,
                ty: IrType::I64,
            }),
            tt(Op::Return),
        ],
    }
}

/// Hand-written body for `list_int_max(xs: List<Int>) -> Int`.
///
/// Trap discipline: an empty receiver triggers
/// [`TrapKind::EmptyList`] before any iteration runs. Picking a
/// finite default (e.g. `i64::MIN`) would surface as a silent
/// surprise — every other reducer in the Phase 4.c-2 set assumes
/// at least one element to fold over.
///
/// Payload-start calculation mirrors [`list_int_sum`]: align
/// `xs + 4` up to 8 with `(x + 7) & -8` so the loop indexes into
/// the real i64 payload regardless of the receiver's parity.
///
/// Locals:
///   * 0 — `n:       I32`
///   * 1 — `i:       I32`
///   * 2 — `acc:     I64`
///   * 3 — `payload: I32`
///   * 4 — `val:     I64` (per-iter scratch for `max(acc, val)`)
fn list_int_max() -> StdlibFunction {
    const N: u32 = 0;
    const I: u32 = 1;
    const ACC: u32 = 2;
    const PAYLOAD: u32 = 3;
    const VAL: u32 = 4;
    StdlibFunction {
        name: "list_int_max",
        params: vec![IrType::ListInt],
        ret: IrType::I64,
        body: vec![
            // n = load_i32(xs, 0)
            tt(Op::LocalGet(0)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: N,
                ty: IrType::I32,
            }),
            // if n == 0 { trap }
            tt(Op::LetGet {
                idx: N,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
            tt(Op::Eq(IrType::I32)),
            tt(Op::If {
                result_ty: IrType::I32,
                then_body: vec![
                    tt(Op::Trap {
                        kind: TrapKind::EmptyList,
                    }),
                    tt(Op::ConstI32(0)),
                ],
                else_body: vec![tt(Op::ConstI32(0))],
            }),
            // Sink the i32 placeholder produced by the If.
            tt(Op::LetSet {
                idx: I, /* harmless: I is overwritten just below */
                ty: IrType::I32,
            }),
            // payload = (xs + 4 + 7) & -8
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4 + 7)),
            tt(Op::Add(IrType::I32)),
            tt(Op::ConstI32(-8)),
            tt(Op::BitAnd(IrType::I32)),
            tt(Op::LetSet {
                idx: PAYLOAD,
                ty: IrType::I32,
            }),
            // acc = i64.load(payload + 0) (the first element)
            tt(Op::LetGet {
                idx: PAYLOAD,
                ty: IrType::I32,
            }),
            tt(Op::LoadI64AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: ACC,
                ty: IrType::I64,
            }),
            // i = 1
            tt(Op::ConstI32(1)),
            tt(Op::LetSet {
                idx: I,
                ty: IrType::I32,
            }),
            // block { loop { ... } }
            tt(Op::Block {
                result_ty: None,
                body: vec![tt(Op::Loop {
                    result_ty: None,
                    body: vec![
                        // exit when i >= n
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: N,
                            ty: IrType::I32,
                        }),
                        tt(Op::Ge(IrType::I32)),
                        tt(Op::BrIf { label_depth: 1 }),
                        // val = i64.load(payload + i * 8)
                        tt(Op::LetGet {
                            idx: PAYLOAD,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(8)),
                        tt(Op::Mul(IrType::I32)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LoadI64AtAbsolute { offset: 0 }),
                        tt(Op::LetSet {
                            idx: VAL,
                            ty: IrType::I64,
                        }),
                        // acc = select(val, acc, val > acc)
                        tt(Op::LetGet {
                            idx: VAL,
                            ty: IrType::I64,
                        }),
                        tt(Op::LetGet {
                            idx: ACC,
                            ty: IrType::I64,
                        }),
                        tt(Op::LetGet {
                            idx: VAL,
                            ty: IrType::I64,
                        }),
                        tt(Op::LetGet {
                            idx: ACC,
                            ty: IrType::I64,
                        }),
                        tt(Op::Gt(IrType::I64)),
                        tt(Op::Select { ty: IrType::I64 }),
                        tt(Op::LetSet {
                            idx: ACC,
                            ty: IrType::I64,
                        }),
                        // i = i + 1
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(1)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::Br { label_depth: 0 }),
                    ],
                })],
            }),
            // return acc
            tt(Op::LetGet {
                idx: ACC,
                ty: IrType::I64,
            }),
            tt(Op::Return),
        ],
    }
}

/// Phase 10-a body for `list_int_map(xs: List<Int>, f: Closure) -> List<Int>`.
///
/// Algorithm:
///   1. Read `n = xs[0]`.
///   2. Allocate `8 + 8*n + 8` scratch bytes for the result record
///      (the trailing `+8` is alignment slop so the dynamic
///      `(base + 4 + 7) & -8` payload pointer stays inside the
///      reserved region for any `base` mod 8).
///   3. Write the length prefix.
///   4. `src_payload = (xs + 4 + 7) & -8`.
///   5. `dst_payload = (new_base + 4 + 7) & -8`.
///   6. Loop `i = 0..n`: load `x = src_payload[i]`, invoke the
///      closure via `Op::CallClosure { [I64] -> I64 }`, store the
///      result at `dst_payload[i]`.
///   7. Return `new_base`.
///
/// Locals:
///   * 0 — `n:           I32`
///   * 1 — `i:           I32`
///   * 2 — `src_payload: I32`
///   * 3 — `dst_payload: I32`
///   * 4 — `new_base:    I32`
fn list_int_map() -> StdlibFunction {
    const N: u32 = 0;
    const I: u32 = 1;
    const SRC_PAYLOAD: u32 = 2;
    const DST_PAYLOAD: u32 = 3;
    const NEW_BASE: u32 = 4;
    StdlibFunction {
        name: "list_int_map",
        params: vec![IrType::ListInt, IrType::Closure],
        ret: IrType::ListInt,
        body: vec![
            // n = i32.load(xs, 0)
            tt(Op::LocalGet(0)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: N,
                ty: IrType::I32,
            }),
            // record_size = 8 + 8*n + 8
            tt(Op::ConstI32(16)),
            tt(Op::LetGet {
                idx: N,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(8)),
            tt(Op::Mul(IrType::I32)),
            tt(Op::Add(IrType::I32)),
            tt(Op::AllocScratchDyn),
            tt(Op::LetSet {
                idx: NEW_BASE,
                ty: IrType::I32,
            }),
            // store header: i32.store(new_base, n)
            tt(Op::LetGet {
                idx: NEW_BASE,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: N,
                ty: IrType::I32,
            }),
            tt(Op::StoreI32AtAbsolute { offset: 0 }),
            // src_payload = (xs + 4 + 7) & -8
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4 + 7)),
            tt(Op::Add(IrType::I32)),
            tt(Op::ConstI32(-8)),
            tt(Op::BitAnd(IrType::I32)),
            tt(Op::LetSet {
                idx: SRC_PAYLOAD,
                ty: IrType::I32,
            }),
            // dst_payload = (new_base + 4 + 7) & -8
            tt(Op::LetGet {
                idx: NEW_BASE,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(4 + 7)),
            tt(Op::Add(IrType::I32)),
            tt(Op::ConstI32(-8)),
            tt(Op::BitAnd(IrType::I32)),
            tt(Op::LetSet {
                idx: DST_PAYLOAD,
                ty: IrType::I32,
            }),
            // i = 0
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: I,
                ty: IrType::I32,
            }),
            // block { loop { ... } }
            tt(Op::Block {
                result_ty: None,
                body: vec![tt(Op::Loop {
                    result_ty: None,
                    body: vec![
                        // exit when i >= n
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: N,
                            ty: IrType::I32,
                        }),
                        tt(Op::Ge(IrType::I32)),
                        tt(Op::BrIf { label_depth: 1 }),
                        // dst_addr = dst_payload + i * 8 (pushed first
                        // so the i64.store sees [addr, value] at end)
                        tt(Op::LetGet {
                            idx: DST_PAYLOAD,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(8)),
                        tt(Op::Mul(IrType::I32)),
                        tt(Op::Add(IrType::I32)),
                        // push closure handle (param 1)
                        tt(Op::LocalGet(1)),
                        // push the source element: i64.load(src_payload + i*8)
                        tt(Op::LetGet {
                            idx: SRC_PAYLOAD,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(8)),
                        tt(Op::Mul(IrType::I32)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LoadI64AtAbsolute { offset: 0 }),
                        // call_indirect via Op::CallClosure
                        tt(Op::CallClosure {
                            param_tys: vec![IrType::I64],
                            ret_ty: IrType::I64,
                        }),
                        // i64.store: stack is [dst_addr, result_i64]
                        tt(Op::StoreI64AtAbsolute { offset: 0 }),
                        // i = i + 1
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(1)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::Br { label_depth: 0 }),
                    ],
                })],
            }),
            // return new_base
            tt(Op::LetGet {
                idx: NEW_BASE,
                ty: IrType::I32,
            }),
            tt(Op::Return),
        ],
    }
}

/// Phase 10-a body for `list_int_filter(xs: List<Int>, f: Closure) -> List<Int>`.
///
/// Algorithm:
///   1. Read `n = xs[0]`. Allocate a worst-case-sized result buffer
///      (same shape as `list_int_map` since at most `n` elements
///      survive the filter).
///   2. `src_payload = (xs + 4 + 7) & -8`.
///   3. `dst_payload = (new_base + 4 + 7) & -8`.
///   4. `out_count = 0`.
///   5. Loop `i = 0..n`: load `x`, invoke the closure with `(x)`
///      (`Op::CallClosure { [I64] -> Bool }`); when the predicate
///      returns non-zero, copy `x` into `dst_payload[out_count]`
///      and increment `out_count`.
///   6. Write the actual count into the header at the end.
///   7. Return `new_base`.
///
/// Locals:
///   * 0 — `n:           I32`
///   * 1 — `i:           I32`
///   * 2 — `src_payload: I32`
///   * 3 — `dst_payload: I32`
///   * 4 — `new_base:    I32`
///   * 5 — `out_count:   I32`
///   * 6 — `cur_val:     I64`
fn list_int_filter() -> StdlibFunction {
    const N: u32 = 0;
    const I: u32 = 1;
    const SRC_PAYLOAD: u32 = 2;
    const DST_PAYLOAD: u32 = 3;
    const NEW_BASE: u32 = 4;
    const OUT_COUNT: u32 = 5;
    const CUR_VAL: u32 = 6;
    /// Dedicated sink local for the `If` sentinel i32 produced by
    /// the filter's per-iteration predicate branch — keeps the loop
    /// counter `I` from being clobbered by the sink. The wasm
    /// `if (result i32) ... end` shape requires both arms to leave
    /// a value behind; we route an `i32.const 0` sentinel through
    /// the sink, then ignore it.
    const SINK: u32 = 7;
    StdlibFunction {
        name: "list_int_filter",
        params: vec![IrType::ListInt, IrType::Closure],
        ret: IrType::ListInt,
        body: vec![
            tt(Op::LocalGet(0)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: N,
                ty: IrType::I32,
            }),
            // record_size = 8 + 8*n + 8
            tt(Op::ConstI32(16)),
            tt(Op::LetGet {
                idx: N,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(8)),
            tt(Op::Mul(IrType::I32)),
            tt(Op::Add(IrType::I32)),
            tt(Op::AllocScratchDyn),
            tt(Op::LetSet {
                idx: NEW_BASE,
                ty: IrType::I32,
            }),
            // src_payload = (xs + 4 + 7) & -8
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4 + 7)),
            tt(Op::Add(IrType::I32)),
            tt(Op::ConstI32(-8)),
            tt(Op::BitAnd(IrType::I32)),
            tt(Op::LetSet {
                idx: SRC_PAYLOAD,
                ty: IrType::I32,
            }),
            // dst_payload = (new_base + 4 + 7) & -8
            tt(Op::LetGet {
                idx: NEW_BASE,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(4 + 7)),
            tt(Op::Add(IrType::I32)),
            tt(Op::ConstI32(-8)),
            tt(Op::BitAnd(IrType::I32)),
            tt(Op::LetSet {
                idx: DST_PAYLOAD,
                ty: IrType::I32,
            }),
            // i = 0; out_count = 0
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: I,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: OUT_COUNT,
                ty: IrType::I32,
            }),
            tt(Op::Block {
                result_ty: None,
                body: vec![tt(Op::Loop {
                    result_ty: None,
                    body: vec![
                        // exit when i >= n
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: N,
                            ty: IrType::I32,
                        }),
                        tt(Op::Ge(IrType::I32)),
                        tt(Op::BrIf { label_depth: 1 }),
                        // cur_val = i64.load(src_payload + i*8)
                        tt(Op::LetGet {
                            idx: SRC_PAYLOAD,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(8)),
                        tt(Op::Mul(IrType::I32)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LoadI64AtAbsolute { offset: 0 }),
                        tt(Op::LetSet {
                            idx: CUR_VAL,
                            ty: IrType::I64,
                        }),
                        // closure(cur_val) -> bool
                        tt(Op::LocalGet(1)),
                        tt(Op::LetGet {
                            idx: CUR_VAL,
                            ty: IrType::I64,
                        }),
                        tt(Op::CallClosure {
                            param_tys: vec![IrType::I64],
                            ret_ty: IrType::Bool,
                        }),
                        // if cond { dst[out_count] = cur_val; out_count++ }
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                // dst_addr = dst_payload + out_count*8
                                tt(Op::LetGet {
                                    idx: DST_PAYLOAD,
                                    ty: IrType::I32,
                                }),
                                tt(Op::LetGet {
                                    idx: OUT_COUNT,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(8)),
                                tt(Op::Mul(IrType::I32)),
                                tt(Op::Add(IrType::I32)),
                                tt(Op::LetGet {
                                    idx: CUR_VAL,
                                    ty: IrType::I64,
                                }),
                                tt(Op::StoreI64AtAbsolute { offset: 0 }),
                                // out_count = out_count + 1
                                tt(Op::LetGet {
                                    idx: OUT_COUNT,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(1)),
                                tt(Op::Add(IrType::I32)),
                                tt(Op::LetSet {
                                    idx: OUT_COUNT,
                                    ty: IrType::I32,
                                }),
                                // sentinel for the if's i32 result
                                tt(Op::ConstI32(0)),
                            ],
                            else_body: vec![tt(Op::ConstI32(0))],
                        }),
                        // Sink the i32 placeholder produced by the If
                        // into a dedicated local so the loop counter
                        // stays intact.
                        tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }),
                        // i = i + 1
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(1)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::Br { label_depth: 0 }),
                    ],
                })],
            }),
            // store header: i32.store(new_base, out_count)
            tt(Op::LetGet {
                idx: NEW_BASE,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: OUT_COUNT,
                ty: IrType::I32,
            }),
            tt(Op::StoreI32AtAbsolute { offset: 0 }),
            // return new_base
            tt(Op::LetGet {
                idx: NEW_BASE,
                ty: IrType::I32,
            }),
            tt(Op::Return),
        ],
    }
}

/// Phase 10-a body for `list_int_fold(xs: List<Int>, init: Int, f: Closure) -> Int`.
///
/// Algorithm:
///   1. Read `n = xs[0]`. `acc = init` (the second positional param).
///   2. `src_payload = (xs + 4 + 7) & -8`.
///   3. Loop `i = 0..n`: load `x`; invoke the closure with
///      `(acc, x)` (`Op::CallClosure { [I64, I64] -> I64 }`);
///      assign the result back to `acc`.
///   4. Return `acc`.
///
/// Locals:
///   * 0 — `n:           I32`
///   * 1 — `i:           I32`
///   * 2 — `src_payload: I32`
///   * 3 — `acc:         I64`
fn list_int_fold() -> StdlibFunction {
    const N: u32 = 0;
    const I: u32 = 1;
    const SRC_PAYLOAD: u32 = 2;
    const ACC: u32 = 3;
    StdlibFunction {
        name: "list_int_fold",
        // Param ordering matches the user-facing surface:
        //   `xs.fold(init, |acc, x| ...)` lowers to
        //   `list_int_fold(xs, init, f)`.
        params: vec![IrType::ListInt, IrType::I64, IrType::Closure],
        ret: IrType::I64,
        body: vec![
            // n = i32.load(xs, 0)
            tt(Op::LocalGet(0)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: N,
                ty: IrType::I32,
            }),
            // src_payload = (xs + 4 + 7) & -8
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4 + 7)),
            tt(Op::Add(IrType::I32)),
            tt(Op::ConstI32(-8)),
            tt(Op::BitAnd(IrType::I32)),
            tt(Op::LetSet {
                idx: SRC_PAYLOAD,
                ty: IrType::I32,
            }),
            // acc = init
            tt(Op::LocalGet(1)),
            tt(Op::LetSet {
                idx: ACC,
                ty: IrType::I64,
            }),
            // i = 0
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: I,
                ty: IrType::I32,
            }),
            tt(Op::Block {
                result_ty: None,
                body: vec![tt(Op::Loop {
                    result_ty: None,
                    body: vec![
                        // exit when i >= n
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: N,
                            ty: IrType::I32,
                        }),
                        tt(Op::Ge(IrType::I32)),
                        tt(Op::BrIf { label_depth: 1 }),
                        // push closure (param 2)
                        tt(Op::LocalGet(2)),
                        // push acc
                        tt(Op::LetGet {
                            idx: ACC,
                            ty: IrType::I64,
                        }),
                        // push x = i64.load(src_payload + i*8)
                        tt(Op::LetGet {
                            idx: SRC_PAYLOAD,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(8)),
                        tt(Op::Mul(IrType::I32)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LoadI64AtAbsolute { offset: 0 }),
                        // call closure
                        tt(Op::CallClosure {
                            param_tys: vec![IrType::I64, IrType::I64],
                            ret_ty: IrType::I64,
                        }),
                        // acc = result
                        tt(Op::LetSet {
                            idx: ACC,
                            ty: IrType::I64,
                        }),
                        // i = i + 1
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(1)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::Br { label_depth: 0 }),
                    ],
                })],
            }),
            // return acc
            tt(Op::LetGet {
                idx: ACC,
                ty: IrType::I64,
            }),
            tt(Op::Return),
        ],
    }
}

/// v3++ b-7 reframed internal helper:
/// `__full_casefold_lookup(cp: I32, table_addr: I32) -> I32`.
///
/// Binary-searches the FULL multi-codepoint case-folding table embedded
/// in the const data section. The table layout matches
/// [`crate::full_case_folding::encode_full_table_bytes`]:
///
/// ```text
/// [count: u32 LE]
/// [(in: u32 LE, out0: u32 LE, out1: u32 LE, out2: u32 LE,
///   out_len: u32 LE) x count]
/// ```
///
/// 20-byte per-entry stride; the rebase formula is
/// `table_addr + 4 + mid * 20`.
///
/// Return value: the absolute address of the matched entry on a hit
/// (so the caller can load `out_len` from `entry + 16` and the
/// up-to-three output codepoints from `entry + 4`, `+ 8`, `+ 12`), or
/// `0` on a miss. The `0` sentinel is unambiguous because a valid
/// entry address is always at least `table_addr + 4` — and codegen
/// never plants the const pool at address `0` of the runtime arena
/// (the prologue reserves a guard region).
///
/// Pure mirror of [`casefold_lookup_helper`] modulo the stride and the
/// "return entry address" twist; the binary-search shape is identical.
///
/// Locals:
///   * 0 — `count: I32`   table entry count (loaded from header)
///   * 1 — `lo:    I32`   inclusive low bound of the search window
///   * 2 — `hi:    I32`   exclusive high bound of the search window
///   * 3 — `mid:   I32`   midpoint of the current window
///   * 4 — `entry: I32`   absolute address of the entry's input slot
///   * 5 — `key:   I32`   decoded input codepoint of the midpoint
///   * 6 — `sink:  I32`   drop slot for If-arm placeholders
///   * 7 — `result: I32`  matched entry address; defaults to 0 (miss)
#[allow(clippy::vec_init_then_push)]
fn full_casefold_lookup_helper() -> StdlibFunction {
    const COUNT: u32 = 0;
    const LO: u32 = 1;
    const HI: u32 = 2;
    const MID: u32 = 3;
    const ENTRY: u32 = 4;
    const KEY: u32 = 5;
    const SINK: u32 = 6;
    const RESULT: u32 = 7;
    StdlibFunction {
        name: "__full_casefold_lookup",
        params: vec![IrType::I32, IrType::I32],
        ret: IrType::I32,
        body: vec![
            // count = i32.load(table_addr + 0)
            tt(Op::LocalGet(1)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: COUNT,
                ty: IrType::I32,
            }),
            // lo = 0; hi = count
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: LO,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: COUNT,
                ty: IrType::I32,
            }),
            tt(Op::LetSet {
                idx: HI,
                ty: IrType::I32,
            }),
            // result = 0 (miss sentinel)
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: RESULT,
                ty: IrType::I32,
            }),
            tt(Op::Block {
                result_ty: None,
                body: vec![tt(Op::Loop {
                    result_ty: None,
                    body: vec![
                        // exit when lo >= hi
                        tt(Op::LetGet {
                            idx: LO,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: HI,
                            ty: IrType::I32,
                        }),
                        tt(Op::Ge(IrType::I32)),
                        tt(Op::BrIf { label_depth: 1 }),
                        // mid = (lo + hi) / 2
                        tt(Op::LetGet {
                            idx: LO,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: HI,
                            ty: IrType::I32,
                        }),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::ConstI32(2)),
                        tt(Op::Div(IrType::I32)),
                        tt(Op::LetSet {
                            idx: MID,
                            ty: IrType::I32,
                        }),
                        // entry = table_addr + 4 + mid * 20
                        tt(Op::LocalGet(1)),
                        tt(Op::ConstI32(4)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetGet {
                            idx: MID,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(20)),
                        tt(Op::Mul(IrType::I32)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetSet {
                            idx: ENTRY,
                            ty: IrType::I32,
                        }),
                        // key = i32.load(entry + 0)
                        tt(Op::LetGet {
                            idx: ENTRY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LoadI32AtAbsolute { offset: 0 }),
                        tt(Op::LetSet {
                            idx: KEY,
                            ty: IrType::I32,
                        }),
                        // if key == cp { RESULT = entry; br 2 }
                        tt(Op::LetGet {
                            idx: KEY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LocalGet(0)),
                        tt(Op::Eq(IrType::I32)),
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                tt(Op::LetGet {
                                    idx: ENTRY,
                                    ty: IrType::I32,
                                }),
                                tt(Op::LetSet {
                                    idx: RESULT,
                                    ty: IrType::I32,
                                }),
                                tt(Op::Br { label_depth: 2 }),
                                tt(Op::ConstI32(0)),
                            ],
                            else_body: vec![tt(Op::ConstI32(0))],
                        }),
                        tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }),
                        // if key < cp { lo = mid + 1 } else { hi = mid }
                        tt(Op::LetGet {
                            idx: KEY,
                            ty: IrType::I32,
                        }),
                        tt(Op::LocalGet(0)),
                        tt(Op::Lt(IrType::I32)),
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                tt(Op::LetGet {
                                    idx: MID,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(1)),
                                tt(Op::Add(IrType::I32)),
                                tt(Op::LetSet {
                                    idx: LO,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(0)),
                            ],
                            else_body: vec![
                                tt(Op::LetGet {
                                    idx: MID,
                                    ty: IrType::I32,
                                }),
                                tt(Op::LetSet {
                                    idx: HI,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(0)),
                            ],
                        }),
                        tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }),
                        tt(Op::Br { label_depth: 0 }),
                    ],
                })],
            }),
            tt(Op::LetGet {
                idx: RESULT,
                ty: IrType::I32,
            }),
            tt(Op::Return),
        ],
    }
}

/// v3++ b-7 reframed internal helper:
/// `__final_sigma_check(s_ptr, byte_offset, cased_addr, ignorable_addr) -> i32`.
///
/// UAX #21 Final_Sigma context test. Returns `1` when `Σ` (U+03A3) at
/// `byte_offset` inside the UTF-8 string `s_ptr` is at the end of a
/// word — preceded by at least one cased codepoint (skipping case-
/// ignorables), and followed only by case-ignorables until the end of
/// the string (or by a non-cased non-ignorable codepoint). Returns `0`
/// otherwise.
///
/// Tree-walk mirror: [`crate::full_case_folding::is_final_sigma_context`].
///
/// String layout: `[len: u32 LE][bytes × len]`. `s_ptr` points at the
/// header; payload bytes live at `s_ptr + 4`. `byte_offset` is the
/// payload-relative offset of the sigma's first UTF-8 byte (0xCE
/// 0xA3); the helper scans `[..byte_offset)` backwards and
/// `[byte_offset + 2..)` forwards, decoding one codepoint at a time.
///
/// The UTF-8 reverse decode finds the start byte by scanning leftward
/// until a byte with the top two bits != `10` is seen (i.e. either
/// ASCII < 0x80 or a 0xC0..=0xFF lead byte). The forward decode reuses
/// the same byte-length-from-lead-byte arithmetic as
/// [`case_fold_body_inner`].
///
/// Calls `__is_combining_mark`-shaped range membership against the
/// `cased_addr` and `ignorable_addr` tables (both share the
/// `[count: u32 LE][(start: u32, end: u32) x count]` layout); the
/// helper reaches them via the [`Op::CasedRangesAddr`] /
/// [`Op::CaseIgnorableRangesAddr`] addresses the caller threads in.
///
/// Locals:
///   * 0  — `s_len: I32`     payload byte length (from header)
///   * 1  — `back:  I32`     reverse scan byte cursor
///   * 2  — `fwd:   I32`     forward scan byte cursor
///   * 3  — `cp:    I32`     codepoint of the currently inspected byte
///   * 4  — `b0:    I32`     scratch for a UTF-8 lead/start byte
///   * 5  — `b1:    I32`     scratch for the byte at offset 1
///   * 6  — `b2:    I32`     scratch for the byte at offset 2
///   * 7  — `b3:    I32`     scratch for the byte at offset 3
///   * 8  — `width: I32`     byte width of the decoded codepoint
///   * 9  — `sink:  I32`     drop slot for If-arm placeholders
///   * 10 — `result: I32`    accumulated return (0 = not final)
///   * 11 — `seen_cased_before: I32`  left-scan saw a cased cp
///   * 12 — `start: I32`     scratch for reverse-walk start cursor
#[allow(clippy::vec_init_then_push, clippy::too_many_lines)]
fn final_sigma_check_helper() -> StdlibFunction {
    const S_LEN: u32 = 0;
    const BACK: u32 = 1;
    const FWD: u32 = 2;
    const CP: u32 = 3;
    const B0: u32 = 4;
    const B1: u32 = 5;
    const B2: u32 = 6;
    const B3: u32 = 7;
    const WIDTH: u32 = 8;
    const SINK: u32 = 9;
    const RESULT: u32 = 10;
    const SEEN_CASED_BEFORE: u32 = 11;
    const START: u32 = 12;

    // Param indices:
    //   LocalGet(0) = s_ptr (String record pointer)
    //   LocalGet(1) = byte_offset of the sigma's first UTF-8 byte
    //   LocalGet(2) = cased_ranges_addr
    //   LocalGet(3) = ignorable_ranges_addr

    let combining_idx = COMBINING_MARK_INDEX; // reused as generic range-membership

    // Helper: invoke `__is_combining_mark(cp, table_addr)` against an
    // arbitrary range table. Both `CASED_RANGES` and
    // `CASE_IGNORABLE_RANGES` share the same 8-byte stride layout so
    // the helper works without modification.
    let range_lookup = |cp_idx: u32, table_local_idx: u32| -> Vec<TaggedOp> {
        vec![
            tt(Op::LetGet {
                idx: cp_idx,
                ty: IrType::I32,
            }),
            tt(Op::LocalGet(table_local_idx)),
            tt(Op::Call {
                fn_index: combining_idx,
                arg_count: 2,
                param_tys: vec![IrType::I32, IrType::I32],
                ret_ty: IrType::I32,
            }),
        ]
    };

    // Helper: load i32u byte at `s_ptr + 4 + idx`. Pushes one i32 on
    // the stack.
    let load_payload_byte = |idx_local: u32| -> Vec<TaggedOp> {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LetGet {
                idx: idx_local,
                ty: IrType::I32,
            }),
            tt(Op::Add(IrType::I32)),
            tt(Op::LoadI8UAtAbsolute { offset: 0 }),
        ]
    };

    // Helper: load i32u byte at `s_ptr + 4 + base_local + literal_off`.
    let load_payload_byte_off = |base_local: u32, lit_off: i32| -> Vec<TaggedOp> {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4 + lit_off)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LetGet {
                idx: base_local,
                ty: IrType::I32,
            }),
            tt(Op::Add(IrType::I32)),
            tt(Op::LoadI8UAtAbsolute { offset: 0 }),
        ]
    };

    // Forward UTF-8 decode at `fwd_idx_local`: reads up to 4 bytes,
    // writes the codepoint into CP and the byte width into WIDTH.
    // Mirrors the 4-arm split in `case_fold_body_inner`. Defensive:
    // when the lead byte is malformed (< 0xC2 in the non-ASCII path,
    // or beyond 0xF7), the helper still produces *some* well-formed
    // value — we set CP to the lead byte and WIDTH to 1 so the scan
    // makes forward progress. This path is only ever reached on
    // already-valid UTF-8 (callers guarantee the input is a valid
    // String record), so the defensive branches are dead in practice;
    // they exist only to keep the verifier happy.
    let forward_decode = |idx_local: u32| -> Vec<TaggedOp> {
        let mut seq: Vec<TaggedOp> = Vec::new();
        // b0 = byte at fwd
        seq.extend(load_payload_byte(idx_local));
        seq.push(tt(Op::LetSet {
            idx: B0,
            ty: IrType::I32,
        }));
        // if b0 < 0x80 { cp = b0; width = 1 }
        seq.push(tt(Op::LetGet {
            idx: B0,
            ty: IrType::I32,
        }));
        seq.push(tt(Op::ConstI32(0x80)));
        seq.push(tt(Op::Lt(IrType::I32)));
        seq.push(tt(Op::If {
            result_ty: IrType::I32,
            then_body: vec![
                tt(Op::LetGet {
                    idx: B0,
                    ty: IrType::I32,
                }),
                tt(Op::LetSet {
                    idx: CP,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(1)),
                tt(Op::LetSet {
                    idx: WIDTH,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(0)),
            ],
            else_body: {
                let mut e: Vec<TaggedOp> = Vec::new();
                // if b0 < 0xE0 { 2-byte }
                e.push(tt(Op::LetGet {
                    idx: B0,
                    ty: IrType::I32,
                }));
                e.push(tt(Op::ConstI32(0xE0)));
                e.push(tt(Op::Lt(IrType::I32)));
                e.push(tt(Op::If {
                    result_ty: IrType::I32,
                    then_body: {
                        let mut t: Vec<TaggedOp> = Vec::new();
                        t.extend(load_payload_byte_off(idx_local, 1));
                        t.push(tt(Op::LetSet {
                            idx: B1,
                            ty: IrType::I32,
                        }));
                        // cp = (b0 & 0x1F) * 64 + (b1 & 0x3F)
                        t.push(tt(Op::LetGet {
                            idx: B0,
                            ty: IrType::I32,
                        }));
                        t.push(tt(Op::ConstI32(0x1F)));
                        t.push(tt(Op::BitAnd(IrType::I32)));
                        t.push(tt(Op::ConstI32(64)));
                        t.push(tt(Op::Mul(IrType::I32)));
                        t.push(tt(Op::LetGet {
                            idx: B1,
                            ty: IrType::I32,
                        }));
                        t.push(tt(Op::ConstI32(0x3F)));
                        t.push(tt(Op::BitAnd(IrType::I32)));
                        t.push(tt(Op::Add(IrType::I32)));
                        t.push(tt(Op::LetSet {
                            idx: CP,
                            ty: IrType::I32,
                        }));
                        t.push(tt(Op::ConstI32(2)));
                        t.push(tt(Op::LetSet {
                            idx: WIDTH,
                            ty: IrType::I32,
                        }));
                        t.push(tt(Op::ConstI32(0)));
                        t
                    },
                    else_body: {
                        let mut e2: Vec<TaggedOp> = Vec::new();
                        // if b0 < 0xF0 { 3-byte }
                        e2.push(tt(Op::LetGet {
                            idx: B0,
                            ty: IrType::I32,
                        }));
                        e2.push(tt(Op::ConstI32(0xF0)));
                        e2.push(tt(Op::Lt(IrType::I32)));
                        e2.push(tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: {
                                let mut t: Vec<TaggedOp> = Vec::new();
                                t.extend(load_payload_byte_off(idx_local, 1));
                                t.push(tt(Op::LetSet {
                                    idx: B1,
                                    ty: IrType::I32,
                                }));
                                t.extend(load_payload_byte_off(idx_local, 2));
                                t.push(tt(Op::LetSet {
                                    idx: B2,
                                    ty: IrType::I32,
                                }));
                                // cp = (b0 & 0x0F)*4096 + (b1 & 0x3F)*64 + (b2 & 0x3F)
                                t.push(tt(Op::LetGet {
                                    idx: B0,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0x0F)));
                                t.push(tt(Op::BitAnd(IrType::I32)));
                                t.push(tt(Op::ConstI32(4096)));
                                t.push(tt(Op::Mul(IrType::I32)));
                                t.push(tt(Op::LetGet {
                                    idx: B1,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0x3F)));
                                t.push(tt(Op::BitAnd(IrType::I32)));
                                t.push(tt(Op::ConstI32(64)));
                                t.push(tt(Op::Mul(IrType::I32)));
                                t.push(tt(Op::Add(IrType::I32)));
                                t.push(tt(Op::LetGet {
                                    idx: B2,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0x3F)));
                                t.push(tt(Op::BitAnd(IrType::I32)));
                                t.push(tt(Op::Add(IrType::I32)));
                                t.push(tt(Op::LetSet {
                                    idx: CP,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(3)));
                                t.push(tt(Op::LetSet {
                                    idx: WIDTH,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0)));
                                t
                            },
                            else_body: {
                                // 4-byte (0xF0..0xF7 assumed; defensive
                                // tail when the byte is beyond 0xF7
                                // collapses to the 4-byte arm and lets
                                // the scan move past it).
                                let mut t: Vec<TaggedOp> = Vec::new();
                                t.extend(load_payload_byte_off(idx_local, 1));
                                t.push(tt(Op::LetSet {
                                    idx: B1,
                                    ty: IrType::I32,
                                }));
                                t.extend(load_payload_byte_off(idx_local, 2));
                                t.push(tt(Op::LetSet {
                                    idx: B2,
                                    ty: IrType::I32,
                                }));
                                t.extend(load_payload_byte_off(idx_local, 3));
                                t.push(tt(Op::LetSet {
                                    idx: B3,
                                    ty: IrType::I32,
                                }));
                                // cp = (b0 & 0x07)*262144 + (b1 & 0x3F)*4096
                                //    + (b2 & 0x3F)*64 + (b3 & 0x3F)
                                t.push(tt(Op::LetGet {
                                    idx: B0,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0x07)));
                                t.push(tt(Op::BitAnd(IrType::I32)));
                                t.push(tt(Op::ConstI32(262144)));
                                t.push(tt(Op::Mul(IrType::I32)));
                                t.push(tt(Op::LetGet {
                                    idx: B1,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0x3F)));
                                t.push(tt(Op::BitAnd(IrType::I32)));
                                t.push(tt(Op::ConstI32(4096)));
                                t.push(tt(Op::Mul(IrType::I32)));
                                t.push(tt(Op::Add(IrType::I32)));
                                t.push(tt(Op::LetGet {
                                    idx: B2,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0x3F)));
                                t.push(tt(Op::BitAnd(IrType::I32)));
                                t.push(tt(Op::ConstI32(64)));
                                t.push(tt(Op::Mul(IrType::I32)));
                                t.push(tt(Op::Add(IrType::I32)));
                                t.push(tt(Op::LetGet {
                                    idx: B3,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0x3F)));
                                t.push(tt(Op::BitAnd(IrType::I32)));
                                t.push(tt(Op::Add(IrType::I32)));
                                t.push(tt(Op::LetSet {
                                    idx: CP,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(4)));
                                t.push(tt(Op::LetSet {
                                    idx: WIDTH,
                                    ty: IrType::I32,
                                }));
                                t.push(tt(Op::ConstI32(0)));
                                t
                            },
                        }));
                        e2
                    },
                }));
                e
            },
        }));
        seq.push(tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }));
        seq
    };

    // Reverse UTF-8 decode terminating at the byte just before
    // `back_idx_local`. Walks backwards by up to 4 bytes until a byte
    // whose top two bits != `10` appears (that's the lead byte of the
    // preceding codepoint). Writes CP and WIDTH; updates `back_idx_local`
    // to point at the lead byte of the decoded codepoint (so a
    // subsequent iteration can keep walking left from `back_idx_local`).
    //
    // Strategy: store the candidate lead offset in START, decrement
    // `back_idx_local` to start scanning from `back - 1`, then probe
    // up to 4 leftward bytes. On a hit, set `back = start` and forward-
    // decode from there to fill CP/WIDTH. Forward-decoding the one
    // codepoint at the freshly-located start guarantees the cp value
    // matches the canonical 4-arm decode shape — re-using the same
    // forward decode keeps the two paths byte-identical.
    let reverse_decode = |back_idx_local: u32| -> Vec<TaggedOp> {
        let mut seq: Vec<TaggedOp> = Vec::new();
        // start = back - 1
        seq.push(tt(Op::LetGet {
            idx: back_idx_local,
            ty: IrType::I32,
        }));
        seq.push(tt(Op::ConstI32(1)));
        seq.push(tt(Op::Sub(IrType::I32)));
        seq.push(tt(Op::LetSet {
            idx: START,
            ty: IrType::I32,
        }));
        // probe leftwards: while (byte at start) & 0xC0 == 0x80 and we
        // haven't walked past 3 bytes already, start -= 1. Block + Br
        // shape so we can break early on a lead byte.
        seq.push(tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: vec![
                    // if start < 0 || (back - start) > 4 { br 1 }
                    // i.e. we've gone too far — give up and decode a
                    // single byte. (Shouldn't happen on valid UTF-8.)
                    tt(Op::LetGet {
                        idx: START,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(0)),
                    tt(Op::Lt(IrType::I32)),
                    tt(Op::BrIf { label_depth: 1 }),
                    // Inspect byte at START.
                    tt(Op::LocalGet(0)),
                    tt(Op::ConstI32(4)),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LetGet {
                        idx: START,
                        ty: IrType::I32,
                    }),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LoadI8UAtAbsolute { offset: 0 }),
                    tt(Op::LetSet {
                        idx: B0,
                        ty: IrType::I32,
                    }),
                    // is_continuation = (b0 & 0xC0) == 0x80
                    tt(Op::LetGet {
                        idx: B0,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(0xC0)),
                    tt(Op::BitAnd(IrType::I32)),
                    tt(Op::ConstI32(0x80)),
                    tt(Op::Eq(IrType::I32)),
                    tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
                            // start -= 1; continue
                            tt(Op::LetGet {
                                idx: START,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(1)),
                            tt(Op::Sub(IrType::I32)),
                            tt(Op::LetSet {
                                idx: START,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(0)),
                        ],
                        else_body: vec![
                            // Hit a lead byte — break.
                            tt(Op::Br { label_depth: 2 }),
                            tt(Op::ConstI32(0)),
                        ],
                    }),
                    tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }),
                    // Safety net: bound the probe to 4 iterations. If
                    // we've stepped 4 bytes back and still see only
                    // continuations, the input is malformed — give up
                    // and decode whatever single byte sits at start.
                    tt(Op::LetGet {
                        idx: back_idx_local,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: START,
                        ty: IrType::I32,
                    }),
                    tt(Op::Sub(IrType::I32)),
                    tt(Op::ConstI32(4)),
                    tt(Op::Ge(IrType::I32)),
                    tt(Op::BrIf { label_depth: 1 }),
                    tt(Op::Br { label_depth: 0 }),
                ],
            })],
        }));
        // Clamp start to >= 0 in case the safety nets fired.
        seq.push(tt(Op::LetGet {
            idx: START,
            ty: IrType::I32,
        }));
        seq.push(tt(Op::ConstI32(0)));
        seq.push(tt(Op::Lt(IrType::I32)));
        seq.push(tt(Op::If {
            result_ty: IrType::I32,
            then_body: vec![
                tt(Op::ConstI32(0)),
                tt(Op::LetSet {
                    idx: START,
                    ty: IrType::I32,
                }),
                tt(Op::ConstI32(0)),
            ],
            else_body: vec![tt(Op::ConstI32(0))],
        }));
        seq.push(tt(Op::LetSet {
            idx: SINK,
            ty: IrType::I32,
        }));
        // Forward-decode one codepoint from START to fill CP / WIDTH.
        seq.extend(forward_decode(START));
        // back = start (caller continues leftwards from here).
        seq.push(tt(Op::LetGet {
            idx: START,
            ty: IrType::I32,
        }));
        seq.push(tt(Op::LetSet {
            idx: back_idx_local,
            ty: IrType::I32,
        }));
        seq
    };

    StdlibFunction {
        name: "__final_sigma_check",
        params: vec![IrType::I32, IrType::I32, IrType::I32, IrType::I32],
        ret: IrType::I32,
        body: vec![
            // s_len = i32.load(s_ptr + 0)
            tt(Op::LocalGet(0)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: S_LEN,
                ty: IrType::I32,
            }),
            // result = 0 (default: not final)
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: RESULT,
                ty: IrType::I32,
            }),
            // seen_cased_before = 0
            tt(Op::ConstI32(0)),
            tt(Op::LetSet {
                idx: SEEN_CASED_BEFORE,
                ty: IrType::I32,
            }),
            // back = byte_offset (will scan leftwards)
            tt(Op::LocalGet(1)),
            tt(Op::LetSet {
                idx: BACK,
                ty: IrType::I32,
            }),
            // ----- LEFT SCAN -----
            // while back > 0 { reverse-decode one cp; if cased: set
            // seen_cased_before=1 and break; if !ignorable: break; else
            // continue. } The scan requires at least one cased cp
            // before the sigma (modulo ignorables) for the form to be
            // final.
            tt(Op::Block {
                result_ty: None,
                body: vec![tt(Op::Loop {
                    result_ty: None,
                    body: {
                        let mut v: Vec<TaggedOp> = Vec::new();
                        // exit if back == 0 (or < 0)
                        v.push(tt(Op::LetGet {
                            idx: BACK,
                            ty: IrType::I32,
                        }));
                        v.push(tt(Op::ConstI32(1)));
                        v.push(tt(Op::Lt(IrType::I32)));
                        v.push(tt(Op::BrIf { label_depth: 1 }));
                        // reverse-decode: writes CP, WIDTH, and resets
                        // back to the lead byte of the decoded cp.
                        v.extend(reverse_decode(BACK));
                        // is_cased = range_lookup(CP, cased_addr)
                        v.extend(range_lookup(CP, 2));
                        v.push(tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                tt(Op::ConstI32(1)),
                                tt(Op::LetSet {
                                    idx: SEEN_CASED_BEFORE,
                                    ty: IrType::I32,
                                }),
                                // br 2 exits the enclosing block.
                                tt(Op::Br { label_depth: 2 }),
                                tt(Op::ConstI32(0)),
                            ],
                            else_body: vec![tt(Op::ConstI32(0))],
                        }));
                        v.push(tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }));
                        // is_ignorable = range_lookup(CP, ignorable_addr)
                        v.extend(range_lookup(CP, 3));
                        v.push(tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![tt(Op::ConstI32(0))],
                            else_body: vec![
                                // Non-cased, non-ignorable — break left
                                // scan (without flipping seen flag).
                                tt(Op::Br { label_depth: 2 }),
                                tt(Op::ConstI32(0)),
                            ],
                        }));
                        v.push(tt(Op::LetSet {
                            idx: SINK,
                            ty: IrType::I32,
                        }));
                        // continue.
                        v.push(tt(Op::Br { label_depth: 0 }));
                        v
                    },
                })],
            }),
            // If we never saw a cased cp on the left, sigma is not
            // final — return 0 directly.
            tt(Op::LetGet {
                idx: SEEN_CASED_BEFORE,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(0)),
            tt(Op::Eq(IrType::I32)),
            tt(Op::If {
                result_ty: IrType::I32,
                then_body: vec![
                    tt(Op::ConstI32(0)),
                    tt(Op::LetSet {
                        idx: RESULT,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(0)),
                ],
                else_body: vec![tt(Op::ConstI32(0))],
            }),
            tt(Op::LetSet {
                idx: SINK,
                ty: IrType::I32,
            }),
            // Only continue the right scan when seen_cased_before == 1.
            tt(Op::LetGet {
                idx: SEEN_CASED_BEFORE,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(1)),
            tt(Op::Eq(IrType::I32)),
            tt(Op::If {
                result_ty: IrType::I32,
                then_body: {
                    let mut t: Vec<TaggedOp> = Vec::new();
                    // fwd = byte_offset + 2 (Σ is 0xCE 0xA3 = 2 bytes)
                    t.push(tt(Op::LocalGet(1)));
                    t.push(tt(Op::ConstI32(2)));
                    t.push(tt(Op::Add(IrType::I32)));
                    t.push(tt(Op::LetSet {
                        idx: FWD,
                        ty: IrType::I32,
                    }));
                    // result = 1 (default until proven non-final by a
                    // cased cp on the right; falls back to 0 only via
                    // the explicit override below).
                    t.push(tt(Op::ConstI32(1)));
                    t.push(tt(Op::LetSet {
                        idx: RESULT,
                        ty: IrType::I32,
                    }));
                    // ----- RIGHT SCAN -----
                    t.push(tt(Op::Block {
                        result_ty: None,
                        body: vec![tt(Op::Loop {
                            result_ty: None,
                            body: {
                                let mut v: Vec<TaggedOp> = Vec::new();
                                // exit if fwd >= s_len
                                v.push(tt(Op::LetGet {
                                    idx: FWD,
                                    ty: IrType::I32,
                                }));
                                v.push(tt(Op::LetGet {
                                    idx: S_LEN,
                                    ty: IrType::I32,
                                }));
                                v.push(tt(Op::Ge(IrType::I32)));
                                v.push(tt(Op::BrIf { label_depth: 1 }));
                                // forward-decode: writes CP and WIDTH.
                                v.extend(forward_decode(FWD));
                                // is_cased = range_lookup(CP, cased_addr)
                                v.extend(range_lookup(CP, 2));
                                v.push(tt(Op::If {
                                    result_ty: IrType::I32,
                                    then_body: vec![
                                        // Cased cp — sigma is not final.
                                        tt(Op::ConstI32(0)),
                                        tt(Op::LetSet {
                                            idx: RESULT,
                                            ty: IrType::I32,
                                        }),
                                        tt(Op::Br { label_depth: 2 }),
                                        tt(Op::ConstI32(0)),
                                    ],
                                    else_body: vec![tt(Op::ConstI32(0))],
                                }));
                                v.push(tt(Op::LetSet {
                                    idx: SINK,
                                    ty: IrType::I32,
                                }));
                                // is_ignorable = range_lookup(CP, ignorable_addr)
                                v.extend(range_lookup(CP, 3));
                                v.push(tt(Op::If {
                                    result_ty: IrType::I32,
                                    then_body: vec![tt(Op::ConstI32(0))],
                                    else_body: vec![
                                        // Non-cased, non-ignorable —
                                        // word-final. Keep result = 1
                                        // and exit.
                                        tt(Op::Br { label_depth: 2 }),
                                        tt(Op::ConstI32(0)),
                                    ],
                                }));
                                v.push(tt(Op::LetSet {
                                    idx: SINK,
                                    ty: IrType::I32,
                                }));
                                // fwd += width
                                v.push(tt(Op::LetGet {
                                    idx: FWD,
                                    ty: IrType::I32,
                                }));
                                v.push(tt(Op::LetGet {
                                    idx: WIDTH,
                                    ty: IrType::I32,
                                }));
                                v.push(tt(Op::Add(IrType::I32)));
                                v.push(tt(Op::LetSet {
                                    idx: FWD,
                                    ty: IrType::I32,
                                }));
                                v.push(tt(Op::Br { label_depth: 0 }));
                                v
                            },
                        })],
                    }));
                    t.push(tt(Op::ConstI32(0)));
                    t
                },
                else_body: vec![tt(Op::ConstI32(0))],
            }),
            tt(Op::LetSet {
                idx: SINK,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: RESULT,
                ty: IrType::I32,
            }),
            tt(Op::Return),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
