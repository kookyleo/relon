//! Runtime façade for the LLVM AOT backend.
//!
//! Phase B widens the evaluator past the bootstrap envelope:
//!
//! - [`LlvmAotEvaluator::from_ir_direct`] keeps the legacy-i64 entry
//!   shape (`(I64...) -> I64`) for hand-built IR fixtures and the
//!   side-by-side `from_ir_direct` benches.
//! - [`LlvmAotEvaluator::from_source`] drives the full
//!   parse + analyze + `lower_workspace_single` + LLVM emit + JIT
//!   pipeline. Matches the cranelift backend's `from_source` shape
//!   so a host can swap the two evaluators by changing the
//!   constructor name.
//!
//! ## Why MCJIT (and not ORC) for Phase B
//!
//! - MCJIT is the simplest engine that inkwell exposes — single
//!   `create_jit_execution_engine` call, no per-symbol resolver
//!   plumbing. The Phase B goal is W1 / W2 production-source parity,
//!   not throughput.
//! - inkwell 0.9.0 wraps both engines, so switching to ORC in
//!   Phase C is a localised diff: one call site here, the emitter
//!   stays untouched.
//! - LLVM 18's MCJIT still handles the W1 / W2 hot path (single
//!   function, no global state, no external symbols).

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use inkwell::context::Context;
use inkwell::execution_engine::ExecutionEngine;
use inkwell::passes::PassBuilderOptions;
use inkwell::targets::{
    CodeModel, InitializationConfig, RelocMode, Target, TargetMachine, TargetTriple,
};
use inkwell::OptimizationLevel;

