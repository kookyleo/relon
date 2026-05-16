#![forbid(unsafe_code)]

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
pub mod ir;
pub mod lowering;
pub mod stdlib;

pub use error::LoweringError;
pub use ir::{Func, IrType, Module, Op, TaggedOp};
pub use lowering::{
    lower_workspace, lower_workspace_single, LoweredEntry, MAIN_PARAMS_SCHEMA_NAME,
    MAIN_RETURN_SCHEMA_NAME, RETURN_VALUE_FIELD_NAME, WASM_LOCAL_IN_LEN, WASM_LOCAL_IN_PTR,
    WASM_LOCAL_OUT_CAP, WASM_LOCAL_OUT_PTR,
};
pub use stdlib::{
    builtin_stdlib, stdlib_function_count, stdlib_function_index, stdlib_method_index,
    StdlibFunction,
};
