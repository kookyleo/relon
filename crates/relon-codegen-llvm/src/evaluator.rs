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

use relon_eval_api::inplace_return::ArenaRegions;
use relon_eval_api::{ClosureData, Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_parser::Node;

use crate::codegen::{
    emit_fast_entry, emit_module_funcs, emit_module_funcs_closed_world,
    emit_module_funcs_closed_world_wasm, emit_module_funcs_wasm, is_buffer_protocol_signature,
    ConstPool, EntryShape, FastPathProfile, WorldMode, ENTRY_SYMBOL, ENTRY_SYMBOL_FAST,
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

/// Codegen target for the object-emit path (S3.X).
///
/// The SAME relon-IR → LLVM-IR emitter feeds both variants — only the
/// `TargetMachine` construction (triple + DataLayout + CPU/features +
/// reloc/code model) differs. `mem.rs` already lays out the arena via
/// i32-offset GEPs (zext-i64 + `i8*` base), so the lowered body is
/// pointer-width agnostic and needs no per-target change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodegenTarget {
    /// Host x86-64 ELF object (the historical default). Triple +
    /// CPU/features come from `TargetMachine::get_default_triple` /
    /// `get_host_cpu_*`, reloc = PIC.
    Native,
    /// `wasm32-wasi` object (`\0asm` magic). Uses the WebAssembly LLVM
    /// backend with the canonical wasm32 DataLayout. Emitted object is
    /// consumed by `wasmtime` (see `crate::wasm_run`).
    Wasm32,
}

/// Reference: the wasm32 DataLayout string LLVM emits for
/// `wasm32-wasi` (little-endian, 32-bit pointers, i64 8-byte aligned).
/// The module DataLayout is set authoritatively from the
/// `TargetMachine`'s target data at emit time; this const documents the
/// expected shape — note the `p:32:32` that lowers the i32-offset arena
/// GEPs to 32-bit linear-memory pointers.
#[allow(dead_code)]
const WASM32_DATA_LAYOUT: &str = "e-m:e-p:32:32-p10:8:8-p20:8:8-i64:64-n32:64-S128-ni:1:10:20";
/// The wasm32 triple. `wasm32-wasi` so the module can later route
/// effectful host fns through WASI imports (P3 §2.2). For pure-compute
/// workloads `wasm32-unknown-unknown` would also work; wasi is the
/// superset.
const WASM32_TRIPLE: &str = "wasm32-wasi";

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
// by default — LLVM's `LLVMContextRef` is per-thread. We mark the
// pair Send/Sync because `run_main` only reaches back into the JIT
// through the cached function pointers (`entry_ptr`, `fast_entry_ptr`),
// which are immutable after construction; the only per-call mutable
// state is the thread-local `LLVM_ARENA_POOL`, which needs no lock.
unsafe impl Send for JitOwned {}
unsafe impl Sync for JitOwned {}

/// Buffer schema metadata captured by `from_source`. Mirrors
/// `relon_codegen_cranelift::evaluator::BufferSchema` — kept inside this
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
    /// Phase 0b: the module's `#native` imports in `import_idx` order.
    /// Carried so [`Self::with_host_fns`] can match a host-supplied
    /// `Arc<dyn RelonFunction>` (keyed by source-level name) to the
    /// `import_idx` the lowering pass assigned.
    native_imports: Vec<relon_ir::ir::NativeImport>,
    /// Phase 0b: host-fn registry installed on every per-call
    /// `ArenaState` so a source-lowered `Op::CallNative` dispatches
    /// through `relon_llvm_call_native`. Behind an `Arc` so the
    /// registry outlives every dispatch without per-call clones; rebuilt
    /// by [`Self::with_host_fns`]. Empty by default — an unregistered
    /// gated call then traps after passing the `CheckCap` gate.
    host_fns: Arc<crate::state::HostFnRegistry>,
    /// Phase 0b: capability bitmask passed as the buffer entry's
    /// trailing `i64 caps` param. The source-lowered `Op::CheckCap`
    /// gate tests bit `cap_bit` of this word; `0` denies every gated
    /// call. Set via [`Self::with_granted_cap`] / [`Self::with_caps`].
    caps_mask: i64,
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
        Self::from_source_with_options_inner(src, None)
    }

    /// Like [`Self::from_source`] but with caller-supplied analyzer
    /// options — the entry point for host-registered `#native` fns.
    /// The host populates `options.host_fn_names` /
    /// `host_fn_signatures` / `host_fn_gates` / `caps` so the analyzer
    /// resolves the calls, runs the single-file capability-reachability
    /// check (a gated call without the statically-granted cap fails the
    /// build here), and the lowering pass emits the `Op::CheckCap`-
    /// guarded `Op::CallNative`.
    ///
    /// The returned evaluator carries an empty host-fn registry and a
    /// zero capability mask; chain [`Self::with_host_fns`] +
    /// [`Self::with_granted_cap`] to wire the runtime dispatch + grant.
    /// Mirrors the cranelift backend's `from_source_with_options`.
    pub fn from_source_with_options(
        src: &str,
        options: &relon_analyzer::AnalyzeOptions,
    ) -> Result<Self, LlvmError> {
        Self::from_source_with_options_inner(src, Some(options))
    }

    fn from_source_with_options_inner(
        src: &str,
        options: Option<&relon_analyzer::AnalyzeOptions>,
    ) -> Result<Self, LlvmError> {
        let (ir, main_schema, return_schema) = Self::lower_source_with_options(src, options)?;
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

    fn lower_source_with_options(
        src: &str,
        options: Option<&relon_analyzer::AnalyzeOptions>,
    ) -> Result<
        (
            relon_ir::ir::Module,
            relon_eval_api::schema_canonical::Schema,
            relon_eval_api::schema_canonical::Schema,
        ),
        LlvmError,
    > {
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
        // build below. Unlike the bytecode / cranelift tiers, the LLVM
        // backend does NOT force `standalone_capability_check`.
        //
        // Phase 0b: a caller-supplied `options` (host `#native` fns)
        // takes precedence — the host already sets `strict_mode: false`
        // on it (see the cranelift `host_options` fixture). We force
        // `strict_mode: false` regardless so the closure surface stays
        // unblocked even if a host left it default-true.
        let owned;
        let options: &relon_analyzer::AnalyzeOptions = match options {
            Some(o) => {
                if o.strict_mode {
                    owned = relon_analyzer::AnalyzeOptions {
                        strict_mode: false,
                        ..o.clone()
                    };
                    &owned
                } else {
                    o
                }
            }
            None => {
                owned = relon_analyzer::AnalyzeOptions {
                    strict_mode: false,
                    ..Default::default()
                };
                &owned
            }
        };
        // Map the shared frontend pipeline error onto this backend's
        // surface: Parse → Parse, Analyze(n) → Analyze(n), and Lowering
        // → Codegen with the historical `lower_workspace_single:` prefix
        // (the LLVM backend has no dedicated `Lowering` variant).
        let lowered = relon_ir::frontend::compile(src, options).map_err(|e| match e {
            relon_ir::FrontendError::Parse(msg) => LlvmError::Parse(msg),
            relon_ir::FrontendError::Analyze(n) => LlvmError::Analyze(n),
            relon_ir::FrontendError::Lowering(msg) => {
                LlvmError::Codegen(format!("lower_workspace_single: {msg}"))
            }
        })?;
        Ok((lowered.module, lowered.main_schema, lowered.return_schema))
    }

    /// Stage 2.⑤ closed-world source constructor. Builds the
    /// buffer-protocol JIT evaluator with `Op::CallNative` lowered to a
    /// direct `call @<host_symbol>`, links + inlines the host shim
    /// bitcode, and reuses the open-world arena-handshake dispatch
    /// (`run_main`) verbatim — the entry symbol / signature are
    /// identical, only the native-dispatch lowering differs. No host-fn
    /// registry / cap mask is needed at runtime: the host body is folded
    /// into the entry by the LTO inline, so there is no dynamic
    /// `relon_llvm_call_native` hop to resolve.
    ///
    /// The differential oracle for this path is the open-world
    /// `from_source_with_options` + `run_main` result (anchored, in
    /// turn, to cranelift's `native_call_from_source`).
    pub fn from_source_closed_world(
        src: &str,
        options: &relon_analyzer::AnalyzeOptions,
        host_shim_src: &str,
    ) -> Result<Self, LlvmError> {
        let (ir, main_schema, return_schema) = Self::lower_source_with_options(src, Some(options))?;
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
        Self::from_ir_inner_world(
            ir,
            param_names,
            Some(schema),
            WorldMode::ClosedWorld,
            Some(host_shim_src),
        )
    }

    fn from_ir_inner(
        ir: relon_ir::ir::Module,
        param_names: Vec<String>,
        buffer_schema: Option<BufferSchema>,
    ) -> Result<Self, LlvmError> {
        Self::from_ir_inner_world(ir, param_names, buffer_schema, WorldMode::OpenWorld, None)
    }

    fn from_ir_inner_world(
        ir: relon_ir::ir::Module,
        param_names: Vec<String>,
        buffer_schema: Option<BufferSchema>,
        world_mode: WorldMode,
        host_shim_src: Option<&str>,
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
        let emit = match world_mode {
            WorldMode::OpenWorld => emit_module_funcs,
            WorldMode::ClosedWorld => emit_module_funcs_closed_world,
        };
        let (_llvm_fn, entry_shape, helper_table, closure_fn_table) = emit(
            ctx_static,
            &module,
            entry,
            buffer_return_size,
            &const_pool,
            &helpers,
            Some(&helper_ir_indices),
            &lambdas,
            &ir.closure_table,
            &ir.imports,
        )?;

        // Stage 2.⑤ closed-world: link + inline the host shim bitcode
        // into the JIT module so the direct `call @<host_symbol>` sites
        // fold into the host body during the O3 pass below. Done before
        // the fast-entry emit so a fast entry (Int-only, no native call)
        // is unaffected; closed-world sources always take the buffer
        // entry because they carry an `Op::CallNative`.
        if matches!(world_mode, WorldMode::ClosedWorld) {
            let shim = host_shim_src.ok_or_else(|| {
                LlvmError::Codegen(
                    "from_ir_inner_world: ClosedWorld requires a host_shim_src".into(),
                )
            })?;
            crate::cocompile::link_and_inline_host_shim(&module, shim, &ir.imports)?;
        }

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

        // Pin every function to the RUNTIME host CPU before MCJIT
        // codegen. The MCJIT engine builders take no MCPU, so without
        // this the X86 backend lowers for generic x86-64 and drops the
        // host `SlowDivide64` narrowing — every i64 `%` / `/` becomes a
        // bare microcoded `idivq` instead of the host `shrq $32; je;
        // divl` fast path. The O3 pipeline and the static object-emit
        // path already target the host; this brings the JIT backend in
        // line. Stamping `target-cpu` / `target-features` (host-queried,
        // never hard-coded) is the lever inkwell 0.9 / MCJIT exposes.
        // Results are byte-identical to the generic lowering — this is a
        // codegen-quality / instruction-selection fix, not a semantics
        // change.
        stamp_host_target_attributes(&module);

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
        // Phase 0b: the native-dispatch helper is also a host-resident
        // extern (it lives in this crate's `.text`, not the JIT arena),
        // so a module that references it must stay on the default JIT
        // builder (Large CodeModel) for the same ±2 GB-relocation reason
        // the `str.contains` shim does.
        let uses_extern_shim = module
            .get_function(crate::str_helpers::RELON_LLVM_STR_CONTAINS_ARENA_SYMBOL)
            .is_some()
            || module
                .get_function(crate::state::RELON_LLVM_CALL_NATIVE_SYMBOL)
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

        // Phase 0b: map the native-dispatch helper symbol to its host
        // address so an emitted `call @relon_llvm_call_native` resolves.
        // The default MCJIT resolver (`dlsym`) cannot see the static
        // from inside this dylib's section layout — same constraint as
        // the `str.contains` shim. No-op when the module never emitted
        // a `CallNative` (the symbol is absent).
        if let Some(cn_fn) = module.get_function(crate::state::RELON_LLVM_CALL_NATIVE_SYMBOL) {
            engine.add_global_mapping(&cn_fn, crate::state::relon_llvm_call_native_addr());
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
            native_imports: ir.imports.clone(),
            host_fns: Arc::new(crate::state::HostFnRegistry::new()),
            caps_mask: 0,
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

    /// Phase 0b: the `#native` imports the lowering pass interned for
    /// this module, in `import_idx` order. Lets a host map fn names to
    /// the slots [`Self::with_host_fns`] fills. Mirrors the cranelift
    /// backend's `native_imports`.
    pub fn native_imports(&self) -> &[relon_ir::ir::NativeImport] {
        &self.native_imports
    }

    /// Phase 0b: register the host's `Arc<dyn RelonFunction>` callables
    /// for source-lowered native-fn dispatch. Each entry is keyed by the
    /// source-level fn name; this matches the name to the `import_idx`
    /// the lowering pass assigned (via [`Self::native_imports`]) and
    /// installs the callable in the evaluator's `import_idx`-keyed
    /// registry. A source-lowered `Op::CallNative` then dispatches to it
    /// through the `relon_llvm_call_native` helper. Names with no
    /// matching `#native` import are skipped. Mirrors the cranelift
    /// backend's `with_host_fns`.
    ///
    /// The capability *guard* is enforced independently by the
    /// `Op::CheckCap` prologue against the granted `caps` mask
    /// ([`Self::with_granted_cap`]) — registering a callable does not
    /// grant its capability.
    pub fn with_host_fns(
        mut self,
        host_fns: &std::collections::HashMap<String, Arc<dyn relon_eval_api::RelonFunction>>,
    ) -> Self {
        let mut registry = crate::state::HostFnRegistry::new();
        for (idx, imp) in self.native_imports.iter().enumerate() {
            if let Some(func) = host_fns.get(&imp.name) {
                registry.register(idx as u32, Arc::clone(func));
            }
        }
        self.host_fns = Arc::new(registry);
        self
    }

    /// Phase 0b: grant a capability bit so the source-lowered
    /// `Op::CheckCap` prologue passes at runtime. Sets bit `bit` in the
    /// `caps` bitmask the buffer entry receives as its trailing `i64`
    /// param. Decoupled from the analyze-time `caps`: a host can grant
    /// statically (build passes the reachability check) yet withhold
    /// here to exercise a stricter runtime posture (the gated call then
    /// traps `CapabilityDenied`). Mirrors the cranelift backend's
    /// `with_granted_cap` outcome class.
    pub fn with_granted_cap(mut self, bit: u32) -> Self {
        if bit < 64 {
            self.caps_mask |= 1i64 << bit;
        }
        self
    }

    /// Phase 0b: set the full `caps` bitmask wholesale (the trailing
    /// `i64` param the buffer entry's `Op::CheckCap` gate tests).
    /// Companion to [`Self::with_granted_cap`] for hosts that already
    /// hold a packed mask.
    pub fn with_caps(mut self, caps_mask: i64) -> Self {
        self.caps_mask = caps_mask;
        self
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

    /// The running host's LLVM CPU name (e.g. `broadwell`, `znver3`),
    /// as queried by `TargetMachine::get_host_cpu_name`. This is the
    /// exact value stamped as the `"target-cpu"` function attribute on
    /// every JIT'd function so the MCJIT backend lowers for the CPU it
    /// runs on (and emits the host idiv-narrowing fast path rather than
    /// a generic bare `idivq`). Exposed so capability tests can confirm
    /// the stamp is the runtime host, never a hard-coded literal.
    pub fn host_target_cpu() -> String {
        TargetMachine::get_host_cpu_name()
            .to_str()
            .unwrap_or("")
            .to_string()
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
        // Construction invariant (see the `(Some, None)` arm at the
        // `fast_entry_ptr`/`fast_path_arity` resolution site): the two
        // fields are always populated together, so a live
        // `fast_entry_ptr` guarantees a live `fast_path_arity`.
        let arity = self
            .fast_path_arity
            .expect("fast_entry_ptr is Some, so fast_path_arity must be Some by construction");
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
        // Construction invariant: `fast_entry_ptr` and `fast_path_arity`
        // are always populated together, so reaching here (entry ptr is
        // Some) guarantees the arity is Some as well.
        let arity = self
            .fast_path_arity
            .expect("fast_entry_ptr is Some, so fast_path_arity must be Some by construction");
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
        // F1: bake `in_ptr` into every input pointer slot (arena-absolute
        // convention), so the JIT body's param reads drop their `+ in_ptr`
        // rebase. `in_ptr` depends only on const-data length.
        let in_ptr_pre = relon_util::align_up(
            u32::try_from(self.const_data.len()).map_err(|_| {
                RuntimeError::IoError("llvm const-data section exceeds u32 range".into())
            })?,
            8,
        );
        let in_bytes = builder
            .finish_arena_absolute(in_ptr_pre)
            .map_err(buffer_to_runtime_error)?;

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
        let out_cap = relon_util::align_up(out_root_size.max(8) + tail_cap + 16, 8);
        let const_data_len = u32::try_from(self.const_data.len()).map_err(|_| {
            RuntimeError::IoError("llvm const-data section exceeds u32 range".into())
        })?;
        let in_ptr = relon_util::align_up(const_data_len, 8);
        let out_ptr = relon_util::align_up(in_ptr + in_len, 8);
        let scratch_base = relon_util::align_up(out_ptr + out_cap, 8);
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
        // Phase 0b: point the per-call state at the host-fn registry so
        // a source-lowered `Op::CallNative` resolves through
        // `relon_llvm_call_native`. The registry lives on the evaluator
        // behind an `Arc` and outlives this dispatch.
        // SAFETY: `self.host_fns` is kept alive for the whole call (and
        // the evaluator's lifetime); the per-call state is the sole
        // owner of the `UnsafeCell` for the dispatch's duration.
        unsafe {
            state.install_host_fns(Arc::as_ptr(&self.host_fns));
        }
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
                    /*caps=*/ self.caps_mask,
                )
            }))
            .map_err(|_| RuntimeError::Unsupported {
                reason: "llvm-aot: JIT entry panicked (no trap-code recovery in Phase B)".into(),
            })?
        };

        // Phase 0b: a `CheckCap` deny or a failed `CallNative` dispatch
        // returns the negative sentinel and records the precise cause in
        // `state.trap_code`. Lift it to a typed `RuntimeError` (the same
        // outcome class the cranelift backend surfaces) before the
        // generic negative-bytes_written path.
        let trap_code = state.trap_code();
        if trap_code != 0 {
            return Err(crate::state::NativeTrap::runtime_error_from_code(trap_code));
        }
        // Decode the buffer return out of the arena. The decode is
        // backend-shared and arena-source-agnostic (host JIT arena here;
        // wasm linear memory in the wasm-evaluator path) — see
        // [`Self::decode_buffer_return`].
        self.decode_buffer_return(
            schema,
            arena,
            ArenaRegions {
                const_data_len: self.const_data.len(),
                in_ptr,
                in_len,
                out_ptr,
                out_cap,
                scratch_base,
                arena_size,
            },
            bytes_written,
        )
    }

    /// Decode a buffer-protocol return out of an arena, given the raw
    /// i32 the entry returned (`bytes_written` / sentinel) and the arena
    /// region boundaries.
    ///
    /// This is the **single** post-call decode the native JIT path and
    /// the wasm-evaluator path share. It is deliberately source-agnostic:
    /// `arena` is just `&[u8]` (the host JIT arena, or a slice of wasm
    /// linear memory rebased to the arena origin), and every region
    /// offset in `regions` is arena-relative, so the wasm host can hand
    /// the same view and offsets the JIT path computes.
    ///
    /// Two paths, identical to the historical inline decode:
    /// - **negative** `ret`: the in-place region-walk sentinel
    ///   `-(root_abs + 1)`. We recover `root_abs`, then defer entirely to
    ///   the backend-shared `relon_eval_api::inplace_return` pipeline
    ///   (region-select → **verifier** → in-place decode). The verifier
    ///   is non-negotiable: an unverified buffer is never decoded, on the
    ///   wasm linear-memory path exactly as on the host path.
    /// - **non-negative** `ret`: the fixed-area / tail-cursor return; the
    ///   `BufferReader` walks `out_buf`.
    fn decode_buffer_return(
        &self,
        schema: &BufferSchema,
        arena: &[u8],
        regions: ArenaRegions,
        ret: i32,
    ) -> Result<Value, RuntimeError> {
        // In-place region-walk return ABI (S2): a negative return value
        // is the in-place sentinel `-(root_abs + 1)`. Instead of a value
        // copied into `out_buf`, the machine code reports the
        // arena-relative offset of the return root — a `List<List<scalar>>`,
        // `List<String>`, or `List<Schema>` value sourced from a `#main`
        // parameter identity.
        // We rebase it to its source region, run the bounds verifier over
        // the whole reachable graph confined to that region, and only on
        // a clean verify decode the value in place. A verifier failure is
        // a loud error — we never decode an unverified in-place return.
        // The decode pipeline (sentinel → region-select → verifier →
        // decode) is shared with the cranelift backend via
        // `relon_eval_api::inplace_return`, and reused verbatim by the
        // wasm host (the arena is then a slice of wasm linear memory).
        if ret < 0 {
            let root_abs = relon_eval_api::inplace_return::decode_inplace_sentinel(ret)?;
            if !is_single_value_wrapper(&schema.return_schema) {
                return Err(RuntimeError::IoError(
                    "llvm-aot in-place return on a non-single-value return schema".into(),
                ));
            }
            return relon_eval_api::inplace_return::decode_inplace_return(
                "llvm-aot",
                arena,
                regions,
                root_abs,
                &schema.return_schema.fields[0],
                &schema.return_layout,
                &schema.return_schema.fields,
            );
        }
        let bw = ret as usize;

        let read_len = bw.max(schema.return_layout.root_size);
        let out_ptr = regions.out_ptr as usize;
        let read_end = out_ptr + read_len;
        if read_end > regions.arena_size || read_end > arena.len() {
            return Err(RuntimeError::IoError(
                "llvm-aot arena too small for return decode".into(),
            ));
        }
        let arena = &arena[..regions.arena_size.min(arena.len())];
        // Object / fixed-area return path: the shared central entry gates
        // the record through the multi-region bounds verifier BEFORE any
        // decode (verify → decode is enforced inside, so no object-return
        // caller can skip it), then walks the backend-shared object-field
        // reader. Under the F1 arena-absolute slot convention the object
        // head sits at `out_ptr` and every pointer slot it carries is an
        // arena-absolute offset, so the reader + verifier walk the **whole
        // arena** anchored at `out_ptr`. The gate confines every followed
        // span to one region (today all in `out`; cross-region object
        // fields stay capped — F1b releases them) and closes the red-line
        // gap where the object path previously decoded with no verifier.
        relon_eval_api::inplace_return::decode_object_return(
            "llvm-aot",
            arena,
            out_ptr,
            regions,
            &schema.return_layout,
            &schema.return_schema,
            is_single_value_wrapper(&schema.return_schema),
        )
    }

    /// Plan a wasm buffer-protocol dispatch: pack the `#main` args into
    /// the input record and compute the same arena layout
    /// `run_main_buffer` lays for the host JIT.
    ///
    /// The wasm host (wasmtime) lays the returned [`WasmBufferDispatch`]
    /// into linear memory, invokes the exported buffer entry, then hands
    /// the post-call arena view back to [`Self::wasm_buffer_decode`]. The
    /// arena layout, the const-data prefix, and the input packing are
    /// **byte-identical** to the host path, so the wasm module — which is
    /// the same LLVM IR retargeted to wasm32 — observes exactly the arena
    /// the JIT body was emitted against. The single divergence is the
    /// arena's absolute base in memory (a host `Vec` vs. a wasm
    /// linear-memory offset), which the wasm body absorbs through its
    /// `arena_base` global; every offset here is arena-relative.
    pub fn wasm_buffer_plan(
        &self,
        args: &HashMap<String, Value>,
    ) -> Result<WasmBufferDispatch, RuntimeError> {
        let schema = self
            .buffer_schema
            .as_ref()
            .ok_or_else(|| RuntimeError::Unsupported {
                reason: "llvm-aot: wasm_buffer_plan called without schema metadata".into(),
            })?;

        // Pack the input record exactly as `run_main_buffer` does.
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
        // F1: bake `in_ptr` into every input pointer slot (arena-absolute
        // convention) — identical to `run_main_buffer`, so the wasm module
        // (same IR retargeted) sees the same input bytes.
        let in_ptr_pre = relon_util::align_up(
            u32::try_from(self.const_data.len()).map_err(|_| {
                RuntimeError::IoError("llvm const-data section exceeds u32 range".into())
            })?,
            8,
        );
        let in_bytes = builder
            .finish_arena_absolute(in_ptr_pre)
            .map_err(buffer_to_runtime_error)?;

        // Lay out the arena identically to `run_main_buffer`.
        let in_len = in_bytes.len() as u32;
        let out_root_size = schema.return_layout.root_size as u32;
        let needs_pointer_indirect_return = return_needs_tail_region(&schema.return_schema);
        let tail_cap: u32 = if needs_pointer_indirect_return {
            65_536
        } else {
            0
        };
        let out_cap = relon_util::align_up(out_root_size.max(8) + tail_cap + 16, 8);
        let const_data_len = u32::try_from(self.const_data.len()).map_err(|_| {
            RuntimeError::IoError("llvm const-data section exceeds u32 range".into())
        })?;
        let in_ptr = relon_util::align_up(const_data_len, 8);
        let out_ptr = relon_util::align_up(in_ptr + in_len, 8);
        let scratch_base = relon_util::align_up(out_ptr + out_cap, 8);
        let scratch_size: u32 = 1_048_576;
        let arena_size = (scratch_base + scratch_size) as usize;

        Ok(WasmBufferDispatch {
            const_data: self.const_data.clone(),
            in_bytes,
            regions: ArenaRegions {
                const_data_len: self.const_data.len(),
                in_ptr,
                in_len,
                out_ptr,
                out_cap,
                scratch_base,
                arena_size,
            },
        })
    }

    /// Decode a wasm buffer-protocol return. `arena` is a slice of the
    /// wasm linear memory **rebased to the arena origin** (i.e.
    /// `&memory[arena_abs .. arena_abs + arena_size]`), so the
    /// arena-relative offsets in `regions` and the arena-relative root in
    /// the negative sentinel resolve exactly as they do on the host JIT
    /// path. `ret` is the i32 the wasm entry returned.
    ///
    /// This routes through the **same** [`Self::decode_buffer_return`] the
    /// host path uses — the in-place sentinel still runs the
    /// `relon_eval_api::inplace_return` verifier over the linear-memory
    /// slice before any decode. There is no wasm-specific decode or
    /// wasm-specific verifier.
    pub fn wasm_buffer_decode(
        &self,
        arena: &[u8],
        regions: ArenaRegions,
        ret: i32,
    ) -> Result<Value, RuntimeError> {
        let schema = self
            .buffer_schema
            .as_ref()
            .ok_or_else(|| RuntimeError::Unsupported {
                reason: "llvm-aot: wasm_buffer_decode called without schema metadata".into(),
            })?;
        self.decode_buffer_return(schema, arena, regions, ret)
    }
}

