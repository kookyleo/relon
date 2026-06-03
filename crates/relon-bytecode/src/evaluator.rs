//! Public façade implementing [`relon_eval_api::Evaluator`].
//!
//! Construction mirrors `relon_codegen_cranelift::AotEvaluator::from_source`:
//! parse → analyze → `lower_workspace_single` → bytecode compile.
//! `run_main` packs the args into virtual local slots, runs the VM,
//! and unpacks the return slots back into a `Value`. The arena
//! marshalling that cranelift uses is gone — the bytecode VM never
//! materialises an arena because it doesn't speak the buffer-protocol
//! load/store ops; the compile pass translates them to `LocalGet` /
//! `LocalSet` against virtual slots indexed off the schema's offset
//! table.
//!
//! v6-δ M2-A is **scaffolding** — only sources whose `#main` body
//! uses arith / cmp / control flow / let-bindings on the inline
//! scalar leaves (`Int` / `Bool` / `Null`) compile cleanly. Stdlib /
//! list / dict / closure / native paths surface as
//! [`BytecodeError::Compile`] and the caller is expected to fall
//! back to the tree-walker or cranelift backend.

use std::collections::HashMap;
use std::sync::Arc;

use relon_eval_api::layout::SchemaLayout;
use relon_eval_api::schema_canonical::Schema;
use relon_eval_api::{
    CapabilityGate, ClosureData, Evaluator, RelonFunction, RuntimeError, Scope, SmolStr, Thunk,
    Value,
};
use relon_ir::ir::Module as IrModule;
use relon_ir::ir::NativeImport;
use relon_ir::{IrType, TaggedOp};
use relon_parser::{Node, TokenRange};
use thiserror::Error;

use crate::compile::{
    build_offset_to_local, compile_function, compile_function_with_closures, BcCompileError,
};
use crate::op::{BcFunction, ExternalPc, StackOrigin};
use crate::vm::{BcVmConfig, BytecodeVm, VmValue};

/// Construction-time errors specific to the bytecode evaluator.
/// Wrap any pipeline step failure that happens **before** the VM
/// dispatches its first op.
#[derive(Debug, Clone, Error)]
pub enum BytecodeError {
    /// Parser rejected the source.
    #[error("parse error: {0}")]
    Parse(String),
    /// Analyzer reported one or more errors.
    #[error("analyzer rejected source: {0} error(s)")]
    Analyze(usize),
    /// IR lowering failed (typically `MissingMain` or schema
    /// resolution).
    #[error("ir lowering: {0}")]
    Lowering(String),
    /// Bytecode compile pass rejected the IR — usually because the
    /// source uses an op outside the M2-A envelope (record / list /
    /// stdlib / native).
    #[error("bytecode compile: {0}")]
    Compile(#[from] BcCompileError),
    /// The lowered module has no entry function.
    #[error("module has no entry function")]
    NoEntry,
    /// Entry shape outside the M2-A envelope (List / Dict / closure
    /// / nested-schema args).
    #[error("unsupported entry shape: {reason}")]
    UnsupportedEntry {
        /// Human-readable description of the mismatch.
        reason: String,
    },
}

/// PC-alignment follow-up #3: bundle of IR data a host needs to drive
/// the trace recorder against the **same** IR the bytecode compile
/// pass consumed.
///
/// Returned by [`BytecodeEvaluator::recording_registration_data`].
/// The `relon-codegen-cranelift` crate translates this into its own
/// `RecordingRegistration` shape (which the recorder dispatcher
/// consumes); both views carry the same two fields, but the
/// translation lives at the boundary so the bytecode crate stays
/// dependency-free from cranelift.
///
/// ## Field semantics
///
/// - `body`: the entry function's `Vec<TaggedOp>`, exactly the slice
///   the bytecode compiler called `compile_seq` against. Walking it
///   through the recorder produces the same per-op `external_pc`
///   increments the bytecode's `ir_pc_map` carries — guards stamped
///   on `external_pc = N` then resume into bytecode index `N - 1`
///   (the IR PC counter is 1-based; entry slot 0 is reserved).
/// - `param_tys`: declared parameter types in declaration order.
///   The recorder driver combines these with the bytecode VM's
///   per-call packed-`u64` argument slots to seed the walker's
///   `(value, IrType)` env before stepping the body.
#[derive(Debug, Clone)]
pub struct RecordingRegistrationData {
    /// IR op stream the recorder must walk to keep its `external_pc`
    /// in lock-step with the bytecode's `ir_pc_map`.
    pub body: Vec<TaggedOp>,
    /// Parameter IR types in declaration order — the recorder pairs
    /// each with the matching slot from the bytecode VM's packed
    /// `[u64]` arg array.
    pub param_tys: Vec<IrType>,
    /// PC-alignment Layer 1: schema-driven `field_offset → arg_slot`
    /// map matching the bytecode VM's `field_offset_to_local` table.
    ///
    /// The production-lowered body reads / writes input args through
    /// `Op::LoadField { offset, ty }` / `Op::LoadStringPtr { offset }`
    /// (without an explicit base pointer on the operand stack), so the
    /// recorder walker needs the same offset→slot mapping the bytecode
    /// compile pass uses to emit `BcOp::LocalGet(slot)`. Carries
    /// `BTreeMap<u32, u32>` entries from `build_offset_to_local`
    /// applied to the main schema layout; empty when the evaluator was
    /// built via the legacy `from_ir_legacy` path (which uses synthetic
    /// `LocalGet(idx)` ops).
    pub field_offset_to_local: std::collections::BTreeMap<u32, u32>,
}

/// Bytecode VM evaluator. Built from a Relon source string via
/// [`Self::from_source`] or directly from an IR module via
/// [`Self::from_ir_legacy`].
#[derive(Debug)]
pub struct BytecodeEvaluator {
    func: BcFunction,
    /// Source range of the entry `#main` directive — used for
    /// diagnostic attachment when the VM trips a sandbox prong.
    entry_range: TokenRange,
    /// Declared `#main` parameter names in declaration order. Used
    /// to project the host's `HashMap<String, Value>` into the
    /// virtual-locals vector.
    param_names: Vec<String>,
    /// Parameter IR types — drives arg packing.
    param_tys: Vec<IrType>,
    /// Return schema. Drives the post-run unpacking.
    return_schema: Option<Schema>,
    /// Return field local-slot start index (right after the input
    /// arg slots).
    return_field_base: u32,
    /// Default VM config. Wrapped in `Arc` so per-`run_main` VM
    /// construction is a refcount bump rather than a deep clone of
    /// the `cap_vtable` (which carries a `HashMap<u32, Arc<dyn ...>>`
    /// of host-fn registrations). Mutating `with_*` methods on
    /// `BytecodeEvaluator` go through `Arc::make_mut`, copying once
    /// on first mutation after a hot share — in practice the
    /// evaluator's setup is single-owner and the make_mut is in-place.
    default_config: Arc<BcVmConfig>,
    /// M2-C lever 5: cached return-shape descriptor derived from the
    /// `return_schema`. Computed once at construction so the per-call
    /// `run_main` epilogue avoids re-walking the schema on every
    /// invoke.
    return_shape: ReturnShape,
    /// M2-C lever 5: cached `return_schema.fields.len() as u32`. The
    /// VM needs this on every `invoke_from_with_stack` call to size the
    /// local-slot reservation past the args span; without the cache the
    /// hot path paid an `Option<&Schema>` walk + a `.fields.len()`
    /// re-read on each invoke. Stored alongside `return_shape` so the
    /// scalar fast-path and the trait-level `run_main` epilogue can
    /// both consume the cached value without the extra schema probe.
    cached_return_field_count: u32,
    /// M2-C lever 5: cached arity of the `#main(...)` declaration.
    /// Mirrors `param_names.len() as usize` but kept on a separate
    /// field so the typed-i64 fast path can validate caller arity
    /// against a single field read.
    cached_param_count: usize,
    /// PC-alignment follow-up #3: clone of the entry function's IR
    /// `body` op stream, retained so the trace recorder can walk the
    /// **same** IR the bytecode compile pass consumed.
    ///
    /// Why this lives on the evaluator: the recorder's `external_pc`
    /// monotone counter is bumped once per `record_op` call; the
    /// bytecode compiler's `ir_pc_next` is bumped once per
    /// `compile_one` call. Both visit the same IR `Op` per increment,
    /// so feeding the recorder a **different** body (e.g. a hand-built
    /// fixture) makes the two counters diverge — the guard's
    /// `external_pc` then routes resume to a bytecode index whose
    /// operand-stack recipe expects a different type lane than the
    /// snapshot's `ssa_slots_copy` carries (e.g. `String`-handle stack
    /// vs `i64` SSA values for an integer-overflow trace shape).
    ///
    /// Hosts that orchestrate the recorder registration externally
    /// (`relon_codegen_cranelift::register_recording`) read this slot via
    /// [`Self::recording_registration_data`] to keep the recorder body
    /// and the bytecode body in lock-step. Empty when the evaluator
    /// was built from a synthetic IR fixture that didn't surface its
    /// body — the legacy `from_ir_legacy` path still populates the
    /// slot, so the production path stays fully aligned.
    entry_body: Vec<TaggedOp>,
    /// PC-alignment Layer 1: cached `field_offset → arg_slot` map
    /// (mirror of the bytecode compile pass's `field_offset_to_local`).
    ///
    /// Surfaced via [`Self::recording_registration_data`] so the trace
    /// recorder walker can resolve no-base `Op::LoadField` /
    /// `Op::LoadStringPtr` / `Op::StoreField` ops against the same arg
    /// slot layout the bytecode VM populates. Empty when the evaluator
    /// was built via the legacy `from_ir_legacy` path (where the IR
    /// uses synthetic `Op::LocalGet(idx)` directly).
    field_offset_to_local: std::collections::BTreeMap<u32, u32>,
    /// `#native` imports the lowering pass interned for this module, in
    /// `import_idx` order. Carried so [`Self::with_host_fns`] can match
    /// a host-supplied `Arc<dyn RelonFunction>` to the slot the
    /// `BcOp::CallNative` op references by index. Empty for every
    /// host-fn-free source (the common case).
    native_imports: Vec<NativeImport>,
}

/// M2-C lever 5: pre-computed return-shape classification used by the
/// typed fast-path and the standard `unpack_return_slots` epilogue.
///
/// The hot W12 fixture (single-scalar return) hits `SingleScalarInt`;
/// the fallback variants preserve the existing schema-walk semantics
/// for the multi-field / legacy paths.
#[derive(Debug, Clone, Copy)]
enum ReturnShape {
    /// Legacy / direct-IR path (no schema). Lift one VmValue as
    /// `Value::Int`.
    LegacyI64,
    /// Single-field return whose field name matches
    /// [`relon_ir::RETURN_VALUE_FIELD_NAME`] and whose type is `Int`.
    /// The slot index is `return_field_base`.
    SingleScalarInt,
    /// Single-field return slot whose type is one of the other
    /// scalars (`Bool` / `Null` / `Float`). The dispatch epilogue
    /// branches on the cached type code.
    SingleScalarFloat,
    /// Single-field return slot of type `Bool`.
    SingleScalarBool,
    /// Single-field return slot of type `Null`.
    SingleScalarNull,
    /// Bytecode-coverage-expansion B-2: single-field return whose
    /// type is `String`. The slot at `return_field_base` holds a
    /// `StringArena` handle; the dispatch epilogue lifts the payload
    /// through [`BcRunOutcome::final_strings`] before the arena
    /// drops.
    SingleScalarString,
    /// Multi-field return record — falls back to the
    /// `unpack_return_slots` branded-dict reconstruction.
    BrandedDict,
}

impl BytecodeEvaluator {
    /// Drive the full pipeline: parse → analyze → IR lower → bytecode
    /// compile.
    pub fn from_source(src: &str) -> Result<Self, BytecodeError> {
        // Open follow-up #2: run the analyzer with `strict_mode: false`
        // so the v1.5 / v1.6 type-surface bans (`ClosureParamTypeMissing`,
        // `ClosureReturnTypeUnknown`, `ExpressionTypeUnknown`, untyped
        // list element warnings, etc.) don't bail the bytecode build
        // for sources the tree-walker accepts. The bans surface real
        // user-facing diagnostics through the tree-walker entry point;
        // bytecode is the deopt landing pad for trace_jit and an
        // internal lowering tier, not a separate user-facing linter.
        //
        // Structural / lowering-relevant diagnostics (`UnknownTypeName`,
        // `MainReturnTypeMismatch`, `FnCallArgTypeMismatch`, ...) still
        // surface as errors under non-strict mode and still gate the
        // build; this relaxation only drops the strict-mode-only soft
        // checks that the tree-walker also tolerates.
        let options = relon_analyzer::AnalyzeOptions {
            strict_mode: false,
            ..Default::default()
        };
        Self::from_source_with_options(src, &options)
    }