use relon_eval_api::{ClosureData, Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_parser::Node;

use crate::emitter::{emit_function, is_buffer_protocol_signature, EntryShape, ENTRY_SYMBOL};
use crate::error::LlvmError;
use crate::state::ArenaState;

/// Maximum positional arity supported by the Phase A legacy-i64
/// entry. Mirrors the cranelift crate's `MAX_LEGACY_ARITY`; the four
/// slots cover every helloworld-style body in the Phase A bootstrap
/// + benchmarks.
///
/// Phase B adds the buffer-protocol path on top — that path is not
/// arity-capped because every IR arg flows through the buffer rather
/// than positional slots.
pub const MAX_LEGACY_ARITY: usize = 4;

// `extern "C"` function pointer aliases for the legacy-i64 entry.
// Five i64 slots accept the v5-β-1 envelope's max arity; shorter
// signatures pass zero in the trailing slots — the emitter only
// declares the parameters the IR has, so unused trailing slots are
// dead-on-arrival.
type LegacyEntryFn4 = unsafe extern "C" fn(i64, i64, i64, i64) -> i64;
type LegacyEntryFn3 = unsafe extern "C" fn(i64, i64, i64) -> i64;
type LegacyEntryFn2 = unsafe extern "C" fn(i64, i64) -> i64;
type LegacyEntryFn1 = unsafe extern "C" fn(i64) -> i64;
type LegacyEntryFn0 = unsafe extern "C" fn() -> i64;

/// `extern "C"` function pointer for the buffer-protocol entry. The
/// state pointer comes first to match the cranelift backend's
/// `BufferEntryFn` so the two evaluators share dispatch shape.
type BufferEntryFn = unsafe extern "C" fn(
    *const ArenaState,
    i32, // in_ptr
    i32, // in_len
    i32, // out_ptr
    i32, // out_cap
    i64, // caps
) -> i32;

/// Owned LLVM JIT state for a single compiled module. The
/// [`Context`] / [`ExecutionEngine`] pair must outlive every call
/// into the JITted function pointer; we park them on the heap behind
/// the evaluator so the host can ignore lifetimes.
struct JitOwned {
    // The `Context` must outlive the ExecutionEngine; we keep it in a
    // pinned heap slot so the engine's borrow stays valid for the
    // evaluator's lifetime.
    _engine: ExecutionEngine<'static>,
    /// Raw entry function pointer resolved once at construction time.
    /// Cached so the hot path is a single indirect call (matches the
    /// cranelift backend's `LegacyEntryFn` stash).
    entry_ptr: usize,
    /// Pre-rendered textual LLVM IR. inkwell 0.9's
    /// `ExecutionEngine::get_module*` is missing, so the dump-time
    /// call cannot reach back to the live module — we pay the
    /// `print_to_string` cost up-front.
    ir_dump: String,
    _ctx: Box<Context>,
}

// SAFETY: the inkwell ExecutionEngine + Context pair is not `Sync`
// by default — LLVM's `LLVMContextRef` is per-thread. The evaluator
// owns a `Mutex` around per-call mutable state so `run_main` can be
// driven from multiple threads safely (each blocks on the same JIT
// — Phase C will explore per-thread engine pools).
unsafe impl Send for JitOwned {}
unsafe impl Sync for JitOwned {}

/// Buffer schema metadata captured by `from_source`. Mirrors
/// `relon_codegen_native::evaluator::BufferSchema` — kept inside this
/// crate (rather than re-imported) so the LLVM backend stays
/// independent.
struct BufferSchema {
    main_schema: relon_eval_api::schema_canonical::Schema,
    return_schema: relon_eval_api::schema_canonical::Schema,
    main_layout: relon_eval_api::layout::OffsetTable,
    return_layout: relon_eval_api::layout::OffsetTable,
}

/// Phase B LLVM AOT evaluator. Either constructed from a pre-lowered
/// IR module via [`Self::from_ir_direct`] (legacy-i64 envelope) or
/// from a `.relon` source via [`Self::from_source`] (buffer-protocol
/// envelope).
pub struct LlvmAotEvaluator {
    jit: JitOwned,
    entry_shape: EntryShape,
    entry_arity: usize,
    param_names: Vec<String>,
    /// Buffer schema for source-driven entries; `None` for direct-IR.
    buffer_schema: Option<BufferSchema>,
}

thread_local! {
    /// Per-thread arena buffer reused across `run_main_buffer` calls
    /// on the same thread. The pool caches the largest `arena_size`
    /// the thread has ever requested; subsequent dispatches reuse
    /// the allocation and only pay a targeted `fill(0)` over the
    /// observable prefix. Mirrors the cranelift backend's
    /// `ARENA_POOL` to keep the dispatch boundary cost comparable.
    static LLVM_ARENA_POOL: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

impl LlvmAotEvaluator {
    /// Compile a pre-lowered IR module into a JIT-resident function
    /// pointer. Accepts either a legacy-i64 entry
    /// (`(I64...) -> I64`) or the buffer-protocol shape
    /// (`(I32, I32, I32, I32, I64) -> I32`); the emitter inspects the
    /// entry signature and picks the matching wrapper.
    ///
    /// `param_names` parallels the cranelift backend's
    /// `from_ir_direct` arg so the `Evaluator::run_main` dispatch
    /// can look up positional args by their declared name. Direct-IR
    /// callers without a schema can pass synthetic
    /// `["arg0", "arg1", …]` names.
    pub fn from_ir_direct(
        ir: relon_ir::ir::Module,
        param_names: Vec<String>,
    ) -> Result<Self, LlvmError> {
        Self::from_ir_inner(ir, param_names, None)
    }

    /// Drive the full `parse → analyze → lower → emit → JIT` pipeline
    /// against a `.relon` source. Matches the cranelift backend's
    /// `AotEvaluator::from_source` shape so hosts can swap the two
    /// evaluators by changing the constructor.
    ///
    /// Phase B accepts the IR shape `lower_workspace_single` emits
    /// for `#main` source with the W1 / W2 production envelope
    /// (range / map / sum). Sources outside that envelope (closures
    /// past peephole, schema-method dispatch, stdlib calls, …) fail
    /// at the LLVM emit step with `LlvmError::Codegen`.
    pub fn from_source(src: &str) -> Result<Self, LlvmError> {
        let (ir, main_schema, return_schema) = Self::lower_source(src)?;
        let main_layout = relon_eval_api::layout::SchemaLayout::offsets_for(&main_schema)
            .map_err(|e| LlvmError::Codegen(format!("main schema layout: {e}")))?;
        let return_layout = relon_eval_api::layout::SchemaLayout::offsets_for(&return_schema)
            .map_err(|e| LlvmError::Codegen(format!("return schema layout: {e}")))?;
        let param_names: Vec<String> = main_schema.fields.iter().map(|f| f.name.clone()).collect();
        let schema = BufferSchema {
            main_schema,
            return_schema,
            main_layout,
            return_layout,
        };
        Self::from_ir_inner(ir, param_names, Some(schema))
    }

    fn lower_source(
        src: &str,
    ) -> Result<
        (
            relon_ir::ir::Module,
            relon_eval_api::schema_canonical::Schema,
            relon_eval_api::schema_canonical::Schema,
        ),
        LlvmError,
    > {
        let ast = relon_parser::parse_document(src).map_err(|e| LlvmError::Parse(e.to_string()))?;
        let analyzed = relon_analyzer::analyze(&ast);
        if analyzed.has_errors() {
            let err_count = analyzed
                .diagnostics
                .iter()
                .filter(|d| d.severity() == relon_analyzer::Severity::Error)
                .count();
            return Err(LlvmError::Analyze(err_count));
        }
        let lowered = relon_ir::lower_workspace_single(&analyzed, &ast)
            .map_err(|e| LlvmError::Codegen(format!("lower_workspace_single: {e}")))?;
        Ok((lowered.module, lowered.main_schema, lowered.return_schema))
    }

    fn from_ir_inner(
        ir: relon_ir::ir::Module,
        param_names: Vec<String>,
        buffer_schema: Option<BufferSchema>,
    ) -> Result<Self, LlvmError> {
        let entry_idx = ir
            .entry_func_index
            .ok_or_else(|| LlvmError::Codegen("IR module has no entry function".into()))?;
        let entry = &ir.funcs[entry_idx];

        // Detect the shape up-front so we can validate `param_names`
        // against the correct envelope.
        let buffer_shape = is_buffer_protocol_signature(&entry.params, entry.ret);
        if !buffer_shape && entry.params.len() > MAX_LEGACY_ARITY {
            return Err(LlvmError::UnsupportedSignature(format!(
                "llvm-aot: {} params exceeds MAX_LEGACY_ARITY={MAX_LEGACY_ARITY}",
                entry.params.len()
            )));
        }
        let declared_arity = if buffer_shape {
            buffer_schema
                .as_ref()
                .map(|s| s.main_schema.fields.len())
                .unwrap_or(0)
        } else {
            entry.params.len()
        };
        if param_names.len() != declared_arity {
            return Err(LlvmError::UnsupportedSignature(format!(
                "llvm-aot: param_names len {} does not match declared arity {declared_arity}",
                param_names.len()
            )));
        }
        if buffer_shape && buffer_schema.is_none() {
            // A direct-IR caller handed in a buffer-protocol IR
            // without schema metadata. We can still JIT-compile,
            // but `run_main` won't be able to pack the input or
            // decode the output. Reject up-front so the host knows.
            return Err(LlvmError::UnsupportedSignature(
                "llvm-aot: buffer-protocol IR requires schema metadata; use from_source".into(),
            ));
        }
        if !buffer_shape && buffer_schema.is_some() {
            return Err(LlvmError::UnsupportedSignature(
                "llvm-aot: schema metadata supplied for non-buffer entry".into(),
            ));
        }

        // Build the LLVM module under a per-evaluator Context. We
        // leak the Context onto the heap and transmute the engine's
        // lifetime to `'static` (see SAFETY note on `JitOwned`).
        let ctx_box: Box<Context> = Box::new(Context::create());
        // SAFETY: `ctx_box` lives on the heap and we never deallocate
        // it before the engine.
        let ctx_static: &'static Context = unsafe { &*(ctx_box.as_ref() as *const Context) };

        let module = ctx_static.create_module("relon_llvm_aot");

        // Buffer-protocol entries return `bytes_written` as i32; under
        // the Phase B envelope this is statically the schema's
        // `return_layout.root_size` (no pointer-indirect StoreField
        // bumps the tail cursor). Legacy entries ignore this value.
        let buffer_return_size = buffer_schema
            .as_ref()
            .map(|s| s.return_layout.root_size as u32)
            .unwrap_or(0);
        let (_llvm_fn, entry_shape) =
            emit_function(ctx_static, &module, entry, buffer_return_size)?;

        module
            .verify()
            .map_err(|e| LlvmError::Codegen(format!("LLVM verifier rejected module: {e}")))?;

        // Run LLVM's `-O3` middle-end pipeline on the module before
        // handing it to MCJIT. MCJIT's `OptimizationLevel::Aggressive`
        // controls backend codegen optimizations (regalloc, instr
        // selection) but does **not** invoke the IR-level passes —
        // `mem2reg`, `instcombine`, `gvn`, `licm`, loop-unroll,
        // SLP-vectorize, etc. live in the middle-end pipeline. Without
        // them the emitted IR's alloca-heavy stack-machine lowering
        // hits the assembler unsimplified, leaving a 100×+ gap vs the
        // equivalent native Rust hot loop.
        //
        // The pipeline is built fresh through `PassBuilderOptions`
        // (the LLVM 17+ new pass manager) since inkwell 0.9 deprecates
        // the legacy `PassManager` for IR-level work on LLVM 18.
        run_default_o3_pipeline(&module)?;

        // Capture the dumped IR *after* the optimizer ran so tests
        // that assert on the IR see the post-opt shape (mem2reg /
        // loop simplification visible). The pre-opt shape is mostly
        // alloca / load / store noise.
        let ir_dump = module.print_to_string().to_string();

        let engine = module
            .create_jit_execution_engine(OptimizationLevel::Aggressive)
            .map_err(|e| LlvmError::Codegen(format!("create_jit_execution_engine: {e}")))?;

        let entry_ptr = engine.get_function_address(ENTRY_SYMBOL).map_err(|e| {
            LlvmError::Codegen(format!(
                "ExecutionEngine could not resolve `{ENTRY_SYMBOL}`: {e}"
            ))
        })?;

        Ok(Self {
            jit: JitOwned {
                _engine: engine,
                entry_ptr,
                ir_dump,
                _ctx: ctx_box,
            },
            entry_shape,
            entry_arity: entry.params.len(),
            param_names,
            buffer_schema,
        })
    }

    /// Number of `#main` arguments expected. Under the buffer-protocol
    /// shape this is the count of declared `#main(...)` params (from
    /// the source schema), not the entry function's IR arity (which
    /// is always 5 for buffer protocol). Under the legacy-i64 shape
    /// the two coincide.
    pub fn arity(&self) -> usize {
        self.param_names.len()
    }

    /// Names of the declared `#main` parameters in declaration order.
    pub fn param_names(&self) -> &[String] {
        &self.param_names
    }

    /// Fast-path entry mirroring `AotEvaluator::run_main_legacy_i64`:
    /// skip the HashMap pack and invoke the JIT entry with a slice of
    /// positional i64 args. Only valid for the legacy-i64 entry shape.
    pub fn run_main_legacy_i64(&self, args: &[i64]) -> Result<i64, RuntimeError> {
        if self.entry_shape != EntryShape::LegacyI64 {
            return Err(RuntimeError::Unsupported {
                reason: "llvm-aot: run_main_legacy_i64 called on buffer-protocol entry".into(),
            });
        }
        if args.len() != self.entry_arity {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "llvm-aot: #main expects {} arg(s), got {}",
                    self.entry_arity,
                    args.len()
                ),
            });
        }
        let ptr = self.jit.entry_ptr;
        // SAFETY: see Phase A `run_main_legacy_i64` for the same
        // transmute-and-call pattern. The cached `entry_ptr` was
        // returned by `ExecutionEngine::get_function_address` at
        // construction time and stays valid for the engine's
        // lifetime.
        unsafe {
            match self.entry_arity {
                0 => {
                    let f: LegacyEntryFn0 = std::mem::transmute(ptr);
                    Ok(f())
                }
                1 => {
                    let f: LegacyEntryFn1 = std::mem::transmute(ptr);
                    Ok(f(args[0]))
                }
                2 => {
                    let f: LegacyEntryFn2 = std::mem::transmute(ptr);
                    Ok(f(args[0], args[1]))
                }
                3 => {
                    let f: LegacyEntryFn3 = std::mem::transmute(ptr);
                    Ok(f(args[0], args[1], args[2]))
                }
                4 => {
                    let f: LegacyEntryFn4 = std::mem::transmute(ptr);
                    Ok(f(args[0], args[1], args[2], args[3]))
                }
                n => Err(RuntimeError::Unsupported {
                    reason: format!("llvm-aot: arity {n} > MAX_LEGACY_ARITY={MAX_LEGACY_ARITY}"),
                }),
            }
        }
    }

    /// Print the emitted LLVM IR. Useful for tests / benchmarks that
    /// want to assert against the lowering output without leaving
    /// the test binary.
    pub fn emit_ir_dump(&self) -> &str {
        &self.jit.ir_dump
    }

    /// Buffer-protocol `run_main`: pack the HashMap-keyed args into
    /// an arena, invoke the JIT, decode the return record.
    fn run_main_buffer(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        let schema = self
            .buffer_schema
            .as_ref()
            .ok_or_else(|| RuntimeError::Unsupported {
                reason: "llvm-aot: run_main_buffer called without schema metadata".into(),
            })?;

        // 1. Pack the args into a buffer using `BufferBuilder`.
        let mut builder = relon_eval_api::buffer::BufferBuilder::new(
            &schema.main_layout,
            &schema.main_schema.fields,
        );
        for field in &schema.main_schema.fields {
            let value = args
                .get(&field.name)
                .ok_or_else(|| RuntimeError::Unsupported {
                    reason: format!("llvm-aot: missing #main arg `{}`", field.name),
                })?;
            write_value_into_builder(&mut builder, field, value)?;
        }
        let in_bytes = builder.finish();

        // 2. Lay out the arena: [in_buf | pad | out_buf]. Phase B
        // does not emit pointer-indirect record returns, so no
        // tail-cursor / scratch region is needed. The const-data
        // pool the cranelift backend prepends is also unused — we
        // don't emit ConstString / ConstListInt yet.
        let in_len = in_bytes.len() as u32;
        let out_root_size = schema.return_layout.root_size as u32;
        // Pad the output reservation to 8 bytes so an i64 return
        // slot stays aligned. The IR's StoreField offset always
        // matches the schema layout, but we add a 16-byte cushion
        // so subsequent fields stay in-bounds if the schema grows.
        let out_cap = align_up(out_root_size.max(8) + 16, 8);
        let in_ptr = 0u32;
        let out_ptr = align_up(in_ptr + in_len, 8);
        let arena_size = (out_ptr + out_cap) as usize;

        // 3. Acquire the per-thread arena buffer, install the
        // input bytes, dispatch. Reentrant calls (a stdlib helper
        // looping back through the evaluator on the same thread)
        // fall back to a fresh `Vec<u8>` — correctness wins over
        // pool reuse on the vanishingly rare path. Phase B does
        // not emit any host-call surface so reentrance is currently
        // impossible; the fallback is still cheap to keep.
        LLVM_ARENA_POOL.with(|cell| match cell.try_borrow_mut() {
            Ok(mut buf) => self.dispatch_with_arena(
                schema, &mut buf, arena_size, in_ptr, in_len, out_ptr, out_cap, &in_bytes,
            ),
            Err(_) => {
                let mut fallback: Vec<u8> = Vec::new();
                self.dispatch_with_arena(
                    schema,
                    &mut fallback,
                    arena_size,
                    in_ptr,
                    in_len,
                    out_ptr,
                    out_cap,
                    &in_bytes,
                )
            }
        })
    }

    /// Inner driver shared by the pooled-arena and fallback-arena
    /// branches of [`Self::run_main_buffer`]. Resizes `arena` to
    /// `arena_size`, copies `in_bytes` into the input region,
    /// invokes the JIT, then decodes the output region.
    #[allow(clippy::too_many_arguments)]
    fn dispatch_with_arena(
        &self,
        schema: &BufferSchema,
        arena: &mut Vec<u8>,
        arena_size: usize,
        in_ptr: u32,
        in_len: u32,
        out_ptr: u32,
        out_cap: u32,
        in_bytes: &[u8],
    ) -> Result<Value, RuntimeError> {
        if arena.len() < arena_size {
            arena.resize(arena_size, 0);
        }
        // Zero only the region the JIT can observe.
        arena[..arena_size].fill(0);
        arena[in_ptr as usize..in_ptr as usize + in_bytes.len()].copy_from_slice(in_bytes);

        let live_arena = &mut arena[..arena_size];
        let state = ArenaState::new(live_arena);
        let state_ptr: *const ArenaState = &state;

        // SAFETY: same pattern as the cranelift backend's
        // `invoke_buffer_entry`. The JIT entry was emitted with the
        // canonical buffer-protocol signature; the cached fn pointer
        // is alive for the engine's lifetime. The arena slice
        // `live_arena` outlives the JIT call.
        let bytes_written = {
            let f: BufferEntryFn = unsafe { std::mem::transmute(self.jit.entry_ptr) };
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| unsafe {
                f(
                    state_ptr,
                    in_ptr as i32,
                    in_len as i32,
                    out_ptr as i32,
                    out_cap as i32,
                    /*caps=*/ 0,
                )
            }))
            .map_err(|_| RuntimeError::Unsupported {
                reason: "llvm-aot: JIT entry panicked (no trap-code recovery in Phase B)".into(),
            })?
        };

        if bytes_written < 0 {
            return Err(RuntimeError::IoError(format!(
                "llvm-aot run_main reported negative bytes_written: {bytes_written}"
            )));
        }
        let bw = bytes_written as usize;

        let read_len = bw.max(schema.return_layout.root_size);
        let read_end = out_ptr as usize + read_len;
        if read_end > arena_size {
            return Err(RuntimeError::IoError(
                "llvm-aot arena too small for return decode".into(),
            ));
        }
        let out_bytes = &arena[out_ptr as usize..read_end];
        let reader = relon_eval_api::buffer::BufferReader::new(
            &schema.return_layout,
            &schema.return_schema.fields,
            out_bytes,
        )
        .map_err(buffer_to_runtime_error)?;
        if is_single_value_wrapper(&schema.return_schema) {
            let field = &schema.return_schema.fields[0];
            read_value_from_reader(&reader, field)
        } else {
            let map = read_record_into_map(&reader, &schema.return_schema)?;
            Ok(Value::branded_dict(
                map,
                Some(schema.return_schema.name.clone()),
            ))
        }
    }
}