/// A planned wasm buffer-protocol dispatch produced by
/// [`LlvmAotEvaluator::wasm_buffer_plan`]: the const-data prefix, the
/// packed input record, and the full arena region layout. The wasm host
/// lays `const_data` at arena offset 0 and `in_bytes` at
/// `regions.in_ptr`, invokes the entry symbol it emitted, then decodes
/// via [`LlvmAotEvaluator::wasm_buffer_decode`].
#[derive(Debug, Clone)]
pub struct WasmBufferDispatch {
    /// Const-pool blob; laid at arena offset 0 (before `in_ptr`).
    pub const_data: Vec<u8>,
    /// Packed input record; laid at `regions.in_ptr`.
    pub in_bytes: Vec<u8>,
    /// Arena region boundaries (all arena-relative).
    pub regions: ArenaRegions,
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
// These mirror what `relon-codegen-cranelift::evaluator` does for
// `write_value_into_builder` / `is_single_value_wrapper` /
// `buffer_to_runtime_error`. The object-return *decode* side is no
// longer mirrored per crate — it lives once in
// `relon_eval_api::inplace_return::decode_object_return`. Kept inside
// this crate so the LLVM backend has no compile-time dep on
// cranelift-native.
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
    // Wave T2: a tuple schema (`is_tuple`) decodes positionally to a
    // `Value::List`, never to a scalar / branded dict — so a 1-tuple
    // `Tuple<Int>` must NOT take the typed-i64 fast path (which would
    // return the wrong container shape). Force it onto the buffer path so
    // the shared `decode_object_return` tuple fork runs.
    !schema.is_tuple && schema.fields.len() == 1 && matches!(schema.fields[0].ty, TypeRepr::Int)
}