    /// Like [`Self::from_source`] but with caller-supplied analyzer
    /// options. This is the entry point for host-registered native
    /// fns: the host populates `options.host_fn_names` /
    /// `host_fn_signatures` / `host_fn_gates` / `caps` so the analyzer
    /// resolves the calls, runs the static capability-reachability
    /// check (a gated call without the granted cap fails the build
    /// here, before any bytecode runs), and the IR lowering pass emits
    /// the `Op::CheckCap`-guarded `Op::CallNative`. Pair with
    /// [`Self::with_host_fns`] to register the `Arc<dyn RelonFunction>`
    /// callables the dispatcher invokes, and [`Self::with_granted_cap`]
    /// to set the runtime grant the per-call consult checks.
    pub fn from_source_with_options(
        src: &str,
        options: &relon_analyzer::AnalyzeOptions,
    ) -> Result<Self, BytecodeError> {
        let ast =
            relon_parser::parse_document(src).map_err(|e| BytecodeError::Parse(e.to_string()))?;
        // Compiled backends analyze per-file with no workspace pass, so
        // force the single-file capability-reachability check on: a
        // gated native call without the granted cap must fail the build
        // here, not slip through to a runtime-only trap.
        let options = relon_analyzer::AnalyzeOptions {
            standalone_capability_check: true,
            ..options.clone()
        };
        let analyzed = relon_analyzer::analyze_with_options(&ast, &options);
        if analyzed.has_errors() {
            let err_count = analyzed
                .diagnostics
                .iter()
                .filter(|d| d.severity() == relon_analyzer::Severity::Error)
                .count();
            return Err(BytecodeError::Analyze(err_count));
        }
        let lowered = relon_ir::lower_workspace_single(&analyzed, &ast)
            .map_err(|e| BytecodeError::Lowering(e.to_string()))?;
        let main_schema = lowered.main_schema;
        let return_schema = lowered.return_schema;

        // Reject argument types outside the M2-A scalar envelope. The
        // VM's compile pass only knows how to lift Int / Bool / Null
        // / Float scalars into virtual locals; list / dict / nested-
        // schema args require buffer-protocol arena loads the
        // scaffold deliberately omits.
        for field in &main_schema.fields {
            if !is_scalar_field(&field.ty) {
                return Err(BytecodeError::UnsupportedEntry {
                    reason: format!(
                        "M2-A scaffold only supports scalar args, got `{}: {:?}`",
                        field.name, field.ty
                    ),
                });
            }
        }
        for field in &return_schema.fields {
            if !is_scalar_field(&field.ty) {
                return Err(BytecodeError::UnsupportedEntry {
                    reason: format!(
                        "M2-A scaffold only supports scalar return fields, got `{}: {:?}`",
                        field.name, field.ty
                    ),
                });
            }
        }

        let main_layout = SchemaLayout::offsets_for(&main_schema)
            .map_err(|e| BytecodeError::Lowering(format!("main schema layout: {e}")))?;
        let return_layout = SchemaLayout::offsets_for(&return_schema)
            .map_err(|e| BytecodeError::Lowering(format!("return schema layout: {e}")))?;

        let in_map = build_offset_to_local(&main_layout);
        let out_map = build_offset_to_local(&return_layout);
        let param_names: Vec<String> = main_schema.fields.iter().map(|f| f.name.clone()).collect();
        let param_tys: Vec<IrType> = main_schema
            .fields
            .iter()
            .map(|f| ir_type_for_field(&f.ty))
            .collect();

        let return_field_base = in_map.len() as u32;

        Self::from_ir_with_maps(
            lowered.module,
            param_names,
            param_tys,
            return_schema,
            return_field_base,
            &in_map,
            &out_map,
        )
    }

