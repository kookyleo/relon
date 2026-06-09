//! Body builders for the "non-Unicode" stdlib bodies — fixed-shape
//! length / arithmetic / predicate / sequence helpers plus the shared
//! [`tt`] op-tag helper that every body builder uses.
//!
//! Bodies in this file:
//!   * `length(String)`, `list_*_length(List<_>)`.
//!   * `abs(Int)`, `min(Int, Int)`, `max(Int, Int)`.
//!   * `is_empty(String)`.
//!   * `concat(String, String)`.
//!   * `substring(String, Int, Int)`, `starts_with(String, String)`,
//!     `contains(String, String)`.
//!   * `list_int_sum / max / map / filter / fold`.
//!
//! Case-fold / locale-aware / title bodies live in
//! [`super::case_fold`]; Unicode normalization bodies live in
//! [`super::normalization`]. The ordered list pinning every entry's
//! wasm slot lives in [`super::registry::builtin_stdlib`].

use crate::ir::{F64UnaryOp, IrType, Op, TaggedOp, TrapKind};
use relon_parser::TokenRange;

use super::signatures::StdlibFunction;

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
pub(super) fn length_string_to_int() -> StdlibFunction {
    StdlibFunction::new("length", vec![IrType::String], IrType::I64, || {
        let range = TokenRange::default();
        vec![
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
        ]
    })
}

/// Hand-written body for `list_int_length(xs: List<Int>) -> Int`.
///
/// `List<Int>` shares the leading `[len: u32 LE]` record header with
/// `String` (the record continues with a 4-byte pad and the i64
/// elements), so the body is byte-identical to the `length` String
/// body — just typed against the `ListInt` slot at the IR level so
/// lowering can dispatch on the receiver's IR type.
pub(super) fn list_int_length_to_int() -> StdlibFunction {
    StdlibFunction::new(
        "list_int_length",
        vec![IrType::ListInt],
        IrType::I64,
        || {
            let range = TokenRange::default();
            vec![
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
            ]
        },
    )
}

/// Phase 10-c length bodies for the new list types.
///
/// Every list record carries the same `[len: u32 LE]` prefix at offset
/// 0 (the trailing payload differs by element type but the header
/// shape is uniform). So the body is byte-identical to
/// [`list_int_length_to_int`] — only the param type tag changes, which
/// drives the IR-level dispatch in [`stdlib_method_index`].
pub(super) fn list_float_length() -> StdlibFunction {
    StdlibFunction::new(
        "list_float_length",
        vec![IrType::ListFloat],
        IrType::I64,
        list_length_record_header_body,
    )
}

pub(super) fn list_bool_length() -> StdlibFunction {
    StdlibFunction::new(
        "list_bool_length",
        vec![IrType::ListBool],
        IrType::I64,
        list_length_record_header_body,
    )
}

pub(super) fn list_string_length() -> StdlibFunction {
    StdlibFunction::new(
        "list_string_length",
        vec![IrType::ListString],
        IrType::I64,
        list_length_record_header_body,
    )
}

pub(super) fn list_schema_length() -> StdlibFunction {
    StdlibFunction::new(
        "list_schema_length",
        vec![IrType::ListSchema],
        IrType::I64,
        list_length_record_header_body,
    )
}

pub(super) fn list_list_length() -> StdlibFunction {
    StdlibFunction::new(
        "list_list_length",
        vec![IrType::ListList],
        IrType::I64,
        list_length_record_header_body,
    )
}

/// Shared body for every `list_<T>_length` shape — they all read the
/// leading `u32 LE` length prefix of the record header. Hoisted so the
/// lazy-build cache can reuse the same builder pointer across the
/// four list types (each entry still has its own `OnceLock`, but the
/// op-stream produced is byte-identical).
fn list_length_record_header_body() -> Vec<TaggedOp> {
    let range = TokenRange::default();
    vec![
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
    ]
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
pub(super) fn abs_int() -> StdlibFunction {
    StdlibFunction::new("abs", vec![IrType::I64], IrType::I64, || {
        let range = TokenRange::default();
        vec![
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
        ]
    })
}

/// Wave R7: `abs(x: Float) -> Float`.
///
/// The bundled `abs` (index 2) is `Int -> Int`; this is the Float
/// overload reached when the speculative scalar-math peephole sees a
/// `Float` argument. Body is a single `F64Unary(Abs)` (`llvm.fabs.f64`
/// / cranelift `fabs` / wasm `f64.abs`) — clears the sign bit, so
/// `abs(-0.0) == 0.0` and `abs(NaN)` stays `NaN`, matching the
/// tree-walk oracle (`f64::abs`).
pub(super) fn abs_float() -> StdlibFunction {
    StdlibFunction::new("abs_float", vec![IrType::F64], IrType::F64, || {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::F64Unary(F64UnaryOp::Abs)),
            tt(Op::Return),
        ]
    })
}

