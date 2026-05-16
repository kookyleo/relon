//! Phase 4.a bundled stdlib registry.
//!
//! v1 stdlib is **bundled**: every compiled module prepends the
//! builtin stdlib function bodies into its wasm function table before
//! any user-defined function. The codegen pass turns each
//! [`StdlibFunction`] into a wasm `func` (params + locals + body) at
//! a fixed index — `0..N` for the N builtin functions, then user
//! functions at `N..N + user_fn_count`.
//!
//! The lowering pass uses [`stdlib_function_index`] to look up the
//! wasm-level callee slot when it lowers `s.length()` /
//! `length(s)` into [`crate::ir::Op::Call`]. The index is **stable**
//! across compiles for a given Relon version because [`builtin_stdlib`]
//! returns the list in fixed order; reordering breaks the wire format
//! for any pre-compiled module, so future phases that add functions
//! must always **append**.
//!
//! Phase 4.a scope:
//!   * `length(s: String) -> Int` — byte length of a String record.
//!
//! Out of scope (deferred to 4.b+):
//!   * Other string ops (`concat`, `upper`, `lower`, ...).
//!   * List ops, math ops, capability-gated stdlib, closures.
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

/// Return the ordered list of builtin stdlib functions. Phase 4.a
/// ships exactly one entry (`length`); subsequent phases append new
/// entries at the end so the function indices of earlier entries stay
/// stable.
pub fn builtin_stdlib() -> Vec<StdlibFunction> {
    vec![length_string_to_int()]
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
}
