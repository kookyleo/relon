//! Public façade implementing [`relon_eval_api::Evaluator`].
//!
//! Construction mirrors [`relon_codegen_native::CraneliftAotEvaluator::from_source`]:
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
use relon_eval_api::{ClosureData, Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_ir::ir::Module as IrModule;
use relon_ir::IrType;
use relon_parser::{Node, TokenRange};
use thiserror::Error;

use crate::compile::{build_offset_to_local, compile_function, BcCompileError};
use crate::op::{BcFunction, ExternalPc};
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
/// [`Self::from_ir`].
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
        let compiled = compile_function(func, in_map, out_map)?;
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
    ) -> Result<Value, RuntimeError> {
        let packed = self.pack_args(args)?;
        let vm = BytecodeVm::new(self.default_config.clone());
        let outcome = vm.invoke_from_with_locals(
            &self.func,
            &packed,
            start_bc_idx,
            extra_locals,
            /*return_slot_count=*/
            self.return_schema
                .as_ref()
                .map(|s| s.fields.len() as u32)
                .unwrap_or(0),
        );
        if let Some(err) = outcome.error {
            return Err(err.into_runtime_error(self.entry_range));
        }
        // Buffer-protocol path: the VM's `Return` value is the
        // bytes_written placeholder (not used by the bytecode VM);
        // the actual return data lives in the virtual return slots.
        Ok(self.unpack_return_slots(&outcome.final_locals))
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
        self.run_main_inner(&args, /*start_bc_idx=*/ 0, &[])
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

    /// v6-δ M2-A core deliverable: route a deopt'd trace's
    /// `external_pc` through the `ir_pc_map` table and resume the VM
    /// at the matching bytecode index. `local_snapshot` is overlaid
    /// past the `#main` args slots so let-bound values the trace
    /// observed are restored before dispatch picks back up.
    ///
    /// ## M2-A scope
    ///
    /// - **Trap PCs**: the VM re-runs the op the deopt fired on. The
    ///   trap fires again with the same `RuntimeError` shape; the
    ///   resume_from_pc_after_each_prong test pins this.
    /// - **Non-trap PCs**: routing is wired but the operand stack is
    ///   restored as empty — the M2-B work widens the
    ///   `DeoptStateSnapshot` payload so the SSA value stack rebuilds
    ///   without re-running the producer ops.
    /// - **Unknown PCs**: gracefully fall back to restarting from
    ///   entry, preserving the args + slot overlay.
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
        self.run_main_inner(&args, start_bc_idx, local_snapshot)
    }
}