    /// Construct from a pre-lowered IR module + manual param info.
    /// Used by direct-IR tests / differential harnesses that bypass
    /// the parse + analyze stages.
    ///
    /// Bytecode-coverage-expansion B-4: the `field_offset_to_local`
    /// map is seeded with `params.len()` synthetic slots — one for
    /// each declared parameter — so the compile pass's
    /// `input_arg_count()` returns the right value. Without this seed,
    /// the let-local base would calculate to `0` and let-bound slots
    /// would collide with the arg slots the VM populates from
    /// `pack_args`. The seed offsets use placeholder values
    /// (`0, 1, 2, ...`) — they're only consulted by
    /// `visit_load_field` / `visit_store_field` which the legacy
    /// callers never emit (they go through `LocalGet` / `LocalSet`).
    pub fn from_ir_legacy(
        module: IrModule,
        param_names: Vec<String>,
    ) -> Result<Self, BytecodeError> {
        let entry_idx = module.entry_func_index.ok_or(BytecodeError::NoEntry)?;
        let func = &module.funcs[entry_idx];
        let entry_range = func.range;
        let param_tys = func.params.clone();
        // PC-alignment follow-up #3: clone the body so the recorder
        // registration path can walk the same IR the compile pass
        // sees. See `entry_body` field docs for the alignment
        // invariant.
        let entry_body = func.body.clone();
        // Seed the input-offset map with one entry per declared param
        // so `compile_function`'s let-local base resolves past the
        // arg slots. The offset key is synthetic — the legacy IR
        // never emits `LoadField` / `StoreField` against these slots.
        let in_map: std::collections::BTreeMap<u32, u32> =
            (0..func.params.len() as u32).map(|i| (i, i)).collect();
        let empty = std::collections::BTreeMap::new();
        let compiled = compile_function(func, &in_map, &empty)?;
        let cached_param_count = param_names.len();
        let native_imports = module.imports.clone();
        Ok(Self {
            func: compiled,
            entry_range,
            param_names,
            param_tys,
            return_schema: None,
            return_field_base: 0,
            default_config: Arc::new(BcVmConfig::default()),
            return_shape: ReturnShape::LegacyI64,
            cached_return_field_count: 0,
            cached_param_count,
            entry_body,
            // Legacy IR uses `Op::LocalGet(idx)` directly, so the
            // recorder walker doesn't need an offset→slot rewrite.
            field_offset_to_local: std::collections::BTreeMap::new(),
            native_imports,
        })
    }

    fn from_ir_with_maps(
        module: IrModule,
        param_names: Vec<String>,
        param_tys: Vec<IrType>,
        return_schema: Schema,
        return_field_base: u32,
        in_map: &std::collections::BTreeMap<u32, u32>,
        out_map: &std::collections::BTreeMap<u32, u32>,
    ) -> Result<Self, BytecodeError> {
        let entry_idx = module.entry_func_index.ok_or(BytecodeError::NoEntry)?;
        let native_imports = module.imports.clone();
        let func = &module.funcs[entry_idx];
        let entry_range = func.range;
        // PC-alignment follow-up #3: clone the body so the recorder
        // registration path can walk the same IR the compile pass
        // sees. See `entry_body` field docs for the alignment
        // invariant.
        let entry_body = func.body.clone();
        // v6-δ M2-B: thread the full `funcs` slice through so the
        // bytecode compile pass can inline simple callees
        // (`Op::Call`) the M2-A scaffold rejected. Phase D widens
        // the entry shape with the module's `closure_table` so
        // `Op::MakeClosure` resolves to the lambda's `Func` index.
        let compiled = compile_function_with_closures(
            func,
            &module.funcs,
            &module.closure_table,
            in_map,
            out_map,
        )?;
        // M2-C lever 5: classify the return schema once so the hot
        // dispatch epilogue branches on a cheap copy-enum rather than
        // re-walking the field vector on every invoke.
        let return_shape = classify_return_shape(&return_schema);
        let cached_return_field_count = return_schema.fields.len() as u32;
        let cached_param_count = param_names.len();
        Ok(Self {
            func: compiled,
            entry_range,
            param_names,
            param_tys,
            return_schema: Some(return_schema),
            return_field_base,
            default_config: Arc::new(BcVmConfig::default()),
            return_shape,
            cached_return_field_count,
            cached_param_count,
            entry_body,
            field_offset_to_local: in_map.clone(),
            native_imports,
        })
    }

    /// Expose the compiled function — used by the differential test
    /// harness to inspect the `ir_pc_map` invariants.
    pub fn function(&self) -> &BcFunction {
        &self.func
    }

    /// PC-alignment follow-up #3: surface the entry function's IR body
    /// and declared parameter types so a host can drive the trace
    /// recorder against the **same** IR the bytecode compile pass
    /// consumed.
    ///
    /// Returns a [`RecordingRegistrationData`] view that the
    /// `relon-codegen-cranelift` crate consumes via its
    /// `register_recording` API. Cloning the body is the slow path
    /// — the host only registers once per `fn_id`, so the per-clone
    /// cost is amortised across every subsequent trace invocation.
    ///
    /// ## Why bytecode hosts need this
    ///
    /// Without this accessor, hosts have to hand-roll the IR body
    /// the recorder walks, which is almost always a different shape
    /// than the bytecode-compiled body. The two PC counters then
    /// diverge: a guard's `external_pc` routes resume to a bytecode
    /// index whose operand-stack recipe expects a different value
    /// lane than the snapshot carries (e.g. `String`-handle stack
    /// vs `i64` SSA values for an integer-overflow trace shape).
    ///
    /// Tests that exercise the dispatcher / deopt path with string-
    /// shape sources go through this accessor so the trace records
    /// against the production-lowered IR and PC alignment holds.
    pub fn recording_registration_data(&self) -> RecordingRegistrationData {
        RecordingRegistrationData {
            body: self.entry_body.clone(),
            param_tys: self.param_tys.clone(),
            field_offset_to_local: self.field_offset_to_local.clone(),
        }
    }

    /// Override the default VM config (max_steps / deadline / cap
    /// vtable). Consumes the owned config and wraps in `Arc` so
    /// subsequent `run_main` calls hand the VM a refcount-bumped
    /// share rather than a deep clone.
    pub fn with_config(mut self, config: BcVmConfig) -> Self {
        self.default_config = Arc::new(config);
        self
    }

    /// M2-B phase 1: install the unified
    /// [`CapabilityGate`] policy boundary on the
    /// evaluator's default VM config. Subsequent `run_main` /
    /// `resume_from_*` calls inherit the gate; per-call configs built
    /// via [`Self::with_config`] are responsible for carrying their
    /// own gate.
    ///
    /// Phase 2 wires this into two enforcement points on the dispatch
    /// path:
    ///
    /// * Pre-dispatch sweep — `BytecodeVm::invoke_from_with_stack`
    ///   consults the gate for every grant-table bit before the first
    ///   op runs; a denial trips
    ///   [`relon_eval_api::RuntimeError::CapabilityDenied`] with the failing bit.
    /// * Trap enrichment — when `BcOp::Trap(CapabilityDenied)` fires
    ///   in a hand-built BcFunction, the VM consults the gate to
    ///   substitute the legacy `u32::MAX` sentinel with the first
    ///   gate-denied [`relon_eval_api::CapabilityBit`].
    ///
    /// For the M2-A scaffold envelope (scalar arith / cmp / control
    /// flow only) the standard `from_source` compile pass emits no
    /// grants and no capability traps, so calling this method remains
    /// behaviourally a no-op on those sources. Hosts that register the
    /// gate ahead of phase 3 IR coverage get the consult mechanism
    /// for free as soon as guarded ops land.
    pub fn with_capability_gate(mut self, gate: Arc<dyn CapabilityGate>) -> Self {
        Arc::make_mut(&mut self.default_config)
            .cap_vtable
            .set_gate(gate);
        self
    }

    /// Register the host's `Arc<dyn RelonFunction>` callables for
    /// native-fn dispatch. Each entry is keyed by the source-level fn
    /// name; this method matches the name to the `import_idx` the
    /// lowering pass assigned (via [`Self::native_imports`]) and
    /// installs the callable at that slot in the VM's capability
    /// vtable. Names with no matching `#native` import in the compiled
    /// module are silently skipped — a registered fn the source never
    /// calls simply isn't lowered, so it has no slot to fill.
    ///
    /// The capability guard is enforced independently by the
    /// `Op::CheckCap` prologue against the grant set via
    /// [`Self::with_granted_cap`] / the installed gate — registering a
    /// callable does **not** grant its capability.
    pub fn with_host_fns(mut self, host_fns: &HashMap<String, Arc<dyn RelonFunction>>) -> Self {
        let slots: Vec<(u32, Arc<dyn RelonFunction>)> = self
            .native_imports
            .iter()
            .enumerate()
            .filter_map(|(idx, imp)| host_fns.get(&imp.name).map(|f| (idx as u32, Arc::clone(f))))
            .collect();
        let cfg = Arc::make_mut(&mut self.default_config);
        for (idx, func) in slots {
            cfg.cap_vtable.register_host_fn(idx, func);
        }
        self
    }

    /// Grant a capability bit on the evaluator's default VM config so
    /// the per-call `Op::CheckCap` / `Op::CallNative` consult passes at
    /// runtime. Decoupled from the analyze-time `caps`: a host can
    /// grant the bit statically (so the build passes the reachability
    /// check) yet withhold it here to exercise a stricter runtime
    /// posture (the call then traps `CapabilityDenied`).
    pub fn with_granted_cap(mut self, bit: u32) -> Self {
        Arc::make_mut(&mut self.default_config)
            .cap_vtable
            .grant(bit);
        self
    }

