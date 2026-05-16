//! Codegen errors surfaced when an IR shape can't be encoded to
//! valid wasm. The Phase 1.beta lowering pass eagerly rejects most
//! ill-formed shapes upstream, so this enum currently only flags
//! mixed-type arithmetic (which can survive lowering when both
//! sides happen to type-check individually but disagree on the
//! arithmetic flavor).

use thiserror::Error;

/// Reasons codegen can fail.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum CodegenError {
    /// An arithmetic op's tagged [`relon_ir::IrType`] disagrees with
    /// what's actually on the virtual wasm stack at emit time. v1.beta
    /// requires pure-i64 or pure-f64 bodies — no implicit promotion.
    #[error(
        "mixed numeric types in arithmetic (Phase 1.beta supports pure-i64 or pure-f64 bodies)"
    )]
    MixedNumericTypes,
    /// Empty IR module — codegen would emit a valid-but-useless wasm
    /// blob. The Phase 1.beta lowering pass guarantees a single
    /// `Func` per `Module`, so hitting this means a caller bypassed
    /// `lower_workspace` / `lower_workspace_single`.
    #[error("IR module has no functions to emit")]
    EmptyModule,
}