/// Marshal a typed [`Value`] into the buffer slot for `field` on the
/// way *into* the JIT body (host → arena).
///
/// ## marshalling-seam contract (host side)
///
/// This dispatcher is one of the per-type marshalling seams S1.A
/// carved out so each leaf type owns a private `marshal_<type>_in`
/// helper rather than living inline in a single fat `match`. Adding a
/// new leaf type means: (1) add an arm here delegating to a new
/// `marshal_<type>_in`, (2) add the symmetric arm to the shared
/// object-return decoder `relon_eval_api::inplace_return` (reached via
/// `decode_object_return`), and (3) widen the build.rs-visible
/// [`EmittedFieldType`] triple (see that enum's docs).
///
/// Note: MCJIT already marshals `Float` / `Schema` here; the
/// build.rs-visible [`EmittedFieldType`] surface is the *narrower* set
/// (see [`lower_field_descriptors`]). Keep the two in mind separately —
/// this seam is the runtime marshaller, `EmittedFieldType` is the
/// AOT-binding signature surface.
fn write_value_into_builder(
    builder: &mut relon_eval_api::buffer::BufferBuilder<'_>,
    field: &relon_eval_api::schema_canonical::Field,
    value: &Value,
) -> Result<(), RuntimeError> {
    use relon_eval_api::schema_canonical::TypeRepr;
    match (&field.ty, value) {
        (TypeRepr::Int, Value::Int(v)) => marshal_int_in(builder, &field.name, *v),
        (TypeRepr::Float, Value::Float(v)) => {
            marshal_float_in(builder, &field.name, v.into_inner())
        }
        (TypeRepr::Float, Value::Int(v)) => marshal_float_in(builder, &field.name, *v as f64),
        (TypeRepr::Bool, Value::Bool(v)) => marshal_bool_in(builder, &field.name, *v),
        (TypeRepr::Null, Value::Null) => marshal_null_in(builder, &field.name),
        (TypeRepr::String, Value::String(s)) => marshal_string_in(builder, &field.name, s),
        (TypeRepr::Schema { schema }, Value::Dict(dict)) => {
            marshal_schema_in(builder, &field.name, schema, dict)
        }
        (TypeRepr::List { element }, Value::List(items)) => {
            marshal_list_in(builder, &field.name, element, items)
        }
        // ----- add new leaf marshalling arm above this line -----
        (ty, v) => Err(RuntimeError::Unsupported {
            reason: format!(
                "llvm-aot: #main arg `{}` got {} but schema expects {ty:?}",
                field.name,
                v.type_name()
            ),
        }),
    }
}