    /// The `#native` imports the lowering pass interned for this
    /// module, in `import_idx` order. Lets a host map fn names to the
    /// slots [`Self::with_host_fns`] fills.
    pub fn native_imports(&self) -> &[NativeImport] {
        &self.native_imports
    }

    /// M2-B phase 4c: install the trace-JIT hot-counter trigger on the
    /// default VM config. Returns `self` so the call chains cleanly
    /// off `from_source`.
    ///
    /// The trigger is consulted on every `run_main` invocation; when
    /// the per-`fn_id` counter crosses the configured threshold (see
    /// [`Self::with_hot_threshold`]), `trigger.on_hot(fn_id, args)`
    /// fires exactly once. Hosts using the cranelift adapter
    /// (`relon_codegen_cranelift::CraneliftHotTrigger`) get the standard
    /// `__relon_jump_to_recorder` recording-driver pipeline; tests can
    /// install a mock to observe the dispatch shape.
    ///
    /// The host is also responsible for stamping a non-`None`
    /// [`crate::BcFunction::fn_id`] on the compiled function —
    /// without one the prologue stays inert. The convenience
    /// [`Self::with_fn_id`] handles the common case where the host
    /// wants the bytecode artefact and the matching cranelift trace
    /// to share the same id.
    pub fn with_hot_trigger(mut self, trigger: crate::hot_counter::HotTraceTriggerHandle) -> Self {
        Arc::make_mut(&mut self.default_config).hot_trigger = Some(trigger);
        self
    }

    /// M2-B phase 4c: override the default hot-counter threshold.
    /// `1` triggers on the very first invocation (smoke-test mode);
    /// the default 1000 mirrors the LuaJIT-style conservative kickoff
    /// the cranelift backend uses.
    pub fn with_hot_threshold(mut self, threshold: u32) -> Self {
        Arc::make_mut(&mut self.default_config).hot_threshold = threshold;
        self
    }

    /// M2-B phase 4c: stamp the cross-backend `fn_id` on the compiled
    /// function. The slot drives the hot-counter prologue's per-id
    /// lookup and matches the id under which the host registered a
    /// [`relon_codegen_cranelift::trace_install::RecordingRegistration`]
    /// for the recorder.
    pub fn with_fn_id(mut self, fn_id: u32) -> Self {
        // `self.func` is owned here, so stamp the id in place instead of
        // deep-cloning the whole `BcFunction` just to set one field
        // (mirrors `BcFunction::with_fn_id`, which only assigns `fn_id`).
        self.func.fn_id = Some(fn_id);
        self
    }

    /// M2-B phase 4c-cont: install the installed-trace lookup on the
    /// default VM config. When set, every `run_main` invocation first
    /// consults the lookup; a hit bypasses the bytecode dispatch loop
    /// entirely (the trace fn writes its return value directly into
    /// the schema's return slot) and a guard-failed trace routes
    /// through [`Self::resume_from_snapshot`] for partial-resume.
    ///
    /// Pair this with [`Self::with_fn_id`] — without a `fn_id` on the
    /// compiled function the dispatcher-switch path is inert.
    pub fn with_trace_lookup(
        mut self,
        lookup: crate::trace_dispatch::InstalledTraceLookupHandle,
    ) -> Self {
        Arc::make_mut(&mut self.default_config).trace_lookup = Some(lookup);
        self
    }

    /// Inspect the entry source range.
    pub fn entry_range(&self) -> TokenRange {
        self.entry_range
    }

    fn pack_args(&self, args: &HashMap<String, Value>) -> Result<Vec<VmValue>, RuntimeError> {
        // Backward-compatible scalar-only packer. Callers that need
        // string lift go through `pack_args_with_strings` directly.
        let (packed, _) = self.pack_args_with_strings(args)?;
        Ok(packed)
    }

    /// Bytecode-coverage-expansion B-2: returns the per-slot packed
    /// `u64` array alongside the `(slot_idx, payload)` list the VM
    /// needs to intern host-supplied strings into the per-invoke
    /// `StringArena`. The packed slot for a string arg holds a `0`
    /// placeholder; the VM overwrites it with the arena handle as
    /// part of its prologue (see
    /// [`BytecodeVm::invoke_from_with_string_args`]).
    #[allow(clippy::type_complexity)]
    fn pack_args_with_strings(
        &self,
        args: &HashMap<String, Value>,
    ) -> Result<(Vec<VmValue>, Vec<(usize, String)>), RuntimeError> {
        let mut packed = Vec::with_capacity(self.param_names.len());
        let mut string_args: Vec<(usize, String)> = Vec::new();
        for (i, name) in self.param_names.iter().enumerate() {
            let value = args.get(name).or_else(|| args.get(&format!("arg{i}")));
            let value = value.ok_or_else(|| RuntimeError::MissingMainArg {
                name: name.clone(),
                range: self.entry_range,
            })?;
            let ty = self.param_tys.get(i).copied().unwrap_or(IrType::I64);
            match (value, ty) {
                (Value::String(s), IrType::String) => {
                    // Placeholder slot — VM rewrites to handle after
                    // arena alloc.
                    packed.push(0u64);
                    string_args.push((i, s.as_str().to_string()));
                }
                (other, _) => {
                    packed.push(value_to_vm(other, ty, name, self.entry_range)?);
                }
            }
        }
        Ok((packed, string_args))
    }

