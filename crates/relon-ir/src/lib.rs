// Relaxed from `forbid` to `deny` so the v3++ item 4 SIMD ASCII fast
// path (`ascii_fold_simd`) can use wasm32 `v128_load` / `v128_store`
// intrinsics, both of which are `unsafe fn` in `core::arch::wasm32`.
// The `unsafe` blocks are confined to that single module behind a
// `#[allow(unsafe_code)]` and each has a SAFETY comment; the rest of
// the crate stays unsafe-free.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

//! Linear-typed IR between `relon-analyzer`'s `AnalyzedTree` and
//! codegen backends (WASM, future native / JS).
//!
//! Phase 1.beta scope (locked in
//! `docs/internal/wasm-backend-design-draft.md` + the binary
//! layout note dated 2026-05-16):
//!
//! * Single `Func` per module (the `#main` entry, exported as
//!   `run_main`).
//! * Scalar value types only — `Int` lowers to `I64`, `Float` to
//!   `F64`. Composite layouts arrive in Phase 2.
//! * Arithmetic on uniform-type bodies. Mixed-type expressions and
//!   non-arithmetic operators fail lowering.
//!
//! See `docs/internal/wasm-crate-structure-2026-05-16.md` for the
//! IR-first crate split rationale.

pub mod error;
pub mod glob;
pub mod ir;
pub mod lowering;
pub mod op_visitor;
pub mod shape_hash;
pub mod stdlib;
pub mod unicode;

// Backwards-compatible re-exports: the eight Unicode-adjacent modules
// previously lived flat under `relon-ir/src/`. They moved into
// [`unicode`] (review-improvement P3, large-file / domain split,
// 2026-05-21). Re-exporting them at the crate root keeps
// `relon_ir::case_folding::...` / `relon_ir::normalization::...` /
// etc. compiling for downstream crates so the move stays
// non-breaking.
pub use crate::unicode::ascii_fold_simd;
pub use crate::unicode::case_folding;
pub use crate::unicode::combining_marks;
pub use crate::unicode::full_case_folding;
pub use crate::unicode::normalization;
pub use crate::unicode::normalization_data;
pub use crate::unicode::whitespace;

pub use error::LoweringError;
pub use ir::{
    ClosureCapture, EffectClass, Func, IrType, Module, NativeImport, Op, TaggedOp, TrapKind,
    NO_CAPABILITY_BIT,
};
pub use lowering::{
    lower_workspace, lower_workspace_single, LoweredEntry, MAIN_PARAMS_SCHEMA_NAME,
    MAIN_RETURN_SCHEMA_NAME, RETURN_VALUE_FIELD_NAME, WASM_LOCAL_CAPS_ARG, WASM_LOCAL_IN_LEN,
    WASM_LOCAL_IN_PTR, WASM_LOCAL_OUT_CAP, WASM_LOCAL_OUT_PTR,
};
pub use op_visitor::{walk_body, walk_op, OpVisitor};
pub use stdlib::{
    builtin_stdlib, stdlib_closure_arg_signature, stdlib_function_count, stdlib_function_index,
    stdlib_method_index, StdlibFunction, GLOB_MATCH_INDEX,
};