// --- per-variant host-side input marshalling helpers (S1.A seam) ---
//
// One `marshal_<type>_in` per leaf type. Future Float/List lanes fill
// their own helper here without touching sibling arms.

fn marshal_int_in(
    builder: &mut relon_eval_api::buffer::BufferBuilder<'_>,
    name: &str,
    v: i64,
) -> Result<(), RuntimeError> {
    builder.write_int(name, v).map_err(buffer_to_runtime_error)
}

fn marshal_float_in(
    builder: &mut relon_eval_api::buffer::BufferBuilder<'_>,
    name: &str,
    v: f64,
) -> Result<(), RuntimeError> {
    builder
        .write_float(name, v)
        .map_err(buffer_to_runtime_error)
}

fn marshal_bool_in(
    builder: &mut relon_eval_api::buffer::BufferBuilder<'_>,
    name: &str,
    v: bool,
) -> Result<(), RuntimeError> {
    builder.write_bool(name, v).map_err(buffer_to_runtime_error)
}

fn marshal_null_in(
    builder: &mut relon_eval_api::buffer::BufferBuilder<'_>,
    name: &str,
) -> Result<(), RuntimeError> {
    builder.write_null(name).map_err(buffer_to_runtime_error)
}

/// Top-level / schema `String` `#main` arg marshalling. The
/// pointer-indirect `BufferBuilder::write_string` appends a
/// `[len: u32 LE][utf8]` record into the parent buffer's tail area and
/// back-patches the 4-byte buffer-relative offset slot the JIT's
/// `LoadStringPtr` reads — the same record shape `ConstString` bakes.
fn marshal_string_in(
    builder: &mut relon_eval_api::buffer::BufferBuilder<'_>,
    name: &str,
    s: &str,
) -> Result<(), RuntimeError> {
    builder
        .write_string(name, s)
        .map_err(buffer_to_runtime_error)
}

/// `List<…>` `#main` arg marshalling. Dispatches on the canonical
/// element type to the matching pointer-indirect `write_list_*` writer,
/// each of which appends the tail record (`[len][payload]` for scalar
/// elements, a `[len][off_0]…` pointer array of `[len][utf8]` String
/// records for `List<String>`) into the parent buffer's tail area and
/// back-patches the 4-byte buffer-relative offset slot the JIT's
/// `LoadList*Ptr` / pointer-indirect `LoadFieldAtAbsolute` reads — the
/// same shapes the ConstPool `add_list_*` blobs bake, so a list `#main`
/// arg and a const list return share one tail-record protocol. Element
/// `Value`s are type-checked against the declared element type;
/// `List<Schema>` (and any other element) stays a loud cap.
fn marshal_list_in(
    builder: &mut relon_eval_api::buffer::BufferBuilder<'_>,
    name: &str,
    element: &relon_eval_api::schema_canonical::TypeRepr,
    items: &[Value],
) -> Result<(), RuntimeError> {
    use relon_eval_api::schema_canonical::TypeRepr;
    let mismatch = |idx: usize, got: &Value, want: &str| RuntimeError::Unsupported {
        reason: format!(
            "llvm-aot: List<{want}> arg `{name}` element #{idx} got {} but expects {want}",
            got.type_name()
        ),
    };
    match element {
        TypeRepr::Int => {
            let mut out = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                match it {
                    Value::Int(v) => out.push(*v),
                    other => return Err(mismatch(i, other, "Int")),
                }
            }
            builder
                .write_list_int(name, &out)
                .map_err(buffer_to_runtime_error)
        }
        TypeRepr::Float => {
            let mut out = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                match it {
                    Value::Float(v) => out.push(v.into_inner()),
                    Value::Int(v) => out.push(*v as f64),
                    other => return Err(mismatch(i, other, "Float")),
                }
            }
            builder
                .write_list_float(name, &out)
                .map_err(buffer_to_runtime_error)
        }
        TypeRepr::Bool => {
            let mut out = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                match it {
                    Value::Bool(v) => out.push(*v),
                    other => return Err(mismatch(i, other, "Bool")),
                }
            }
            builder
                .write_list_bool(name, &out)
                .map_err(buffer_to_runtime_error)
        }
        TypeRepr::String => {
            let mut out: Vec<&str> = Vec::with_capacity(items.len());
            for (i, it) in items.iter().enumerate() {
                match it {
                    Value::String(s) => out.push(s.as_str()),
                    other => return Err(mismatch(i, other, "String")),
                }
            }
            builder
                .write_list_string(name, &out)
                .map_err(buffer_to_runtime_error)
        }
        TypeRepr::Schema { schema } => marshal_list_schema_in(builder, name, schema, items),
        TypeRepr::List { element: inner } => marshal_list_list_in(builder, name, inner, items),
        other => Err(RuntimeError::Unsupported {
            reason: format!(
                "llvm-aot: List element type {other:?} for arg `{name}` is not yet materialised \
                 (List<Int/Float/Bool/String/Schema> + List<List<scalar>>)"
            ),
        }),
    }
}

