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

use crate::emitter::{
    emit_fast_entry, emit_module_funcs, is_buffer_protocol_signature, ConstPool, EntryShape,
    FastPathProfile, ENTRY_SYMBOL, ENTRY_SYMBOL_FAST,
};
use crate::error::LlvmError;
use crate::state::ArenaState;
use crate::str_helpers::RELON_LLVM_STR_CONTAINS_ARENA_SYMBOL;
use inkwell::module::Linkage;
use inkwell::targets::FileType;
use inkwell::values::FunctionValue;
use std::path::Path;

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

// Phase D.1 fast-path typed entries. Arity-specialised C ABI shapes
// up to 8 args — the arity cap matches `emit_fast_entry`'s envelope.
type FastEntryFn0 = unsafe extern "C" fn() -> i64;
type FastEntryFn1 = unsafe extern "C" fn(i64) -> i64;
type FastEntryFn2 = unsafe extern "C" fn(i64, i64) -> i64;
type FastEntryFn3 = unsafe extern "C" fn(i64, i64, i64) -> i64;
type FastEntryFn4 = unsafe extern "C" fn(i64, i64, i64, i64) -> i64;
type FastEntryFn5 = unsafe extern "C" fn(i64, i64, i64, i64, i64) -> i64;
type FastEntryFn6 = unsafe extern "C" fn(i64, i64, i64, i64, i64, i64) -> i64;
type FastEntryFn7 = unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64) -> i64;
type FastEntryFn8 = unsafe extern "C" fn(i64, i64, i64, i64, i64, i64, i64, i64) -> i64;

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
    /// Phase D.1: typed fast entry pointer resolved at construction
    /// time when the source qualifies for the dispatch-boundary fast
    /// path. `None` when the IR fails to lower against the fast
    /// envelope (string ops, sandbox traps, etc.) — `run_main` falls
    /// back to the buffer entry transparently in that case.
    fast_entry_ptr: Option<usize>,
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
    /// Phase D.1: when `Some`, the JIT module exported a typed
    /// `(i64...) -> i64` fast entry alongside the buffer entry. Held
    /// here so `run_main` can pick the fast pointer when the supplied
    /// args match the eligible shape. Length equals the fast-entry
    /// arity (matches `buffer_schema.main_schema.fields.len()` when
    /// every field is `Int`).
    fast_path_arity: Option<usize>,
    /// Phase E.1: const-data bytes the IR's `Op::ConstString` /
    /// `Op::ConstList*` records reference through arena-relative i32
    /// offsets. The host copies this blob into the arena prefix at
    /// every dispatch so the JIT-emitted `iconst(I32, offset)` lands
    /// on the right record.
    const_data: Vec<u8>,
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
        // W7 closure-as-value (Phase F.W7): the production source
        // `#main(Int n) -> Dict { #internal fib: (k) => ..., result: fib(n) }`
        // trips the v1.5 / v1.6 strict-mode type-surface diagnostics
        // (`ClosureParamTypeMissing`, `ClosureReturnTypeUnknown`,
        // `ExpressionTypeUnknown`) even though IR lowering accepts the
        // shape via `lower_anon_dict_body`. Mirror the bytecode tier
        // (`relon-bytecode::evaluator::from_source`): run the analyzer
        // with `strict_mode: false` so the soft bans don't gate LLVM
        // codegen. Hard structural errors (`UnknownTypeName`,
        // `MainReturnTypeMismatch`, etc.) still surface as `Error`-
        // severity diagnostics under non-strict mode and still gate the
        // build below.
        let analyzed = relon_analyzer::analyze_with_options(
            &ast,
            &relon_analyzer::AnalyzeOptions {
                strict_mode: false,
                ..Default::default()
            },
        );
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
        // Phase E.1: build the const-data pool by walking every
        // function body in `ir`. The blob is shipped to the host
        // alongside the JIT engine and copied to the arena prefix at
        // every dispatch so `Op::ConstString { idx }` resolves to a
        // stable arena-relative offset.
        let const_pool = ConstPool::from_module(&ir)?;
        // Phase E.2: collect every IR sibling function (non-entry,
        // non-lambda) so the LLVM emit pass can lower them alongside
        // the entry. The entry's `Op::Call` lowering resolves
        // user-defined sibling calls through the returned helper
        // table.
        //
        // Phase F.W7: collect the lambdas (funcs registered in
        // `closure_table`) separately so the emit pass can apply the
        // widened `(state, captures_ptr, ...params) -> ret` signature
        // and seed the closure function-pointer table. The IR's
        // `closure_table` maps a `fn_table_idx` to an `ir.funcs`
        // index; we mirror that order so the emit pass's
        // `closure_fn_table[fn_table_idx]` matches what `MakeClosure`
        // references.
        let lambda_ir_idx_set: std::collections::HashSet<u32> =
            ir.closure_table.iter().copied().collect();
        let helpers: Vec<&relon_ir::ir::Func> = ir
            .funcs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != entry_idx && !lambda_ir_idx_set.contains(&(*i as u32)))
            .map(|(_, f)| f)
            .collect();
        let helper_ir_indices: Vec<u32> = ir
            .funcs
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != entry_idx && !lambda_ir_idx_set.contains(&(*i as u32)))
            .map(|(i, _)| i as u32)
            .collect();
        let lambdas: Vec<&relon_ir::ir::Func> = ir
            .closure_table
            .iter()
            .map(|&ir_idx| &ir.funcs[ir_idx as usize])
            .collect();
        let (_llvm_fn, entry_shape, helper_table, closure_fn_table) = emit_module_funcs(
            ctx_static,
            &module,
            entry,
            buffer_return_size,
            &const_pool,
            &helpers,
            Some(&helper_ir_indices),
            &lambdas,
            &ir.closure_table,
        )?;

        // Phase D.1 / D.2: attempt to emit the typed fast-path entry
        // alongside the buffer entry whenever the schema qualifies.
        // Emission failure is treated as a "no fast path available"
        // condition rather than a hard error — the IR can stay on
        // the buffer entry, which is correct (just slower).
        //
        // We discover eligibility from the `buffer_schema` (declared
        // `#main` params + return) and the IR body. Sources that
        // touch ops outside the fast envelope (strings, sandbox
        // traps, non-self-recursive closures with non-virtualisable
        // captures, etc.) fail emission inside `emit_fast_entry`; we
        // capture the error to the IR dump for post-mortem and
        // continue with the buffer-only module.
        let fast_profile = buffer_schema
            .as_ref()
            .and_then(|s| build_fast_path_profile(s).ok());
        let mut fast_emit_diagnostic: Option<String> = None;
        if let Some(profile) = fast_profile.as_ref() {
            match emit_fast_entry(
                ctx_static,
                &module,
                entry,
                profile,
                &helper_table,
                &closure_fn_table,
            ) {
                Ok(_) => {}
                Err(e) => {
                    fast_emit_diagnostic = Some(format!("{e}"));
                    // Roll back the partially-emitted fast entry so
                    // the module verifies cleanly with just the
                    // buffer entry. inkwell's `delete` is unsafe
                    // because it invalidates any outstanding
                    // `FunctionValue` handle; the emitter dropped
                    // its handle when `emit_fast_entry` returned.
                    if let Some(f) = module.get_function(ENTRY_SYMBOL_FAST) {
                        unsafe { f.delete() };
                    }
                }
            }
        }

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
        // Debug: capture pre-opt IR if the env requests it via
        // `RELON_LLVM_DUMP_PREOPT=1`. The pre-opt shape is mostly
        // alloca / load / store noise but is useful when verifying
        // that emitter changes survived the dispatch path (post-opt
        // IR can have aggressive constant folding that makes brand-
        // new branches invisible). The flag is intentionally opt-in
        // so production paths never pay the second IR dump.
        let preopt_dump: Option<String> = std::env::var_os("RELON_LLVM_DUMP_PREOPT")
            .map(|_| module.print_to_string().to_string());

        run_default_o3_pipeline(&module)?;

        // Capture the dumped IR *after* the optimizer ran so tests
        // that assert on the IR see the post-opt shape (mem2reg /
        // loop simplification visible). The pre-opt shape is mostly
        // alloca / load / store noise.
        let mut ir_dump = module.print_to_string().to_string();
        if let Some(p) = preopt_dump {
            ir_dump = format!("; --- PRE-OPT IR ---\n{p}\n; --- POST-OPT IR ---\n{ir_dump}");
        }

        // Phase L profile-first: dump post-O3 IR + host-targeted ASM
        // to `$RELON_LLVM_DUMP_DIR/` when the env var is set. The dump
        // mirrors the actual MCJIT codegen path (same TargetMachine
        // knobs as `run_default_o3_pipeline`) so the .s file matches
        // what the JIT engine actually emits at JIT-resolve time.
        if let Some(dir) = std::env::var_os("RELON_LLVM_DUMP_DIR") {
            let dir = std::path::PathBuf::from(dir);
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(dir.join("module.post_o3.ll"), &ir_dump);
            // Re-create a TargetMachine matching the JIT path so the
            // dumped ASM is byte-equivalent to what MCJIT codegen
            // hands to the loader. The codegen-side OptLevel for MCJIT
            // is `Aggressive` (see `create_jit_execution_engine` call
            // below); mirror that here.
            if let Ok(()) = Target::initialize_native(&InitializationConfig::default()) {
                let triple_str = TargetMachine::get_default_triple();
                if let Ok(target) = Target::from_triple(&triple_str) {
                    let cpu = TargetMachine::get_host_cpu_name();
                    let features = TargetMachine::get_host_cpu_features();
                    if let Ok(triple_utf8) = triple_str.as_str().to_str() {
                        let triple = TargetTriple::create(triple_utf8);
                        if let Some(machine) = target.create_target_machine(
                            &triple,
                            cpu.to_str().unwrap_or(""),
                            features.to_str().unwrap_or(""),
                            OptimizationLevel::Aggressive,
                            RelocMode::Default,
                            CodeModel::JITDefault,
                        ) {
                            let _ = machine.write_to_file(
                                &module,
                                FileType::Assembly,
                                &dir.join("module.s"),
                            );
                            let _ = machine.write_to_file(
                                &module,
                                FileType::Object,
                                &dir.join("module.o"),
                            );
                        }
                        // Dump variant: CodeModel::Small + RelocMode::PIC
                        // so we can A/B with `module.s` and see whether the
                        // recursive call shrinks to a PC-rel `callq <sym>`.
                        if let Some(machine) = target.create_target_machine(
                            &triple,
                            cpu.to_str().unwrap_or(""),
                            features.to_str().unwrap_or(""),
                            OptimizationLevel::Aggressive,
                            RelocMode::PIC,
                            CodeModel::Small,
                        ) {
                            let _ = machine.write_to_file(
                                &module,
                                FileType::Assembly,
                                &dir.join("module.small_pic.s"),
                            );
                        }
                        // Dump variant: CodeModel::Small + RelocMode::Static.
                        if let Some(machine) = target.create_target_machine(
                            &triple,
                            cpu.to_str().unwrap_or(""),
                            features.to_str().unwrap_or(""),
                            OptimizationLevel::Aggressive,
                            RelocMode::Static,
                            CodeModel::Small,
                        ) {
                            let _ = machine.write_to_file(
                                &module,
                                FileType::Assembly,
                                &dir.join("module.small_static.s"),
                            );
                        }
                    }
                }
            }
        }

        // Phase L codegen-quality: pick the MCJIT engine builder by
        // whether the module references the host-side `contains` shim.
        //
        // - **No extern** -> use the custom memory manager + Small
        //   CodeModel. All same-module calls collapse to direct
        //   `callq <pcrel32>` instead of MCJIT's default
        //   `movabsq + callq *%reg` (Large CodeModel). For tight
        //   recursive bodies like W7 fib this saves ~0.2 ns / call
        //   on Intel; multiplied by fib(22)'s ~35 k call tree it
        //   closes ~10 µs of the gap vs the rustc LTO build.
        //
        // - **Extern present** -> stay on the default JIT builder
        //   (Large CodeModel) because the host-side shim lives in
        //   the executable's `.text` which is typically > 2 GB away
        //   from the JIT's freshly-mmap'd code arena. A 32-bit
        //   PC-relative relocation would fail to resolve; the Large
        //   CodeModel's `movabsq + indirect` pattern handles it.
        //
        // Detection is purely structural — we look up the shim
        // symbol on the module. The emitter declares it lazily, so
        // its presence means "this module has at least one extern
        // call site that needs `add_global_mapping` after engine
        // creation".
        let uses_extern_shim = module
            .get_function(crate::str_helpers::RELON_LLVM_STR_CONTAINS_ARENA_SYMBOL)
            .is_some();
        let force_default_mcjit = std::env::var_os("RELON_LLVM_FORCE_DEFAULT_MCJIT").is_some();
        let engine = if uses_extern_shim || force_default_mcjit {
            module
                .create_jit_execution_engine(OptimizationLevel::Aggressive)
                .map_err(|e| LlvmError::Codegen(format!("create_jit_execution_engine: {e}")))?
        } else {
            let mm = crate::mcjit_mm::ContiguousCodeMemoryManager::new();
            module
                .create_mcjit_execution_engine_with_memory_manager(
                    mm,
                    OptimizationLevel::Aggressive,
                    inkwell::targets::CodeModel::Small,
                    /*no_frame_pointer_elim=*/ false,
                    /*enable_fast_isel=*/ false,
                )
                .map_err(|e| {
                    LlvmError::Codegen(format!(
                        "create_mcjit_execution_engine_with_memory_manager (Small CodeModel): {e}"
                    ))
                })?
        };

        // Phase F.1: wire the host shim that backs the LLVM AOT
        // `contains(haystack, needle) -> Bool` fast path. The emitter
        // declares this symbol with `Linkage::External` whenever a
        // module references it; MCJIT needs an explicit address
        // mapping because the default resolver (`dlsym`) cannot see
        // statics from inside the current dylib's strip-able section
        // layout. We register unconditionally — if the module never
        // referenced the symbol the mapping is a no-op.
        if let Some(shim_fn) =
            module.get_function(crate::str_helpers::RELON_LLVM_STR_CONTAINS_ARENA_SYMBOL)
        {
            engine.add_global_mapping(
                &shim_fn,
                crate::str_helpers::relon_llvm_str_contains_arena_addr(),
            );
        }

        let entry_ptr = engine.get_function_address(ENTRY_SYMBOL).map_err(|e| {
            LlvmError::Codegen(format!(
                "ExecutionEngine could not resolve `{ENTRY_SYMBOL}`: {e}"
            ))
        })?;

        // Phase D.1: resolve the typed fast-entry pointer when the
        // module exported one. Resolution failure here is *not* an
        // emit-side bug — the symbol simply wasn't emitted (or was
        // rolled back) — so we treat it as "no fast path" silently.
        let (fast_entry_ptr, fast_path_arity) = match (&fast_profile, &fast_emit_diagnostic) {
            (Some(profile), None) => match engine.get_function_address(ENTRY_SYMBOL_FAST) {
                Ok(ptr) => (Some(ptr), Some(profile.arg_offsets.len())),
                Err(_) => (None, None),
            },
            _ => (None, None),
        };
        // Stash the fast-emit diagnostic (if any) into the IR dump so
        // post-mortem tests can assert on it without needing a
        // dedicated getter. The dump is only consumed by tests so the
        // overhead doesn't matter at runtime.
        let ir_dump = match fast_emit_diagnostic {
            Some(diag) => format!("; fast-emit diagnostic: {diag}\n{ir_dump}"),
            None => ir_dump,
        };

        Ok(Self {
            jit: JitOwned {
                _engine: engine,
                entry_ptr,
                fast_entry_ptr,
                ir_dump,
                _ctx: ctx_box,
            },
            entry_shape,
            entry_arity: entry.params.len(),
            param_names,
            buffer_schema,
            fast_path_arity,
            const_data: const_pool.bytes,
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

    /// Phase D.1: does this evaluator have a JIT-resident fast entry
    /// the host can dispatch through when args are all-Int? Exposed
    /// for the smoke tests that assert the fast path is wired up;
    /// benches use it to log which row hit the fast vs buffer path.
    pub fn has_fast_path(&self) -> bool {
        self.jit.fast_entry_ptr.is_some()
    }

    /// Phase D.1: arity of the typed fast entry, when one was emitted.
    /// Matches `arity()` for source-driven entries that qualify; `None`
    /// when the source falls back to the buffer-only path.
    pub fn fast_path_arity(&self) -> Option<usize> {
        self.fast_path_arity
    }

    /// Phase L codegen-quality debug helper: raw address of the typed
    /// fast-entry function in the JIT-allocated code arena. Returns
    /// `None` if the source falls back to the buffer entry. Hosts use
    /// this to disassemble the MCJIT-produced machine code at runtime
    /// (`xxd` / `objdump --disassemble-all` on a byte slice) — useful
    /// for confirming whether the engine emitted direct `callq <pcrel>`
    /// vs the Large-CodeModel `movabsq + callq *%reg` shape.
    pub fn fast_entry_runtime_addr(&self) -> Option<usize> {
        self.jit.fast_entry_ptr
    }

    /// Phase L codegen-quality debug helper: raw address of the
    /// buffer-protocol entry function in the JIT-allocated code arena.
    /// Always populated for a successful `from_source` build.
    pub fn entry_runtime_addr(&self) -> usize {
        self.jit.entry_ptr
    }

    /// Phase D.1 dispatch-boundary fast path: invoke the typed fast
    /// entry directly with positional `i64` args. Bypasses the
    /// `HashMap` pack, `BufferBuilder` writes, arena setup, and
    /// `BufferReader` decode entirely — the call chain is
    /// `Rust caller → cached fn pointer → JIT body → i64 return`.
    ///
    /// Returns `Err(Unsupported)` when the evaluator was built without
    /// a fast entry (source past the Int-only envelope, or
    /// constructed via `from_ir_direct`).
    pub fn run_main_legacy_i64_fast(&self, args: &[i64]) -> Result<i64, RuntimeError> {
        let ptr = self
            .jit
            .fast_entry_ptr
            .ok_or_else(|| RuntimeError::Unsupported {
                reason:
                    "llvm-aot: fast entry not available; source not Int-only or fast-emit failed"
                        .into(),
            })?;
        let arity = self.fast_path_arity.unwrap_or(0);
        if args.len() != arity {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "llvm-aot fast path: #main expects {arity} arg(s), got {}",
                    args.len()
                ),
            });
        }
        // SAFETY: the cached pointer came back from
        // `ExecutionEngine::get_function_address(ENTRY_SYMBOL_FAST)`
        // which guarantees the symbol is live for the engine's
        // lifetime. The arity-specialised dispatch table mirrors the
        // typed signature `emit_fast_entry` produced for this
        // function shape.
        unsafe {
            let r = match arity {
                0 => {
                    let f: FastEntryFn0 = std::mem::transmute(ptr);
                    f()
                }
                1 => {
                    let f: FastEntryFn1 = std::mem::transmute(ptr);
                    f(args[0])
                }
                2 => {
                    let f: FastEntryFn2 = std::mem::transmute(ptr);
                    f(args[0], args[1])
                }
                3 => {
                    let f: FastEntryFn3 = std::mem::transmute(ptr);
                    f(args[0], args[1], args[2])
                }
                4 => {
                    let f: FastEntryFn4 = std::mem::transmute(ptr);
                    f(args[0], args[1], args[2], args[3])
                }
                5 => {
                    let f: FastEntryFn5 = std::mem::transmute(ptr);
                    f(args[0], args[1], args[2], args[3], args[4])
                }
                6 => {
                    let f: FastEntryFn6 = std::mem::transmute(ptr);
                    f(args[0], args[1], args[2], args[3], args[4], args[5])
                }
                7 => {
                    let f: FastEntryFn7 = std::mem::transmute(ptr);
                    f(
                        args[0], args[1], args[2], args[3], args[4], args[5], args[6],
                    )
                }
                8 => {
                    let f: FastEntryFn8 = std::mem::transmute(ptr);
                    f(
                        args[0], args[1], args[2], args[3], args[4], args[5], args[6], args[7],
                    )
                }
                n => {
                    return Err(RuntimeError::Unsupported {
                        reason: format!("llvm-aot fast path: arity {n} > 8 dispatch cap"),
                    });
                }
            };
            Ok(r)
        }
    }

    /// Try the fast path first: when the schema qualifies and every
    /// supplied arg is `Int`, dispatch through the typed JIT entry
    /// and wrap the i64 result. Returns `Ok(None)` when the fast
    /// path isn't applicable for this call (caller falls back to the
    /// buffer entry). `Ok(Some(v))` on a successful fast dispatch;
    /// `Err` only when the dispatch itself failed.
    fn try_run_main_fast(
        &self,
        args: &HashMap<String, Value>,
    ) -> Result<Option<Value>, RuntimeError> {
        if self.jit.fast_entry_ptr.is_none() {
            return Ok(None);
        }
        let arity = self.fast_path_arity.unwrap_or(0);
        if arity != self.param_names.len() {
            // Schema arity mismatch — shouldn't happen if
            // `build_fast_path_profile` agreed, but be defensive.
            return Ok(None);
        }
        let mut argv = [0i64; 8];
        for (i, name) in self.param_names.iter().enumerate() {
            match args.get(name) {
                Some(Value::Int(v)) => argv[i] = *v,
                _ => return Ok(None), // missing or non-Int arg → fall back
            }
        }
        let r = self.run_main_legacy_i64_fast(&argv[..arity])?;
        // Phase D.2: re-wrap the i64 result to match the buffer
        // path's `Value` shape. The fast-path profile gate accepts
        // both the canonical `Ret { value: Int }` wrapper (Phase
        // D.1 — surfaces as bare `Value::Int`) and any user-declared
        // anon-record return collapsed to a single Int field (Phase
        // D.2 — surfaces as `Value::Dict { <field_name>: Int }` to
        // match `run_main_buffer`'s `read_record_into_map` decode).
        // `is_single_value_wrapper` discriminates the two — strict
        // canonical name match → bare scalar; otherwise → branded
        // dict.
        if let Some(schema) = self.buffer_schema.as_ref() {
            if is_single_value_wrapper(&schema.return_schema) {
                Ok(Some(Value::Int(r)))
            } else {
                let field_name = schema.return_schema.fields[0].name.clone();
                let mut map: HashMap<String, Value> = HashMap::with_capacity(1);
                map.insert(field_name, Value::Int(r));
                Ok(Some(Value::branded_dict(
                    map,
                    Some(schema.return_schema.name.clone()),
                )))
            }
        } else {
            Ok(Some(Value::Int(r)))
        }
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

        // 2. Lay out the arena. Phase E.1 widens the layout to match
        // the cranelift backend: `[const_data | pad | in_buf | pad |
        // out_buf (root + tail cap) | pad | scratch]`. The const-data
        // pool lives at offset 0; ConstString-emitted offsets point
        // directly at the records inside it. The scratch region at
        // the tail backs the bump allocator (`AllocScratchDyn`).
        let in_len = in_bytes.len() as u32;
        let out_root_size = schema.return_layout.root_size as u32;
        // For String / List return types we reserve a chunky tail-
        // cursor region so pointer-indirect StoreField can stamp the
        // payload past the fixed-area slot without re-allocating on
        // every dispatch.
        let needs_pointer_indirect_return = return_needs_tail_region(&schema.return_schema);
        // Cap the output region:
        //   * fixed area: max(root_size, 8) padded to 8.
        //   * tail area: 64 KiB cushion for String returns (W3 hits
        //     ~3 KiB per dispatch at STRING_CONCAT_N = 3 000; a 64 KiB
        //     reservation keeps the bump path away from arena edges
        //     without ballooning the allocation).
        let tail_cap: u32 = if needs_pointer_indirect_return {
            65_536
        } else {
            0
        };
        let out_cap = align_up(out_root_size.max(8) + tail_cap + 16, 8);
        let const_data_len = u32::try_from(self.const_data.len()).map_err(|_| {
            RuntimeError::IoError("llvm const-data section exceeds u32 range".into())
        })?;
        let in_ptr = align_up(const_data_len, 8);
        let out_ptr = align_up(in_ptr + in_len, 8);
        let scratch_base = align_up(out_ptr + out_cap, 8);
        // Scratch region size: 64 KiB matches the cranelift backend's
        // figure; the W3 hot-loop concat writes ~3*N bytes total but
        // the scratch cursor never resets within a dispatch (each
        // iteration's intermediate string sticks around until
        // run-end) so we need enough headroom for the worst-case
        // W3-style `O(N^2)` allocation pattern.
        let scratch_size: u32 = 1_048_576; // 1 MiB
        let arena_size = (scratch_base + scratch_size) as usize;

        // 3. Acquire the per-thread arena buffer, install the
        // input bytes, dispatch. Reentrant calls (a stdlib helper
        // looping back through the evaluator on the same thread)
        // fall back to a fresh `Vec<u8>` — correctness wins over
        // pool reuse on the vanishingly rare path.
        LLVM_ARENA_POOL.with(|cell| match cell.try_borrow_mut() {
            Ok(mut buf) => self.dispatch_with_arena(
                schema,
                &mut buf,
                arena_size,
                in_ptr,
                in_len,
                out_ptr,
                out_cap,
                scratch_base,
                &in_bytes,
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
                    scratch_base,
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
        scratch_base: u32,
        in_bytes: &[u8],
    ) -> Result<Value, RuntimeError> {
        if arena.len() < arena_size {
            arena.resize(arena_size, 0);
        }
        // Zero only the region the JIT can observe before writing —
        // const_data is overwritten in full, in_bytes are copied on
        // top of the input area, the out region must read as zero
        // because pointer-indirect StoreField bumps into a
        // freshly-zero tail cursor, and the scratch tail is written
        // before being read by the JIT itself.
        let observable_end = (out_ptr + out_cap) as usize;
        debug_assert!(observable_end <= arena_size);
        debug_assert!(self.const_data.len() <= in_ptr as usize);
        arena[self.const_data.len()..observable_end].fill(0);
        if !self.const_data.is_empty() {
            arena[..self.const_data.len()].copy_from_slice(&self.const_data);
        }
        arena[in_ptr as usize..in_ptr as usize + in_bytes.len()].copy_from_slice(in_bytes);

        let live_arena = &mut arena[..arena_size];
        let state = ArenaState::new(live_arena, scratch_base);
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
        // Phase D.1 dispatch-boundary fast path: try the typed entry
        // first. Falls through to the buffer-protocol path on
        // mismatch (non-Int args, schema past the Int-only envelope,
        // no fast entry emitted) — transparent to the host.
        if let Some(v) = self.try_run_main_fast(&args)? {
            return Ok(v);
        }
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

/// Phase D.2: looser sibling of [`is_single_value_wrapper`] used to
/// gate the typed-i64 fast-path. Accepts any single-field record whose
/// sole field is `Int` — the canonical `Ret { value: Int }` wrapper
/// **and** any user-declared `#main(...) -> Dict` whose anon-record
/// lowering collapsed to one `Int` field (W7's `{ result: Int }` is
/// the motivating case).
///
/// The strict [`is_single_value_wrapper`] check stays in place for the
/// `run_main` buffer decoder — branded user dicts must still surface
/// as `Value::Dict` for the host, not be unwrapped to a bare scalar.
fn is_single_int_field_record(schema: &relon_eval_api::schema_canonical::Schema) -> bool {
    use relon_eval_api::schema_canonical::TypeRepr;
    schema.fields.len() == 1 && matches!(schema.fields[0].ty, TypeRepr::Int)
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
        // Phase E.1: String return-value decode. The pointer-indirect
        // StoreField path wrote the `[len:u32][utf8]` record into the
        // tail region of the output buffer and stamped its buffer-
        // relative offset into the fixed-area slot. `BufferReader`
        // walks the same protocol to materialise the borrowed `&str`,
        // which we then copy into an owned `Value::String`.
        TypeRepr::String => reader
            .read_string(&field.name)
            .map(|s| Value::String(s.into()))
            .map_err(buffer_to_runtime_error),
        other => Err(RuntimeError::Unsupported {
            reason: format!(
                "llvm-aot: return field `{}` type {other:?} not supported in Phase B",
                field.name
            ),
        }),
    }
}

/// Phase E.1: does the return schema include any pointer-indirect
/// type (`String` / `List*`)? Drives the output buffer's tail-cap
/// sizing — fixed-area-only returns don't need the 64 KiB cushion.
fn return_needs_tail_region(schema: &relon_eval_api::schema_canonical::Schema) -> bool {
    use relon_eval_api::schema_canonical::TypeRepr;
    schema.fields.iter().any(|f| {
        matches!(
            f.ty,
            TypeRepr::String | TypeRepr::List { .. } | TypeRepr::Schema { .. }
        )
    })
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

/// Phase D.1 / D.2: discover whether `schema` qualifies for the typed
/// fast-path entry. Eligibility requires every declared `#main` arg
/// to be `Int` (Inline scalar at 8 / 8) and the return record to
/// carry a single `Int` field — either the canonical
/// `Ret { value: Int }` wrapper (Phase D.1) or any user-declared
/// `#main(...) -> Dict` whose anon-record lowering collapsed to one
/// `Int` field (Phase D.2 — W7's `{ result: Int }` is the motivating
/// shape). Returns the `FastPathProfile` mapping param-declaration
/// order to buffer offsets when eligible.
fn build_fast_path_profile(schema: &BufferSchema) -> Result<FastPathProfile, ()> {
    use relon_eval_api::schema_canonical::TypeRepr;
    // Every declared #main arg must be `Int`. Pointer-indirect /
    // floating-point / bool / null are out — those would require
    // f64 / i32 fast-entry slots we don't enumerate.
    for f in &schema.main_schema.fields {
        if !matches!(f.ty, TypeRepr::Int) {
            return Err(());
        }
    }
    // Single-Int-field record return only. Any other shape
    // (multi-field record, branded sub-schema with non-Int leaves,
    // tail-cursor String/List) escapes the typed-i64 envelope.
    if !is_single_int_field_record(&schema.return_schema) {
        return Err(());
    }
    // Collect each arg's buffer offset from the layout — declaration
    // order is what the JIT entry is parameterised by.
    let mut arg_offsets: Vec<u32> = Vec::with_capacity(schema.main_layout.fields.len());
    for (i, f) in schema.main_schema.fields.iter().enumerate() {
        // Layout's `fields` mirrors `main_schema.fields` order; cross-
        // check the names so a future schema reorder surfaces.
        let lo = schema.main_layout.fields.get(i).ok_or(())?;
        if lo.name != f.name {
            return Err(());
        }
        arg_offsets.push(lo.offset as u32);
    }
    // Arity cap — matches `emit_fast_entry`'s `arity > 8` guard.
    if arg_offsets.len() > 8 {
        return Err(());
    }
    let ret_offset = schema
        .return_layout
        .fields
        .first()
        .map(|f| f.offset as u32)
        .ok_or(())?;
    Ok(FastPathProfile {
        arg_offsets,
        ret_offset,
    })
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
/// Which ABI shape the emitted entry symbol exposes. Drives the
/// build.rs binding-generator's choice between a typed `(i64...) -> i64`
/// extern declaration (fast path) and a buffer-protocol call through
/// `relon-rs-shims::call_buffer_entry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmittedEntryShape {
    /// `extern "C" fn(i64, ...) -> i64`. Source qualified for the
    /// dispatch-boundary fast path (Int-only `#main(Int...) -> Int`,
    /// arity <= 8, no string/list/closure). The binding wraps the
    /// extern with a thin Rust shim.
    FastInt,
    /// Full buffer-protocol entry:
    /// `extern "C" fn(*const ArenaState, i32, i32, i32, i32, i64) -> i32`.
    /// Source has string/list arguments or returns, calls into
    /// stdlib helpers, or uses helper functions. The binding marshals
    /// typed Rust args into / out of an arena buffer through
    /// `relon-rs-shims::call_buffer_entry`.
    Buffer,
}

/// One declared `#main` parameter (or `value` field on the return
/// schema), in declaration order. Tells the build.rs binding generator
/// what Rust type to expose for each slot and at what byte offset the
/// buffer-protocol arena writer / reader should access it.
#[derive(Debug, Clone)]
pub struct EmittedField {
    /// Field name as declared in source.
    pub name: String,
    /// Pre-computed byte offset of the slot inside its enclosing
    /// fixed area (main_params record for args, return record for
    /// the return slot).
    pub offset: u32,
    /// Erased canonical type tag. Build.rs maps each to the matching
    /// Rust type for the binding signature.
    pub ty: EmittedFieldType,
}

/// Erased canonical type tag the build.rs binding generator uses to
/// pick the Rust type for each `#main` parameter / return slot.
///
/// Phase 2 covers `Int` / `Bool` / `String` / `Null`. Float, Lists,
/// nested schemas, and closure-valued returns surface as
/// `UnsupportedSignature` at emit-object time so the binding never
/// sees a type tag it can't handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmittedFieldType {
    /// `i64`. Inline slot at offset, 8/8.
    Int,
    /// `bool`. Inline slot at offset, 1/1.
    Bool,
    /// `()`. Inline slot at offset, 1/1 (always reads as zero).
    Null,
    /// `&str` / `String`. Pointer-indirect: fixed slot is a 4-byte
    /// buffer-relative offset to a `[len: u32 LE][utf8 bytes]` tail
    /// record. Build.rs uses `BufferBuilder::write_string` to pack
    /// inputs and `BufferReader::read_string` to decode outputs.
    String,
}

/// Metadata returned by [`LlvmAotEvaluator::emit_object`] so the
/// build.rs caller can stamp matching `extern "C"` declarations and
/// marshalling code into the generated Rust shim.
///
/// The shape carried by [`Self::shape`] decides the binding shape:
/// fast-path entries get a thin `extern "C" fn(i64, ...) -> i64`
/// wrapper; buffer-protocol entries route through
/// `relon-rs-shims::call_buffer_entry` with typed Rust args.
#[derive(Debug, Clone)]
pub struct EmitObjectInfo {
    /// Exported C ABI symbol name (chosen by the caller; the emitter
    /// renames the JIT-side default to this).
    pub entry_symbol: String,
    /// Number of declared `#main` parameters. For fast-path entries
    /// this equals the C ABI arity; for buffer-protocol entries the C
    /// ABI arity is always 6, while this field reports the
    /// user-visible `#main` arity.
    pub entry_arity: usize,
    /// Declared parameter names in `#main(...)` declaration order.
    /// Build.rs uses these to name the Rust shim's args.
    pub param_names: Vec<String>,
    /// Which extern signature the emitted symbol carries. Drives the
    /// binding generator's dispatch shape.
    pub shape: EmittedEntryShape,
    /// Declared `#main` parameters with byte-offsets and type tags.
    /// Used by the buffer-protocol binding to pack input args into
    /// the arena. Empty under [`EmittedEntryShape::FastInt`] (the
    /// fast path reads args from positional registers, not the
    /// buffer).
    pub main_fields: Vec<EmittedField>,
    /// Return record fields. Phase 2 lowering always wraps the
    /// `#main` return in a single-field schema `Ret { value: T }`,
    /// so this vector has exactly one entry. Empty under
    /// [`EmittedEntryShape::FastInt`].
    pub return_fields: Vec<EmittedField>,
    /// Fixed-area byte size of the input record. The buffer-protocol
    /// binding allocates `in_len = main_root_size + tail_len_for_strings`
    /// bytes. Zero under [`EmittedEntryShape::FastInt`].
    pub main_root_size: u32,
    /// Fixed-area byte size of the return record. The buffer-protocol
    /// binding reserves at least this much in the output region.
    /// Zero under [`EmittedEntryShape::FastInt`].
    pub return_root_size: u32,
    /// Whether the return schema contains pointer-indirect leaves
    /// (`String` / `List*`) — drives the binding's tail-cap sizing.
    pub return_has_tail: bool,
    /// Const-pool blob the JIT body references through arena-relative
    /// i32 offsets (`Op::ConstString` records). The binding copies
    /// this verbatim to `arena[..const_data.len()]` before every
    /// dispatch. Empty under [`EmittedEntryShape::FastInt`] (the fast
    /// path doesn't touch the const pool).
    pub const_data: Vec<u8>,
    /// `true` when the emitted body references the
    /// `relon_llvm_str_contains_arena` host shim. Build.rs uses this
    /// to decide whether to add the `relon-rs-shims` staticlib to
    /// the linker invocation.
    pub references_str_contains_shim: bool,
}

impl LlvmAotEvaluator {
    /// AOT entry: compile `src` into a relocatable ELF object file
    /// suitable for linker consumption (build.rs path).
    ///
    /// Phase 2 envelope:
    ///
    /// - When the source qualifies for the dispatch-boundary fast
    ///   path (Int-only `#main(Int...) -> Int`, arity <= 8, no
    ///   pointer-indirect leaves, no stdlib call overhead), the
    ///   emitted symbol carries the typed
    ///   `extern "C" fn(i64, ...) -> i64` shape — the Phase 1 trivial
    ///   path. No `SandboxState`, no const-pool, no shim
    ///   dependency.
    /// - Otherwise the symbol carries the full buffer-protocol entry
    ///   shape `extern "C" fn(*const ArenaState, i32, i32, i32, i32,
    ///   i64) -> i32`. The build.rs binding generator routes typed
    ///   Rust args through `relon-rs-shims::call_buffer_entry` to
    ///   marshal them into / out of the arena.
    ///
    /// In both modes the emitter returns an [`EmitObjectInfo`] that
    /// carries the metadata the binding generator needs (entry shape,
    /// schema field offsets, const-pool blob, shim reference flag).
    ///
    /// Returns [`LlvmError::UnsupportedSignature`] when the declared
    /// `#main` signature mixes types Phase 2 hasn't wired marshalling
    /// for yet (`Float`, `List*`, nested schemas as args, closure
    /// returns) — Phase 3 widens the surface.
    pub fn emit_object(
        src: &str,
        entry_symbol: &str,
        out_path: &Path,
    ) -> Result<EmitObjectInfo, LlvmError> {
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

        // Materialise the per-field metadata up-front so we can hand
        // it back regardless of whether we end up on the fast or
        // buffer-protocol path. Surfaces an `UnsupportedSignature`
        // for type tags Phase 2 hasn't wired marshalling for yet —
        // the build.rs binding side can't generate a Rust wrapper
        // for an unknown leaf type.
        let main_fields = lower_field_descriptors(&schema.main_schema, &schema.main_layout)?;
        let return_fields = lower_field_descriptors(&schema.return_schema, &schema.return_layout)?;

        let entry_idx = ir
            .entry_func_index
            .ok_or_else(|| LlvmError::Codegen("IR module has no entry function".into()))?;
        let entry = &ir.funcs[entry_idx];

        // Verify the IR carries the canonical buffer-protocol entry
        // signature. `lower_workspace_single` always produces this
        // shape today; failing the check means an IR-layer change
        // slipped past the test gates.
        if !crate::emitter::is_buffer_protocol_signature(&entry.params, entry.ret) {
            return Err(LlvmError::UnsupportedSignature(
                "relon-rs build: lowering produced a non-buffer entry shape".into(),
            ));
        }

        // Fast-path eligibility — Int-only schema, arity <= 8, no
        // pointer-indirect leaves. Sources that don't qualify drop to
        // the buffer-protocol path below.
        let fast_profile = build_fast_path_profile(&schema).ok();

        let ctx = Context::create();
        let module = ctx.create_module("relon_rs_object");

        // Phase E.1 const-pool blob; needed by buffer-protocol bodies
        // for `Op::ConstString { idx }` resolution. The fast path
        // doesn't reference the pool (Int-only bodies have no
        // ConstString ops) so the blob ends up empty in that branch.
        let const_pool = ConstPool::from_module(&ir)?;

        let (shape, references_str_contains_shim) = match fast_profile {
            Some(ref profile) => {
                // Fast-path entry only. Same shape the Phase 1 trivial
                // demo path emitted — pure i64 in / i64 out, no
                // SandboxState pointer, no const-pool copy.
                //
                // Phase D.2: the W7 anon-Dict-return shape needs the
                // module-wide helper / closure tables so the fast entry
                // can resolve in-body `Op::Call` / `Op::CallClosure`
                // sites. Empty tables are fine for Phase D.1's pure
                // Int-arithmetic bodies (W1) — the emitter just never
                // looks them up.
                let helper_table: HashMap<u32, FunctionValue<'_>> = HashMap::new();
                let closure_fn_table: Vec<FunctionValue<'_>> = Vec::new();
                let llvm_fn = emit_fast_entry(
                    &ctx,
                    &module,
                    entry,
                    profile,
                    &helper_table,
                    &closure_fn_table,
                )?;
                llvm_fn.as_global_value().set_name(entry_symbol);
                llvm_fn.set_linkage(Linkage::External);
                (EmittedEntryShape::FastInt, false)
            }
            None => {
                // Buffer-protocol entry. Routes through
                // `emit_module_funcs` so user-defined helper functions
                // and bundled-stdlib bodies (Phase 2 P1 surface) lower
                // alongside the entry.
                let buffer_return_size = schema.return_layout.root_size as u32;
                let lambda_ir_idx_set: std::collections::HashSet<u32> =
                    ir.closure_table.iter().copied().collect();
                let helpers: Vec<&relon_ir::ir::Func> = ir
                    .funcs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != entry_idx && !lambda_ir_idx_set.contains(&(*i as u32)))
                    .map(|(_, f)| f)
                    .collect();
                let helper_ir_indices: Vec<u32> = ir
                    .funcs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != entry_idx && !lambda_ir_idx_set.contains(&(*i as u32)))
                    .map(|(i, _)| i as u32)
                    .collect();
                let lambdas: Vec<&relon_ir::ir::Func> = ir
                    .closure_table
                    .iter()
                    .map(|&ir_idx| &ir.funcs[ir_idx as usize])
                    .collect();
                let (llvm_fn, _entry_shape, _helper_table, _closure_fn_table) = emit_module_funcs(
                    &ctx,
                    &module,
                    entry,
                    buffer_return_size,
                    &const_pool,
                    &helpers,
                    Some(&helper_ir_indices),
                    &lambdas,
                    &ir.closure_table,
                )?;
                // Rename the canonical buffer entry to the build.rs-
                // supplied symbol and force external linkage so the
                // consuming binary's linker can resolve it.
                llvm_fn.as_global_value().set_name(entry_symbol);
                llvm_fn.set_linkage(Linkage::External);

                // Detect whether the emitted module references the
                // `relon_llvm_str_contains_arena` host shim — drives
                // build.rs's decision to add the `relon-rs-shims`
                // staticlib to the linker invocation. We check by
                // name lookup against the LLVM module since the emit
                // pass declares the extern lazily on first
                // `Op::Call { contains }` site.
                let needs_shim = module
                    .get_function(RELON_LLVM_STR_CONTAINS_ARENA_SYMBOL)
                    .is_some();
                (EmittedEntryShape::Buffer, needs_shim)
            }
        };

        module.verify().map_err(|e| {
            LlvmError::Codegen(format!("LLVM verifier rejected object module: {e}"))
        })?;

        // Lay down O3 IR-level passes before backend codegen. Same
        // pipeline the JIT path uses, so the AOT and JIT performance
        // profiles stay comparable.
        run_default_o3_pipeline(&module)?;

        // Write a relocatable ELF object. The target machine's
        // `RelocMode::PIC` produces position-independent code so the
        // linker can fold this into either a `staticlib` or a final
        // executable without relocation drama.
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
                RelocMode::PIC,
                CodeModel::Default,
            )
            .ok_or_else(|| LlvmError::Codegen("create_target_machine returned null".into()))?;

        if let Some(parent) = out_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| LlvmError::Codegen(format!("create out dir `{parent:?}`: {e}")))?;
            }
        }
        machine
            .write_to_file(&module, FileType::Object, out_path)
            .map_err(|e| LlvmError::Codegen(format!("write object `{out_path:?}`: {e}")))?;

        // For the fast path the binding's arity matches the LLVM
        // entry signature's i64-slot count. For the buffer path
        // there's no per-Rust-arg correspondence with the LLVM
        // signature (which is always 6 slots), so we report the
        // user-visible `#main` arity instead.
        let entry_arity = main_fields.len();
        let main_root_size = schema.main_layout.root_size as u32;
        let return_root_size = schema.return_layout.root_size as u32;
        let return_has_tail = return_needs_tail_region(&schema.return_schema);
        let const_data = match shape {
            EmittedEntryShape::FastInt => Vec::new(),
            EmittedEntryShape::Buffer => const_pool.bytes,
        };
        let (main_fields_out, return_fields_out, main_root_size_out, return_root_size_out) =
            match shape {
                EmittedEntryShape::FastInt => (Vec::new(), Vec::new(), 0, 0),
                EmittedEntryShape::Buffer => {
                    (main_fields, return_fields, main_root_size, return_root_size)
                }
            };

        Ok(EmitObjectInfo {
            entry_symbol: entry_symbol.to_string(),
            entry_arity,
            param_names,
            shape,
            main_fields: main_fields_out,
            return_fields: return_fields_out,
            main_root_size: main_root_size_out,
            return_root_size: return_root_size_out,
            return_has_tail: matches!(shape, EmittedEntryShape::Buffer) && return_has_tail,
            const_data,
            references_str_contains_shim,
        })
    }
}

