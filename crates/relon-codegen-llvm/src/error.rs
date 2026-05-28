//! Error types surfaced by the LLVM AOT pipeline.
//!
//! Modelled after `relon_codegen_cranelift::CraneliftError` so the
//! relon facade can wrap both backends through the same
//! `BackendError` shape — switching backends should not change which
//! variants the host needs to match on.

use thiserror::Error;

/// Top-level error type for the LLVM AOT backend. Construction sites
/// fall into four buckets:
///
/// * [`LlvmError::Parse`] — the parser rejected the source. Only
///   produced through `from_source` paths; the direct-IR entry
///   points cannot hit this arm.
/// * [`LlvmError::Analyze`] — analyzer rejected the source. Reports
///   the diagnostic count instead of the full message bundle to keep
///   the error variant cheap to clone; hosts that want the full
///   diagnostic stream should drive `relon_analyzer::analyze`
///   directly.
/// * [`LlvmError::UnsupportedSignature`] — the IR module's entry
///   signature is outside the Phase A bootstrap envelope (legacy
///   `(I64...) -> I64`). Buffer-protocol entries fall here today.
/// * [`LlvmError::Codegen`] — the LLVM emitter / JIT engine refused
///   the input (unsupported op, IR builder failure, JIT setup
///   failure). The carried `String` is the inkwell builder's own
///   message; we don't try to map it to a typed enum at this stage.
#[derive(Debug, Error)]
pub enum LlvmError {
    /// Parser failure. Only reachable through `from_source`-style
    /// entry points (Phase B+); direct-IR paths skip this stage.
    #[error("parse error: {0}")]
    Parse(String),

    /// Analyzer rejected the source with `count` errors. Phase A
    /// returns the count so the relon facade can wrap it without
    /// having to allocate a diagnostic vector.
    #[error("analyze error: {0} diagnostic(s)")]
    Analyze(usize),

    /// The IR module's entry function does not match the Phase A
    /// envelope (legacy `(I64...) -> I64`). The carried string names
    /// the rejected param / return type so the host can decide
    /// whether to retry through the cranelift backend.
    #[error("unsupported entry signature: {0}")]
    UnsupportedSignature(String),

    /// LLVM emitter / JIT engine surfaced an error during compile.
    /// Wraps the inkwell builder message rather than a typed enum
    /// — Phase A keeps the surface narrow so the bootstrap test can
    /// fail loudly without locking us into a permanent error
    /// taxonomy.
    #[error("LLVM codegen failed: {0}")]
    Codegen(String),
}
