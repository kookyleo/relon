//! Phase 8: wasm-AOT backend implementation of [`relon_eval_api::Evaluator`].
//!
//! [`WasmAotEvaluator`] drives a precompiled wasm module through the
//! binary handshake the codegen pass laid down (`run_main(in_ptr,
//! in_len, out_ptr, out_cap) -> bytes_written`). The struct keeps both
//! the parsed [`WasmModule`] (so it can translate traps and read
//! `relon.abi` metadata) and a `wasmtime::Module` (so it can
//! instantiate cheap per-call sessions without re-decoding wasm bytes).
//!
//! Scope (locked at Phase 8):
//!
//! * Only `run_main` is real. The other four [`Evaluator`] methods
//!   (`eval` / `eval_root` / `force_thunk` / `invoke_closure`) return
//!   [`RuntimeError::Unsupported`] — the wasm AOT pipeline consumes
//!   the AST at compile time, leaves nothing to evaluate at runtime,
//!   and the static topo-sort means there are no live thunks or
//!   closures to drive.
//! * Single-file source only. `#import`-spanning workspaces are a
//!   Phase 9 goal; the construction path runs the per-file analyzer.
//! * Schema field types supported: `Int`, `Float`, `Bool`, `Null`,
//!   `String`, `List<Int>`, plus nested branded `Schema { ... }` for
//!   the dict-return path. Anything else surfaces as
//!   [`BuildError::UnsupportedFieldType`] up front (matching the IR
//!   lowering's own supported leaves).