/// Marshal a `List<Schema>` arg: each element is a branded
/// `Value::Dict` written as a sub-record into the parent buffer's tail
/// through [`relon_eval_api::buffer::ListRecordWriter`]. The list
/// header's per-entry offsets and the inner sub-records' own pointer
/// slots are relocated into the parent's coordinate system by
/// `finish_entry` / `finish_list_record`. Mirrors the cranelift backend.
fn marshal_list_schema_in(
    builder: &mut relon_eval_api::buffer::BufferBuilder<'_>,
    name: &str,
    schema: &relon_eval_api::schema_canonical::Schema,
    items: &[Value],
) -> Result<(), RuntimeError> {
    let elem_layout = relon_eval_api::layout::SchemaLayout::offsets_for(schema).map_err(|e| {
        RuntimeError::Unsupported {
            reason: format!("llvm-aot: List<Schema> arg `{name}` element layout: {e}"),
        }
    })?;
    let mut writer = builder
        .list_record_writer(name, &elem_layout, schema)
        .map_err(buffer_to_runtime_error)?;
    for (i, it) in items.iter().enumerate() {
        let Value::Dict(dict) = it else {
            return Err(RuntimeError::Unsupported {
                reason: format!(
                    "llvm-aot: List<Schema> arg `{name}` element #{i} got {} but expects a \
                     branded record",
                    it.type_name()
                ),
            });
        };
        let mut child = writer.start_entry();
        write_schema_into_builder(&mut child, schema, dict, name)?;
        writer
            .finish_entry(builder, child)
            .map_err(buffer_to_runtime_error)?;
    }
    builder
        .finish_list_record(writer)
        .map_err(buffer_to_runtime_error)
}

/// Marshal a nested `List<List<scalar>>` arg. Each element is itself a
/// `Value::List` of inline-fixed scalars (`Int` / `Float` / `Bool`)
/// serialised into a `[len][payload]` inner record; the outer header is
/// a pointer array of offsets to those records. Mirrors the cranelift
/// backend; inner pointer-array element lists (`List<List<String>>`)
/// stay a loud cap at the layout pass.
fn marshal_list_list_in(
    builder: &mut relon_eval_api::buffer::BufferBuilder<'_>,
    name: &str,
    inner: &relon_eval_api::schema_canonical::TypeRepr,
    items: &[Value],
) -> Result<(), RuntimeError> {
    use relon_eval_api::schema_canonical::TypeRepr;
    // `List<List<scalar>>` keeps the inline-fixed inner-record writer;
    // `List<List<String|Schema|List>>` (F5) routes through the recursive
    // doubly-nested pointer-array marshaller.
    match inner {
        TypeRepr::Int | TypeRepr::Float | TypeRepr::Bool => {
            relon_eval_api::buffer::write_nested_scalar_list(builder, name, inner, items)
                .map_err(buffer_to_runtime_error)
        }
        _ => relon_eval_api::buffer::write_nested_pointer_array_list(builder, name, inner, items)
            .map_err(buffer_to_runtime_error),
    }
}

/// Phase 0b: Schema-typed `#main` arg marshalling. A branded
/// `Value::Dict` (e.g. `#main(Outer o)`) lands here.
/// `BufferBuilder::sub_record` / `finish_sub_record` (eval-api
/// Phase 9.b-1) write the sub-record into the parent buffer's tail area
/// and back-patch the 4-byte buffer-relative offset slot in the fixed
/// area — exactly the slot `LoadSchemaPtr` reads. We recurse over the
/// sub-fields (including nested Inner); `finish_sub_record`'s internal
/// `relocate_pointers` rebases the child's own pointer slots into the
/// parent's coordinate system.
fn marshal_schema_in(
    builder: &mut relon_eval_api::buffer::BufferBuilder<'_>,
    name: &str,
    schema: &relon_eval_api::schema_canonical::Schema,
    dict: &relon_eval_api::ValueDict,
) -> Result<(), RuntimeError> {
    let sub_layout = relon_eval_api::layout::SchemaLayout::offsets_for(schema).map_err(|e| {
        RuntimeError::Unsupported {
            reason: format!("llvm-aot: schema arg `{name}` layout: {e}"),
        }
    })?;
    let mut child = builder
        .sub_record(name, &sub_layout, &schema.fields)
        .map_err(buffer_to_runtime_error)?;
    write_schema_into_builder(&mut child, schema, dict, name)?;
    builder
        .finish_sub_record(name, child)
        .map_err(buffer_to_runtime_error)
}

/// Recursively fill `child` (a detached sub-record builder) with the
/// fields of `schema`, pulling each value out of the branded `dict`.
/// Nested `Schema`-typed fields recurse through
/// [`write_value_into_builder`]'s Schema arm, which re-enters this
/// helper one layer down.
///
/// `parent_field` is only used for error messages so a missing nested
/// field names its enclosing slot.
fn write_schema_into_builder(
    child: &mut relon_eval_api::buffer::BufferBuilder<'_>,
    schema: &relon_eval_api::schema_canonical::Schema,
    dict: &relon_eval_api::ValueDict,
    parent_field: &str,
) -> Result<(), RuntimeError> {
    for sub_field in &schema.fields {
        let sub_value =
            dict.map
                .get(sub_field.name.as_str())
                .ok_or_else(|| RuntimeError::Unsupported {
                    reason: format!(
                        "llvm-aot: schema arg `{parent_field}` is missing field `{}`",
                        sub_field.name
                    ),
                })?;
        write_value_into_builder(child, sub_field, sub_value)?;
    }
    Ok(())
}

// The object-return field decode (`read_value_from_reader` /
// `read_record_into_map` and the per-type `marshal_*_out` seam) now
// lives once in `relon_eval_api::inplace_return` and is reached through
// `decode_object_return`; both AOT backends share that single copy, so a
// new return field type is added in exactly one place.

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

/// Phase D.1 / D.2: discover whether `schema` qualifies for the typed
/// fast-path entry. Eligibility requires every declared `#main` arg
/// to be `Int` (Inline scalar at 8 / 8) and the return record to
/// carry a single `Int` field — either the canonical
/// `Ret { value: Int }` wrapper (Phase D.1) or any user-declared
/// `#main(...) -> Dict` whose anon-record lowering collapsed to one
/// `Int` field (Phase D.2 — W7's `{ result: Int }` is the motivating
/// shape). Returns the `FastPathProfile` mapping param-declaration
/// Whether the typed `(i64..) -> i64` fast entry can lower `entry`'s
/// body. The fast entry runs with **no `*state` pointer and an empty
/// const-pool** (see `emit_fast_entry`), so any op that resolves
/// against the arena-prefix const-pool — `Op::ConstString` and the
/// `Op::ConstList*` family — cannot be materialised on it. Such a body
/// must take the buffer entry even when its `#main` schema is otherwise
/// fast-eligible (W4: `Int -> Int` schema over a `"axb"` string
/// literal). Returns `false` if any reachable op references the pool.
///
/// This is the object-emit analogue of MCJIT's
/// emit-fast-then-roll-back-on-failure dance: rather than emit a fast
/// entry, watch it fail, and delete it, we predict the failure here and
/// route straight to the buffer entry (the object module has no second
/// "buffer entry also present" fallback to fall onto).
fn fast_entry_emittable(entry: &relon_ir::ir::Func) -> bool {
    !body_references_const_pool(&entry.body)
}

fn body_references_const_pool(body: &[relon_ir::ir::TaggedOp]) -> bool {
    use relon_ir::ir::Op;
    for tagged in body {
        let hit = match &tagged.op {
            Op::ConstString { .. }
            | Op::ConstListInt { .. }
            | Op::ConstListFloat { .. }
            | Op::ConstListBool { .. }
            | Op::ConstListString { .. } => true,
            Op::Block { body, .. } | Op::Loop { body, .. } => body_references_const_pool(body),
            Op::If {
                then_body,
                else_body,
                ..
            } => body_references_const_pool(then_body) || body_references_const_pool(else_body),
            // `Op::Call` inlines a bundled-stdlib body whose own const-
            // pool ops would resolve against the same (empty, on the fast
            // entry) pool. Mirror `ConstPool::collect_op`'s stdlib
            // recursion so a stdlib body that bakes a literal also forces
            // the buffer entry.
            Op::Call { fn_index, .. } => {
                let stdlib = relon_ir::stdlib::builtin_stdlib();
                stdlib
                    .get(*fn_index as usize)
                    .map(|callee| body_references_const_pool(&callee.body_owned()))
                    .unwrap_or(false)
            }
            _ => false,
        };
        if hit {
            return true;
        }
    }
    false
}

/// P3 §2.2 wasm closed-world routing: derive a per-`import_idx`
/// effectful flag from the IR's `Op::CheckCap` → `Op::CallNative` shape.
///
/// The IR lowering (`try_lower_native_call`) emits one `Op::CheckCap`
/// per capability bit a host fn's gate requires *immediately before* the
/// call's argument evaluation, then the `Op::CallNative`. A **pure**
/// host fn (empty gate) emits zero preceding CheckCaps; an **effectful**
/// one (reads clock / IO / side effect — gated by a capability) emits at
/// least one. The `NativeImport.cap_bit` carried into codegen is always
/// `NO_CAPABILITY_BIT` (the guard rides the CheckCap ops, not the call),
/// so this CheckCap-presence scan is the in-codegen signal that survives
/// IR lowering — no analyzer/IR change required.
///
/// Returns `effectful[i] == true` iff import index `i`'s call site is
/// guarded by a preceding CheckCap. Walks every function body
/// (entry + helpers + lambdas), maintaining a per-body count of pending
/// CheckCaps consumed by the next CallNative. A pure call nested inside
/// an effectful call's arguments carries no CheckCap of its own, so it
/// won't be mis-flagged.
fn compute_effectful_imports(ir: &relon_ir::ir::Module) -> Vec<bool> {
    let mut effectful = vec![false; ir.imports.len()];
    for func in &ir.funcs {
        scan_body_effectful(&func.body, &mut effectful);
    }
    effectful
}