impl Evaluator for LlvmAotEvaluator {
    fn eval(&self, _node: &Node, _scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "llvm-aot: `eval` is not supported".into(),
        })
    }

    fn eval_root(&self, _scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "llvm-aot: `eval_root` is not supported".into(),
        })
    }

    fn run_main(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        match self.entry_shape {
            EntryShape::Buffer => self.run_main_buffer(args),
            EntryShape::LegacyI64 => {
                // Pack the HashMap into a positional i64 argv using
                // the declared parameter order.
                let mut argv = [0i64; MAX_LEGACY_ARITY];
                for (i, name) in self.param_names.iter().enumerate() {
                    let v = args.get(name).ok_or_else(|| RuntimeError::Unsupported {
                        reason: format!("llvm-aot: missing #main arg `{name}`"),
                    })?;
                    match v {
                        Value::Int(n) => argv[i] = *n,
                        other => {
                            return Err(RuntimeError::Unsupported {
                                reason: format!(
                                    "llvm-aot: legacy-i64 #main arg `{name}` is {} (Int only)",
                                    other.type_name()
                                ),
                            });
                        }
                    }
                }
                let r = self.run_main_legacy_i64(&argv[..self.entry_arity])?;
                Ok(Value::Int(r))
            }
        }
    }

    fn force_thunk(&self, _thunk: &Arc<Thunk>) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "llvm-aot: `force_thunk` is not supported".into(),
        })
    }

    fn invoke_closure(
        &self,
        _closure: &ClosureData,
        _args: &[Value],
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "llvm-aot: `invoke_closure` is not supported".into(),
        })
    }
}

