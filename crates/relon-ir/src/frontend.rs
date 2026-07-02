//! Shared compiled-backend frontend pipeline.
//!
//! Every compiled backend (cranelift-native AOT, LLVM AOT, and future
//! targets) re-implements the same parse → analyze → lower triplet before
//! it diverges into backend-specific schema / layout / codegen work.
//! [`compile`] extracts that triplet behind one entry so the three
//! backends stop drifting on the order of steps, the error mapping,
//! and the `has_errors()` diagnostic-count convention.
//!
//! The function is policy-free: the caller builds its own
//! [`relon_analyzer::AnalyzeOptions`] (compiled backends force
//! `standalone_capability_check: true`; some additionally relax
//! `strict_mode`) and passes it in. [`compile`] only runs the three
//! pipeline stages and maps each stage's failure into a
//! [`FrontendError`] variant the backend translates to its own error
//! type.

use relon_analyzer::{AnalyzeOptions, Diagnostic};

use crate::lowering::{lower_workspace_single, LoweredEntry};

/// Failure surfaced by the shared [`compile`] pipeline. Each variant
/// maps 1:1 onto the matching variant every backend already carries
/// (`Parse(String)` / `Analyze(usize)` / `Lowering(String)`), so the
/// backend's `From` / `match` translation is lossless.
#[derive(Debug, Clone, thiserror::Error)]
pub enum FrontendError {
    /// The parser rejected the source. Carries the `Display` of the
    /// parser error.
    #[error("parse error: {0}")]
    Parse(String),
    /// The analyzer reported one or more `Error`-severity diagnostics.
    /// Carries the error count (matching the backends'
    /// `has_errors()` + diagnostic-filter convention).
    #[error("analyzer rejected source: {0} error(s)")]
    Analyze(usize),
    /// IR lowering (`lower_workspace_single`) failed. Carries the
    /// `Display` of the `LoweringError`.
    #[error("ir lowering: {0}")]
    Lowering(String),
}

/// Drive the shared compiled-backend frontend: parse the source,
/// analyze it under the caller-supplied `options`, and lower the
/// analyzed tree's entry module.
///
/// Steps, in order:
///
/// 1. [`relon_parser::parse_document`] — `Err` → [`FrontendError::Parse`].
/// 2. [`relon_analyzer::analyze_with_options`] — when the result
///    `has_errors()`, count the `Error`-severity diagnostics and
///    return [`FrontendError::Analyze`].
/// 3. [`lower_workspace_single`] — `Err` → [`FrontendError::Lowering`].
///
/// Returns the whole [`LoweredEntry`] (carrying `.module`,
/// `.main_schema`, `.return_schema`) so each backend keeps its own
/// post-lower handling. The `options` are consumed verbatim — this
/// function applies no policy of its own (no forced
/// `standalone_capability_check`, no `strict_mode` relaxation); the
/// caller owns that.
pub fn compile(src: &str, options: &AnalyzeOptions) -> Result<LoweredEntry, FrontendError> {
    compile_with_suppressed(src, options, |_| false)
}

/// Like [`compile`], but an `Error`-severity diagnostic for which
/// `suppress` returns `true` is dropped from the analyze gate (it neither
/// counts toward [`FrontendError::Analyze`] nor blocks lowering).
///
/// This is the seam a compiled backend uses to accept a source shape its
/// IR lowering already handles even though a strict-mode *soft-ban*
/// diagnostic flags it — e.g. the LLVM backend's closure-as-value dict
/// surface (`ClosureParamTypeMissing` / `ClosureReturnTypeUnknown` /
/// `ExpressionTypeUnknown`), which `lower_anon_dict_body` lowers fine.
///
/// The predicate is deliberately narrow: it only sees the diagnostics it
/// explicitly matches, so every hard structural error
/// (`UnknownTypeName`, `MainReturnTypeMismatch`, …) and the
/// capability-reachability check keep gating the build. Passing
/// `|_| false` recovers [`compile`] verbatim.
pub fn compile_with_suppressed<F>(
    src: &str,
    options: &AnalyzeOptions,
    suppress: F,
) -> Result<LoweredEntry, FrontendError>
where
    F: Fn(&Diagnostic) -> bool,
{
    let ast = relon_parser::parse_document(src).map_err(|e| FrontendError::Parse(e.to_string()))?;
    let analyzed = relon_analyzer::analyze_with_options(&ast, options);
    let err_count = analyzed
        .diagnostics
        .iter()
        .filter(|d| d.severity() == relon_analyzer::Severity::Error)
        .filter(|d| !suppress(d))
        .count();
    if err_count > 0 {
        return Err(FrontendError::Analyze(err_count));
    }
    lower_workspace_single(&analyzed, &ast).map_err(|e| FrontendError::Lowering(e.to_string()))
}