fn scan_body_effectful(body: &[relon_ir::ir::TaggedOp], effectful: &mut [bool]) {
    use relon_ir::ir::Op;
    // Pending CheckCaps in declaration order ahead of the next CallNative
    // in this op sequence. The lowering pins them right before the call's
    // args, so a non-zero count when a CallNative is reached marks that
    // import effectful.
    let mut pending_check_caps: u32 = 0;
    for tagged in body {
        match &tagged.op {
            Op::CheckCap { .. } => pending_check_caps += 1,
            Op::CallNative { import_idx, .. } => {
                if pending_check_caps > 0 {
                    if let Some(slot) = effectful.get_mut(*import_idx as usize) {
                        *slot = true;
                    }
                }
                pending_check_caps = 0;
            }
            // Nested control flow: recurse so a CheckCap-guarded call
            // inside a branch / loop is still flagged. A nested block
            // starts its own pending count.
            Op::Block { body, .. } | Op::Loop { body, .. } => {
                scan_body_effectful(body, effectful);
            }
            Op::If {
                then_body,
                else_body,
                ..
            } => {
                scan_body_effectful(then_body, effectful);
                scan_body_effectful(else_body, effectful);
            }
            _ => {}
        }
    }
}

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
///
/// ## Three-crate triple contract
///
/// This tag is the byte-for-byte-identical seam shared by three crates;
/// the enum is mirrored (not shared) so the runtime shim and build
/// generator don't take a dep on this codegen crate:
///
/// 1. `relon_codegen_llvm` (this enum) — produced by
///    [`lower_field_descriptors`].
/// 2. `relon_rs_shims::EmittedFieldType` — the runtime mirror;
///    `call_buffer_entry` packs/unpacks per variant.
/// 3. `relon_rs_build` — `rust_type_for` maps each variant to the Rust
///    surface type + `ArgValue` / `RetValue` constructor.
///
/// **Adding a variant is a four-touch change**: (1) add the variant
/// here + its arm in [`lower_field_descriptors`]; (2) add the mirror
/// variant + the `*_in` / `*_out` sibling helpers in
/// `relon_rs_shims::marshal`; (3) add the `rust_type_for` table row in
/// `relon_rs_build`; (4) extend the cross-crate round-trip guard test.
/// The guard test in `relon-rs-build/tests/marshal_roundtrip.rs` fails
/// closed if any of the three drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmittedFieldType {
    /// `i64`. Inline slot at offset, 8/8.
    Int,
    /// `f64`. Inline slot at offset, 8/8 (8 LE bytes, IEEE-754).
    Float,
    /// `bool`. Inline slot at offset, 1/1.
    Bool,
    /// `()`. Inline slot at offset, 1/1 (always reads as zero).
    Null,
    /// `&str` / `String`. Pointer-indirect: fixed slot is a 4-byte
    /// buffer-relative offset to a `[len: u32 LE][utf8 bytes]` tail
    /// record. Build.rs uses `BufferBuilder::write_string` to pack
    /// inputs and `BufferReader::read_string` to decode outputs.
    String,
    /// `&[i64]` / `Vec<i64>`. Pointer-indirect (like `String`): the
    /// fixed slot is a 4-byte buffer-relative offset to a
    /// `[len: u32 LE][pad to 8][i64 LE …]` tail record (8/8-inline
    /// elements, byte-identical to the ConstPool `add_list_int` blob).
    /// Build.rs uses `BufferBuilder::write_list_int` to pack inputs and
    /// `BufferReader::read_list_int` to decode outputs.
    ListInt,
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
        // Thin wrapper preserving the historical 3-arg signature the
        // rs-build `emit_all` calls (Stage 2 keeps this call site
        // stable). Default options (no host `#native` declarations) +
        // open-world dispatch — byte-identical to the pre-S2.⑤ path.
        let options = relon_analyzer::AnalyzeOptions {
            strict_mode: false,
            ..Default::default()
        };
        Self::emit_object_with_options(
            src,
            entry_symbol,
            out_path,
            &options,
            WorldMode::OpenWorld,
            None,
        )
    }

    /// Stage 2.⑤ options-carrying object-emit seam.
    ///
    /// Threads a caller-supplied [`relon_analyzer::AnalyzeOptions`] (so
    /// host `#native` declarations resolve — the W1-C capability-gate
    /// e2e enabler) and a [`WorldMode`] through the object-emit path.
    ///
    /// - [`WorldMode::OpenWorld`] (the [`Self::emit_object`] default):
    ///   `Op::CallNative` lowers to the dynamic `relon_llvm_call_native`
    ///   helper. `host_shim_src` is ignored.
    /// - [`WorldMode::ClosedWorld`]: `Op::CallNative` lowers to a direct
    ///   `call @<host_symbol>`; `host_shim_src` (the `#[no_mangle]
    ///   extern "C"` host crate) is compiled to LLVM-18 bitcode, linked
    ///   into the emitted module, force-inlined, and folded by O3 — so
    ///   every native call collapses to the host fn body in the `.o`.
    ///   A `None` shim on the closed-world path is an error when the
    ///   source actually imports a host fn.
    pub fn emit_object_with_options(
        src: &str,
        entry_symbol: &str,
        out_path: &Path,
        options: &relon_analyzer::AnalyzeOptions,
        world_mode: WorldMode,
        host_shim_src: Option<&str>,
    ) -> Result<EmitObjectInfo, LlvmError> {
        // Default target is the host (native x86-64 ELF). S3.X adds the
        // wasm32 retarget via `emit_object_for_target`.
        Self::emit_object_for_target(
            src,
            entry_symbol,
            out_path,
            options,
            world_mode,
            host_shim_src,
            CodegenTarget::Native,
        )
    }

    /// S3.X object-emit seam parameterised by [`CodegenTarget`].
    ///
    /// `CodegenTarget::Native` is byte-identical to the historical
    /// [`Self::emit_object_with_options`] path. `CodegenTarget::Wasm32`
    /// runs the SAME relon-IR → LLVM-IR emitter but constructs a
    /// `wasm32-wasi` `TargetMachine` (+ stamps the module's wasm32
    /// triple / DataLayout) so `write_to_file` emits a `\0asm` object
    /// instead of an ELF `.o`. The lowered body is unchanged — `mem.rs`
    /// already lays the arena out via pointer-width-agnostic i32-offset
    /// GEPs.
    ///
    /// Wasm32 supports both worlds (P3 §2.2). Open-world routes every
    /// `#native` host fn through a WASI import. Closed-world co-compiles
    /// the **pure-compute** host fns into the wasm unit and inlines them
    /// (via `link_and_inline_host_shim_wasm_pure_only`), while still
    /// routing **effectful** (capability-gated) host fns through WASI
    /// imports — symmetric with the native closed-world inline.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_object_for_target(
        src: &str,
        entry_symbol: &str,
        out_path: &Path,
        options: &relon_analyzer::AnalyzeOptions,
        world_mode: WorldMode,
        host_shim_src: Option<&str>,
        target: CodegenTarget,
    ) -> Result<EmitObjectInfo, LlvmError> {
        let (ir, main_schema, return_schema) = Self::lower_source_with_options(src, Some(options))?;
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
        //
        // This strict projection only matters to the **build.rs binding
        // generator**, which consumes `main_fields` / `return_fields` to
        // stamp the typed Rust wrapper — that path is `Native` only. The
        // `Wasm32` target feeds the **wasm-evaluator host**, which packs
        // its input and decodes its return through `wasm_buffer_plan` /
        // `wasm_buffer_decode` (driven by the full `BufferSchema`), never
        // these erased descriptors. So a `#main` carrying a pointer-array
        // list param/return the binding can't marshal (e.g. an in-place
        // `List<List<scalar>>` / `List<String>` / `List<Schema>` identity)
        // must still emit a runnable wasm body. We therefore only enforce
        // the binding-marshallability gate on `Native`; on `Wasm32` an
        // unbindable leaf yields an empty descriptor vec (the wasm host
        // ignores it) rather than aborting the emit.
        let descriptors_strict = matches!(target, CodegenTarget::Native);
        let (main_fields, return_fields) = if descriptors_strict {
            (
                lower_field_descriptors(&schema.main_schema, &schema.main_layout)?,
                lower_field_descriptors(&schema.return_schema, &schema.return_layout)?,
            )
        } else {
            (
                lower_field_descriptors(&schema.main_schema, &schema.main_layout)
                    .unwrap_or_default(),
                lower_field_descriptors(&schema.return_schema, &schema.return_layout)
                    .unwrap_or_default(),
            )
        };

        let entry_idx = ir
            .entry_func_index
            .ok_or_else(|| LlvmError::Codegen("IR module has no entry function".into()))?;
        let entry = &ir.funcs[entry_idx];

        // Verify the IR carries the canonical buffer-protocol entry
        // signature. `lower_workspace_single` always produces this
        // shape today; failing the check means an IR-layer change
        // slipped past the test gates.
        if !crate::codegen::is_buffer_protocol_signature(&entry.params, entry.ret) {
            return Err(LlvmError::UnsupportedSignature(
                "relon-rs build: lowering produced a non-buffer entry shape".into(),
            ));
        }

        // Fast-path eligibility — Int-only schema, arity <= 8, no
        // pointer-indirect leaves. Sources that don't qualify drop to
        // the buffer-protocol path below.
        //
        // Stage 2.⑤: the closed-world path always takes the buffer
        // entry — `Op::CallNative` needs the `*state` pointer only the
        // buffer entry threads (the fast entry has no state slot). An
        // Int-only `#main` that calls a host fn would otherwise match
        // the fast profile and emit an entry the native-dispatch
        // lowering rejects. Force buffer mode for closed-world.
        let fast_profile = match world_mode {
            WorldMode::ClosedWorld => None,
            // P3 §2.2: a module that calls a `#native` host fn must take
            // the buffer entry even when its `#main` schema is Int-only
            // and would otherwise match the fast profile — `Op::CallNative`
            // / the preceding `Op::CheckCap` need the `*state` pointer and
            // the trailing `caps` slot only the buffer entry threads (the
            // fast `(i64..)->i64` entry has neither). Same reasoning the
            // closed-world arm uses to force buffer mode.
            WorldMode::OpenWorld if !ir.imports.is_empty() => None,
            WorldMode::OpenWorld => build_fast_path_profile(&schema).ok(),
        };

        let ctx = Context::create();
        let module = ctx.create_module("relon_rs_object");

        // Phase E.1 const-pool blob; needed by buffer-protocol bodies
        // for `Op::ConstString { idx }` resolution. The fast path
        // doesn't reference the pool (Int-only bodies have no
        // ConstString ops) so the blob ends up empty in that branch.
        let const_pool = ConstPool::from_module(&ir)?;

        // Phase D fast-entry eligibility is decided from the `#main`
        // schema alone (Int args, single-Int return). That envelope is
        // necessary but not sufficient: a fast-qualifying schema can
        // still wrap a body that touches ops the `(i64..) -> i64` fast
        // entry can't lower — most notably `Op::ConstString` /
        // `Op::ConstList*`, which resolve against the arena-prefix
        // const-pool the fast entry has no state pointer to reach (it
        // emits with an empty pool). W4
        // (`range(n).map(=>"axb").filter(s.contains("x")).len()`) is the
        // canonical case: an `Int -> Int` schema over a string-literal
        // body. The in-process MCJIT path (`from_ir_inner_world`) emits
        // the buffer entry first and treats a failed fast-entry emit as
        // a soft "no fast path", rolling the fast entry back and keeping
        // the buffer entry. The object-emit path historically emitted
        // *only* the fast entry, so the same body hard-failed here with
        // a `missing const-pool entry`. Mirror MCJIT: try the fast entry
        // first, and on emit failure fall through to the buffer entry
        // (which lowers `Op::ConstString` against the real const-pool).
        let fast_profile = match fast_profile {
            // W7 recursive-closure Dict: a module that declares lambdas
            // (`#internal fib: (k) => ... fib(...)`) can match the fast
            // `(i64..) -> i64` envelope (Int `#main`, single-Int `result`
            // field) yet its body emits `Op::MakeClosure` /
            // `Op::CallClosure`, which resolve a lambda FunctionValue from
            // the module-wide `closure_fn_table`. The fast-only object-emit
            // branch emits *only* the fast entry with empty helper / closure
            // tables (it never declares + emits the lambda bodies), so
            // `MakeClosure fn_table_idx=N` hits an empty table. The buffer
            // path routes through `emit_module_funcs`, which declares every
            // lambda up-front (forward reference for `fib`'s self-call) and
            // emits each lambda body — the only place closures lower
            // correctly for static object emit. Force the buffer entry
            // whenever the module declares any lambda. The in-process MCJIT
            // path (`from_ir_inner_world`) already gets this for free: it
            // emits the buffer module first (lambdas declared + emitted) and
            // only *adds* a fast entry on top, reusing the populated table.
            Some(profile) if fast_entry_emittable(entry) && ir.closure_table.is_empty() => {
                Some(profile)
            }
            _ => None,
        };

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
                // Stage 2.⑤ / P3 §2.2: pick the dispatch emitter by world
                // mode + target. Native open-world (default / rs-build
                // today) keeps the dynamic `relon_llvm_call_native` hop;
                // native closed-world lowers `Op::CallNative` to a direct
                // `call @<host>` that the host-bitcode link + inline below
                // folds away. wasm32 open-world lowers `Op::CallNative` to a
                // **wasm import** call (`crate::wasi_host`). wasm32
                // closed-world (P3 §2.2 co-compile) inlines the
                // **pure-compute** host fns into the wasm unit while routing
                // **effectful** ones (capability-gated) through wasm imports
                // — `effectful_imports` carries the per-import split derived
                // from the IR's CheckCap shape.
                let effectful_imports = compute_effectful_imports(&ir);
                let llvm_fn = match (world_mode, target) {
                    (WorldMode::ClosedWorld, CodegenTarget::Wasm32) => {
                        emit_module_funcs_closed_world_wasm(
                            &ctx,
                            &module,
                            entry,
                            buffer_return_size,
                            &const_pool,
                            &helpers,
                            Some(&helper_ir_indices),
                            &lambdas,
                            &ir.closure_table,
                            &ir.imports,
                            &effectful_imports,
                        )?
                        .0
                    }
                    (world_mode, target) => {
                        let emit = match (world_mode, target) {
                            (WorldMode::OpenWorld, CodegenTarget::Wasm32) => emit_module_funcs_wasm,
                            (WorldMode::OpenWorld, CodegenTarget::Native) => emit_module_funcs,
                            (WorldMode::ClosedWorld, _) => emit_module_funcs_closed_world,
                        };
                        emit(
                            &ctx,
                            &module,
                            entry,
                            buffer_return_size,
                            &const_pool,
                            &helpers,
                            Some(&helper_ir_indices),
                            &lambdas,
                            &ir.closure_table,
                            &ir.imports,
                        )?
                        .0
                    }
                };
                // Rename the canonical buffer entry to the build.rs-
                // supplied symbol and force external linkage so the
                // consuming binary's linker can resolve it.
                llvm_fn.as_global_value().set_name(entry_symbol);
                llvm_fn.set_linkage(Linkage::External);

                // Closed-world: link the host shim bitcode into THIS
                // module + force-inline every imported host fn so the
                // direct `call @<host>` sites collapse to the host body.
                // Reuses the `crate::cocompile` link/inline orchestration.
                // Native links the host shim built for the host triple;
                // wasm32 links the host shim built for
                // `wasm32-unknown-unknown` so the inlined body matches the
                // wasm unit's pointer width. Either way only the
                // pre-declared (pure) host fns carry a direct `call @<host>`
                // to fold — effectful imports stay as wasm imports.
                if matches!(world_mode, WorldMode::ClosedWorld) {
                    let shim = host_shim_src.ok_or_else(|| {
                        LlvmError::Codegen(
                            "emit_object_with_options: ClosedWorld requires a host_shim_src \
                             (the #[no_mangle] extern \"C\" host crate to link + inline)"
                                .into(),
                        )
                    })?;
                    match target {
                        CodegenTarget::Wasm32 => {
                            crate::cocompile::link_and_inline_host_shim_wasm_pure_only(
                                &module,
                                shim,
                                &ir.imports,
                                &effectful_imports,
                            )?;
                        }
                        CodegenTarget::Native => {
                            crate::cocompile::link_and_inline_host_shim(
                                &module,
                                shim,
                                &ir.imports,
                            )?;
                        }
                    }
                }

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

        // Construct the object-emit `TargetMachine` for the requested
        // target up front so the same machine drives both the O3
        // pipeline and the backend codegen below.
        let (machine, target_triple) = create_object_target_machine(target)?;

        // Stamp the module's triple + DataLayout so the lowered pointer
        // width / endianness match the machine. Native inherits the
        // host triple LLVM already uses; wasm32 needs the explicit
        // `wasm32-wasi` triple + 32-bit DataLayout or the
        // verifier/codegen would default to the host's 64-bit layout.
        // Pulling the DataLayout straight from the machine's target data
        // keeps it authoritative for whichever target we built.
        module.set_triple(&TargetTriple::create(&target_triple));
        module.set_data_layout(&machine.get_target_data().get_data_layout());

        match target {
            CodegenTarget::Native => {
                // Stamp the host CPU onto every function so the
                // per-function subtarget matches the host `TargetMachine`.
                // Keeps the AOT and MCJIT paths consistent.
                stamp_host_target_attributes(&module);
                // Host-targeted O3 (same pipeline the JIT path uses).
                run_default_o3_pipeline(&module)?;
            }
            CodegenTarget::Wasm32 => {
                // No host-CPU stamping (x86 features are meaningless for
                // wasm and would mis-narrow lowering). Run O3 against the
                // wasm32 machine so the middle-end optimises for the wasm
                // target's DataLayout.
                let opts = PassBuilderOptions::create();
                module
                    .run_passes("default<O3>", &machine, opts)
                    .map_err(|e| LlvmError::Codegen(format!("wasm32 run_passes O3: {e}")))?;
            }
        }

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
        let ty = emitted_field_type_for(&f.ty).ok_or_else(|| {
            LlvmError::UnsupportedSignature(format!(
                "relon-rs build (Phase 2): field `{}` type {:?} not yet wired for marshalling",
                f.name, f.ty
            ))
        })?;
        out.push(EmittedField {
            name: f.name.clone(),
            offset: lo.offset as u32,
            ty,
        });
    }
    Ok(out)
}