// ---------------------------------------------------------------------------
// Buffer-protocol packing / unpacking helpers.
//
// These mirror what `relon-codegen-native::evaluator` does for
// `write_value_into_builder` / `read_value_from_reader` /
// `read_record_into_map` / `is_single_value_wrapper` /
// `buffer_to_runtime_error`. Kept inside this crate so the LLVM
// backend has no compile-time dep on cranelift-native.
// ---------------------------------------------------------------------------

fn buffer_to_runtime_error(e: relon_eval_api::buffer::BufferError) -> RuntimeError {
    RuntimeError::IoError(format!("llvm-aot buffer: {e}"))
}

fn is_single_value_wrapper(schema: &relon_eval_api::schema_canonical::Schema) -> bool {
    schema.name == relon_ir::MAIN_RETURN_SCHEMA_NAME
        && schema.fields.len() == 1
        && schema.fields[0].name == relon_ir::RETURN_VALUE_FIELD_NAME
}

fn write_value_into_builder(
    builder: &mut relon_eval_api::buffer::BufferBuilder<'_>,
    field: &relon_eval_api::schema_canonical::Field,
    value: &Value,
) -> Result<(), RuntimeError> {
    use relon_eval_api::schema_canonical::TypeRepr;
    match (&field.ty, value) {
        (TypeRepr::Int, Value::Int(v)) => builder
            .write_int(&field.name, *v)
            .map_err(buffer_to_runtime_error),
        (TypeRepr::Float, Value::Float(v)) => builder
            .write_float(&field.name, v.into_inner())
            .map_err(buffer_to_runtime_error),
        (TypeRepr::Float, Value::Int(v)) => builder
            .write_float(&field.name, *v as f64)
            .map_err(buffer_to_runtime_error),
        (TypeRepr::Bool, Value::Bool(v)) => builder
            .write_bool(&field.name, *v)
            .map_err(buffer_to_runtime_error),
        (TypeRepr::Null, Value::Null) => builder
            .write_null(&field.name)
            .map_err(buffer_to_runtime_error),
        (ty, v) => Err(RuntimeError::Unsupported {
            reason: format!(
                "llvm-aot: #main arg `{}` got {} but schema expects {ty:?}",
                field.name,
                v.type_name()
            ),
        }),
    }
}