/// Wave R7: `floor(x: Float) -> Int`.
///
/// `F64Unary(Floor)` then `F64ToI64Sat`, reproducing the tree-walk
/// oracle `f64::floor() as i64` (round toward -inf, then Rust's
/// saturating float→int cast). Result type is `Int`, matching the
/// oracle (`MathFloor` returns `Value::Int`).
pub(super) fn floor_float_to_int() -> StdlibFunction {
    StdlibFunction::new("floor", vec![IrType::F64], IrType::I64, || {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::F64Unary(F64UnaryOp::Floor)),
            tt(Op::F64ToI64Sat),
            tt(Op::Return),
        ]
    })
}

/// Wave R7: `ceil(x: Float) -> Int`.
///
/// `F64Unary(Ceil)` then `F64ToI64Sat` — oracle `f64::ceil() as i64`.
pub(super) fn ceil_float_to_int() -> StdlibFunction {
    StdlibFunction::new("ceil", vec![IrType::F64], IrType::I64, || {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::F64Unary(F64UnaryOp::Ceil)),
            tt(Op::F64ToI64Sat),
            tt(Op::Return),
        ]
    })
}

/// Wave R7: `round(x: Float) -> Int`.
///
/// `F64Unary(Nearest)` (round-to-nearest, ties-to-even) then
/// `F64ToI64Sat` — oracle `f64::round_ties_even() as i64`. The IEEE
/// default ties-to-even is deliberate: the tree-walk `MathRound` uses
/// `round_ties_even`, NOT `round` (ties-away). cranelift `nearest` /
/// wasm `f64.nearest` / LLVM `llvm.roundeven.f64` all implement
/// ties-to-even.
pub(super) fn round_float_to_int() -> StdlibFunction {
    StdlibFunction::new("round", vec![IrType::F64], IrType::I64, || {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::F64Unary(F64UnaryOp::Nearest)),
            tt(Op::F64ToI64Sat),
            tt(Op::Return),
        ]
    })
}

/// Wave R7: `sqrt(x: Float) -> Float`.
///
/// Single `F64Unary(Sqrt)` — oracle `<f64>.sqrt()`. A negative operand
/// yields `NaN` per IEEE-754 (the oracle does NOT error), and the
/// native `sqrt` instruction on every backend produces the same NaN
/// bit pattern.
pub(super) fn sqrt_float() -> StdlibFunction {
    StdlibFunction::new("sqrt", vec![IrType::F64], IrType::F64, || {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::F64Unary(F64UnaryOp::Sqrt)),
            tt(Op::Return),
        ]
    })
}

/// Hand-written body for `min(a: Int, b: Int) -> Int`.
///
/// Stack arrangement: push `[a, b, a < b]` so wasm `select` returns
/// `a` when `a < b` and `b` otherwise.
pub(super) fn min_int() -> StdlibFunction {
    StdlibFunction::new("min", vec![IrType::I64, IrType::I64], IrType::I64, || {
        let range = TokenRange::default();
        vec![
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
        ]
    })
}

/// Hand-written body for `max(a: Int, b: Int) -> Int`.
///
/// Mirror of [`min_int`] with the comparison flipped.
pub(super) fn max_int() -> StdlibFunction {
    StdlibFunction::new("max", vec![IrType::I64, IrType::I64], IrType::I64, || {
        let range = TokenRange::default();
        vec![
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
        ]
    })
}