/// Project one canonical [`TypeRepr`] onto the build.rs-visible
/// [`EmittedFieldType`] tag, or `None` when the leaf isn't yet wired for
/// AOT-binding marshalling.
///
/// This is the per-variant accept-set table for the
/// [`EmittedFieldType`] triple's codegen end. To widen the AOT signature
/// surface (e.g. Float / List lanes), add the matching arm here — the
/// `None` fall-through keeps every still-unsupported leaf surfacing as
/// `UnsupportedSignature` rather than silently emitting a tag the shim
/// can't decode.
fn emitted_field_type_for(
    ty: &relon_eval_api::schema_canonical::TypeRepr,
) -> Option<EmittedFieldType> {
    use relon_eval_api::schema_canonical::TypeRepr;
    match ty {
        TypeRepr::Int => Some(EmittedFieldType::Int),
        TypeRepr::Float => Some(EmittedFieldType::Float),
        TypeRepr::Bool => Some(EmittedFieldType::Bool),
        TypeRepr::Null => Some(EmittedFieldType::Null),
        TypeRepr::String => Some(EmittedFieldType::String),
        TypeRepr::List { element } if matches!(element.as_ref(), TypeRepr::Int) => {
            Some(EmittedFieldType::ListInt)
        }
        // ----- add new AOT-marshallable leaf type above this line -----
        _ => None,
    }
}