use crate::{compile_lowered_entry, WasmModule};
use relon_eval_api::buffer::{BufferBuilder, BufferError, BufferReader};
use relon_eval_api::layout::{OffsetTable, SchemaLayout};
use relon_eval_api::schema_canonical::{Field, Schema, TypeRepr};
use relon_eval_api::{Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_parser::Node;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use thiserror::Error;
use wasmtime::{
    Engine, Global, GlobalType, Linker, Memory, Module as WtModule, Mutability, Store, TypedFunc,
    Val, ValType,
};

/// Default `out_cap` (in bytes) when a host doesn't override it.
/// 4 KiB matches the codegen's `DATA_SECTION_BASE` and is plenty for
/// the v1 single-record return shapes (no recursive aggregates yet).
/// Hosts that emit larger tail records can supply a fresh evaluator
/// configured via [`WasmAotEvaluator::with_out_cap`].
const DEFAULT_OUT_CAP: u32 = 4096;

/// Errors surfaced while building a [`WasmAotEvaluator`].
///
/// The construction pipeline runs parse → per-file analyzer → IR
/// lowering → wasm codegen → wasmtime compile. Each stage's failure
/// shape lands in a dedicated variant so callers can route diagnostics
/// without losing the staging information.
#[derive(Debug, Error)]
pub enum BuildError {
    /// `parse_document` rejected the source. Carries the parser
    /// error's display form (the parser surface itself isn't
    /// re-exported here because hosts already touch it through
    /// `relon_parser::ParseDocumentError`).
    #[error("parse error: {0}")]
    ParseError(String),
    /// The per-file analyzer reported one or more `Error`-severity
    /// diagnostics. Joined into a single message so the BuildError
    /// stays a single thiserror enum surface.
    #[error("analyzer reported errors:\n  - {0}")]
    AnalyzerError(String),
    /// IR lowering failed (missing `#main`, unsupported type in a
    /// schema field, ...).
    #[error("lowering error: {0}")]
    LoweringError(String),
    /// Wasm codegen failed — usually a hand-built IR that escaped
    /// the lowering pass's invariants.
    #[error("codegen error: {0}")]
    CodegenError(String),
    /// [`WasmModule::from_bytes`] failed to decode the emitted
    /// custom sections. Indicates a codegen bug or a corrupted
    /// custom section.
    #[error("wasm load error: {0}")]
    WasmLoadError(String),
    /// `wasmtime::Module::new` or `Engine::default` failed to
    /// JIT-compile / validate the emitted module. Stringified so
    /// the public surface doesn't propagate `wasmtime::Error`.
    #[error("wasm instantiate error: {0}")]
    WasmInstantiateError(String),
    /// The supplied schema field type is not yet wired through the
    /// `BufferBuilder` / `BufferReader` path. The supported leaves
    /// match the IR lowering's allowed set: Int / Float / Bool /
    /// Null / String / List<Int> / nested Schema.
    #[error("unsupported field type `{type_label}` in field `{field}` (wasm-aot v1)")]
    UnsupportedFieldType {
        /// Field name that carries the unsupported type.
        field: String,
        /// Human-readable type label (`"Option"`, `"Result"`, ...).
        type_label: &'static str,
    },
}

/// Phase 8 wasm-AOT backend. Holds a precompiled wasm module plus the
/// schemas / layouts the host needs to bridge `Value` ↔ binary
/// handshake. Implements [`Evaluator`] with `run_main` as the only
/// real entry point.
pub struct WasmAotEvaluator {
    /// Parsed wasm module wrapping the raw bytes + decoded
    /// `relon.abi` / `relon.srcmap` / `relon.host_fns` / `relon.uctab`
    /// sections. Used for trap translation.
    module: WasmModule,
    /// Wasmtime compilation engine. Kept on the evaluator so per-call
    /// `instantiate` doesn't pay for engine setup.
    engine: Engine,
    /// JIT-compiled module ready for instantiation. One per evaluator;
    /// reused across `run_main` calls.
    compiled: WtModule,
    /// Canonical `#main` param schema. Used for `BufferBuilder`
    /// construction.
    main_schema: Schema,
    /// Canonical return schema. Used for `BufferReader` construction.
    return_schema: Schema,
    /// Precomputed layout for `main_schema`.
    main_layout: OffsetTable,
    /// Precomputed layout for `return_schema`.
    return_layout: OffsetTable,
    /// Out buffer capacity in bytes used for each `run_main` call.
    /// Host can override via [`Self::with_out_cap`] before evaluating.
    out_cap: u32,
}

impl WasmAotEvaluator {
    /// Compile `src` end-to-end and return a ready-to-call evaluator.
    ///
    /// Pipeline: `parse_document` → `relon_analyzer::analyze` →
    /// `relon_ir::lower_workspace_single` → `compile_lowered_entry` →
    /// `WasmModule::from_bytes` → `wasmtime::Module::new`.
    pub fn from_source(src: &str) -> Result<Self, BuildError> {
        let ast =
            relon_parser::parse_document(src).map_err(|e| BuildError::ParseError(e.to_string()))?;
        let analyzed = relon_analyzer::analyze(&ast);
        if analyzed.has_errors() {
            let joined = analyzed
                .diagnostics
                .iter()
                .filter(|d| d.severity() == relon_analyzer::Severity::Error)
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join("\n  - ");
            return Err(BuildError::AnalyzerError(joined));
        }
        let lowered = relon_ir::lower_workspace_single(&analyzed, &ast)
            .map_err(|e| BuildError::LoweringError(e.to_string()))?;
        let bytes =
            compile_lowered_entry(&lowered).map_err(|e| BuildError::CodegenError(e.to_string()))?;
        Self::from_bytes(bytes, lowered.main_schema, lowered.return_schema)
    }

    /// Build an evaluator from already-emitted wasm bytes plus the
    /// canonical schemas they were compiled against. Useful when a
    /// host caches the compiled output and re-loads it from disk.
    pub fn from_bytes(
        bytes: Vec<u8>,
        main_schema: Schema,
        return_schema: Schema,
    ) -> Result<Self, BuildError> {
        Self::reject_unsupported_fields(&main_schema)?;
        Self::reject_unsupported_fields(&return_schema)?;

        let main_layout = SchemaLayout::offsets_for(&main_schema)
            .map_err(|e| BuildError::LoweringError(format!("main schema layout: {e}")))?;
        let return_layout = SchemaLayout::offsets_for(&return_schema)
            .map_err(|e| BuildError::LoweringError(format!("return schema layout: {e}")))?;

        let module =
            WasmModule::from_bytes(bytes).map_err(|e| BuildError::WasmLoadError(e.to_string()))?;
        let engine = Engine::default();
        let compiled = WtModule::new(&engine, module.bytes())
            .map_err(|e| BuildError::WasmInstantiateError(e.to_string()))?;

        Ok(Self {
            module,
            engine,
            compiled,
            main_schema,
            return_schema,
            main_layout,
            return_layout,
            out_cap: DEFAULT_OUT_CAP,
        })
    }

    /// Override the `out_cap` byte budget used for each `run_main`
    /// call. Defaults to 4 KiB; bump it when the return schema's
    /// tail records (strings / list<Int>) can exceed the budget.
    pub fn with_out_cap(mut self, out_cap: u32) -> Self {
        self.out_cap = out_cap;
        self
    }

    /// Borrow the wrapped [`WasmModule`] — useful for hosts that
    /// want to inspect the parsed `relon.abi` payload or render trap
    /// traces through `module.lookup_pc` outside the trait surface.
    pub fn wasm_module(&self) -> &WasmModule {
        &self.module
    }

    /// Borrow the `#main` schema this evaluator was compiled
    /// against — useful for hosts driving `BufferBuilder` manually.
    pub fn main_schema(&self) -> &Schema {
        &self.main_schema
    }

    /// Borrow the return schema this evaluator was compiled against.
    pub fn return_schema(&self) -> &Schema {
        &self.return_schema
    }

    /// Recursively check that every field of `schema` (and any
    /// nested branded sub-schemas) uses a type the buffer
    /// builder / reader actually supports. Surfaces a precise
    /// [`BuildError::UnsupportedFieldType`] at construction so
    /// the `run_main` hot path doesn't have to defend against it.
    fn reject_unsupported_fields(schema: &Schema) -> Result<(), BuildError> {
        for field in &schema.fields {
            Self::check_type_repr(&field.name, &field.ty)?;
        }
        Ok(())
    }

    fn check_type_repr(field: &str, ty: &TypeRepr) -> Result<(), BuildError> {
        match ty {
            TypeRepr::Int
            | TypeRepr::Float
            | TypeRepr::Bool
            | TypeRepr::Null
            | TypeRepr::String => Ok(()),
            TypeRepr::List { element } => {
                if matches!(element.as_ref(), TypeRepr::Int) {
                    Ok(())
                } else {
                    Err(BuildError::UnsupportedFieldType {
                        field: field.to_string(),
                        type_label: "List (non-Int element)",
                    })
                }
            }
            TypeRepr::Schema { schema } => Self::reject_unsupported_fields(schema),
            TypeRepr::Option { .. } => Err(BuildError::UnsupportedFieldType {
                field: field.to_string(),
                type_label: "Option",
            }),
            TypeRepr::Result { .. } => Err(BuildError::UnsupportedFieldType {
                field: field.to_string(),
                type_label: "Result",
            }),
        }
    }

    /// The real `run_main` implementation, shared by the trait
    /// surface and any future host-facing variant that takes an
    /// explicit scope. Builds the `in_buf` from `args`, instantiates
    /// the wasm module, calls `run_main`, and decodes the returned
    /// bytes back into a [`Value`].
    fn run_main_inner(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        // Stage 1: build the input buffer. `BufferBuilder::finish`
        // returns the fixed-area + tail-area bytes the wasm side
        // expects at `in_ptr..in_ptr+in_len`.
        let in_bytes = self.build_input(&args)?;

        // Stage 2: spin up a fresh wasmtime session per call. v1
        // doesn't cache the `Store` because cross-call state (mutable
        // memory) would leak between independent invocations; Phase 9
        // can add a pooled mode for the hot loop.
        let mut store: Store<()> = Store::new(&self.engine, ());

        // The wasm module imports `(global $relon_caps_avail i64)`.
        // v1 grants every bit so the `check_cap` prologue never
        // trips; sandbox tightening lives one phase out.
        let caps_avail = Global::new(
            &mut store,
            GlobalType::new(ValType::I64, Mutability::Const),
            Val::I64(i64::MAX),
        )
        .map_err(|e| RuntimeError::IoError(format!("wasm caps_avail global: {e}")))?;

        let mut linker: Linker<()> = Linker::new(&self.engine);
        linker
            .define(&mut store, "env", "relon_caps_avail", caps_avail)
            .map_err(|e| RuntimeError::IoError(format!("wasm linker define: {e}")))?;

        let instance = linker
            .instantiate(&mut store, &self.compiled)
            .map_err(|e| self.module.translate_trap(&e))?;
        let memory: Memory = instance.get_memory(&mut store, "memory").ok_or_else(|| {
            RuntimeError::IoError("wasm module missing `memory` export".to_string())
        })?;
        let run_main: TypedFunc<(i32, i32, i32, i32), i32> = instance
            .get_typed_func(&mut store, "run_main")
            .map_err(|e| RuntimeError::IoError(format!("wasm `run_main` export: {e}")))?;

        // Stage 3: place in_buf + out_buf inside the wasm linear
        // memory. We anchor at the `relon_data_top` export (the codegen
        // sets that to the byte right after the const-data section) so
        // we never trample on the read-only data the wasm reads at
        // runtime.
        let data_top = instance
            .get_global(&mut store, "relon_data_top")
            .and_then(|g| match g.get(&mut store) {
                Val::I32(v) => Some(v as u32),
                _ => None,
            })
            .unwrap_or(crate::DATA_SECTION_BASE);

        let in_ptr = align_up(data_top, 8);
        let in_len = in_bytes.len() as u32;
        let out_ptr = align_up(in_ptr + in_len, 8);
        let out_cap = self.out_cap;

        // Ensure the wasm memory is big enough to hold both buffers.
        let needed_end = (out_ptr + out_cap) as usize;
        let current_size = memory.data_size(&store);
        if needed_end > current_size {
            const PAGE_SIZE: usize = 64 * 1024;
            let grow_pages = needed_end.div_ceil(PAGE_SIZE) - (current_size / PAGE_SIZE);
            memory
                .grow(&mut store, grow_pages as u64)
                .map_err(|e| RuntimeError::IoError(format!("wasm memory.grow: {e}")))?;
        }

        memory
            .write(&mut store, in_ptr as usize, &in_bytes)
            .map_err(|e| RuntimeError::IoError(format!("wasm memory write (in_buf): {e}")))?;

        let bytes_written = run_main
            .call(
                &mut store,
                (in_ptr as i32, in_len as i32, out_ptr as i32, out_cap as i32),
            )
            .map_err(|e| self.module.translate_trap(&e))?;
        if bytes_written < 0 {
            return Err(RuntimeError::IoError(format!(
                "wasm run_main reported negative bytes_written: {bytes_written}"
            )));
        }
        let bytes_written = bytes_written as usize;

        // Stage 4: read out_buf back and decode through BufferReader.
        let mut out_bytes = vec![0u8; bytes_written.max(self.return_layout.root_size)];
        memory
            .read(&mut store, out_ptr as usize, &mut out_bytes)
            .map_err(|e| RuntimeError::IoError(format!("wasm memory read (out_buf): {e}")))?;
        self.decode_return(&out_bytes)
    }

    /// Pack `args` into the wasm input buffer using `BufferBuilder`.
    ///
    /// Walks `main_schema.fields` in declaration order so every slot
    /// gets initialised — missing entries trip
    /// [`RuntimeError::MissingMainArg`] before we ever launch the
    /// wasm side (the entry function's `in_len` guard would catch a
    /// short buffer too, but only after a wasm trap, which is
    /// strictly worse diagnostics).
    fn build_input(&self, args: &HashMap<String, Value>) -> Result<Vec<u8>, RuntimeError> {
        let mut builder = BufferBuilder::new(&self.main_layout, &self.main_schema.fields);
        for field in &self.main_schema.fields {
            let value = args
                .get(&field.name)
                .ok_or_else(|| RuntimeError::MissingMainArg {
                    name: field.name.clone(),
                    range: relon_parser::TokenRange::default(),
                })?;
            write_value_into_builder(&mut builder, field, value, &self.main_schema.name)?;
        }
        Ok(builder.finish())
    }

    /// Decode the wasm-emitted return record into a [`Value`].
    ///
    /// When the return schema carries a single `value` field (the
    /// IR-synthesised wrapper for primitive returns), the result is
    /// just that field. When it carries a user schema (the dict /
    /// branded-record path), the entire fixed area is read as a
    /// branded `Value::Dict`.
    fn decode_return(&self, out_bytes: &[u8]) -> Result<Value, RuntimeError> {
        let reader = BufferReader::new(&self.return_layout, &self.return_schema.fields, out_bytes)
            .map_err(buffer_to_runtime_error)?;
        // The IR lowering synthesises a `Ret { value: T }` wrapper for
        // primitive returns. We detect that shape and unwrap it so the
        // host sees the primitive directly. For user-typed returns
        // (named schema), the entire record is the dict.
        if is_single_value_wrapper(&self.return_schema) {
            let field = &self.return_schema.fields[0];
            read_value_from_reader(&reader, field, &self.return_schema)
        } else {
            // Treat as a branded record matching `return_schema.name`.
            let map = read_record_into_map(&reader, &self.return_schema)?;
            Ok(Value::branded_dict(
                map,
                Some(self.return_schema.name.clone()),
            ))
        }
    }
}

/// Round `value` up to the next multiple of `align`. `align` is
/// expected to be a power of two; callers pass `8` for the
/// in_buf / out_buf placement, which dwarfs every leaf alignment
/// the v1 layout asks for.
fn align_up(value: u32, align: u32) -> u32 {
    let rem = value % align;
    if rem == 0 {
        value
    } else {
        value + (align - rem)
    }
}

/// Detect the IR-synthesised `Ret { value: T }` wrapper. Matches when
/// the schema's name is exactly [`relon_ir::MAIN_RETURN_SCHEMA_NAME`]
/// and it carries a single field named
/// [`relon_ir::RETURN_VALUE_FIELD_NAME`].
fn is_single_value_wrapper(schema: &Schema) -> bool {
    schema.name == relon_ir::MAIN_RETURN_SCHEMA_NAME
        && schema.fields.len() == 1
        && schema.fields[0].name == relon_ir::RETURN_VALUE_FIELD_NAME
}

/// Write `value` into the matching slot of `builder`. Surface a
/// `MainArgTypeMismatch` when the caller-supplied [`Value`] doesn't
/// shape-match the schema's declared type.
fn write_value_into_builder(
    builder: &mut BufferBuilder<'_>,
    field: &Field,
    value: &Value,
    schema_name: &str,
) -> Result<(), RuntimeError> {
    match (&field.ty, value) {
        (TypeRepr::Int, Value::Int(v)) => builder
            .write_int(&field.name, *v)
            .map_err(buffer_to_runtime_error),
        (TypeRepr::Float, Value::Float(v)) => builder
            .write_float(&field.name, v.into_inner())
            .map_err(buffer_to_runtime_error),
        // Accept Int → Float promotion so JSON `1` flows into a Float
        // slot without forcing the caller to spell `1.0`. Matches the
        // tree-walker's existing leniency at the host boundary.
        (TypeRepr::Float, Value::Int(v)) => builder
            .write_float(&field.name, *v as f64)
            .map_err(buffer_to_runtime_error),
        (TypeRepr::Bool, Value::Bool(v)) => builder
            .write_bool(&field.name, *v)
            .map_err(buffer_to_runtime_error),
        (TypeRepr::Null, Value::Null) => builder
            .write_null(&field.name)
            .map_err(buffer_to_runtime_error),
        (TypeRepr::String, Value::String(s)) => builder
            .write_string(&field.name, s.as_str())
            .map_err(buffer_to_runtime_error),
        (TypeRepr::List { element }, Value::List(items)) if matches!(element.as_ref(), TypeRepr::Int) => {
            let mut ints: Vec<i64> = Vec::with_capacity(items.len());
            for (i, item) in items.iter().enumerate() {
                match item {
                    Value::Int(n) => ints.push(*n),
                    other => {
                        return Err(RuntimeError::MainArgTypeMismatch {
                            name: format!("{}[{}]", field.name, i),
                            expected: "Int".to_string(),
                            found: other.type_name().to_string(),
                            range: relon_parser::TokenRange::default(),
                        });
                    }
                }
            }
            builder
                .write_list_int(&field.name, &ints)
                .map_err(buffer_to_runtime_error)
        }
        // Nested branded sub-record: not yet supported on the input
        // side because BufferBuilder doesn't expose a `sub_record`
        // writer counterpart. v1 leaves dict-typed `#main` parameters
        // for Phase 9.
        (TypeRepr::Schema { .. }, _) => Err(RuntimeError::Unsupported {
            reason: format!(
                "wasm-aot backend does not yet support Schema-typed `#main` arg `{field}` (schema `{schema}`)",
                field = field.name,
                schema = schema_name,
            ),
        }),
        (expected, found) => Err(RuntimeError::MainArgTypeMismatch {
            name: field.name.clone(),
            expected: type_label(expected).to_string(),
            found: found.type_name().to_string(),
            range: relon_parser::TokenRange::default(),
        }),
    }
}

/// Read a single field out of `reader` as a [`Value`]. Recurses into
/// nested branded sub-records so the dict-return path resolves all the
/// way down without a separate driver.
fn read_value_from_reader(
    reader: &BufferReader<'_>,
    field: &Field,
    parent_schema: &Schema,
) -> Result<Value, RuntimeError> {
    match &field.ty {
        TypeRepr::Int => reader
            .read_int(&field.name)
            .map(Value::Int)
            .map_err(buffer_to_runtime_error),
        TypeRepr::Float => reader
            .read_float(&field.name)
            .map(|f| Value::Float(ordered_float::OrderedFloat(f)))
            .map_err(buffer_to_runtime_error),
        TypeRepr::Bool => reader
            .read_bool(&field.name)
            .map(Value::Bool)
            .map_err(buffer_to_runtime_error),
        TypeRepr::Null => reader
            .read_null(&field.name)
            .map(|()| Value::Null)
            .map_err(buffer_to_runtime_error),
        TypeRepr::String => reader
            .read_string(&field.name)
            .map(|s| Value::String(s.to_string()))
            .map_err(buffer_to_runtime_error),
        TypeRepr::List { element } if matches!(element.as_ref(), TypeRepr::Int) => reader
            .read_list_int(&field.name)
            .map(|v| Value::list(v.into_iter().map(Value::Int).collect()))
            .map_err(buffer_to_runtime_error),
        TypeRepr::Schema { schema } => {
            // Borrow the sub-record reader anchored at the parent's
            // pointer slot, then walk every field of the nested schema.
            let sub_layout = SchemaLayout::offsets_for(schema)
                .map_err(|e| RuntimeError::IoError(format!("wasm sub-record layout: {e}")))?;
            let sub_reader = reader
                .sub_record(&field.name, &sub_layout, &schema.fields)
                .map_err(buffer_to_runtime_error)?;
            let map = read_record_into_map(&sub_reader, schema)?;
            Ok(Value::branded_dict(map, Some(schema.name.clone())))
        }
        other => Err(RuntimeError::Unsupported {
            reason: format!(
                "wasm-aot backend cannot read field `{field}` of type `{ty:?}` in schema `{schema}`",
                field = field.name,
                ty = other,
                schema = parent_schema.name,
            ),
        }),
    }
}

/// Drain every field of `schema` into a sorted `BTreeMap<String,
/// Value>`. The `BTreeMap` matches the [`relon_eval_api::ValueDict`]
/// inner shape so the caller can wrap the result with
/// `Value::branded_dict` without resorting.
fn read_record_into_map(
    reader: &BufferReader<'_>,
    schema: &Schema,
) -> Result<BTreeMap<String, Value>, RuntimeError> {
    let mut map = BTreeMap::new();
    for field in &schema.fields {
        let value = read_value_from_reader(reader, field, schema)?;
        map.insert(field.name.clone(), value);
    }
    Ok(map)
}

/// Map a [`BufferError`] back into a [`RuntimeError`] so the
/// trait surface stays uniform. Buffer-side mismatches always
/// indicate an ABI / schema-drift bug and surface as
/// [`RuntimeError::IoError`] with a descriptive prefix.
fn buffer_to_runtime_error(e: BufferError) -> RuntimeError {
    RuntimeError::IoError(format!("wasm buffer: {e}"))
}

/// Map a [`TypeRepr`] to its human-readable label.
fn type_label(ty: &TypeRepr) -> &'static str {
    match ty {
        TypeRepr::Null => "Null",
        TypeRepr::Bool => "Bool",
        TypeRepr::Int => "Int",
        TypeRepr::Float => "Float",
        TypeRepr::String => "String",
        TypeRepr::List { .. } => "List",
        TypeRepr::Option { .. } => "Option",
        TypeRepr::Result { .. } => "Result",
        TypeRepr::Schema { .. } => "Schema",
    }
}

