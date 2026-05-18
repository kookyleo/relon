//! Error surface for the cranelift-native AOT backend.
//!
//! Split out of `lib.rs` so the public re-exports stay narrow. The
//! enum mirrors `relon_codegen_wasm::BuildError` in shape so the
//! `AutoEvaluator::build_aot` site can adopt either backend without
//! reshaping its `String`-stringified pipeline.

use thiserror::Error;

/// Errors produced while building / running a [`crate::CraneliftAotEvaluator`].
#[derive(Debug, Error)]
pub enum CraneliftError {
    /// Parser rejected the source. Mirrors `relon::Error::Parse`'s
    /// surface so the facade can chain the two without losing the
    /// upstream message.
    #[error("parse error: {0}")]
    Parse(String),

    /// Per-module analyzer reported one or more `Error`-severity
    /// diagnostics. The aggregated count keeps the public message
    /// short; full diagnostics live on the workspace if the host
    /// drove that path.
    #[error("analyzer reported {0} error(s)")]
    Analyze(usize),

    /// Phase 1.beta IR lowering rejected the analyzed tree. Stringified
    /// from `relon_ir::lowering::LoweringError` so the dependency stays
    /// internal to this crate.
    #[error("ir lowering failed: {0}")]
    Lowering(String),

    /// Cranelift host-target detection failed. Most likely means the
    /// build host is on an unsupported architecture. Surfaces the
    /// underlying lookup error string.
    #[error("cranelift host detection failed: {0}")]
    HostTarget(String),

    /// Cranelift JIT module builder rejected the ISA / target shape.
    #[error("cranelift JIT setup failed: {0}")]
    JitSetup(String),

    /// IR -> Cranelift IR lowering tripped on an unsupported op or a
    /// type / arity mismatch the IR-side validator missed.
    ///
    /// v5-beta-1 supports a deliberately narrow subset (arith + cmp +
    /// control flow + a couple of stdlib calls); everything else
    /// surfaces here so the `AutoEvaluator` can cleanly fall back to
    /// the wasm-AOT or tree-walk tier.
    #[error("cranelift codegen lowering failed: {0}")]
    Codegen(String),

    /// `cranelift_module::Module::define_function` (or
    /// `declare_function`) rejected the emitted IR. Wraps the cranelift
    /// `ModuleError` stringified to keep the public surface narrow.
    #[error("cranelift module define failed: {0}")]
    ModuleDefine(String),

    /// v5-beta-1 only supports `#main(Int, ...)`-shaped entries
    /// returning `Int`. Anything outside that envelope surfaces here so
    /// the auto-tier wrapper can route to the wasm-AOT / tree-walker
    /// without polluting `Codegen` with shape errors that are not
    /// implementation bugs.
    #[error("unsupported #main signature: {0}")]
    UnsupportedSignature(String),

    /// The module-cache file on disk could not be read / parsed.
    #[error("cache load failed: {0}")]
    Cache(String),
}
