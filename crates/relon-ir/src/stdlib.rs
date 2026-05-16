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
//! Out of scope (deferred to 4.c+):
//!   * String mutators / builders (`concat`, `upper`, `lower`,
//!     `substring`) — require a wasm-side scratch heap allocator.
//!   * `List<Int>` aggregations (`sum`, `map`, `filter`, `fold`) —
//!     require wasm loop ops (`Block` / `Loop` / `Br` / `BrIf`).
//!   * Capability-gated stdlib, closures.
//!
//! See `docs/internal/wasm-backend-design-draft.md` Section 4 for the
//! bundling rationale.

use crate::ir::{IrType, Op, TaggedOp};
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
pub fn builtin_stdlib() -> Vec<StdlibFunction> {
    vec![
        length_string_to_int(),
        list_int_length_to_int(),
        abs_int(),
        min_int(),
        max_int(),
        is_empty_string(),
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

/// Resolve a stdlib function name to its wasm-level function index in
/// the combined module. Returns `None` for unknown names; callers
/// surface the lowering-level error themselves.
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
        _ => None,
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