fn read_value_from_reader(
    reader: &relon_eval_api::buffer::BufferReader<'_>,
    field: &relon_eval_api::schema_canonical::Field,
) -> Result<Value, RuntimeError> {
    use relon_eval_api::schema_canonical::TypeRepr;
    match &field.ty {
        TypeRepr::Int => reader
            .read_int(&field.name)
            .map(Value::Int)
            .map_err(buffer_to_runtime_error),
        // Phase B does not exercise Float returns through the LLVM
        // backend (W1 / W2 are Int-only). When the W3+ work calls
        // for Float we'll add `ordered-float` as a dep or convert
        // through `f64::to_bits` round-trips — for now reject so a
        // future regression surfaces explicitly.
        TypeRepr::Float => Err(RuntimeError::Unsupported {
            reason: format!(
                "llvm-aot: return field `{}` Float not yet supported in Phase B",
                field.name
            ),
        }),
        TypeRepr::Bool => reader
            .read_bool(&field.name)
            .map(Value::Bool)
            .map_err(buffer_to_runtime_error),
        TypeRepr::Null => Ok(Value::Null),
        other => Err(RuntimeError::Unsupported {
            reason: format!(
                "llvm-aot: return field `{}` type {other:?} not supported in Phase B",
                field.name
            ),
        }),
    }
}