    /// Bytecode-coverage-expansion B-2: list of local slots whose
    /// return-schema type is `String`. The VM lifts each slot's arena
    /// payload into [`BcRunOutcome::final_strings`] before drop so
    /// `unpack_return_slots_with_strings` can rebuild the
    /// `Value::String` without retaining the dead arena.
    fn string_return_slots(&self) -> Vec<usize> {
        let Some(schema) = self.return_schema.as_ref() else {
            return Vec::new();
        };
        use relon_eval_api::schema_canonical::TypeRepr;
        let base = self.return_field_base as usize;
        schema
            .fields
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                if matches!(f.ty, TypeRepr::String) {
                    Some(base + i)
                } else {
                    None
                }
            })
            .collect()
    }

    /// M2-B phase 4c-cont: lift the trace's `result_slot` value into
    /// a Relon [`Value`].
    ///
    /// The bytecode VM's regular `Return` path goes through
    /// [`Self::unpack_return_slots`] which reads `final_locals`
    /// (populated by the schema-driven `StoreField` lowering). On the
    /// trace-bypass path we don't run the dispatch loop, so there's
    /// no `final_locals` snapshot — the trace fn writes its return
    /// value into `TraceContext::result_slot`, which we get back as a
    /// raw `u64`. Decode against the return schema's first (and, for
    /// the M2-B trace envelope, only) field. Multi-field returns are
    /// out of scope until the trace runtime widens
    /// [`relon_trace_abi::TraceContext`] beyond a single `result_slot`
    /// — until then a multi-field schema falls back to the bytecode
    /// path via the recorder declining to compile a multi-output
    /// trace.
    fn pack_trace_result(&self, result: u64) -> Value {
        let Some(schema) = self.return_schema.as_ref() else {
            return Value::Int(result as i64);
        };
        if schema.fields.len() == 1 {
            return decode_field(&schema.fields[0].ty, result);
        }
        // Multi-field return: trace envelope only carries the first
        // slot; remaining fields stay at the zero-init value the
        // dispatch loop would have produced. This shape is not
        // expected in production today (trace recorder declines to
        // compile when the IR has more than one StoreField), but we
        // surface a defined value rather than panicking so unit tests
        // that exercise the dispatcher with synthetic schemas don't
        // fall off a cliff.
        let mut map: std::collections::BTreeMap<SmolStr, Value> = std::collections::BTreeMap::new();
        for (i, f) in schema.fields.iter().enumerate() {
            let raw = if i == 0 { result } else { 0 };
            map.insert(SmolStr::from(f.name.as_str()), decode_field(&f.ty, raw));
        }
        Value::branded_dict(map, Some(schema.name.clone()))
    }

    fn unpack_return_slots(&self, locals: &[VmValue]) -> Value {
        // Backward-compatible scalar-only variant. String-return
        // shapes ride through `unpack_return_slots_with_strings`.
        self.unpack_return_slots_with_strings(locals, &Default::default(), None)
    }

    /// Bytecode-coverage-expansion B-2: same as
    /// [`Self::unpack_return_slots`] but consumes the
    /// `BcRunOutcome::final_strings` map for string-return shapes and
    /// the popped-stack `BcRunOutcome::value` for the legacy-i64
    /// shape (where the result lives on the operand stack instead of
    /// a virtual return slot).
    fn unpack_return_slots_with_strings(
        &self,
        locals: &[VmValue],
        final_strings: &std::collections::HashMap<usize, String>,
        return_value: Option<VmValue>,
    ) -> Value {
        // M2-C lever 5: branch on the cached `ReturnShape` so the hot
        // single-scalar epilogue avoids the `Option<&Schema>` +
        // schema-fields walk it previously paid every invoke.
        let slot = self.return_field_base as usize;
        match self.return_shape {
            ReturnShape::LegacyI64 => {
                // Bytecode-coverage-expansion B-4: the legacy direct-IR
                // shape (`from_ir_legacy`) ends `run_main` with the
                // result on the operand stack and a bare `Op::Return`,
                // so the canonical answer is `outcome.value`. The
                // pre-B-4 read of `locals[0]` only happened to work
                // for single-arg IR where the body's `Return` value
                // was the arg slot itself — multi-arg IR (toggle_loop
                // shape: `(n, toggle) -> Int`) returned `locals[0]`
                // = `n` and produced the wrong answer.
                Value::Int(
                    return_value
                        .or_else(|| locals.first().copied())
                        .unwrap_or(0) as i64,
                )
            }
            ReturnShape::SingleScalarInt => {
                Value::Int(locals.get(slot).copied().unwrap_or(0) as i64)
            }
            ReturnShape::SingleScalarBool => {
                Value::Bool((locals.get(slot).copied().unwrap_or(0) as u32) != 0)
            }
            ReturnShape::SingleScalarNull => Value::Null,
            ReturnShape::SingleScalarFloat => {
                use ordered_float::OrderedFloat;
                let bits = locals.get(slot).copied().unwrap_or(0);
                Value::Float(OrderedFloat(f64::from_bits(bits)))
            }
            ReturnShape::SingleScalarString => {
                // Bytecode-coverage-expansion B-2: the VM lifted the
                // arena payload into `final_strings` before drop. An
                // absent entry (e.g. an early trap path) falls back
                // to an empty string so the type still matches.
                let payload = final_strings.get(&slot).cloned().unwrap_or_default();
                Value::String(SmolStr::from(payload.as_str()))
            }
            ReturnShape::BrandedDict => {
                // Multi-field return record. The schema must be
                // present — `BrandedDict` is set by
                // `classify_return_shape` only when one was supplied.
                let schema = self
                    .return_schema
                    .as_ref()
                    .expect("BrandedDict implies Some(return_schema)");
                use relon_eval_api::schema_canonical::TypeRepr;
                let mut map: std::collections::BTreeMap<SmolStr, Value> =
                    std::collections::BTreeMap::new();
                for (i, f) in schema.fields.iter().enumerate() {
                    let s = self.return_field_base as usize + i;
                    let v = if matches!(f.ty, TypeRepr::String) {
                        let payload = final_strings.get(&s).cloned().unwrap_or_default();
                        Value::String(SmolStr::from(payload.as_str()))
                    } else {
                        decode_field(&f.ty, locals.get(s).copied().unwrap_or(0))
                    };
                    map.insert(SmolStr::from(f.name.as_str()), v);
                }
                Value::branded_dict(map, Some(schema.name.clone()))
            }
        }
    }

    /// Internal `run_main` core — kept separate so `resume_from_pc`
    /// can route in without re-validating args.
    fn run_main_inner(
        &self,
        args: &HashMap<String, Value>,
        start_bc_idx: usize,
        extra_locals: &[VmValue],
        initial_stack: &[VmValue],
    ) -> Result<Value, RuntimeError> {
        // Bytecode-coverage-expansion B-2: pack args via the string-
        // aware path and forward the lift sides through the VM. The
        // refactored entry stays scalar-equivalent for non-string
        // signatures (`string_args` empty + `string_return_slots`
        // empty → identical dispatch to the previous code path).
        let (packed, string_args) = self.pack_args_with_strings(args)?;
        let string_arg_refs: Vec<(usize, &str)> = string_args
            .iter()
            .map(|(slot, payload)| (*slot, payload.as_str()))
            .collect();
        let string_return_slots = self.string_return_slots();
        let vm = BytecodeVm::new(self.default_config.clone());
        let outcome = vm.invoke_from_with_string_io(
            &self.func,
            &packed,
            start_bc_idx,
            extra_locals,
            /*return_slot_count=*/
            self.cached_return_slot_count(),
            initial_stack,
            &string_arg_refs,
            &string_return_slots,
        );
        if let Some(err) = outcome.error {
            return Err(err.into_runtime_error(self.entry_range));
        }
        // Buffer-protocol path: the VM's `Return` value is the
        // bytes_written placeholder (not used by the bytecode VM);
        // the actual return data lives in the virtual return slots
        // — plus `final_strings` for any String-typed return field.
        // For the legacy direct-IR shape the popped stack value is
        // the canonical return — see `ReturnShape::LegacyI64`.
        Ok(self.unpack_return_slots_with_strings(
            &outcome.final_locals,
            &outcome.final_strings,
            outcome.value,
        ))
    }

    /// Rebuild the operand stack at `bc_idx` from the compile-time
    /// recipe + the supplied snapshot fragments.
    ///
    /// Each [`StackOrigin`] is materialised independently:
    ///
    /// - `Local(slot)` — read `args[slot]` (for the input-arg span)
    ///   or `extra_locals[slot - overlay_base]` for let-bound slots.
    ///   When the slot points past the args **and** the snapshot
    ///   doesn't carry that local (extra_locals shorter than the
    ///   referenced let-slot), the recipe slot falls back to `0` —
    ///   the M2-B trade-off: deep mid-expression resumes without a
    ///   matching local snapshot still produce a defined value, but
    ///   may diverge from the original computation. Tests cover the
    ///   common shapes (input-arg only / single let).
    /// - `Const(v)` — push the literal.
    /// - `Snapshot(idx)` — read `value_stack_copy[idx]`; if absent,
    ///   fall back to `0` so the resume never panics.
    fn materialise_stack(
        &self,
        bc_idx: usize,
        args_packed: &[VmValue],
        extra_locals: &[VmValue],
        value_stack_copy: &[u64],
    ) -> Vec<VmValue> {
        let recipe = match self.func.stack_recipe.get(bc_idx) {
            Some(r) => r,
            None => return Vec::new(),
        };
        let overlay_base = args_packed.len()
            + self
                .return_schema
                .as_ref()
                .map(|s| s.fields.len())
                .unwrap_or(0);
        let mut out = Vec::with_capacity(recipe.len());
        for entry in recipe {
            let v = match entry {
                StackOrigin::Local(slot) => {
                    let idx = *slot as usize;
                    if idx < args_packed.len() {
                        args_packed[idx]
                    } else if idx >= overlay_base && idx - overlay_base < extra_locals.len() {
                        extra_locals[idx - overlay_base]
                    } else {
                        0
                    }
                }
                StackOrigin::Const(v) => *v,
                StackOrigin::Snapshot(idx) => {
                    value_stack_copy.get(*idx as usize).copied().unwrap_or(0)
                }
            };
            out.push(v);
        }
        out
    }
}

/// M2-C lever 5: classify the return schema into a `ReturnShape` once
/// at construction. The hot epilogue then switches on the cheap
/// `Copy`-enum rather than walking `schema.fields` on every invoke.
fn classify_return_shape(schema: &Schema) -> ReturnShape {
    use relon_eval_api::schema_canonical::TypeRepr;
    if schema.fields.len() == 1 && schema.fields[0].name == relon_ir::RETURN_VALUE_FIELD_NAME {
        return match schema.fields[0].ty {
            TypeRepr::Int => ReturnShape::SingleScalarInt,
            TypeRepr::Float => ReturnShape::SingleScalarFloat,
            TypeRepr::Bool => ReturnShape::SingleScalarBool,
            TypeRepr::Null => ReturnShape::SingleScalarNull,
            TypeRepr::String => ReturnShape::SingleScalarString,
            _ => ReturnShape::BrandedDict,
        };
    }
    ReturnShape::BrandedDict
}

/// Decide whether a schema field type is in the M2-A scalar envelope.
fn is_scalar_field(ty: &relon_eval_api::schema_canonical::TypeRepr) -> bool {
    use relon_eval_api::schema_canonical::TypeRepr;
    // Bytecode-coverage-expansion B-2: `String` is included in the
    // scalar envelope because the IR-lift path lowers `String` args /
    // returns to the same u64-shaped record slot the dispatch loop
    // already uses for handles. The actual payload lives in the
    // VM's per-invoke `StringArena`; the slot itself holds the
    // arena handle. Lift in/out happens through
    // `BytecodeVm::invoke_from_with_string_io`.
    matches!(
        ty,
        TypeRepr::Int | TypeRepr::Float | TypeRepr::Bool | TypeRepr::Null | TypeRepr::String
    )
}