impl Evaluator for WasmAotEvaluator {
    /// Not supported: wasm-AOT has no AST at runtime.
    fn eval(&self, _node: &Node, _scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "wasm-aot backend does not support arbitrary node evaluation".to_string(),
        })
    }

    /// Not supported: wasm-AOT compiles the document into `run_main`;
    /// the document's body is no longer reachable as an AST.
    fn eval_root(&self, _scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "wasm-aot backend does not support eval_root (no AST at runtime)".to_string(),
        })
    }

    fn run_main(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        self.run_main_inner(args)
    }

    /// Not supported: wasm-AOT topologically schedules every binding
    /// at compile time, so there are no live thunks at runtime.
    fn force_thunk(&self, _thunk: &Arc<Thunk>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "wasm-aot backend has no live thunks (topo-eager evaluation)".to_string(),
        })
    }

    /// Not supported: wasm-AOT does not surface closures as
    /// first-class values; user `fn` declarations lower to
    /// wasm-function calls, not [`Value::Closure`].
    fn invoke_closure(
        &self,
        _closure: &relon_eval_api::ClosureData,
        _args: &[Value],
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "wasm-aot backend does not expose first-class closures".to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_handles_already_aligned() {
        assert_eq!(align_up(0, 8), 0);
        assert_eq!(align_up(8, 8), 8);
        assert_eq!(align_up(16, 8), 16);
    }

    #[test]
    fn align_up_rounds_up() {
        assert_eq!(align_up(1, 8), 8);
        assert_eq!(align_up(7, 8), 8);
        assert_eq!(align_up(9, 8), 16);
    }
}