/// Walk a `(Schema, OffsetTable)` pair and project the per-field
/// declaration into the build.rs-visible [`EmittedField`] shape. The
/// type tag is erased into [`EmittedFieldType`] for the Phase 2
/// supported leaf set; any unsupported leaf surfaces as
/// [`LlvmError::UnsupportedSignature`] so build.rs never generates a
/// binding it can't compile.
fn lower_field_descriptors(
    schema: &relon_eval_api::schema_canonical::Schema,
    layout: &relon_eval_api::layout::OffsetTable,
) -> Result<Vec<EmittedField>, LlvmError> {
    use relon_eval_api::schema_canonical::TypeRepr;
    let mut out = Vec::with_capacity(schema.fields.len());
    for (i, f) in schema.fields.iter().enumerate() {
        let lo = layout.fields.get(i).ok_or_else(|| {
            LlvmError::Codegen(format!(
                "lower_field_descriptors: layout missing slot for field `{}`",
                f.name
            ))
        })?;
        if lo.name != f.name {
            return Err(LlvmError::Codegen(format!(
                "lower_field_descriptors: schema/layout name mismatch at slot {i}: schema=`{}`, layout=`{}`",
                f.name, lo.name
            )));
        }
        let ty = match &f.ty {
            TypeRepr::Int => EmittedFieldType::Int,
            TypeRepr::Bool => EmittedFieldType::Bool,
            TypeRepr::Null => EmittedFieldType::Null,
            TypeRepr::String => EmittedFieldType::String,
            other => {
                return Err(LlvmError::UnsupportedSignature(format!(
                    "relon-rs build (Phase 2): field `{}` type {other:?} not yet wired for marshalling",
                    f.name
                )));
            }
        };
        out.push(EmittedField {
            name: f.name.clone(),
            offset: lo.offset as u32,
            ty,
        });
    }
    Ok(out)
}

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
