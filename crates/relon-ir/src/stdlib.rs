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
        // FOLDED = __casefold_lookup(cp, upper-or-lower-table-addr)
        vec![
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
            tt(Op::ConstI32(0)),
        ]
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
    // decode → fold → encode
    loop_body.extend(decode_seq);
    loop_body.extend(fold_seq);
    loop_body.extend(encode_seq);
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

    StdlibFunction {
        name,
        params: vec![IrType::String],
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
        (IrType::String, "substring") => stdlib_function_index("substring"),
        (IrType::String, "starts_with") => stdlib_function_index("starts_with"),
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
