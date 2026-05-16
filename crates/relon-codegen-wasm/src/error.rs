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
use crate::unreachable_table::UnreachableTableError;
use relon_ir::IrType;
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
    /// Phase 2.b: the canonical schema's offset table couldn't be
    /// computed (variable-size leaf, overflow, ...) or one of the
    /// derived sizes overflowed the u32 slot width used by the
    /// binary handshake size guards. Wraps the layout-side error
    /// stringified so the public surface stays narrow.
    #[error("layout error: {0}")]
    Layout(String),
    /// Phase 2.c: a comparison op (`<`, `<=`, `>`, `>=`, `==`, `!=`)
    /// landed in the codegen with operand types outside the
    /// `Int`/`Float`/`Bool`/`Null` supported set. Ordering on Bool /
    /// Null in particular is rejected by upstream type checking, but
    /// the codegen keeps the gate so a hand-built IR can't sneak it
    /// in.
    #[error("invalid comparison operand type: `{ty:?}`")]
    InvalidComparisonOperandType {
        /// The IR type the comparison op was tagged with.
        ty: IrType,
    },
    /// Phase 2.c: an `if` (ternary) lowered with branches that
    /// disagree on their result type. Wasm's `if`-block requires both
    /// arms to push the same value type, so this surfaces a body the
    /// lowering pass should have already rejected via
    /// `LoweringError::IfBranchTypeMismatch`.
    #[error("if branches disagree on type: then=`{then_ty:?}`, else=`{else_ty:?}`")]
    IfBranchTypeMismatch {
        /// IR type the `then` branch produced.
        then_ty: IrType,
        /// IR type the `else` branch produced.
        else_ty: IrType,
    },
    /// Phase 4.a: an `Op::Call` arrived with operand types that don't
    /// match the callee's declared parameter signature, or the
    /// `arg_count` disagrees with `param_tys.len()`. Surfaces a
    /// lowering-side bug (the lowering pass already verified the
    /// shape; this is the codegen belt-and-braces).
    #[error(
        "call type mismatch: callee fn_index={fn_index} arg_count={arg_count} param_tys.len()={param_tys_len}"
    )]
    CallTypeMismatch {
        /// Combined wasm-module function index of the callee.
        fn_index: u32,
        /// Argument count declared on the op.
        arg_count: u32,
        /// Length of the op's `param_tys` vector.
        param_tys_len: u32,
    },
    /// Phase 2.c: a `StoreField` of a type the wasm side can't emit
    /// a single-instruction store for (currently `String` /
    /// `ListInt`). The return surface only covers `Int` / `Float` /
    /// `Bool` / `Null`; lowering should reject earlier — this is a
    /// belt-and-braces guard.
    #[error("unsupported store type: `{ty:?}`")]
    UnsupportedStoreFieldType {
        /// The IR type carried on the offending `StoreField`.
        ty: IrType,
    },
    /// Phase 6: the caller-supplied [`crate::HostFnTable`] doesn't
    /// agree with the IR module's `imports` list. The position-by-
    /// position correspondence is part of the wire format (each
    /// `Op::CallNative { import_idx }` targets the matching entry
    /// in both vectors); mismatched lengths break that invariant
    /// before codegen even attempts to emit.
    #[error(
        "host-fns table arity mismatch: ir_imports={ir_imports}, table_entries={table_entries}"
    )]
    HostFnTableArityMismatch {
        /// Number of `NativeImport` entries in the IR module.
        ir_imports: u32,
        /// Number of `HostFnEntry` rows in the supplied table.
        table_entries: u32,
    },
    /// Phase 6: an `Op::CallNative` references an import index past
    /// the imports the IR module declared. Surfaces a hand-built IR
    /// bug — `lower_workspace_*` keeps the two in sync by design.
    #[error("Op::CallNative import_idx={import_idx} out of range (import_count={import_count})")]
    CallNativeImportOutOfRange {
        /// Offending `import_idx` value on the op.
        import_idx: u32,
        /// Number of imports the IR module declared.
        import_count: u32,
    },
    /// Phase 6: an `Op::CallNative` arrived at codegen with operand
    /// types or counts that disagree with the declared host-fn
    /// signature. Lowering is responsible for matching them — this
    /// is the codegen belt-and-braces.
    #[error(
        "Op::CallNative arg mismatch: import_idx={import_idx} param_tys.len()={param_tys_len}"
    )]
    CallNativeArgCountMismatch {
        /// Import index whose signature was violated.
        import_idx: u32,
        /// Length of the op's `param_tys` vector.
        param_tys_len: u32,
    },
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
    /// Phase 6: `relon.host_fns` payload was located but failed to
    /// decode. Distinct from [`Self::MissingHostFn`] /
    /// [`Self::HostFnSignatureDrift`] — those fire after a successful
    /// decode against the supplied [`crate::HostFnTable`].
    #[error(transparent)]
    HostFns(#[from] crate::host_fns::HostFnError),
    /// Phase 7: `relon.uctab` payload was located but failed to
    /// decode (bad magic, future format version, unknown kind_tag,
    /// ...). The translator can survive without this table — it
    /// just falls through to `WasmTrapUnclassified` — but a present-
    /// but-malformed section is a stronger signal than a missing one
    /// and we surface it explicitly so host SDKs don't silently
    /// degrade trap diagnostics.
    #[error(transparent)]
    UnreachableTable(#[from] UnreachableTableError),
    /// Phase 6: the wasm module declared a `#native` import the host
    /// SDK did not register. The host SDK can choose to recover by
    /// registering the missing fn or refuse-to-load entirely.
    #[error("host fn `{name}` declared by module but not registered with the host SDK")]
    MissingHostFn {
        /// Name of the missing import. Mirrors the `name` field of
        /// the wasm `(import "env" <name> ...)` line.
        name: String,
    },
    /// Phase 6: the host SDK registered a `#native` fn under the
    /// matching name, but its canonical signature hash disagrees with
    /// what the wasm module embedded. Either the host's binding or
    /// the wasm module's expectation is stale.
    #[error("host fn `{name}` signature drift")]
    HostFnSignatureDrift {
        /// Name of the offending import.
        name: String,
        /// `"params"` or `"return"` depending on which hash half
        /// disagreed. Lets the host SDK surface a more specific
        /// diagnostic without re-running the comparison.
        which: &'static str,
    },
}
