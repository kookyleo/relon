// Relaxed from `forbid` to `deny` so the v3++ item 4 SIMD ASCII fast
// path (`ascii_fold_simd`) can use wasm32 `v128_load` / `v128_store`
// intrinsics, both of which are `unsafe fn` in `core::arch::wasm32`.
// The `unsafe` blocks are confined to that single module behind a
// `#[allow(unsafe_code)]` and each has a SAFETY comment; the rest of
// the crate stays unsafe-free.
#![deny(unsafe_code)]
#![deny(unsafe_op_in_unsafe_fn)]

//! Linear-typed IR between `relon-analyzer`'s `AnalyzedTree` and the
//! compiled backends: `relon-codegen-cranelift` (JIT / object cache)
//! and `relon-codegen-llvm` (native AOT + wasm32 object emission).
//!
//! Current scope (grown well past the original single-function,
//! scalar-only Phase 1.beta):
//!
//! * An [`ir::Module`] carries multiple [`ir::Func`]s — the `#main`
//!   entry plus lowered closures (via `Module::closure_table`) and
//!   bundled stdlib bodies — alongside host `#native` imports.
//! * Value coverage spans scalars (`I64` / `F64` / `Bool` / `String`),
//!   homogeneous lists, fixed-arity tuples, `#enum` / `Option` /
//!   `Result` variant records, and schema-rooted dict returns; the
//!   lowered surface includes `match` dispatch, f-strings, `&sibling`
//!   / `&root` references, spread, and field decorators.
//! * Anything outside the supported surface fails lowering *loudly*
//!   at a registered cap site (`lowering::cap::LOWERING_CAP_IDS`) —
//!   the auto backend then falls back to the tree-walk evaluator. The
//!   supported surface is pinned by the coverage ledger and the
//!   no-fallback tests in `relon-test-harness`.
//!
//! See `docs/internal/archive/wasm-crate-structure-2026-05-16.md` for
//! the historical IR-first crate split rationale.

pub mod effect;
pub mod error;
pub mod float_str;
pub mod frontend;
pub mod intern;
pub mod ir;
pub mod lowering;
pub mod op_visitor;
pub mod shape_hash;
pub mod stdlib;

// Backwards-compatible re-exports: the Unicode tables / algorithms and
// the glob matcher were extracted into the leaf `relon-unicode` crate
// so the tree-walk evaluator can consume them without depending on
// `relon-ir`. Re-exporting them at the crate root keeps the codegen
// backends' `relon_ir::ascii_fold_simd::...` / `relon_ir::glob::...` /
// `relon_ir::normalization::...` / etc. paths compiling unchanged, so
// the move stays non-breaking for downstream crates.
pub use relon_unicode::ascii_fold_simd;
pub use relon_unicode::case_folding;
pub use relon_unicode::combining_marks;
pub use relon_unicode::full_case_folding;
pub use relon_unicode::glob;
pub use relon_unicode::normalization;
pub use relon_unicode::normalization_data;
pub use relon_unicode::whitespace;

pub use error::LoweringError;
pub use frontend::{compile, FrontendError};
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
    stdlib_method_index, StdlibFunction, CONCAT_INDEX, CONTAINS_INDEX, GLOB_MATCH_INDEX,
    IS_EMPTY_INDEX, LENGTH_INDEX, SUBSTRING_INDEX,
};