fn ir_type_for_field(ty: &relon_eval_api::schema_canonical::TypeRepr) -> IrType {
    use relon_eval_api::schema_canonical::TypeRepr;
    match ty {
        TypeRepr::Int => IrType::I64,
        TypeRepr::Float => IrType::F64,
        TypeRepr::Bool => IrType::Bool,
        TypeRepr::Null => IrType::Null,
        TypeRepr::String => IrType::String,
        _ => IrType::I64,
    }
}

fn decode_field(ty: &relon_eval_api::schema_canonical::TypeRepr, raw: u64) -> Value {
    use ordered_float::OrderedFloat;
    use relon_eval_api::schema_canonical::TypeRepr;
    match ty {
        TypeRepr::Int => Value::Int(raw as i64),
        TypeRepr::Float => Value::Float(OrderedFloat(f64::from_bits(raw))),
        TypeRepr::Bool => Value::Bool((raw as u32) != 0),
        TypeRepr::Null => Value::Null,
        // Bytecode-coverage-expansion B-2: `String`-typed return slots
        // are recovered through the `BcRunOutcome::final_strings` lift
        // (the VM reads the arena before drop). The fallback here is
        // an empty string for the rare path where a caller invokes
        // `decode_field` without going through the lift-aware
        // `unpack_return_slots_v2` — keeps behaviour defined.
        TypeRepr::String => Value::String(relon_eval_api::SmolStr::default()),
        _ => Value::Int(raw as i64),
    }
}

/// Map a Relon scalar [`Value`] into the VM's `u64` slot.
fn value_to_vm(
    value: &Value,
    ty: IrType,
    name: &str,
    range: TokenRange,
) -> Result<VmValue, RuntimeError> {
    match (value, ty) {
        (Value::Int(v), IrType::I64) | (Value::Int(v), IrType::I32) => Ok(*v as u64),
        (Value::Bool(b), IrType::Bool) => Ok(if *b { 1 } else { 0 }),
        (Value::Null, IrType::Null) => Ok(0),
        (Value::Float(f), IrType::F64) => Ok(f.0.to_bits()),
        (other, _) => Err(RuntimeError::MainArgTypeMismatch {
            name: name.to_string(),
            expected: ir_type_name(ty),
            found: other.type_name().to_string(),
            range,
        }),
    }
}

fn ir_type_name(ty: IrType) -> String {
    match ty {
        IrType::I32 | IrType::I64 => "Int".into(),
        IrType::F64 => "Float".into(),
        IrType::Bool => "Bool".into(),
        IrType::Null => "Null".into(),
        IrType::String => "String".into(),
        IrType::ListInt
        | IrType::ListFloat
        | IrType::ListBool
        | IrType::ListString
        | IrType::ListSchema => "List".into(),
        IrType::Closure => "Closure".into(),
    }
}

impl Evaluator for BytecodeEvaluator {
    fn eval(&self, _node: &Node, _scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "bytecode VM backend: `eval` requires AST access; use the tree-walking backend"
                .into(),
        })
    }

    fn eval_root(&self, _scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason:
                "bytecode VM backend: `eval_root` requires AST access; use the tree-walking backend"
                    .into(),
        })
    }

    fn run_main(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        // M2-B phase 4c-cont: dispatcher switch. When an installed
        // trace exists for this `fn_id`, bypass the bytecode dispatch
        // loop and route through the JIT'd trace fn. The cranelift
        // backend's entry-fn prologue does the same thing in machine
        // code; the bytecode VM does it in Rust at the evaluator
        // boundary.
        //
        // The three outcomes:
        //   - `NoTrace`        — fall through to regular dispatch.
        //   - `Success { r }`  — trace ran clean; pack `r` into the
        //                        return slot and skip the dispatch
        //                        loop entirely (the user-visible win).
        //   - `Deopt { snap }` — a guard fired mid-trace; route the
        //                        snapshot into the bytecode VM's
        //                        partial-resume so dispatch picks up
        //                        exactly where the trace bailed.
        //
        // We only consult the lookup on the **outer** invocation
        // entry; partial-resume re-entries land on the `resume_from_*`
        // paths and don't pass through here, so a deopt → resume →
        // re-deopt cycle can't accidentally bounce off the trace
        // again before the bytecode VM finishes the job.
        if let (Some(fn_id), Some(lookup)) =
            (self.func.fn_id, self.default_config.trace_lookup.as_ref())
        {
            // Bytecode-coverage-expansion B-2: pack via the string-
            // aware path so any `String`-typed arg slot is observed
            // by `try_invoke` (the recorder uses the packed slot to
            // route the call). The placeholder `0` for string slots
            // is overwritten by the VM's prologue if `run_main_inner`
            // is taken; the trace's snapshot copy on the Deopt path
            // sees `0` for the string slot (recorders that observe
            // string args must drive through the dedicated string
            // entry — outside this scaffold's scope) but the resume
            // path always re-runs through `run_main_inner` which
            // re-packs cleanly.
            let (packed, string_args) = self.pack_args_with_strings(&args)?;
            let string_arg_refs: Vec<(usize, &str)> = string_args
                .iter()
                .map(|(slot, payload)| (*slot, payload.as_str()))
                .collect();
            match lookup.try_invoke(fn_id, &packed) {
                crate::trace_dispatch::TraceInvokeOutcome::NoTrace => {
                    // Re-use the pre-packed args via the string-aware
                    // packed-args variant so the string slot picks up
                    // its arena handle on the VM side.
                    return self.run_main_inner_with_packed_strings(
                        &packed,
                        &string_arg_refs,
                        0,
                        &[],
                        &[],
                    );
                }
                crate::trace_dispatch::TraceInvokeOutcome::Success { result } => {
                    return Ok(self.pack_trace_result(result));
                }
                crate::trace_dispatch::TraceInvokeOutcome::Deopt { snapshot } => {
                    // Drive the partial-resume path through the
                    // sub-task-B convenience alias so the call site
                    // explicitly names what's happening. The snapshot
                    // carries `external_pc` + slot copies;
                    // `resume_from_deopt` routes those onto the
                    // bytecode VM's `start_bc_idx`.
                    return self.resume_from_deopt(args, &snapshot);
                }
            }
        }
        self.run_main_inner(&args, /*start_bc_idx=*/ 0, &[], &[])
    }

    fn force_thunk(&self, _thunk: &Arc<Thunk>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "bytecode VM backend: thunks are tree-walker only".into(),
        })
    }

    fn invoke_closure(
        &self,
        _closure: &ClosureData,
        _args: &[Value],
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "bytecode VM backend: closures land in v6-δ M2-B".into(),
        })
    }

    /// v6-δ M2-B: real partial-resume entry point.
    ///
    /// Routes the trace-side `external_pc` to a bytecode index via
    /// [`BcFunction::bc_index_for_pc`], rebuilds the operand stack
    /// from the compile-time [`StackOrigin`] recipe + the supplied
    /// snapshot fragments, then continues dispatch.
    ///
    /// ## Snapshot layout convention
    ///
    /// `local_snapshot` carries — concatenated — the
    /// `DeoptStateSnapshot::ssa_slots_copy` followed by an optional
    /// `value_stack_copy`. We split the slice at the recipe's
    /// expected `Snapshot` count: every `StackOrigin::Snapshot(idx)`
    /// in the recipe consults `value_stack_copy[idx]`. Hosts that
    /// only own an `ssa_slots_copy` (M2-A behaviour) pass it as the
    /// full `local_snapshot`; recipes that need value-stack data will
    /// see zeroes there and fall back gracefully.
    ///
    /// The deopt-driver variant
    /// [`Self::resume_from_snapshot`] supplies the split arms
    /// directly so the host doesn't have to flatten them.
    ///
    /// ## Behaviour
    ///
    /// - `external_pc == 0`: replay from entry (matches `run_main`).
    /// - Known PC at empty-stack boundary: dispatch resumes from
    ///   `bc_idx` directly. The 4-prong sandbox prong tests cover
    ///   this path.
    /// - Known PC mid-expression: the stack recipe rebuilds the
    ///   operand stack; values fall back to `0` for missing recipe
    ///   data (Snapshot indices the host didn't carry).
    /// - Unknown PC: graceful fallback to entry (run_main).
    fn resume_from_pc(
        &self,
        args: HashMap<String, Value>,
        external_pc: u64,
        local_snapshot: &[u64],
    ) -> Result<Value, RuntimeError> {
        let start_bc_idx = self
            .func
            .bc_index_for_pc(external_pc as ExternalPc)
            .unwrap_or(0);
        if start_bc_idx == 0 {
            return self.run_main_inner(&args, /*start_bc_idx=*/ 0, local_snapshot, &[]);
        }
        // Split `local_snapshot` into ssa_slots_copy (used as
        // extra_locals overlay) and value_stack_copy (used for
        // Snapshot recipe entries). M2-B convention: split at the
        // last Snapshot index referenced by the recipe.
        let max_snapshot = self
            .func
            .stack_recipe
            .get(start_bc_idx)
            .map(|recipe| {
                recipe
                    .iter()
                    .filter_map(|o| match o {
                        StackOrigin::Snapshot(i) => Some(*i as usize + 1),
                        _ => None,
                    })
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0);
        let (extra_locals, value_stack_copy): (&[u64], &[u64]) =
            if local_snapshot.len() >= max_snapshot {
                let split = local_snapshot.len() - max_snapshot;
                (&local_snapshot[..split], &local_snapshot[split..])
            } else {
                (&[][..], local_snapshot)
            };
        // Bytecode-coverage-expansion B-2: pack via the string-aware
        // path so string args picked up by the VM's prologue at slot
        // overwrite time. Without this, the resume-from-deopt path
        // sees `0` in every `String`-typed slot and downstream
        // `BcOp::StrConcat` / `StrContains` / `StrSubstring` operate
        // on the wrong handle.
        let (packed, string_args) = self.pack_args_with_strings(&args)?;
        let string_arg_refs: Vec<(usize, &str)> = string_args
            .iter()
            .map(|(slot, payload)| (*slot, payload.as_str()))
            .collect();
        let initial_stack =
            self.materialise_stack(start_bc_idx, &packed, extra_locals, value_stack_copy);
        self.run_main_inner_with_packed_strings(
            &packed,
            &string_arg_refs,
            start_bc_idx,
            extra_locals,
            &initial_stack,
        )
    }
}