/// Stamp the runtime host CPU/feature set onto every function in the
/// module as `"target-cpu"` / `"target-features"` string function
/// attributes.
///
/// ## Why this exists (correctness, not a micro-opt)
///
/// The MCJIT execution engine is created without an MCPU/MAttr —
/// `MCJITCompilerOptions` exposes no CPU field, and inkwell's
/// `create_*_execution_engine*` builders take only an
/// [`OptimizationLevel`] (+ a `CodeModel` on the memory-manager
/// variant). With no CPU pinned, the X86 backend lowers for **generic
/// x86-64** and drops every host-tuning decision the per-CPU
/// `SubtargetFeatures` would have enabled. The one that bites hardest:
/// the `SlowDivide64` tuning that narrows a 64-bit `idivq` whose
/// operands provably fit in 32 bits into the host `shrq $32; je; divl`
/// fast path. Generic codegen always emits the bare microcoded
/// `idivq`, so every i64 `%` / `/` runs the slow divider at runtime.
///
/// The `default<O3>` middle-end pipeline already runs against a host
/// `TargetMachine` (see [`run_default_o3_pipeline`]) and the static
/// object-emit path bakes the host CPU into its `TargetMachine` too,
/// so both of those already lower for the host. Only the **MCJIT
/// backend codegen** was generic. LLVM resolves a function's subtarget
/// from its `"target-cpu"` / `"target-features"` string attributes
/// when present, so stamping the host values here makes the MCJIT
/// backend lower each function for the CPU it will actually run on —
/// identical results, correct host instruction selection.
///
/// The CPU/features are queried from the running host
/// ([`TargetMachine::get_host_cpu_name`] /
/// [`TargetMachine::get_host_cpu_features`]) — the SAME source the O3
/// pipeline uses — so this is correct on any machine and never pins a
/// hard-coded microarchitecture.
fn stamp_host_target_attributes(module: &inkwell::module::Module<'_>) {
    // `get_host_cpu_*` reads the running CPU via LLVM's host
    // introspection; no native-target init is required for these two
    // queries, but every caller has already initialised the native
    // target by this point (verify -> O3 -> engine).
    let cpu = TargetMachine::get_host_cpu_name();
    let features = TargetMachine::get_host_cpu_features();
    let cpu = cpu.to_str().unwrap_or("");
    let features = features.to_str().unwrap_or("");
    if cpu.is_empty() {
        // Host introspection failed; leave the module generic rather
        // than stamping an empty/bogus CPU. The engine still works,
        // just without host narrowing (the pre-fix behaviour).
        return;
    }
    let ctx = module.get_context();
    let cpu_attr = ctx.create_string_attribute("target-cpu", cpu);
    let features_attr = ctx.create_string_attribute("target-features", features);
    let mut func = module.get_first_function();
    while let Some(f) = func {
        // Only stamp functions with a body. Pure declarations (the
        // `relon_llvm_str_contains_arena` host shim, intrinsics) have
        // no IR to lower, and stamping a target-cpu on an external
        // declaration is harmless but pointless.
        if f.count_basic_blocks() > 0 {
            // Idempotent: replace any pre-existing stamp so a re-run
            // (or an emitter that already set one) lands on the host.
            f.remove_string_attribute(inkwell::attributes::AttributeLoc::Function, "target-cpu");
            f.remove_string_attribute(
                inkwell::attributes::AttributeLoc::Function,
                "target-features",
            );
            f.add_attribute(inkwell::attributes::AttributeLoc::Function, cpu_attr);
            f.add_attribute(inkwell::attributes::AttributeLoc::Function, features_attr);
        }
        func = f.get_next_function();
    }
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

/// Build the object-emit `TargetMachine` for the requested
/// [`CodegenTarget`]. Native bakes the host CPU/features + PIC reloc;
/// Wasm32 initialises the WebAssembly backend and pins the
/// `wasm32-wasi` triple. The triple String returned alongside lets the
/// caller stamp the module's target-triple (the DataLayout is pulled
/// from the machine's target data) so the wasm object's pointer width /
/// endianness match the machine.
fn create_object_target_machine(
    target: CodegenTarget,
) -> Result<(TargetMachine, String), LlvmError> {
    match target {
        CodegenTarget::Native => {
            Target::initialize_native(&InitializationConfig::default())
                .map_err(|e| LlvmError::Codegen(format!("initialize_native: {e}")))?;
            let triple_str = TargetMachine::get_default_triple();
            let t = Target::from_triple(&triple_str)
                .map_err(|e| LlvmError::Codegen(format!("target from_triple: {e}")))?;
            let cpu = TargetMachine::get_host_cpu_name();
            let features = TargetMachine::get_host_cpu_features();
            let triple = TargetTriple::create(
                triple_str
                    .as_str()
                    .to_str()
                    .map_err(|e| LlvmError::Codegen(format!("triple utf8: {e}")))?,
            );
            let machine = t
                .create_target_machine(
                    &triple,
                    cpu.to_str().unwrap_or(""),
                    features.to_str().unwrap_or(""),
                    OptimizationLevel::Aggressive,
                    RelocMode::PIC,
                    CodeModel::Default,
                )
                .ok_or_else(|| LlvmError::Codegen("create_target_machine returned null".into()))?;
            let triple_owned = triple_str
                .as_str()
                .to_str()
                .map_err(|e| LlvmError::Codegen(format!("triple utf8: {e}")))?
                .to_string();
            Ok((machine, triple_owned))
        }
        CodegenTarget::Wasm32 => {
            // The WebAssembly backend lives behind the `target-webassembly`
            // inkwell feature; `initialize_webassembly` registers it.
            Target::initialize_webassembly(&InitializationConfig::default());
            let triple = TargetTriple::create(WASM32_TRIPLE);
            let t = Target::from_triple(&triple)
                .map_err(|e| LlvmError::Codegen(format!("wasm32 target from_triple: {e}")))?;
            // No host-CPU narrowing for wasm; the MVP+ feature set is
            // controlled by the wasm runtime (wasmtime defaults). Reloc
            // is irrelevant for the wasm object model — `Static`/`Default`
            // both produce a relocatable `\0asm` object.
            //
            // `+bulk-memory`: lower `llvm.memcpy` / `llvm.memset` to the
            // native `memory.copy` / `memory.fill` ops instead of a libc
            // `env::memcpy` import. The pointer-indirect String / List
            // return-store path (`emit_store_field_pointer_indirect`)
            // emits a `memcpy`; without bulk-memory wasm-ld leaves an
            // unresolved `env::memcpy` import that no standard WASI host
            // satisfies. wasmtime enables bulk-memory by default, so the
            // emitted module stays ecosystem-portable.
            let machine = t
                .create_target_machine(
                    &triple,
                    /*cpu=*/ "",
                    /*features=*/ "+bulk-memory",
                    OptimizationLevel::Aggressive,
                    RelocMode::Static,
                    CodeModel::Default,
                )
                .ok_or_else(|| {
                    LlvmError::Codegen("wasm32 create_target_machine returned null".into())
                })?;
            Ok((machine, WASM32_TRIPLE.to_string()))
        }
    }
}