fn read_record_into_map(
    reader: &relon_eval_api::buffer::BufferReader<'_>,
    schema: &relon_eval_api::schema_canonical::Schema,
) -> Result<HashMap<String, Value>, RuntimeError> {
    let mut out = HashMap::with_capacity(schema.fields.len());
    for f in &schema.fields {
        let v = read_value_from_reader(reader, f)?;
        out.insert(f.name.clone(), v);
    }
    Ok(out)
}

fn align_up(value: u32, align: u32) -> u32 {
    let rem = value % align;
    if rem == 0 {
        value
    } else {
        value + (align - rem)
    }
}

/// Run LLVM's `-O3` middle-end pipeline on `module`. The host-side
/// JIT engine handles backend codegen-time optimisation; this
/// function fills in the IR-level passes (mem2reg, instcombine, gvn,
/// licm, loop-unroll, SLP-vectorize, …) that MCJIT does not invoke
/// on its own.
///
/// The implementation lazily initialises LLVM's native target the
/// first time it is called — required by `Target::from_triple` /
/// `create_target_machine`. Subsequent calls re-use the initialised
/// target state.
fn run_default_o3_pipeline(module: &inkwell::module::Module<'_>) -> Result<(), LlvmError> {
    Target::initialize_native(&InitializationConfig::default())
        .map_err(|e| LlvmError::Codegen(format!("initialize_native: {e}")))?;
    let triple_str = TargetMachine::get_default_triple();
    let target = Target::from_triple(&triple_str)
        .map_err(|e| LlvmError::Codegen(format!("target from_triple: {e}")))?;
    let cpu = TargetMachine::get_host_cpu_name();
    let features = TargetMachine::get_host_cpu_features();
    let triple = TargetTriple::create(
        triple_str
            .as_str()
            .to_str()
            .map_err(|e| LlvmError::Codegen(format!("triple utf8: {e}")))?,
    );
    let machine = target
        .create_target_machine(
            &triple,
            cpu.to_str().unwrap_or(""),
            features.to_str().unwrap_or(""),
            OptimizationLevel::Aggressive,
            RelocMode::Default,
            CodeModel::JITDefault,
        )
        .ok_or_else(|| LlvmError::Codegen("create_target_machine returned null".into()))?;
    let opts = PassBuilderOptions::create();
    module
        .run_passes("default<O3>", &machine, opts)
        .map_err(|e| LlvmError::Codegen(format!("run_passes O3: {e}")))?;
    Ok(())
}
