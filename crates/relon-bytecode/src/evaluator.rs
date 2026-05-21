//! Public façade implementing [`relon_eval_api::Evaluator`].
//!
//! Construction mirrors `relon_codegen_native::CraneliftAotEvaluator::from_source`:
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
use relon_eval_api::{CapabilityGate, ClosureData, Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_ir::ir::Module as IrModule;
use relon_ir::IrType;
use relon_parser::{Node, TokenRange};
use thiserror::Error;

use crate::compile::{
    build_offset_to_local, compile_function, compile_function_in_module, BcCompileError,
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
    /// Default VM config. Cloned per `run_main` so concurrent calls
    /// don't share the resource counter.
    default_config: BcVmConfig,
}

impl BytecodeEvaluator {
    /// Drive the full pipeline: parse → analyze → IR lower → bytecode
    /// compile.
    pub fn from_source(src: &str) -> Result<Self, BytecodeError> {
        let ast =
            relon_parser::parse_document(src).map_err(|e| BytecodeError::Parse(e.to_string()))?;
        let analyzed = relon_analyzer::analyze(&ast);
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
    /// the parse + analyze stages. The field-offset maps are
    /// empty — the IR must use `LocalGet(idx)` directly.
    pub fn from_ir_legacy(
        module: IrModule,
        param_names: Vec<String>,
    ) -> Result<Self, BytecodeError> {
        let entry_idx = module.entry_func_index.ok_or(BytecodeError::NoEntry)?;
        let func = &module.funcs[entry_idx];
        let entry_range = func.range;
        let param_tys = func.params.clone();
        let empty = std::collections::BTreeMap::new();
        let compiled = compile_function(func, &empty, &empty)?;
        Ok(Self {
            func: compiled,
            entry_range,
            param_names,
            param_tys,
            return_schema: None,
            return_field_base: 0,
            default_config: BcVmConfig::default(),
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
        let func = &module.funcs[entry_idx];
        let entry_range = func.range;
        // v6-δ M2-B: thread the full `funcs` slice through so the
        // bytecode compile pass can inline simple callees
        // (`Op::Call`) the M2-A scaffold rejected.
        let compiled = compile_function_in_module(func, &module.funcs, in_map, out_map)?;
        Ok(Self {
            func: compiled,
            entry_range,
            param_names,
            param_tys,
            return_schema: Some(return_schema),
            return_field_base,
            default_config: BcVmConfig::default(),
        })
    }

    /// Expose the compiled function — used by the differential test
    /// harness to inspect the `ir_pc_map` invariants.
    pub fn function(&self) -> &BcFunction {
        &self.func
    }

    /// Override the default VM config (max_steps / deadline / cap
    /// vtable).
    pub fn with_config(mut self, config: BcVmConfig) -> Self {
        self.default_config = config;
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
    ///   [`relon_eval_api::RuntimeError::WasmCapabilityDenied`] with the failing bit.
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
        self.default_config.cap_vtable.set_gate(gate);
        self
    }

    /// M2-B phase 4c: install the trace-JIT hot-counter trigger on the
    /// default VM config. Returns `self` so the call chains cleanly
    /// off `from_source`.
    ///
    /// The trigger is consulted on every `run_main` invocation; when
    /// the per-`fn_id` counter crosses the configured threshold (see
    /// [`Self::with_hot_threshold`]), `trigger.on_hot(fn_id, args)`
    /// fires exactly once. Hosts using the cranelift adapter
    /// (`relon_codegen_native::CraneliftHotTrigger`) get the standard
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
        self.default_config.hot_trigger = Some(trigger);
        self
    }

    /// M2-B phase 4c: override the default hot-counter threshold.
    /// `1` triggers on the very first invocation (smoke-test mode);
    /// the default 1000 mirrors the LuaJIT-style conservative kickoff
    /// the cranelift backend uses.
    pub fn with_hot_threshold(mut self, threshold: u32) -> Self {
        self.default_config.hot_threshold = threshold;
        self
    }

    /// M2-B phase 4c: stamp the cross-backend `fn_id` on the compiled
    /// function. The slot drives the hot-counter prologue's per-id
    /// lookup and matches the id under which the host registered a
    /// [`relon_codegen_native::trace_install::RecordingRegistration`]
    /// for the recorder.
    pub fn with_fn_id(mut self, fn_id: u32) -> Self {
        self.func = self.func.clone().with_fn_id(fn_id);
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
        self.default_config.trace_lookup = Some(lookup);
        self
    }

    /// Inspect the entry source range.
    pub fn entry_range(&self) -> TokenRange {
        self.entry_range
    }

    fn pack_args(&self, args: &HashMap<String, Value>) -> Result<Vec<VmValue>, RuntimeError> {
        let mut packed = Vec::with_capacity(self.param_names.len());
        for (i, name) in self.param_names.iter().enumerate() {
            let value = args.get(name).or_else(|| args.get(&format!("arg{i}")));
            let value = value.ok_or_else(|| RuntimeError::MissingMainArg {
                name: name.clone(),
                range: self.entry_range,
            })?;
            let ty = self.param_tys.get(i).copied().unwrap_or(IrType::I64);
            packed.push(value_to_vm(value, ty, name, self.entry_range)?);
        }
        Ok(packed)
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
        let mut map: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
        for (i, f) in schema.fields.iter().enumerate() {
            let raw = if i == 0 { result } else { 0 };
            map.insert(f.name.clone(), decode_field(&f.ty, raw));
        }
        Value::branded_dict(map, Some(schema.name.clone()))
    }

    fn unpack_return_slots(&self, locals: &[VmValue]) -> Value {
        let Some(schema) = self.return_schema.as_ref() else {
            // Legacy / direct-IR path: the VM returns one slot.
            return Value::Int(locals.first().copied().unwrap_or(0) as i64);
        };
        if schema.fields.len() == 1 && schema.fields[0].name == relon_ir::RETURN_VALUE_FIELD_NAME {
            // Single-value wrapper: lift the bare scalar.
            let f = &schema.fields[0];
            let slot = self.return_field_base as usize;
            return decode_field(&f.ty, locals.get(slot).copied().unwrap_or(0));
        }
        // Multi-field return record (Dict). Reconstruct a branded dict.
        let mut map: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
        for (i, f) in schema.fields.iter().enumerate() {
            let slot = self.return_field_base as usize + i;
            map.insert(
                f.name.clone(),
                decode_field(&f.ty, locals.get(slot).copied().unwrap_or(0)),
            );
        }
        Value::branded_dict(map, Some(schema.name.clone()))
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
        let packed = self.pack_args(args)?;
        let vm = BytecodeVm::new(self.default_config.clone());
        let outcome = vm.invoke_from_with_stack(
            &self.func,
            &packed,
            start_bc_idx,
            extra_locals,
            /*return_slot_count=*/
            self.return_schema
                .as_ref()
                .map(|s| s.fields.len() as u32)
                .unwrap_or(0),
            initial_stack,
        );
        if let Some(err) = outcome.error {
            return Err(err.into_runtime_error(self.entry_range));
        }
        // Buffer-protocol path: the VM's `Return` value is the
        // bytes_written placeholder (not used by the bytecode VM);
        // the actual return data lives in the virtual return slots.
        Ok(self.unpack_return_slots(&outcome.final_locals))
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

/// Decide whether a schema field type is in the M2-A scalar envelope.
fn is_scalar_field(ty: &relon_eval_api::schema_canonical::TypeRepr) -> bool {
    use relon_eval_api::schema_canonical::TypeRepr;
    matches!(
        ty,
        TypeRepr::Int | TypeRepr::Float | TypeRepr::Bool | TypeRepr::Null
    )
}

fn ir_type_for_field(ty: &relon_eval_api::schema_canonical::TypeRepr) -> IrType {
    use relon_eval_api::schema_canonical::TypeRepr;
    match ty {
        TypeRepr::Int => IrType::I64,
        TypeRepr::Float => IrType::F64,
        TypeRepr::Bool => IrType::Bool,
        TypeRepr::Null => IrType::Null,
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
            let packed = self.pack_args(&args)?;
            match lookup.try_invoke(fn_id, &packed) {
                crate::trace_dispatch::TraceInvokeOutcome::NoTrace => {
                    // Re-use the pre-packed args via the
                    // packed-args variant so we don't double-pack.
                    return self.run_main_inner_with_packed(&packed, 0, &[], &[]);
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
        let packed = self.pack_args(&args)?;
        let initial_stack =
            self.materialise_stack(start_bc_idx, &packed, extra_locals, value_stack_copy);
        self.run_main_inner_with_packed(&packed, start_bc_idx, extra_locals, &initial_stack)
    }
}

impl BytecodeEvaluator {
    /// Variant of [`Self::run_main_inner`] that takes pre-packed
    /// args. The resume path goes through here because it already
    /// packed the args while materialising the stack recipe; using
    /// this method avoids re-packing.
    fn run_main_inner_with_packed(
        &self,
        packed: &[VmValue],
        start_bc_idx: usize,
        extra_locals: &[VmValue],
        initial_stack: &[VmValue],
    ) -> Result<Value, RuntimeError> {
        let vm = BytecodeVm::new(self.default_config.clone());
        let outcome = vm.invoke_from_with_stack(
            &self.func,
            packed,
            start_bc_idx,
            extra_locals,
            self.return_schema
                .as_ref()
                .map(|s| s.fields.len() as u32)
                .unwrap_or(0),
            initial_stack,
        );
        if let Some(err) = outcome.error {
            return Err(err.into_runtime_error(self.entry_range));
        }
        Ok(self.unpack_return_slots(&outcome.final_locals))
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
        Ok((self.unpack_return_slots(&outcome.final_locals), metrics))
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
                    Ok((
                        Some(self.unpack_return_slots(&outcome.final_locals)),
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
        // Pack args best-effort: the metrics-only variant supplies an
        // empty map and we tolerate `MissingMainArg` by falling back
        // to a zeroed `packed` vector (the trap is what we're after
        // anyway, not the value). The full-resume variant goes
        // through `pack_args` strictly.
        let packed = match self.pack_args(args) {
            Ok(p) => p,
            Err(_) if args.is_empty() => vec![0u64; self.param_names.len()],
            Err(e) => return Err(e),
        };
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
        let outcome = vm.invoke_from_with_stack(
            &self.func,
            &packed,
            start_bc_idx,
            &snapshot.ssa_slots_copy,
            self.return_schema
                .as_ref()
                .map(|s| s.fields.len() as u32)
                .unwrap_or(0),
            &initial_stack,
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
            self.return_schema
                .as_ref()
                .map(|s| s.fields.len() as u32)
                .unwrap_or(0),
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
