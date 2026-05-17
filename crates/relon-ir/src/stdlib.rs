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

/// Hand-written body for `upper(s: String) -> String`. Returns a
/// fresh scratch record with the ASCII lowercase letters folded to
/// uppercase; bytes outside `0x61..=0x7a` (`'a'..='z'`) pass through
/// unchanged, so multi-byte UTF-8 sequences are preserved bit-for-bit
/// even though they don't get codepoint-aware case folding.
fn upper_string() -> StdlibFunction {
    case_fold_body("upper", /* to_upper = */ true)
}

/// Mirror of [`upper_string`] folding `'A'..='Z'` to lowercase.
fn lower_string() -> StdlibFunction {
    case_fold_body("lower", /* to_upper = */ false)
}

/// Shared body generator for `upper` / `lower`. The `to_upper`
/// boolean flips the byte range we touch (`0x61..=0x7a` for upper,
/// `0x41..=0x5a` for lower) and the delta applied (`-0x20` for
/// upper, `+0x20` for lower — flipping bit 5 of the ASCII codepoint).
///
/// Algorithm: alloc a scratch record sized like the input, write the
/// length prefix, then walk i = 0..len with a single `Block { Loop }`
/// stack-machine pattern, reading each byte with `i32.load8_u`, case-
/// folding via an `if (b in [lo, hi]) { b + delta } else { b }`
/// branch, and storing the result with `i32.store8`.
///
/// Locals:
///   * 0 — `len:  I32`
///   * 1 — `base: I32`
///   * 2 — `i:    I32`
fn case_fold_body(name: &'static str, to_upper: bool) -> StdlibFunction {
    const LEN: u32 = 0;
    const BASE: u32 = 1;
    const I: u32 = 2;
    let (lo, hi, delta): (i32, i32, i32) = if to_upper {
        (0x61, 0x7a, -0x20)
    } else {
        (0x41, 0x5a, 0x20)
    };
    StdlibFunction {
        name,
        params: vec![IrType::String],
        ret: IrType::String,
        body: vec![
            // len = load_i32(s, 0)
            tt(Op::LocalGet(0)),
            tt(Op::LoadI32AtAbsolute { offset: 0 }),
            tt(Op::LetSet {
                idx: LEN,
                ty: IrType::I32,
            }),
            // base = alloc_scratch_dyn(len + 4)
            tt(Op::LetGet {
                idx: LEN,
                ty: IrType::I32,
            }),
            tt(Op::ConstI32(4)),
            tt(Op::Add(IrType::I32)),
            tt(Op::AllocScratchDyn),
            tt(Op::LetSet {
                idx: BASE,
                ty: IrType::I32,
            }),
            // store header: i32.store(base + 0, len)
            tt(Op::LetGet {
                idx: BASE,
                ty: IrType::I32,
            }),
            tt(Op::LetGet {
                idx: LEN,
                ty: IrType::I32,
            }),
            tt(Op::StoreI32AtAbsolute { offset: 0 }),
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
                        // if i >= len { br 1 } (exit the outer block)
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::LetGet {
                            idx: LEN,
                            ty: IrType::I32,
                        }),
                        tt(Op::Ge(IrType::I32)),
                        tt(Op::BrIf { label_depth: 1 }),
                        // dst = base + 4 + i (pushed first for the store)
                        tt(Op::LetGet {
                            idx: BASE,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(4)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::Add(IrType::I32)),
                        // b = i32.load8_u(s + 4 + i)
                        tt(Op::LocalGet(0)),
                        tt(Op::ConstI32(4)),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LetGet {
                            idx: I,
                            ty: IrType::I32,
                        }),
                        tt(Op::Add(IrType::I32)),
                        tt(Op::LoadI8UAtAbsolute { offset: 0 }),
                        // folded = if b >= lo && b <= hi { b + delta } else { b }
                        //
                        // We use a nested If on a single condition
                        // (`b >= lo && b <= hi`) computed as
                        // `(b - lo) <= (hi - lo)` so the branch
                        // collapses to one wasm `if`. We need `b`
                        // again on both arms, so stash it into the
                        // STORE_TMP via tee-ish pattern: actually we
                        // recompute b from the load. The chain is:
                        //   stack: [dst, b]
                        //   tee to a tmp ⇒ we don't have a tee op.
                        // Simpler: spill b into a fresh let-local
                        // (`B`, idx 3) before the branch so each arm
                        // can use it.
                        tt(Op::LetSet {
                            idx: 3,
                            ty: IrType::I32,
                        }),
                        // cond = (b - lo) <= (hi - lo)
                        tt(Op::LetGet {
                            idx: 3,
                            ty: IrType::I32,
                        }),
                        tt(Op::ConstI32(lo)),
                        tt(Op::Sub(IrType::I32)),
                        tt(Op::ConstI32(hi - lo)),
                        tt(Op::Le(IrType::I32)),
                        // if cond { b + delta } else { b }
                        tt(Op::If {
                            result_ty: IrType::I32,
                            then_body: vec![
                                tt(Op::LetGet {
                                    idx: 3,
                                    ty: IrType::I32,
                                }),
                                tt(Op::ConstI32(delta)),
                                tt(Op::Add(IrType::I32)),
                            ],
                            else_body: vec![tt(Op::LetGet {
                                idx: 3,
                                ty: IrType::I32,
                            })],
                        }),
                        // i32.store8(dst, folded)  (dst is the first
                        // operand pushed at the top of this iter)
                        tt(Op::StoreI8AtAbsolute { offset: 0 }),
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
                        // br 0 — continue the inner loop
                        tt(Op::Br { label_depth: 0 }),
                    ],
                })],
            }),
            // return base
            tt(Op::LetGet {
                idx: BASE,
                ty: IrType::I32,
            }),
            tt(Op::Return),
        ],
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
}
