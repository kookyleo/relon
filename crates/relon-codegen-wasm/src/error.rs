//! Codegen errors surfaced when an IR shape can't be encoded to
//! valid wasm. The Phase 1.beta lowering pass eagerly rejects most
//! ill-formed shapes upstream, so this enum currently only flags
//! mixed-type arithmetic (which can survive lowering when both
//! sides happen to type-check individually but disagree on the
//! arithmetic flavor).
//!
//! Phase 2.a adds [`LoadError`] for the loader-side surface
//! ([`crate::WasmModule::from_bytes`]) — distinct from `CodegenError`
//! because the load path can fail in shapes the codegen path
//! cannot (e.g. a third-party stripped the `relon.abi` section).

use crate::abi::AbiError;
use crate::srcmap::SrcMapError;
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
    /// Phase 1.gamma srcmap pass disagreed with the emitted code
    /// section — usually because the IR-recorded op count drifted
    /// from what wasmparser reads back out of the same module, or
    /// the secondary scan failed to parse. Surfaces an internal
    /// invariant rather than a user-facing shape; should never
    /// trigger from a `lower_workspace_*` produced IR.
    #[error("srcmap pass failed: {0}")]
    SrcMapEncode(String),
}

/// Failure modes when loading an already-compiled wasm module via
/// [`crate::WasmModule::from_bytes`].
///
/// The loader walks the module's custom sections to extract the
/// `relon.abi` + `relon.srcmap` payloads. Any shape failure surfaces
/// here so host SDKs can map each variant to a stable user-facing
/// `RuntimeError` (Phase 7).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// The wasm parse itself failed (truncated module, bad section
    /// header, ...). Carries the wasmparser error stringified so the
    /// dependency surface stays narrow on the public re-exports.
    #[error("wasm parse failed: {0}")]
    WasmParse(String),
    /// Couldn't locate one of the custom sections required by the
    /// Relon ABI. Distinct from [`Self::Abi`] / [`Self::SrcMap`]
    /// because those variants only fire after the section was
    /// located and its payload turned out to be malformed.
    #[error("expected custom section `{name}` is missing")]
    MissingCustomSection {
        /// Section name the loader was looking for.
        name: &'static str,
    },
    /// `relon.abi` payload was located but failed validation. Wraps
    /// the abi-specific failure variant so callers can match on it.
    #[error(transparent)]
    Abi(#[from] AbiError),
    /// `relon.srcmap` payload was located but failed parse. Wraps
    /// the srcmap-specific failure variant.
    #[error(transparent)]
    SrcMap(#[from] SrcMapError),
}