impl BytecodeEvaluator {
    /// M2-C lever 1: typed-i64 fast-path entry mirroring the cranelift
    /// `run_main_legacy_i64` shape.
    ///
    /// Hosts that already hold their `#main(...)` arguments as a flat
    /// `&[i64]` (e.g. benchmark harnesses, FFI bridges, the trace-JIT
    /// dispatch boundary) pay zero `HashMap<String, Value>` lookup
    /// cost: args land directly in the VM's `u64` slots via a
    /// `as u64` reinterpret, and the return shape is decoded against
    /// the pre-classified [`ReturnShape::SingleScalar`] schema slot
    /// (cached at construction time — see lever 5).
    ///
    /// Returns `Err(RuntimeError::Unsupported)` when the entry's
    /// `#main(...)` schema is not in the typed-i64 envelope:
    /// non-`Int` parameter types or a multi-field return record. The
    /// caller should fall back to [`Self::run_main`] in that case.
    ///
    /// The trace-JIT dispatcher-switch path is intentionally **not**
    /// consulted here — the fast path is for hot benchmarks and the
    /// recorder/installed-trace overhead would defeat the purpose.
    /// Hosts that need the trace bypass should keep calling the
    /// trait-level `run_main`.
    pub fn run_main_i64(&self, args: &[i64]) -> Result<i64, RuntimeError> {
        // Param-count + type check: every declared param must be the
        // `I64` lane and the caller must supply exactly that many
        // args. Anything richer falls back to the trait surface.
        // M2-C lever 5: arity check reads the cached `param_count`
        // field so the hot path doesn't pay a `Vec::len` indirection.
        if args.len() != self.cached_param_count {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "bytecode VM run_main_i64: arity mismatch (expected {} args, got {})",
                    self.cached_param_count,
                    args.len()
                ),
            });
        }
        for ty in &self.param_tys {
            if !matches!(ty, IrType::I64 | IrType::I32) {
                return Err(RuntimeError::Unsupported {
                    reason: format!(
                        "bytecode VM run_main_i64: non-i64 param type {ty:?} \
                         falls outside the fast-path envelope; use run_main"
                    ),
                });
            }
        }
        // Return-shape check: single Int scalar only — multi-field
        // dicts / non-Int returns fall back to the trait surface.
        match self.return_shape {
            ReturnShape::SingleScalarInt | ReturnShape::LegacyI64 => {}
            _ => {
                return Err(RuntimeError::Unsupported {
                    reason: "bytecode VM run_main_i64: return shape outside Int scalar envelope; \
                         use run_main"
                        .into(),
                });
            }
        };
        // Reinterpret each i64 arg through the VM's u64 lane —
        // matches the same cast `value_to_vm` would have produced
        // for `(Value::Int(v), IrType::I64) => v as u64`. We use a
        // small inline buffer for the common 0..4 args case so the
        // typed path avoids the heap allocation. For wider arities
        // the fallback is a regular `Vec`.
        let mut inline: [u64; 4] = [0; 4];
        let packed: &[u64] = if args.len() <= 4 {
            for (i, v) in args.iter().enumerate() {
                inline[i] = *v as u64;
            }
            &inline[..args.len()]
        } else {
            let mut tmp: Vec<u64> = Vec::with_capacity(args.len());
            for v in args {
                tmp.push(*v as u64);
            }
            // Lifetime: extend through `tmp` — but we need `&[u64]`
            // outliving `inline`. Branch the invoke separately for
            // the wide arity below.
            return self.run_main_i64_inner(&tmp);
        };
        self.run_main_i64_inner(packed)
    }

    /// Core of the typed-i64 fast path.
    ///
    /// M2-C lever 7 (2026-05-22): drives the bytecode VM through
    /// [`BytecodeVm::invoke_pooled_typed_i64`], the alloc-free typed
    /// fast entry. Differences from the general path:
    ///
    /// * Thread-local pooled `Vec<u64>` scratch for locals / stack —
    ///   no per-call `vec![0u64; N]` / `Vec::with_capacity` alloc once
    ///   the buffer is warm.
    /// * Returns the schema-decoded value directly — skips the
    ///   [`crate::vm::BcRunOutcome`] `final_locals` Vec move on return.
    ///
    /// The [`BytecodeVm::new`] + `default_config.clone()` cost remains
    /// per-call for now — `BytecodeVm` is intentionally `!Sync` (the
    /// per-call inline cache lives behind a `RefCell`), so it can't be
    /// cached on the `Send + Sync` evaluator without a wider rework.
    /// The W12 row's allocator pressure is the dominant remaining
    /// cost; lever 7's pool clears it.
    fn run_main_i64_inner(&self, packed: &[u64]) -> Result<i64, RuntimeError> {
        let vm = BytecodeVm::new(self.default_config.clone());
        let return_slot_count = self.cached_return_slot_count();
        let return_slot_idx = match self.return_shape {
            ReturnShape::LegacyI64 => 0,
            _ => self.return_field_base,
        };
        let raw = vm
            .invoke_pooled_typed_i64(&self.func, packed, return_slot_count, return_slot_idx)
            .map_err(|err| err.into_runtime_error(self.entry_range))?;
        Ok(raw as i64)
    }

    /// M2-C lever 5: cached return-slot count. Reads the
    /// `cached_return_field_count` field populated once at construction
    /// time — no `Option<Schema>` walk + no `.fields.len()` indirection
    /// on the hot dispatch path. `LegacyI64` short-circuits to 0
    /// (direct-IR tests don't carry a schema and the VM returns a
    /// single i64 slot through the stack).
    #[inline(always)]
    fn cached_return_slot_count(&self) -> u32 {
        match self.return_shape {
            ReturnShape::LegacyI64 => 0,
            _ => self.cached_return_field_count,
        }
    }

    /// Variant of [`Self::run_main_inner`] that takes pre-packed
    /// args. The resume path goes through here because it already
    /// packed the args while materialising the stack recipe; using
    /// this method avoids re-packing.
    /// Bytecode-coverage-expansion B-2: packed-args entry that
    /// accepts a pre-resolved `(slot_idx, &str)` list for string args
    /// and lifts the matching `String`-typed return slots out of the
    /// per-invoke `StringArena`. Used by the trace-dispatcher branch
    /// on the `NoTrace` path and by the resume-from-deopt path so the
    /// bytecode body sees the same handle `pack_args_with_strings`
    /// would have planted.
    fn run_main_inner_with_packed_strings(
        &self,
        packed: &[VmValue],
        string_args: &[(usize, &str)],
        start_bc_idx: usize,
        extra_locals: &[VmValue],
        initial_stack: &[VmValue],
    ) -> Result<Value, RuntimeError> {
        let string_return_slots = self.string_return_slots();
        let vm = BytecodeVm::new(self.default_config.clone());
        let outcome = vm.invoke_from_with_string_io(
            &self.func,
            packed,
            start_bc_idx,
            extra_locals,
            self.cached_return_slot_count(),
            initial_stack,
            string_args,
            &string_return_slots,
        );
        if let Some(err) = outcome.error {
            return Err(err.into_runtime_error(self.entry_range));
        }
        Ok(self.unpack_return_slots_with_strings(
            &outcome.final_locals,
            &outcome.final_strings,
            outcome.value,
        ))
    }

    /// Deopt-driver-facing API: rehydrate from an explicit
    /// `DeoptStateSnapshot` directly. Callers that hold the snapshot
    /// (typically `TraceJitState::invoke_with_resume`) use this
    /// instead of flattening through `resume_from_pc`.
    pub fn resume_from_snapshot(
        &self,
        args: HashMap<String, Value>,
        snapshot: &relon_trace_abi::DeoptStateSnapshot,
    ) -> Result<Value, RuntimeError> {
        let (value, _) = self.resume_from_snapshot_with_metrics(args, snapshot)?;
        Ok(value)
    }

    /// M2-B phase 4c-cont sub-task B: full deopt → bytecode handoff
    /// entry point.
    ///
    /// Convenience alias for [`Self::resume_from_snapshot`] surfaced
    /// at this name so the public API mirrors the
    /// [`crate::trace_dispatch::TraceInvokeOutcome::Deopt`] arm the
    /// dispatcher switch routes through internally. Hosts that hold
    /// a snapshot (e.g. when manually orchestrating the trace
    /// pipeline outside of [`Self::run_main`]) can call this directly
    /// to skip the lookup re-consult.
    ///
    /// The semantic is identical to `resume_from_snapshot`: rebuild
    /// the operand stack from the per-PC recipe + snapshot fragments,
    /// dispatch the bytecode VM starting at the snapshot's
    /// `external_pc`, propagate any trap on the resumed dispatch as
    /// the public [`RuntimeError`] envelope.
    pub fn resume_from_deopt(
        &self,
        args: HashMap<String, Value>,
        snapshot: &relon_trace_abi::DeoptStateSnapshot,
    ) -> Result<Value, RuntimeError> {
        self.resume_from_snapshot(args, snapshot)
    }

    /// Same as [`Self::resume_from_snapshot`] but also returns
    /// instrumentation metrics — total ops dispatched and the last
    /// bytecode index visited. Used by the M2-B integration test that
    /// asserts the resume path is strictly shorter than the full
    /// entry-to-trap path.
    pub fn resume_from_snapshot_with_metrics(
        &self,
        args: HashMap<String, Value>,
        snapshot: &relon_trace_abi::DeoptStateSnapshot,
    ) -> Result<(Value, ResumeMetrics), RuntimeError> {
        let (outcome, metrics) = self.resume_via_vm(&args, snapshot)?;
        if let Some(err) = outcome.error {
            return Err(err.into_runtime_error(self.entry_range));
        }
        // PC-alignment follow-up #3: route the unpack through the
        // string-aware variant so `SingleScalarString` return shapes
        // pick up their `final_strings` payload. The pre-fix variant
        // called `unpack_return_slots` (which substitutes an empty
        // `final_strings` map), so any string-shape deopt resume that
        // reached the `Return` op produced an empty string regardless
        // of the actual VM output. The legacy / non-string return
        // shapes (`LegacyI64` / scalar-Int / scalar-Bool / …) ignore
        // the `final_strings` map, so this is a strict generalisation
        // without behaviour change for the integer benchmark fixtures.
        Ok((
            self.unpack_return_slots_with_strings(
                &outcome.final_locals,
                &outcome.final_strings,
                outcome.value,
            ),
            metrics,
        ))
    }

    /// Companion to [`Self::resume_from_snapshot_with_metrics`] that
    /// surfaces the [`ResumeMetrics`] even when the VM re-traps. The
    /// caller has to interpret the trap on its own; we never return
    /// the raw `Value` because the VM didn't produce one. M2-B
    /// integration tests use this to verify the resume's start
    /// `bc_idx` lines up with the trap PC even when the trap re-fires.
    pub fn resume_from_snapshot_metrics_only(
        &self,
        snapshot: &relon_trace_abi::DeoptStateSnapshot,
    ) -> Result<(Option<Value>, ResumeMetrics), RuntimeError> {
        // Pack args as zeros — caller already proved the trap is the
        // expected envelope; we only need the metrics here.
        let args = HashMap::new();
        match self.resume_via_vm(&args, snapshot) {
            Ok((outcome, metrics)) => {
                if outcome.error.is_some() {
                    Ok((None, metrics))
                } else {
                    // PC-alignment follow-up #3: mirror the string-aware
                    // unpack from `resume_from_snapshot_with_metrics` so
                    // string-shape return slots picked up by the VM
                    // make it back through the metrics-only entry too.
                    Ok((
                        Some(self.unpack_return_slots_with_strings(
                            &outcome.final_locals,
                            &outcome.final_strings,
                            outcome.value,
                        )),
                        metrics,
                    ))
                }
            }
            Err(e) => Err(e),
        }
    }

    fn resume_via_vm(
        &self,
        args: &HashMap<String, Value>,
        snapshot: &relon_trace_abi::DeoptStateSnapshot,
    ) -> Result<(crate::vm::BcRunOutcome, ResumeMetrics), RuntimeError> {
        let start_bc_idx = self
            .func
            .bc_index_for_pc(snapshot.external_pc as ExternalPc)
            .unwrap_or(0);
        // PC-alignment follow-up #3: pack via the string-aware path so
        // `String`-typed arg slots receive their arena handle when the
        // VM prologue runs. The pre-fix variant called `pack_args`,
        // which planted a `0` placeholder into every `String` slot —
        // downstream `BcOp::StrConcat` / `StrContains` / `StrSubstring`
        // would then resolve handle `0` and either crash or return
        // garbage. The string-aware path mirrors what `run_main` and
        // `resume_from_pc` already do, so all three entry points read
        // the same string handles. Metrics-only callers with no args
        // still fall back to a zeroed packed vec — the trap is what
        // they're after, not the resolved value.
        let (packed, string_args) = match self.pack_args_with_strings(args) {
            Ok(p) => p,
            Err(_) if args.is_empty() => (vec![0u64; self.param_names.len()], Vec::new()),
            Err(e) => return Err(e),
        };
        let string_arg_refs: Vec<(usize, &str)> = string_args
            .iter()
            .map(|(slot, payload)| (*slot, payload.as_str()))
            .collect();
        let string_return_slots = self.string_return_slots();
        let initial_stack = if start_bc_idx == 0 {
            Vec::new()
        } else {
            self.materialise_stack(
                start_bc_idx,
                &packed,
                &snapshot.ssa_slots_copy,
                &snapshot.value_stack_copy,
            )
        };
        let vm = BytecodeVm::new(self.default_config.clone());
        let outcome = vm.invoke_from_with_string_io(
            &self.func,
            &packed,
            start_bc_idx,
            &snapshot.ssa_slots_copy,
            self.cached_return_slot_count(),
            &initial_stack,
            &string_arg_refs,
            &string_return_slots,
        );
        let metrics = ResumeMetrics {
            steps: outcome.steps,
            last_bc_idx: outcome.last_bc_idx,
            start_bc_idx,
        };
        Ok((outcome, metrics))
    }

    /// Run `args` through the full pipeline starting at entry and
    /// return the metrics; companion to
    /// [`Self::resume_from_snapshot_with_metrics`] so tests can prove
    /// the resume path is strictly shorter than the full path.
    pub fn run_main_with_metrics(
        &self,
        args: HashMap<String, Value>,
    ) -> Result<(Value, ResumeMetrics), RuntimeError> {
        let packed = self.pack_args(&args)?;
        let vm = BytecodeVm::new(self.default_config.clone());
        let outcome = vm.invoke_from_with_stack(
            &self.func,
            &packed,
            0,
            &[],
            self.cached_return_slot_count(),
            &[],
        );
        let metrics = ResumeMetrics {
            steps: outcome.steps,
            last_bc_idx: outcome.last_bc_idx,
            start_bc_idx: 0,
        };
        if let Some(err) = outcome.error {
            return Err(err.into_runtime_error(self.entry_range));
        }
        Ok((self.unpack_return_slots(&outcome.final_locals), metrics))
    }
}

/// Instrumentation surface for the M2-B partial-resume integration
/// tests. The numbers are diagnostic only — they're not part of the
/// production path and the host should not condition behaviour on
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResumeMetrics {
    /// Total ops dispatched (including the trapping op).
    pub steps: u64,
    /// Last bytecode index visited before exit.
    pub last_bc_idx: usize,
    /// Bytecode index the run started at (`0` for `run_main`).
    pub start_bc_idx: usize,
}