/// Hand-written body for `is_empty(s: String) -> Bool`.
///
/// Reads the record's `u32 LE` length prefix via [`Op::ReadStringLen`]
/// (which widens to `I64`), compares against the i64 constant zero,
/// and returns the `Bool` result of the equality op directly.
pub(super) fn is_empty_string() -> StdlibFunction {
    StdlibFunction::new("is_empty", vec![IrType::String], IrType::Bool, || {
        let range = TokenRange::default();
        vec![
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
        ]
    })
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
pub(super) fn tt(op: Op) -> TaggedOp {
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
pub(super) fn concat_string_string() -> StdlibFunction {
    StdlibFunction::new(
        "concat",
        vec![IrType::String, IrType::String],
        IrType::String,
        concat_string_string_body,
    )
}

fn concat_string_string_body() -> Vec<TaggedOp> {
    const LEN_A: u32 = 0;
    const LEN_B: u32 = 1;
    const BASE: u32 = 2;
    vec![
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
    ]
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
pub(super) fn substring_string() -> StdlibFunction {
    StdlibFunction::new(
        "substring",
        vec![IrType::String, IrType::I64, IrType::I64],
        IrType::String,
        substring_string_body,
    )
}

fn substring_string_body() -> Vec<TaggedOp> {
    const S_LEN: u32 = 0;
    const START_I32: u32 = 1;
    const LEN_I32: u32 = 2;
    const BASE: u32 = 3;
    vec![
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
    ]
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
pub(super) fn starts_with_string() -> StdlibFunction {
    StdlibFunction::new(
        "starts_with",
        vec![IrType::String, IrType::String],
        IrType::Bool,
        starts_with_string_body,
    )
}

fn starts_with_string_body() -> Vec<TaggedOp> {
    const S_LEN: u32 = 0;
    const P_LEN: u32 = 1;
    const I: u32 = 2;
    const ACC: u32 = 3;
    vec![
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
    ]
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
pub(super) fn contains_string() -> StdlibFunction {
    StdlibFunction::new(
        "contains",
        vec![IrType::String, IrType::String],
        IrType::Bool,
        contains_string_body,
    )
}

/// Registry entry for `glob_match(s: String, pattern: String) -> Bool`.
///
/// Tier-2 LuaJIT-pattern-subset glob matcher: `*` / `?` / `[set]` /
/// `[^set]` plus `\`-escapes, anchored on both ends, char-by-char
/// Unicode, case-sensitive. The matcher itself lives in
/// [`crate::glob::glob_match`].
///
/// ## Why the body traps
///
/// The body builder emits a single `Op::Trap` so any backend that
/// inlines the IR body literally (`wasm` AOT, future bytecode
/// inliner) fails loudly rather than silently returning `false`. The
/// production lowering paths route around the trap:
///
/// * **Tree-walker** registers its own Rust impl in
///   `relon-evaluator::stdlib::StringGlobMatch` and never reads the
///   IR body.
/// * **Cranelift native codegen** intercepts `Op::Call { fn_index ==
///   STDLIB_IDX_GLOB_MATCH }` in `emit_call_stdlib` and emits an
///   indirect call to the `RelonGlobMatch` vtable slot instead of
///   inlining the body.
/// * **Bytecode / trace-JIT** treat the call as an opaque stdlib
///   dispatch — they fall back to the tree-walker bridge until the
///   inline-lowering follow-up phase lands.
///
/// The IR body still occupies a fixed slot (`STDLIB_IDX_GLOB_MATCH =
/// 37`) so the wire format stays stable: any module compiled against
/// this build references slot 37, and future bundle reordering would
/// surface via the `stdlib_index_consistency` drift guard.
pub(super) fn glob_match_string() -> StdlibFunction {
    StdlibFunction::new(
        "glob_match",
        vec![IrType::String, IrType::String],
        IrType::Bool,
        glob_match_string_body,
    )
}

fn glob_match_string_body() -> Vec<TaggedOp> {
    // The body is unreachable on the supported lowering paths (see
    // the registry doc-comment above for the routing matrix). We
    // still need to push the declared return type onto the virtual
    // stack so the wasm verifier / cranelift `emit_body` pre-check
    // accepts the function shape: `Op::Trap` claims `[] -> [...]`,
    // which lets us follow it with a sentinel `ConstBool` that's
    // verifier-visible but dynamically unreachable.
    vec![
        tt(Op::Trap {
            kind: TrapKind::IndexOutOfBounds,
        }),
        tt(Op::ConstBool(false)),
        tt(Op::Return),
    ]
}

fn contains_string_body() -> Vec<TaggedOp> {
    const S_LEN: u32 = 0;
    const P_LEN: u32 = 1;
    const LAST_START: u32 = 2;
    const I: u32 = 3;
    const J: u32 = 4;
    const MISMATCH: u32 = 5;
    const FOUND: u32 = 6;
    vec![
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
    ]
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
pub(super) fn list_int_sum() -> StdlibFunction {
    StdlibFunction::new(
        "list_int_sum",
        vec![IrType::ListInt],
        IrType::I64,
        list_int_sum_body,
    )
}

fn list_int_sum_body() -> Vec<TaggedOp> {
    const N: u32 = 0;
    const I: u32 = 1;
    const ACC: u32 = 2;
    const PAYLOAD: u32 = 3;
    vec![
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
    ]
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
pub(super) fn list_int_max() -> StdlibFunction {
    StdlibFunction::new(
        "list_int_max",
        vec![IrType::ListInt],
        IrType::I64,
        list_int_max_body,
    )
}

fn list_int_max_body() -> Vec<TaggedOp> {
    const N: u32 = 0;
    const I: u32 = 1;
    const ACC: u32 = 2;
    const PAYLOAD: u32 = 3;
    const VAL: u32 = 4;
    vec![
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
    ]
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
pub(super) fn list_int_map() -> StdlibFunction {
    StdlibFunction::new(
        "list_int_map",
        vec![IrType::ListInt, IrType::Closure],
        IrType::ListInt,
        list_int_map_body,
    )
}

fn list_int_map_body() -> Vec<TaggedOp> {
    const N: u32 = 0;
    const I: u32 = 1;
    const SRC_PAYLOAD: u32 = 2;
    const DST_PAYLOAD: u32 = 3;
    const NEW_BASE: u32 = 4;
    vec![
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
    ]
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
pub(super) fn list_int_filter() -> StdlibFunction {
    StdlibFunction::new(
        "list_int_filter",
        vec![IrType::ListInt, IrType::Closure],
        IrType::ListInt,
        list_int_filter_body,
    )
}

fn list_int_filter_body() -> Vec<TaggedOp> {
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
    vec![
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
    ]
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
pub(super) fn list_int_fold() -> StdlibFunction {
    // Param ordering matches the user-facing surface:
    //   `xs.fold(init, |acc, x| ...)` lowers to
    //   `list_int_fold(xs, init, f)`.
    StdlibFunction::new(
        "list_int_fold",
        vec![IrType::ListInt, IrType::I64, IrType::Closure],
        IrType::I64,
        list_int_fold_body,
    )
}

fn list_int_fold_body() -> Vec<TaggedOp> {
    const N: u32 = 0;
    const I: u32 = 1;
    const SRC_PAYLOAD: u32 = 2;
    const ACC: u32 = 3;
    vec![
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
    ]
}

// =====================================================================
// Wave R3b: typed list higher-order ops over `List<Float>` and the
// element-type-changing numeric `map` shapes (Int<->Float).
//
// Every list record these bodies touch shares the `List<Int>` layout
// (`[len: u32 LE][pad: u32][8-byte elements...]`, payload at
// `(base + 4 + 7) & -8`). `List<Float>` stores each element as the
// raw 8-byte IEEE-754 bit pattern in the same slot, so the only
// difference from the `list_int_*` bodies is which typed load/store
// op moves the element (an `f64.load` / `f64.store` keeps the value in
// an f-register so the per-element closure call sees a Float) and the
// `Op::CallClosure` param / ret type tag. The structural op stream
// (sizing, payload alignment, loop, header writeback) is identical;
// the helpers below take the element-load / element-store / closure
// signature as parameters and emit the shared shape.
//
// String list HOFs (`List<String>` map / filter / reduce, or any map
// whose closure returns a `String`) stay capped: a `List<String>`
// result requires a runtime pointer-array builder with the
// arena-relative handle-slot convention, which is not yet a proven
// four-way substrate. Those surfaces fall through the typed-HOF
// peephole and cap loudly through the existing dispatch-miss path.

/// Shared map body parameterised by the element representation.
///
/// `load_elem` reads element `i` from the source payload (an
/// `Op::LoadI64AtAbsolute` for Int sources, `Op::LoadF64AtAbsolute`
/// for Float sources). `store_elem` writes the closure result into the
/// destination payload (`StoreI64AtAbsolute` for Int results,
/// `StoreF64AtAbsolute` for Float results). `param_ty` / `ret_ty` tag
/// the per-element `Op::CallClosure` so codegen routes the value
/// through the right register class. Byte-identical in structure to
/// [`list_int_map_body`].
fn list_map_body_typed(
    load_elem: fn(u32) -> Op,
    store_elem: fn(u32) -> Op,
    param_ty: IrType,
    ret_ty: IrType,
) -> Vec<TaggedOp> {
    const N: u32 = 0;
    const I: u32 = 1;
    const SRC_PAYLOAD: u32 = 2;
    const DST_PAYLOAD: u32 = 3;
    const NEW_BASE: u32 = 4;
    vec![
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
        tt(Op::Block {
            result_ty: None,
            body: vec![tt(Op::Loop {
                result_ty: None,
                body: vec![
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
                    // dst_addr = dst_payload + i * 8
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
                    // push the source element: load(src_payload + i*8)
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
                    tt(load_elem(0)),
                    tt(Op::CallClosure {
                        param_tys: vec![param_ty],
                        ret_ty,
                    }),
                    // store: stack is [dst_addr, result]
                    tt(store_elem(0)),
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
        tt(Op::LetGet {
            idx: NEW_BASE,
            ty: IrType::I32,
        }),
        tt(Op::Return),
    ]
}

/// Shared filter body parameterised by the element representation. The
/// predicate closure always returns `Bool`; only the element load /
/// store op and the closure param type vary. Byte-identical in
/// structure to [`list_int_filter_body`].
fn list_filter_body_typed(
    load_elem: fn(u32) -> Op,
    store_elem: fn(u32) -> Op,
    param_ty: IrType,
    cur_val_ty: IrType,
) -> Vec<TaggedOp> {
    const N: u32 = 0;
    const I: u32 = 1;
    const SRC_PAYLOAD: u32 = 2;
    const DST_PAYLOAD: u32 = 3;
    const NEW_BASE: u32 = 4;
    const OUT_COUNT: u32 = 5;
    const CUR_VAL: u32 = 6;
    const SINK: u32 = 7;
    vec![
        tt(Op::LocalGet(0)),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: N,
            ty: IrType::I32,
        }),
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
        tt(Op::LocalGet(0)),
        tt(Op::ConstI32(4 + 7)),
        tt(Op::Add(IrType::I32)),
        tt(Op::ConstI32(-8)),
        tt(Op::BitAnd(IrType::I32)),
        tt(Op::LetSet {
            idx: SRC_PAYLOAD,
            ty: IrType::I32,
        }),
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
                    // cur_val = load(src_payload + i*8)
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
                    tt(load_elem(0)),
                    tt(Op::LetSet {
                        idx: CUR_VAL,
                        ty: cur_val_ty,
                    }),
                    // closure(cur_val) -> bool
                    tt(Op::LocalGet(1)),
                    tt(Op::LetGet {
                        idx: CUR_VAL,
                        ty: cur_val_ty,
                    }),
                    tt(Op::CallClosure {
                        param_tys: vec![param_ty],
                        ret_ty: IrType::Bool,
                    }),
                    tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
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
                                ty: cur_val_ty,
                            }),
                            tt(store_elem(0)),
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
                            tt(Op::ConstI32(0)),
                        ],
                        else_body: vec![tt(Op::ConstI32(0))],
                    }),
                    tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }),
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
        tt(Op::LetGet {
            idx: NEW_BASE,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: OUT_COUNT,
            ty: IrType::I32,
        }),
        tt(Op::StoreI32AtAbsolute { offset: 0 }),
        tt(Op::LetGet {
            idx: NEW_BASE,
            ty: IrType::I32,
        }),
        tt(Op::Return),
    ]
}

/// Filter a pointer-array list (`List<Enum>`, `List<Option<_>>`,
/// `List<Result<_, _>>`, `List<Schema>`, or nested pointer-list values).
/// The closure receives each element handle as an arena-relative `I32` and
/// the result is a new pointer-array list in scratch.
fn list_pointer_filter_body() -> Vec<TaggedOp> {
    const N: u32 = 0;
    const I: u32 = 1;
    const SRC_PAYLOAD: u32 = 2;
    const DST_PAYLOAD: u32 = 3;
    const NEW_BASE: u32 = 4;
    const OUT_COUNT: u32 = 5;
    const CUR_VAL: u32 = 6;
    const SINK: u32 = 7;
    vec![
        tt(Op::LocalGet(0)),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: N,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(4)),
        tt(Op::LetGet {
            idx: N,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(4)),
        tt(Op::Mul(IrType::I32)),
        tt(Op::Add(IrType::I32)),
        tt(Op::AllocScratchDyn),
        tt(Op::LetSet {
            idx: NEW_BASE,
            ty: IrType::I32,
        }),
        tt(Op::LocalGet(0)),
        tt(Op::ConstI32(4)),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetSet {
            idx: SRC_PAYLOAD,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: NEW_BASE,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(4)),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetSet {
            idx: DST_PAYLOAD,
            ty: IrType::I32,
        }),
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
                    tt(Op::LetGet {
                        idx: SRC_PAYLOAD,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: I,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(4)),
                    tt(Op::Mul(IrType::I32)),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LoadI32AtAbsolute { offset: 0 }),
                    tt(Op::LetSet {
                        idx: CUR_VAL,
                        ty: IrType::I32,
                    }),
                    tt(Op::LocalGet(1)),
                    tt(Op::LetGet {
                        idx: CUR_VAL,
                        ty: IrType::I32,
                    }),
                    tt(Op::CallClosure {
                        param_tys: vec![IrType::I32],
                        ret_ty: IrType::Bool,
                    }),
                    tt(Op::If {
                        result_ty: IrType::I32,
                        then_body: vec![
                            tt(Op::LetGet {
                                idx: DST_PAYLOAD,
                                ty: IrType::I32,
                            }),
                            tt(Op::LetGet {
                                idx: OUT_COUNT,
                                ty: IrType::I32,
                            }),
                            tt(Op::ConstI32(4)),
                            tt(Op::Mul(IrType::I32)),
                            tt(Op::Add(IrType::I32)),
                            tt(Op::LetGet {
                                idx: CUR_VAL,
                                ty: IrType::I32,
                            }),
                            tt(Op::StoreI32AtAbsolute { offset: 0 }),
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
                            tt(Op::ConstI32(0)),
                        ],
                        else_body: vec![tt(Op::ConstI32(0))],
                    }),
                    tt(Op::LetSet {
                        idx: SINK,
                        ty: IrType::I32,
                    }),
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
        tt(Op::LetGet {
            idx: NEW_BASE,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: OUT_COUNT,
            ty: IrType::I32,
        }),
        tt(Op::StoreI32AtAbsolute { offset: 0 }),
        tt(Op::LetGet {
            idx: NEW_BASE,
            ty: IrType::I32,
        }),
        tt(Op::Return),
    ]
}

/// Shared fold body parameterised by the element / accumulator
/// representation (Int -> `I64`, Float -> `F64`). The accumulator and
/// each element share `acc_ty`; the closure is `(acc_ty, acc_ty) ->
/// acc_ty`. Byte-identical in structure to [`list_int_fold_body`].
fn list_fold_body_typed(load_elem: fn(u32) -> Op, acc_ty: IrType) -> Vec<TaggedOp> {
    const N: u32 = 0;
    const I: u32 = 1;
    const SRC_PAYLOAD: u32 = 2;
    const ACC: u32 = 3;
    vec![
        tt(Op::LocalGet(0)),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: N,
            ty: IrType::I32,
        }),
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
            ty: acc_ty,
        }),
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
                    tt(Op::LocalGet(2)),
                    tt(Op::LetGet {
                        idx: ACC,
                        ty: acc_ty,
                    }),
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
                    tt(load_elem(0)),
                    tt(Op::CallClosure {
                        param_tys: vec![acc_ty, acc_ty],
                        ret_ty: acc_ty,
                    }),
                    tt(Op::LetSet {
                        idx: ACC,
                        ty: acc_ty,
                    }),
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
        tt(Op::LetGet {
            idx: ACC,
            ty: acc_ty,
        }),
        tt(Op::Return),
    ]
}

/// `list_float_map(xs: List<Float>, f: Closure<F64 -> F64>) -> List<Float>`.
pub(super) fn list_float_map() -> StdlibFunction {
    StdlibFunction::new(
        "list_float_map",
        vec![IrType::ListFloat, IrType::Closure],
        IrType::ListFloat,
        || {
            list_map_body_typed(
                |o| Op::LoadF64AtAbsolute { offset: o },
                |o| Op::StoreF64AtAbsolute { offset: o },
                IrType::F64,
                IrType::F64,
            )
        },
    )
}

/// `list_int_map_to_float(xs: List<Int>, f: Closure<I64 -> F64>) -> List<Float>`
/// — element-type-changing map whose closure return type widens the
/// result list to `List<Float>`.
pub(super) fn list_int_map_to_float() -> StdlibFunction {
    StdlibFunction::new(
        "list_int_map_to_float",
        vec![IrType::ListInt, IrType::Closure],
        IrType::ListFloat,
        || {
            list_map_body_typed(
                |o| Op::LoadI64AtAbsolute { offset: o },
                |o| Op::StoreF64AtAbsolute { offset: o },
                IrType::I64,
                IrType::F64,
            )
        },
    )
}

/// `list_float_map_to_int(xs: List<Float>, f: Closure<F64 -> I64>) -> List<Int>`
/// — element-type-changing map (Float source, Int closure result).
pub(super) fn list_float_map_to_int() -> StdlibFunction {
    StdlibFunction::new(
        "list_float_map_to_int",
        vec![IrType::ListFloat, IrType::Closure],
        IrType::ListInt,
        || {
            list_map_body_typed(
                |o| Op::LoadF64AtAbsolute { offset: o },
                |o| Op::StoreI64AtAbsolute { offset: o },
                IrType::F64,
                IrType::I64,
            )
        },
    )
}

/// `list_float_filter(xs: List<Float>, f: Closure<F64 -> Bool>) -> List<Float>`.
pub(super) fn list_float_filter() -> StdlibFunction {
    StdlibFunction::new(
        "list_float_filter",
        vec![IrType::ListFloat, IrType::Closure],
        IrType::ListFloat,
        || {
            list_filter_body_typed(
                |o| Op::LoadF64AtAbsolute { offset: o },
                |o| Op::StoreF64AtAbsolute { offset: o },
                IrType::F64,
                IrType::F64,
            )
        },
    )
}

/// `list_list_filter(xs: ListList, f: Closure<I32 -> Bool>) -> ListList`.
pub(super) fn list_list_filter() -> StdlibFunction {
    StdlibFunction::new(
        "list_list_filter",
        vec![IrType::ListList, IrType::Closure],
        IrType::ListList,
        list_pointer_filter_body,
    )
}

/// `list_float_fold(xs: List<Float>, init: Float, f: Closure<(F64, F64) -> F64>) -> Float`.
pub(super) fn list_float_fold() -> StdlibFunction {
    StdlibFunction::new(
        "list_float_fold",
        vec![IrType::ListFloat, IrType::F64, IrType::Closure],
        IrType::F64,
        || list_fold_body_typed(|o| Op::LoadF64AtAbsolute { offset: o }, IrType::F64),
    )
}

/// Wave R3c shared body for the String-result `map` family
/// (`list_string_map`, `list_int_map_to_string`,
/// `list_float_map_to_string`). The result is a `List<String>` value:
/// a pointer-array record `[count: u32][off_0: u32]…[off_{n-1}: u32]`
/// whose `off_i` are arena-relative offsets to per-element String
/// records (`[len: u32][utf8]`). This matches the `write_list_string`
/// layout the return ABI / verifier walk byte-for-byte (4-byte slots,
/// 4-aligned header), so a result built here is decoded identically by
/// `read_list_string_at`.
///
/// Why no relocation is needed: every per-element String handle the
/// closure returns is **already** an arena-relative offset (a const-pool
/// literal, a scratch-built `StrConcatN` / `IntToStr` result — all in the
/// same flat arena), and the result header lives in scratch. The slots we
/// store are therefore arena-absolute as-is — exactly the single global
/// arena-relative pointer convention the verifier asserts. There is no
/// child-buffer paste, so the "single-delta relocation" the host-side
/// `BufferBuilder` performs at parameter-marshal time does not apply here.
///
/// `load_elem` reads one source element at `src_payload + i*src_stride`;
/// `src_stride` is `8` for `List<Int>` / `List<Float>` sources (8-byte
/// inline slots) and `4` for a `List<String>` source (pointer-array
/// slots). `src_8aligned` selects the source payload-start formula:
/// `(xs + 4 + 7) & -8` for the inline 8-byte sources, `xs + 4` for the
/// 4-byte pointer-array String source.
///
/// Locals:
///   * 0 — `n:           I32`
///   * 1 — `i:           I32`
///   * 2 — `src_payload: I32`
///   * 3 — `dst_payload: I32`
///   * 4 — `new_base:    I32` (4-aligned result-header offset)
fn list_map_to_string_body(
    load_elem: fn(u32) -> Op,
    param_ty: IrType,
    src_stride: i32,
    src_8aligned: bool,
) -> Vec<TaggedOp> {
    const N: u32 = 0;
    const I: u32 = 1;
    const SRC_PAYLOAD: u32 = 2;
    const DST_PAYLOAD: u32 = 3;
    const NEW_BASE: u32 = 4;
    // Source payload-start: 8-byte sources align to 8; the String
    // pointer-array source starts its 4-byte slots right after the
    // length prefix at `xs + 4`.
    let src_payload_setup: Vec<TaggedOp> = if src_8aligned {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4 + 7)),
            tt(Op::Add(IrType::I32)),
            tt(Op::ConstI32(-8)),
            tt(Op::BitAnd(IrType::I32)),
            tt(Op::LetSet {
                idx: SRC_PAYLOAD,
                ty: IrType::I32,
            }),
        ]
    } else {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LetSet {
                idx: SRC_PAYLOAD,
                ty: IrType::I32,
            }),
        ]
    };
    let mut body = vec![
        // n = i32.load(xs, 0)
        tt(Op::LocalGet(0)),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: N,
            ty: IrType::I32,
        }),
        // record_size = 4 + 4*n + 4 (the trailing +4 is alignment slop so
        // the 4-aligned header fits regardless of the raw scratch base).
        tt(Op::ConstI32(8)),
        tt(Op::LetGet {
            idx: N,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(4)),
        tt(Op::Mul(IrType::I32)),
        tt(Op::Add(IrType::I32)),
        tt(Op::AllocScratchDyn),
        // new_base = (raw_base + 3) & -4
        tt(Op::ConstI32(3)),
        tt(Op::Add(IrType::I32)),
        tt(Op::ConstI32(-4)),
        tt(Op::BitAnd(IrType::I32)),
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
    ];
    body.extend(src_payload_setup);
    body.extend([
        // dst_payload = new_base + 4 (entries start, 4-byte slots)
        tt(Op::LetGet {
            idx: NEW_BASE,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(4)),
        tt(Op::Add(IrType::I32)),
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
                    // dst_addr = dst_payload + i * 4 (pushed first so the
                    // i32.store sees [addr, handle] at the end)
                    tt(Op::LetGet {
                        idx: DST_PAYLOAD,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: I,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(4)),
                    tt(Op::Mul(IrType::I32)),
                    tt(Op::Add(IrType::I32)),
                    // push closure handle (param 1)
                    tt(Op::LocalGet(1)),
                    // push the source element: load(src_payload + i*stride)
                    tt(Op::LetGet {
                        idx: SRC_PAYLOAD,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: I,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(src_stride)),
                    tt(Op::Mul(IrType::I32)),
                    tt(Op::Add(IrType::I32)),
                    tt(load_elem(0)),
                    // closure(elem) -> String (i32 arena-relative handle)
                    tt(Op::CallClosure {
                        param_tys: vec![param_ty],
                        ret_ty: IrType::String,
                    }),
                    // i32.store: stack is [dst_addr, string_handle]
                    tt(Op::StoreI32AtAbsolute { offset: 0 }),
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
    ]);
    body
}

/// `list_string_map(xs: List<String>, f: Closure<String -> String>) ->
/// List<String>`.
pub(super) fn list_string_map() -> StdlibFunction {
    StdlibFunction::new(
        "list_string_map",
        vec![IrType::ListString, IrType::Closure],
        IrType::ListString,
        || {
            list_map_to_string_body(
                |o| Op::LoadI32AtAbsolute { offset: o },
                IrType::String,
                4,
                false,
            )
        },
    )
}

/// `list_int_map_to_string(xs: List<Int>, f: Closure<Int -> String>) ->
/// List<String>` — element-type-changing map whose closure return type
/// makes the result list a `List<String>`.
pub(super) fn list_int_map_to_string() -> StdlibFunction {
    StdlibFunction::new(
        "list_int_map_to_string",
        vec![IrType::ListInt, IrType::Closure],
        IrType::ListString,
        || {
            list_map_to_string_body(
                |o| Op::LoadI64AtAbsolute { offset: o },
                IrType::I64,
                8,
                true,
            )
        },
    )
}

/// `list_float_map_to_string(xs: List<Float>, f: Closure<Float -> String>)
/// -> List<String>` — Float source, String closure result.
pub(super) fn list_float_map_to_string() -> StdlibFunction {
    StdlibFunction::new(
        "list_float_map_to_string",
        vec![IrType::ListFloat, IrType::Closure],
        IrType::ListString,
        || {
            list_map_to_string_body(
                |o| Op::LoadF64AtAbsolute { offset: o },
                IrType::F64,
                8,
                true,
            )
        },
    )
}

/// Shared body for `map` forms whose closure returns a variant record
/// pointer, for example `List<Int>.map((x) => Some(x))` or
/// `List<Int>.map((x) => x > 0 ? Stat.Up : Stat.Down)`. The result is a
/// 4-byte pointer-array list `[count][off_i]...`, matching the host
/// buffer representation for `List<Option<T>>`, `List<Result<T, E>>`, and
/// `List<CustomEnum>`.
fn list_map_to_variant_list_body(
    load_elem: fn(u32) -> Op,
    param_ty: IrType,
    src_stride: i32,
    src_8aligned: bool,
) -> Vec<TaggedOp> {
    const N: u32 = 0;
    const I: u32 = 1;
    const SRC_PAYLOAD: u32 = 2;
    const DST_PAYLOAD: u32 = 3;
    const NEW_BASE: u32 = 4;
    let src_payload_setup: Vec<TaggedOp> = if src_8aligned {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4 + 7)),
            tt(Op::Add(IrType::I32)),
            tt(Op::ConstI32(-8)),
            tt(Op::BitAnd(IrType::I32)),
            tt(Op::LetSet {
                idx: SRC_PAYLOAD,
                ty: IrType::I32,
            }),
        ]
    } else {
        vec![
            tt(Op::LocalGet(0)),
            tt(Op::ConstI32(4)),
            tt(Op::Add(IrType::I32)),
            tt(Op::LetSet {
                idx: SRC_PAYLOAD,
                ty: IrType::I32,
            }),
        ]
    };
    let mut body = vec![
        tt(Op::LocalGet(0)),
        tt(Op::LoadI32AtAbsolute { offset: 0 }),
        tt(Op::LetSet {
            idx: N,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(8)),
        tt(Op::LetGet {
            idx: N,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(4)),
        tt(Op::Mul(IrType::I32)),
        tt(Op::Add(IrType::I32)),
        tt(Op::AllocScratchDyn),
        tt(Op::ConstI32(3)),
        tt(Op::Add(IrType::I32)),
        tt(Op::ConstI32(-4)),
        tt(Op::BitAnd(IrType::I32)),
        tt(Op::LetSet {
            idx: NEW_BASE,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: NEW_BASE,
            ty: IrType::I32,
        }),
        tt(Op::LetGet {
            idx: N,
            ty: IrType::I32,
        }),
        tt(Op::StoreI32AtAbsolute { offset: 0 }),
    ];
    body.extend(src_payload_setup);
    body.extend([
        tt(Op::LetGet {
            idx: NEW_BASE,
            ty: IrType::I32,
        }),
        tt(Op::ConstI32(4)),
        tt(Op::Add(IrType::I32)),
        tt(Op::LetSet {
            idx: DST_PAYLOAD,
            ty: IrType::I32,
        }),
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
                    tt(Op::LetGet {
                        idx: DST_PAYLOAD,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: I,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(4)),
                    tt(Op::Mul(IrType::I32)),
                    tt(Op::Add(IrType::I32)),
                    tt(Op::LocalGet(1)),
                    tt(Op::LetGet {
                        idx: SRC_PAYLOAD,
                        ty: IrType::I32,
                    }),
                    tt(Op::LetGet {
                        idx: I,
                        ty: IrType::I32,
                    }),
                    tt(Op::ConstI32(src_stride)),
                    tt(Op::Mul(IrType::I32)),
                    tt(Op::Add(IrType::I32)),
                    tt(load_elem(0)),
                    tt(Op::CallClosure {
                        param_tys: vec![param_ty],
                        ret_ty: IrType::I32,
                    }),
                    tt(Op::StoreI32AtAbsolute { offset: 0 }),
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
        tt(Op::LetGet {
            idx: NEW_BASE,
            ty: IrType::I32,
        }),
        tt(Op::Return),
    ]);
    body
}

/// `list_int_map_to_variant_list(xs, f)` where `f: Int -> variant`.
pub(super) fn list_int_map_to_variant_list() -> StdlibFunction {
    StdlibFunction::new(
        "list_int_map_to_variant_list",
        vec![IrType::ListInt, IrType::Closure],
        IrType::ListList,
        || {
            list_map_to_variant_list_body(
                |o| Op::LoadI64AtAbsolute { offset: o },
                IrType::I64,
                8,
                true,
            )
        },
    )
}

/// `list_float_map_to_variant_list(xs, f)` where `f: Float -> variant`.
pub(super) fn list_float_map_to_variant_list() -> StdlibFunction {
    StdlibFunction::new(
        "list_float_map_to_variant_list",
        vec![IrType::ListFloat, IrType::Closure],
        IrType::ListList,
        || {
            list_map_to_variant_list_body(
                |o| Op::LoadF64AtAbsolute { offset: o },
                IrType::F64,
                8,
                true,
            )
        },
    )
}

/// `list_string_map_to_variant_list(xs, f)` where `f: String -> variant`.
pub(super) fn list_string_map_to_variant_list() -> StdlibFunction {
    StdlibFunction::new(
        "list_string_map_to_variant_list",
        vec![IrType::ListString, IrType::Closure],
        IrType::ListList,
        || {
            list_map_to_variant_list_body(
                |o| Op::LoadI32AtAbsolute { offset: o },
                IrType::String,
                4,
                false,
            )
        },
    )
}
