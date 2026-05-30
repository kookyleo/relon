//! IR -> Cranelift IR lowering.
//!
//! v5-beta-1 deliberately narrows the supported IR surface to keep
//! the cranelift pipeline focused on the HelloWorld-tier scenarios:
//!
//! * Integer arithmetic (`Add` / `Sub` / `Mul` / `Div` / `Mod`) on `I64`.
//! * Six comparisons (`Eq` / `Ne` / `Lt` / `Le` / `Gt` / `Ge`).
//! * `ConstI64` / `ConstI32` / `ConstBool` literals plus `Return`.
//! * `LocalGet` and `LetGet` / `LetSet` for parameter / let-binding
//!   access.
//! * `If` for conditional control flow.
//! * `Call` for the narrow stdlib subset hard-wired in `evaluator.rs`
//!   (`length` of a constant String, `abs(Int)`).
//! * `ConstString` + `ReadStringLen` to validate the bounds-check
//!   path against constant String pointers.
//! * `CallNative` + `CheckCap` so the capability gate has an end-to-end
//!   exercise.
//!
//! Everything outside that envelope surfaces as
//! [`crate::CraneliftError::Codegen`] / [`CraneliftError::UnsupportedSignature`]
//! so the auto-tier wrapper can cleanly fall back to the wasm-AOT or
//! tree-walking backend without crashing the host.
//!
//! The lowering is intentionally one-pass and produces typed cranelift
//! values directly; no virtual-stack abstraction is needed because the
//! IR's stack discipline is shallow and well-typed by lowering time.
//! The cranelift verifier catches the few corner cases the lowering
//! pass might still mis-handle (type leaks across branches, etc.).

use std::collections::HashMap;

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{
    AbiParam, Function, GlobalValue, Inst, InstBuilder, MemFlags, SigRef, Signature, UserFuncName,
    Value as CValue,
};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context as CodegenContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext, Variable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{DataId, Linkage, Module as CrModule};

use relon_ir::ir::{IrType, Module as IrModule, Op, TaggedOp};

use crate::error::CraneliftError;
use crate::sandbox::{SandboxConfig, TrapKind, STATE_OFFSET_TAIL_CURSOR};
use crate::vtable::{VtableSlot, VTABLE_SYMBOL};

mod arith;
mod call;
mod closure;
mod const_pool;
mod const_pool_emit;
mod control_flow;
mod field;
mod guard;
mod hot_counter;
mod memory;
mod op_visitor;
mod record;

use const_pool::ConstPool;
use guard::{
    declare_vtable_data, emit_indirect_host_call, make_cap_lookup_signature,
    make_glob_match_signature, make_now_signature, make_raise_trap_signature,
};
use hot_counter::emit_hot_counter_inject;

/// Output of a successful compile: a JIT module plus the entry's
/// function ID so the host can resolve a raw function pointer through
/// `JITModule::get_finalized_function` later.
pub struct CompiledModule {
    pub module: JITModule,
    pub entry_fn_id: cranelift_module::FuncId,
    /// Number of `Int` parameters the entry expects (after the
    /// implicit sandbox-state pointer). Used by the runtime
    /// trampoline to materialise the `extern "C"` invocation.
    pub entry_arity: usize,
    /// Source range of the lowered `#main` directive — used by the
    /// runtime to attach trap diagnostics.
    pub entry_range: relon_parser::TokenRange,
    /// Calling convention shape the host trampoline must match.
    pub entry_shape: EntryShape,
    /// Const-data bytes the entry references through `ConstString` /
    /// `ConstList*`. The host trampoline copies these into the arena
    /// prefix before each invocation; the cranelift code refers to
    /// them through hardcoded `[len:u32 LE][payload]` record
    /// offsets emitted at compile time.
    pub const_data: Vec<u8>,
    /// Stage 5 Phase C.4: per-module closure table. Each entry is the
    /// `FuncId` of a lambda the lowering pass emitted; the host
    /// resolves each id through `get_finalized_function` after JIT
    /// finalize and installs the resulting `Vec<usize>` into the
    /// `SandboxState`. The `Op::CallClosure` lowering reads the host-
    /// fn pointer through that table, indexed by the closure handle's
    /// `fn_table_idx` field.
    pub closure_func_ids: Vec<cranelift_module::FuncId>,
    /// v5-γ stage 2: data symbol holding the `__relon_capability_vtable`
    /// slot array. The JIT pipeline populates it post-finalize via
    /// `JITModule::get_finalized_data(vtable_data_id)`; the
    /// `cranelift-object` pipeline emits the symbol as `Linkage::Export`
    /// so the host's `dlsym` round-trip resolves it after `dlopen`.
    pub vtable_data_id: cranelift_module::DataId,
}

/// How the host trampoline talks to the JIT entry.
///
/// v5-β-2 lands two shapes side-by-side:
///
/// * `LegacyI64Args` — the original v5-β-1 envelope: every IR param
///   is `I64`, return is `I64`. Used by direct-IR callers and the
///   existing codegen unit tests.
/// * `BufferProtocol` — matches the wasm-AOT `run_main` signature
///   (`fn run_main(in_ptr: i32, in_len: i32, out_ptr: i32, out_cap:
///   i32, caps: i64) -> i32`). Selected when the IR's entry
///   parameters match `[I32, I32, I32, I32, I64]` — the canonical
///   shape `lower_workspace_single` emits for every user source.
///
/// Selecting the shape from the IR rather than a separate flag keeps
/// the API surface narrow: the lowering pass is the source of truth
/// on whether the body speaks buffer protocol or raw i64s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryShape {
    /// Legacy: `(*state, i64...) -> i64`. v5-β-1 shape.
    LegacyI64Args,
    /// Buffer protocol: `(*state, i32 in_ptr, i32 in_len, i32 out_ptr,
    /// i32 out_cap, i64 caps) -> i32`. v5-β-2 shape that matches the
    /// wasm-AOT side. Loads + stores against the in/out buffer go
    /// through the `arena_base + buf_ptr + offset` formula.
    BufferProtocol,
}

/// IR param signature that triggers [`EntryShape::BufferProtocol`].
/// Mirrors the locals layout `lower_workspace_single` synthesises for
/// every user `#main` source.
fn is_buffer_protocol_signature(params: &[IrType], ret: IrType) -> bool {
    matches!(
        params,
        [
            IrType::I32,
            IrType::I32,
            IrType::I32,
            IrType::I32,
            IrType::I64
        ]
    ) && matches!(ret, IrType::I32)
}

/// Build a cranelift JIT module and lower the IR's entry function
/// into it. v5-beta-1 only emits one function (the `#main` entry);
/// auxiliary stdlib bodies the IR references are lowered as inline
/// helpers via the `Call` path.
#[cfg(test)]
pub fn compile_module(
    ir: &IrModule,
    sandbox: &SandboxConfig,
) -> Result<CompiledModule, CraneliftError> {
    compile_module_with(ir, sandbox, /* return_root_size= */ 0)
}

/// Same as [`compile_module`], but with an explicit `return_root_size`
/// hint. The hint is consumed by the buffer-protocol epilogue when the
/// body emits no pointer-indirect stores (in that case the JIT returns
/// `return_root_size` as `bytes_written` so the host trampoline reads
/// the full fixed-area record). Callers that don't have schema
/// metadata pass `0`; the trampoline already reads `max(bw,
/// return_root_size)` so a zero hint only affects pointer-indirect-
/// returning bodies, which the from_ir_direct path doesn't use.
pub fn compile_module_with(
    ir: &IrModule,
    sandbox: &SandboxConfig,
    return_root_size: u32,
) -> Result<CompiledModule, CraneliftError> {
    let entry_idx = ir
        .entry_func_index
        .ok_or_else(|| CraneliftError::Codegen("module has no entry function".into()))?;
    let entry = &ir.funcs[entry_idx];

    // Scan the entry body for ConstString / ConstList* ops and build
    // the per-module const-data pool. We pass the resolved
    // `idx -> offset` map into the Codegen so `ConstString { idx }`
    // can lower to a plain `iconst(I32, offset)`. The const-data
    // bytes themselves ride along on `CompiledModule.const_data` —
    // the host trampoline copies them into the arena prefix before
    // each invocation.
    let const_pool = ConstPool::from_module(ir)?;

    // Detect the entry shape. v5-β-2 supports two:
    //   - Legacy `(I64, ..., I64) -> I64` — direct-IR test path.
    //   - Buffer-protocol `(I32, I32, I32, I32, I64) -> I32` — what
    //     `lower_workspace_single` synthesises for every user source.
    // Anything else falls back to the legacy-shape gate and surfaces
    // as `UnsupportedSignature` so the host can pick a different
    // backend.
    let entry_shape = if is_buffer_protocol_signature(&entry.params, entry.ret) {
        EntryShape::BufferProtocol
    } else {
        // Legacy validation: every param must be I64, return must be
        // I64.
        for (i, param) in entry.params.iter().enumerate() {
            if !matches!(param, IrType::I64) {
                return Err(CraneliftError::UnsupportedSignature(format!(
                    "cranelift-native: param #{i} is {param:?} (expected I64 or buffer-protocol shape)"
                )));
            }
        }
        if !matches!(entry.ret, IrType::I64) {
            return Err(CraneliftError::UnsupportedSignature(format!(
                "cranelift-native: return is {:?} (expected I64 or buffer-protocol I32)",
                entry.ret
            )));
        }
        EntryShape::LegacyI64Args
    };

    // Cranelift ISA + flag setup. We pin `is_pic = false` because the
    // JIT loads code into heap-allocated executable pages and never
    // links via the system dynamic loader; PIC would cost an extra
    // `mov` per global access without buying anything.
    let mut flag_builder = settings::builder();
    flag_builder
        .set("is_pic", "false")
        .map_err(|e| CraneliftError::JitSetup(format!("is_pic flag: {e}")))?;
    flag_builder
        .set("opt_level", "speed")
        .map_err(|e| CraneliftError::JitSetup(format!("opt_level flag: {e}")))?;
    // Enable verifier in debug builds so accidentally malformed IR
    // surfaces with a useful message instead of producing
    // miscompiled code that segfaults at run time.
    #[cfg(debug_assertions)]
    flag_builder
        .set("enable_verifier", "true")
        .map_err(|e| CraneliftError::JitSetup(format!("enable_verifier flag: {e}")))?;
    let flags = settings::Flags::new(flag_builder);

    let isa_builder = cranelift_native::builder()
        .map_err(|e| CraneliftError::HostTarget(format!("cranelift-native: {e}")))?;
    let isa = isa_builder
        .finish(flags)
        .map_err(|e| CraneliftError::JitSetup(format!("isa finish: {e}")))?;

    // Build a JIT module with the default symbol set. v5-γ stage 2:
    // we no longer register host helper symbols by address here.
    // Instead, every helper call indirects through the
    // `__relon_capability_vtable` data symbol (see crate::vtable); the
    // post-finalize step (in `evaluator.rs`) writes the live host fn
    // pointers into the table.
    //
    // v6-γ M2/M3: in addition we pre-register the four trace JIT
    // runtime helpers (`__relon_trace_save_deopt`,
    // `__relon_trace_resolve_call`, `__relon_trace_inline_cache_lookup`
    // and the codegen-cranelift-side `__relon_jump_to_recorder`) so that
    // (a) HotCounter prologues injected into entry functions can call
    // into the recorder helper and (b) JIT-installed trace fns can
    // call the trace runtime helpers without a separate symbol
    // resolution step.
    let mut jit_builder =
        JITBuilder::with_isa(isa.clone(), cranelift_module::default_libcall_names());
    crate::trace_install::register_trace_runtime_symbols(&mut jit_builder);
    let mut module = JITModule::new(jit_builder);

    let LoweredArtifacts {
        entry_fn_id,
        vtable_data_id,
        closure_func_ids,
    } = lower_module_into(
        &mut module,
        ir,
        entry,
        entry_shape,
        sandbox,
        return_root_size,
        &const_pool,
    )?;

    module
        .finalize_definitions()
        .map_err(|e| CraneliftError::ModuleDefine(format!("finalize: {e}")))?;

    Ok(CompiledModule {
        module,
        entry_fn_id,
        entry_arity: entry.params.len(),
        entry_range: entry.range,
        entry_shape,
        const_data: const_pool.bytes,
        closure_func_ids,
        vtable_data_id,
    })
}

/// Output of [`compile_module_to_object_bytes`].
pub struct ObjectArtifact {
    /// ET_REL ELF bytes ready for `relon-object-link::link_to_dyn`.
    pub et_rel_bytes: Vec<u8>,
    /// Entry shape detected from the IR — the loader uses this to
    /// pick the right calling-convention shim.
    pub entry_shape: EntryShape,
    /// Entry arity (number of IR-declared `#main` params; doesn't
    /// count the implicit sandbox-state pointer).
    pub entry_arity: usize,
    /// Source range of the lowered `#main` directive — used by the
    /// runtime to attach trap diagnostics.
    pub entry_range: relon_parser::TokenRange,
    /// Const-data bytes the entry references through `ConstString` /
    /// `ConstList*`. The host trampoline copies these into the arena
    /// prefix before each invocation (identical to the JIT path).
    pub const_data: Vec<u8>,
    /// Symbol name the host `dlsym`s to find the entry function. The
    /// `lower_module_into` driver always declares this as
    /// `Linkage::Export run_main`.
    pub entry_symbol: &'static str,
    /// Symbol name the host `dlsym`s to find the capability vtable
    /// data slot. The host writes its function pointers into the
    /// vtable after `dlopen` returns.
    pub vtable_symbol: &'static str,
    /// `__closure_<N>` symbol names paired with their original IR
    /// `closure_table` index. The host `dlsym`s each one after
    /// `dlopen` so `SandboxState::closure_table_base` resolves to the
    /// loaded ET_DYN's function pointers.
    pub closure_symbols: Vec<String>,
}

/// v5-γ stage 2: emit the full module via `cranelift-object` for the
/// dlopen-execution cache path. Mirrors [`compile_module_with`] but
/// targets a `cranelift_object::ObjectModule` so the output is an
/// ET_REL ready for `relon-object-link::link_to_dyn`. The dlopen'd
/// ET_DYN imports only the [`crate::vtable::VTABLE_SYMBOL`] data
/// slot; every host helper call indirects through that table.
pub fn compile_module_to_object_bytes(
    ir: &IrModule,
    sandbox: &SandboxConfig,
    return_root_size: u32,
) -> Result<ObjectArtifact, CraneliftError> {
    use cranelift_object::{ObjectBuilder, ObjectModule};

    let entry_idx = ir
        .entry_func_index
        .ok_or_else(|| CraneliftError::Codegen("module has no entry function".into()))?;
    let entry = &ir.funcs[entry_idx];
    let const_pool = ConstPool::from_module(ir)?;

    let entry_shape = if is_buffer_protocol_signature(&entry.params, entry.ret) {
        EntryShape::BufferProtocol
    } else {
        for (i, param) in entry.params.iter().enumerate() {
            if !matches!(param, IrType::I64) {
                return Err(CraneliftError::UnsupportedSignature(format!(
                    "cranelift-native: param #{i} is {param:?} (expected I64 or buffer-protocol shape)"
                )));
            }
        }
        if !matches!(entry.ret, IrType::I64) {
            return Err(CraneliftError::UnsupportedSignature(format!(
                "cranelift-native: return is {:?} (expected I64 or buffer-protocol I32)",
                entry.ret
            )));
        }
        EntryShape::LegacyI64Args
    };

    // `is_pic = true` is required for ELF SHARED objects — the dynamic
    // linker `ld.so` refuses to load non-PIC `.so` files. The verifier
    // stays on in debug builds for the same reason as the JIT path.
    let mut flag_builder = settings::builder();
    flag_builder
        .set("is_pic", "true")
        .map_err(|e| CraneliftError::JitSetup(format!("is_pic flag: {e}")))?;
    flag_builder
        .set("opt_level", "speed")
        .map_err(|e| CraneliftError::JitSetup(format!("opt_level flag: {e}")))?;
    #[cfg(debug_assertions)]
    flag_builder
        .set("enable_verifier", "true")
        .map_err(|e| CraneliftError::JitSetup(format!("enable_verifier flag: {e}")))?;
    let flags = settings::Flags::new(flag_builder);

    let isa_builder = cranelift_native::builder()
        .map_err(|e| CraneliftError::HostTarget(format!("cranelift-native: {e}")))?;
    let isa = isa_builder
        .finish(flags)
        .map_err(|e| CraneliftError::JitSetup(format!("isa finish: {e}")))?;

    let obj_builder = ObjectBuilder::new(
        isa,
        "relon-native-cache",
        cranelift_module::default_libcall_names(),
    )
    .map_err(|e| CraneliftError::JitSetup(format!("object builder: {e}")))?;
    let mut module = ObjectModule::new(obj_builder);

    let LoweredArtifacts {
        entry_fn_id: _,
        vtable_data_id: _,
        closure_func_ids,
    } = lower_module_into(
        &mut module,
        ir,
        entry,
        entry_shape,
        sandbox,
        return_root_size,
        &const_pool,
    )?;

    // Collect the closure symbol names so the host can `dlsym` each
    // after `dlopen`. The lambda declarations inside
    // `lower_module_into` use the deterministic `__closure_<N>` name
    // scheme; we just regenerate the list here so the loader doesn't
    // have to parse the ET_DYN's `.dynsym` table.
    let closure_symbols = (0..closure_func_ids.len())
        .map(|i| format!("__closure_{i}"))
        .collect::<Vec<_>>();

    let product = module.finish();
    let et_rel_bytes = product
        .emit()
        .map_err(|e| CraneliftError::Codegen(format!("object emit: {e}")))?;

    Ok(ObjectArtifact {
        et_rel_bytes,
        entry_shape,
        entry_arity: entry.params.len(),
        entry_range: entry.range,
        const_data: const_pool.bytes,
        entry_symbol: "run_main",
        vtable_symbol: VTABLE_SYMBOL,
        closure_symbols,
    })
}

/// Artefacts returned by [`lower_module_into`]. The caller owns the
/// `Module`-flavoured finalize step (`JITModule::finalize_definitions`
/// vs `ObjectModule::finish().emit()`) so this struct only carries
/// the IDs the runtime resolves post-finalize.
struct LoweredArtifacts {
    entry_fn_id: cranelift_module::FuncId,
    vtable_data_id: DataId,
    closure_func_ids: Vec<cranelift_module::FuncId>,
}

/// v5-γ stage 2: shared lowering pass for both `JITModule` (live
/// in-process JIT) and `ObjectModule` (cranelift-object emit ->
/// dlopen). Declares the vtable data symbol, the entry function, and
/// every closure-table lambda; lowers each body via the same
/// [`Codegen`] state machine; defines the cranelift IR into the
/// module. The caller drives the per-backend finalize step.
fn lower_module_into<M: CrModule>(
    module: &mut M,
    ir: &IrModule,
    entry: &relon_ir::ir::Func,
    entry_shape: EntryShape,
    sandbox: &SandboxConfig,
    return_root_size: u32,
    const_pool: &ConstPool,
) -> Result<LoweredArtifacts, CraneliftError> {
    let vtable_data_id = declare_vtable_data(module)?;

    // Pre-compute the three host-fn signatures the codegen indirects
    // through. The signatures match the slot ABI documented in
    // `crate::vtable::VtableSlot`.
    let raise_trap_sig = make_raise_trap_signature(module.target_config().pointer_type());
    let now_sig = make_now_signature(module.target_config().pointer_type());
    let cap_lookup_sig = make_cap_lookup_signature(module.target_config().pointer_type());
    let glob_match_sig = make_glob_match_signature(module.target_config().pointer_type());

    // Build the entry signature. The exact shape depends on
    // `entry_shape`: legacy IR carries `I64...` user args, while the
    // buffer-protocol IR carries the four wasm handshake i32 slots +
    // the i64 capabilities argument.
    let pointer_ty = module.target_config().pointer_type();
    let mut entry_sig = Signature::new(CallConv::SystemV);
    entry_sig.params.push(AbiParam::new(pointer_ty)); // state pointer
    match entry_shape {
        EntryShape::LegacyI64Args => {
            for _ in &entry.params {
                entry_sig.params.push(AbiParam::new(I64));
            }
            entry_sig.returns.push(AbiParam::new(I64));
        }
        EntryShape::BufferProtocol => {
            for p in &entry.params {
                let ty = match p {
                    IrType::I32 => I32,
                    IrType::I64 => I64,
                    other => {
                        return Err(CraneliftError::Codegen(format!(
                            "buffer-protocol entry param {other:?} unsupported"
                        )));
                    }
                };
                entry_sig.params.push(AbiParam::new(ty));
            }
            entry_sig.returns.push(AbiParam::new(I32));
        }
    }

    let entry_fn_id = module
        .declare_function("run_main", Linkage::Export, &entry_sig)
        .map_err(|e| CraneliftError::ModuleDefine(format!("declare run_main: {e}")))?;

    // Stage 5 Phase C.4: declare every lambda func referenced by the
    // module's `closure_table` *before* lowering the entry body so the
    // entry's `Op::MakeClosure` lowering can capture each lambda's
    // `FuncId` for the runtime closure-table population step. Each
    // lambda has the cranelift signature
    //   (state, captures_ptr: i32, params...) -> ret_ty
    // — the captures_ptr is prepended to the IR-declared param list
    // and points at the captures struct the call site materialised in
    // the scratch arena.
    let mut closure_func_ids: Vec<cranelift_module::FuncId> = Vec::new();
    let mut closure_signatures: Vec<Signature> = Vec::new();
    for (slot, &func_idx) in ir.closure_table.iter().enumerate() {
        let lambda = ir.funcs.get(func_idx as usize).ok_or_else(|| {
            CraneliftError::Codegen(format!(
                "closure_table[{slot}] -> funcs[{func_idx}] out of range (module has {} funcs)",
                ir.funcs.len()
            ))
        })?;
        // Lambda signature: `(state, ...lambda.params) -> ret`. The IR
        // lowering pass (`lower_closure_as_value`) already prepends the
        // `captures_ptr: i32` as `lambda.params[0]`, so the captures
        // pointer is the FIRST element of `lambda.params` — we must NOT
        // push a second synthetic captures param. Doing so widened the
        // signature by one slot (`(state, captures, captures, ...args)`),
        // shifting every user arg right by one: the `Op::CallClosure`
        // call site (which correctly passes `(state, captures, ...args)`)
        // then landed each user arg in the previous param's register and
        // left the real value slot uninitialised. For a filter predicate
        // `(v) => v < K` this meant `v` read garbage, so the predicate
        // returned an arbitrary constant for the whole run — selective
        // filters silently dropped everything (or kept everything). This
        // mirrors the LLVM backend's `(state, ...lambda.params)` shape.
        let mut sig = Signature::new(CallConv::SystemV);
        sig.params.push(AbiParam::new(pointer_ty)); // state pointer
        for p in &lambda.params {
            sig.params.push(AbiParam::new(ir_ty_to_cl(*p)?));
        }
        if !matches!(lambda.ret, IrType::Null) {
            sig.returns.push(AbiParam::new(ir_ty_to_cl(lambda.ret)?));
        }
        let name = format!("__closure_{slot}");
        let id = module
            .declare_function(&name, Linkage::Local, &sig)
            .map_err(|e| CraneliftError::ModuleDefine(format!("declare {name}: {e}")))?;
        closure_func_ids.push(id);
        closure_signatures.push(sig);
    }

    // Emit the function body.
    let mut ctx = CodegenContext::new();
    ctx.func = Function::with_name_signature(UserFuncName::user(0, 0), entry_sig);

    let mut builder_ctx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
        let entry_block = builder.create_block();
        builder.append_block_params_for_function_params(entry_block);
        builder.switch_to_block(entry_block);
        builder.seal_block(entry_block);

        // Pull the sandbox state pointer + Int args out of the entry
        // block parameters.
        let block_params: Vec<_> = builder.block_params(entry_block).to_vec();
        let state_ptr = block_params[0];
        let arg_values: Vec<CValue> = block_params[1..].to_vec();

        // v6-γ M2: optionally emit a HotCounter prologue. The helper
        // creates two new blocks (`hot_block` / `normal_block`),
        // branches between them, fills the hot path with a
        // `__relon_jump_to_recorder` call + sentinel return, and
        // leaves the builder positioned on `normal_block` so the rest
        // of the entry codegen flows unchanged.
        if let Some(fn_id) = sandbox.trace_jit_fn_id {
            emit_hot_counter_inject(&mut builder, pointer_ty, entry_shape, fn_id, &arg_values);
        }

        // v5-γ stage 2: import the capability vtable as a GlobalValue
        // on the current function. Every host-helper call indirects
        // through `load(vtable_base + slot_offset) -> fn_ptr` followed
        // by `call_indirect(sig, fn_ptr, args)` — see
        // `Codegen::emit_host_fn_call`.
        let vtable_gv = module.declare_data_in_func(vtable_data_id, builder.func);
        let raise_trap_sig_ref = builder.import_signature(raise_trap_sig.clone());
        let now_sig_ref = builder.import_signature(now_sig.clone());
        let cap_lookup_sig_ref = builder.import_signature(cap_lookup_sig.clone());
        let glob_match_sig_ref = builder.import_signature(glob_match_sig.clone());

        // Pre-allocate the trap block + a block param that carries
        // the i64 trap code. Every guard branches here with its
        // TrapKind code as an argument; cranelift handles phi nodes
        // automatically when the block has a parameter. We fill the
        // block's body at the very end (after the function body has
        // emitted all its guard branches) so the FunctionBuilder
        // never sees a half-filled block on a `switch_to_block`
        // call.
        let trap_block = builder.create_block();
        builder.append_block_param(trap_block, I64);

        let mut codegen = Codegen {
            builder: &mut builder,
            sandbox,
            state_ptr,
            vtable_gv,
            raise_trap_sig_ref,
            now_sig_ref,
            cap_lookup_sig_ref,
            glob_match_sig_ref,
            pointer_ty,
            frontend_config: module.target_config(),
            entry_shape,
            locals: HashMap::new(),
            let_locals: HashMap::new(),
            arg_values: &arg_values,
            stack: Vec::new(),
            ir,
            trap_block: Some(trap_block),
            label_stack: Vec::new(),
            inline_frames: Vec::new(),
            const_pool,
            record_locals: HashMap::new(),
            needs_tail_cursor: matches!(entry_shape, EntryShape::BufferProtocol)
                && body_needs_tail_cursor(&entry.body),
            return_root_size,
            mode: CodegenMode::Entry,
        };

        codegen.emit_prologue();
        codegen.emit_body(&entry.body)?;

        // Now fill the trap block body. Every guard branched in with
        // its `TrapKind as i64` as the block param; we call
        // `relon_raise_trap(state, code)` (via vtable indirection) and
        // return a sentinel zero of the entry's return type so the
        // host trampoline can detect the trap via `state.trap_code()`.
        builder.switch_to_block(trap_block);
        let code = builder.block_params(trap_block)[0];
        emit_indirect_host_call(
            &mut builder,
            vtable_gv,
            pointer_ty,
            VtableSlot::RelonRaiseTrap,
            raise_trap_sig_ref,
            &[state_ptr, code],
        );
        let zero = match entry_shape {
            EntryShape::LegacyI64Args => builder.ins().iconst(I64, 0),
            EntryShape::BufferProtocol => builder.ins().iconst(I32, 0),
        };
        builder.ins().return_(&[zero]);
        builder.seal_block(trap_block);

        // The lowering for `Return` already wired the `return`
        // instruction. If the body never emits a return, the cranelift
        // verifier will surface that as an error.

        builder.finalize();
    }

    module
        .define_function(entry_fn_id, &mut ctx)
        .map_err(|e| CraneliftError::ModuleDefine(format!("define run_main: {e}")))?;

    // Stage 5 Phase C.4: compile each lambda function. Each one uses
    // the cranelift signature `(state, captures_ptr, params...) -> ret`
    // — the captures_ptr is the first user-visible local (slot 0 in
    // the cranelift block-param sense, but the IR's `LocalGet` slots
    // start at 1 because the IR pass numbers user params from 1 onward
    // when a captures arg precedes them... actually the IR pass keeps
    // user params at `LocalGet 0..N`, so we need to shift the
    // cranelift slot map at the body entry to "skip" the captures
    // slot when resolving `LocalGet(idx)`).
    for (slot, (func_id, sig)) in closure_func_ids
        .iter()
        .copied()
        .zip(closure_signatures.iter())
        .enumerate()
    {
        let lambda_idx = ir.closure_table[slot] as usize;
        let lambda = &ir.funcs[lambda_idx];
        let mut lambda_ctx = CodegenContext::new();
        lambda_ctx.func =
            Function::with_name_signature(UserFuncName::user(0, (slot as u32) + 1), sig.clone());
        let mut lambda_builder_ctx = FunctionBuilderContext::new();
        {
            let mut builder = FunctionBuilder::new(&mut lambda_ctx.func, &mut lambda_builder_ctx);
            let entry_block = builder.create_block();
            builder.append_block_params_for_function_params(entry_block);
            builder.switch_to_block(entry_block);
            builder.seal_block(entry_block);

            // Block-param layout matches the signature
            // `(state, ...lambda.params)` where `lambda.params[0]` is the
            // captures pointer the IR prepended. So:
            //   * block_params[0] — state pointer
            //   * block_params[1] — captures_ptr (== lambda.params[0])
            //   * block_params[1..] — the IR-visible params, indexed by
            //     `LocalGet(idx)` (LocalGet(0) == captures_ptr,
            //     LocalGet(1) == first user arg, ...).
            let block_params: Vec<_> = builder.block_params(entry_block).to_vec();
            let lambda_state_ptr = block_params[0];
            let captures_ptr = block_params[1];
            let lambda_arg_values: Vec<CValue> = block_params[1..].to_vec();

            // v5-γ stage 2: import the capability vtable as a
            // GlobalValue on this lambda. Each helper call indirects
            // through `vtable_base + slot_offset` (see
            // `Codegen::emit_host_fn_call`).
            let vtable_gv = module.declare_data_in_func(vtable_data_id, builder.func);
            let raise_trap_sig_ref = builder.import_signature(raise_trap_sig.clone());
            let now_sig_ref = builder.import_signature(now_sig.clone());
            let cap_lookup_sig_ref = builder.import_signature(cap_lookup_sig.clone());
            let glob_match_sig_ref = builder.import_signature(glob_match_sig.clone());

            let trap_block = builder.create_block();
            builder.append_block_param(trap_block, I64);

            // Lambdas use the same entry shape as the entry function
            // for the purposes of `LocalGet` typing — but since each
            // lambda's params are IR-declared independently, we
            // override the entry-shape-derived local typing through
            // `lambda_param_tys`. The Codegen looks up `LocalGet(idx)`
            // against `arg_values` first; we've already routed the
            // captures_ptr to a dedicated slot so the IR-side
            // `LocalGet(idx)` resolves to `arg_values[idx]` which is
            // the user param at position `idx + 1` in the cranelift
            // block-params (we sliced past the captures_ptr).
            let mut codegen = Codegen {
                builder: &mut builder,
                sandbox,
                state_ptr: lambda_state_ptr,
                vtable_gv,
                raise_trap_sig_ref,
                now_sig_ref,
                cap_lookup_sig_ref,
                glob_match_sig_ref,
                pointer_ty,
                frontend_config: module.target_config(),
                // Lambdas use the LegacyI64Args entry shape for
                // `LocalGet` typing because their params are
                // IR-declared (i64 / i32 / ...) rather than the
                // buffer-handshake fixed shape. The `lambda_param_tys`
                // field carries the per-param typing so the
                // `LocalGet` resolution matches.
                entry_shape: EntryShape::LegacyI64Args,
                locals: HashMap::new(),
                let_locals: HashMap::new(),
                arg_values: &lambda_arg_values,
                stack: Vec::new(),
                ir,
                trap_block: Some(trap_block),
                label_stack: Vec::new(),
                inline_frames: Vec::new(),
                const_pool,
                record_locals: HashMap::new(),
                needs_tail_cursor: false,
                return_root_size: 0,
                mode: CodegenMode::Lambda {
                    captures_ptr,
                    lambda_param_tys: &lambda.params,
                },
            };

            codegen.emit_prologue();
            codegen.emit_body(&lambda.body)?;

            builder.switch_to_block(trap_block);
            let code = builder.block_params(trap_block)[0];
            emit_indirect_host_call(
                &mut builder,
                vtable_gv,
                pointer_ty,
                VtableSlot::RelonRaiseTrap,
                raise_trap_sig_ref,
                &[lambda_state_ptr, code],
            );
            // Lambdas always return a typed value (the IR-declared
            // ret_ty). On trap-block exit we emit a typed zero so the
            // verifier accepts the synthetic return.
            let zero_v = if matches!(lambda.ret, IrType::I64) {
                builder.ins().iconst(I64, 0)
            } else if matches!(lambda.ret, IrType::F64) {
                builder.ins().f64const(0.0)
            } else {
                builder.ins().iconst(I32, 0)
            };
            builder.ins().return_(&[zero_v]);
            builder.seal_block(trap_block);

            builder.finalize();
        }

        module
            .define_function(func_id, &mut lambda_ctx)
            .map_err(|e| CraneliftError::ModuleDefine(format!("define __closure_{slot}: {e}")))?;
    }

    Ok(LoweredArtifacts {
        entry_fn_id,
        vtable_data_id,
        closure_func_ids,
    })
}

/// Map a generic IR type to its cranelift slot type. Used by the
/// inline `Op::Call` lowering to size the exit block-param of an
/// inlined callee.
fn ir_ty_to_cl(ty: IrType) -> Result<cranelift_codegen::ir::Type, CraneliftError> {
    Ok(match ty {
        IrType::I64 => I64,
        IrType::F64 => cranelift_codegen::ir::types::F64,
        IrType::I32 | IrType::Bool | IrType::Null => I32,
        // Pointer-indirect leaves carry an i32 buffer-relative
        // offset in the IR's wasm-shaped slot model. Cranelift
        // mirrors that as a plain i32.
        IrType::String
        | IrType::ListInt
        | IrType::ListFloat
        | IrType::ListBool
        | IrType::ListString
        | IrType::ListSchema
        | IrType::Closure => I32,
    })
}

/// Map an IR `LoadField` / `StoreField` `ty` to the cranelift load
/// type, byte width, and stack tag.
///
/// Returns `(cranelift_load_type, byte_width, virtual_stack_ty)`.
/// `cranelift_load_type` is what cranelift's `load`/`store` opcode
/// width key cares about; `byte_width` is consumed by the bounds
/// check; `virtual_stack_ty` documents what the IR-side stack
/// expects after the load.
pub(super) fn field_load_shape(
    ty: IrType,
) -> Result<(cranelift_codegen::ir::Type, u32, IrType), CraneliftError> {
    match ty {
        IrType::I64 => Ok((I64, 8, IrType::I64)),
        IrType::F64 => Ok((cranelift_codegen::ir::types::F64, 8, IrType::F64)),
        IrType::I32 => Ok((I32, 4, IrType::I32)),
        IrType::Bool | IrType::Null => Ok((cranelift_codegen::ir::types::I8, 1, IrType::Bool)),
        // Pointer-indirect leaves: the fixed-area slot holds a single
        // i32 buffer-relative offset. Loads / stores against the slot
        // therefore use an `i32` access width — the IR-visible value
        // is treated as `IrType::I32` so subsequent ops (Add / memcpy
        // arithmetic / etc.) can manipulate it as a pointer.
        IrType::String
        | IrType::ListInt
        | IrType::ListFloat
        | IrType::ListBool
        | IrType::ListString
        | IrType::ListSchema
        | IrType::Closure => Ok((I32, 4, IrType::I32)),
    }
}

/// Walk the body to decide whether it allocates anything inside the
/// `out_buf` tail area (pointer-indirect StoreField, dict-construction
/// ops, `EmitTailRecordFromAbsoluteAddr`).
///
/// When `true`, the entry prologue must initialise `state.tail_cursor`
/// to `return_root_size` so the first tail allocation lands
/// immediately past the fixed area; the epilogue then returns the
/// post-bump cursor as `bytes_written`. When `false`, the cursor stays
/// at 0 and the epilogue returns `return_root_size` (the host
/// trampoline reads at least that many bytes either way, so the value
/// only matters when the body actually wrote past the fixed area).
fn body_needs_tail_cursor(body: &[TaggedOp]) -> bool {
    for tagged in body {
        match &tagged.op {
            Op::StoreField {
                ty:
                    IrType::String
                    | IrType::ListInt
                    | IrType::ListFloat
                    | IrType::ListBool
                    | IrType::ListString
                    | IrType::ListSchema,
                ..
            } => return true,
            Op::AllocRootRecord { .. }
            | Op::AllocSubRecord { .. }
            | Op::EmitTailRecordFromAbsoluteAddr { .. } => return true,
            Op::If {
                then_body,
                else_body,
                ..
            } if body_needs_tail_cursor(then_body) || body_needs_tail_cursor(else_body) => {
                return true;
            }
            Op::Block { body, .. } | Op::Loop { body, .. } if body_needs_tail_cursor(body) => {
                return true;
            }
            _ => {}
        }
    }
    false
}

/// Alignment + tag a pointer-indirect record needs when copied into
/// the tail area.
///
/// Mirrors `relon_codegen_wasm`'s record-size / alignment table:
///
/// * `String` / `ListBool` — 4-byte aligned `[len:4][bytes]`.
/// * `ListInt` / `ListFloat` — 8-byte aligned `[len:4][pad:4][i64/f64 ×n]`.
/// * `ListString` / `ListSchema` — pointer-array shapes that need
///   per-entry relocation. We refuse them on this path; codegen
///   surfaces `UnsupportedStoreFieldType` so the harness reports
///   `CraneliftUnsupported` rather than miscompiling.
pub(super) fn pointer_indirect_record_align(ty: IrType) -> Result<u32, CraneliftError> {
    match ty {
        IrType::String | IrType::ListBool => Ok(4),
        IrType::ListInt | IrType::ListFloat => Ok(8),
        IrType::ListString | IrType::ListSchema | IrType::Closure => Err(CraneliftError::Codegen(
            format!("pointer-indirect record alignment for {ty:?} not yet supported"),
        )),
        _ => Err(CraneliftError::Codegen(format!(
            "type {ty:?} is not pointer-indirect"
        ))),
    }
}

/// Per-function lowering state. Owns the cranelift builder and tracks
/// the running operand stack alongside variable bindings.
struct Codegen<'a, 'b> {
    builder: &'a mut FunctionBuilder<'b>,
    sandbox: &'a SandboxConfig,
    state_ptr: CValue,
    /// v5-γ stage 2: GlobalValue for the `__relon_capability_vtable`
    /// data symbol. Every host-helper call indirects through this
    /// base + a per-slot byte offset; see [`VtableSlot`].
    vtable_gv: GlobalValue,
    /// Pre-built cranelift signature for `relon_raise_trap`. Imported
    /// into the current function once during `compile_module_with`
    /// and reused for every `Op::RaiseTrap` lowering.
    ///
    /// Reserved for future op coverage. v5-beta-1 doesn't emit
    /// `raise_trap` directly — every guard uses cranelift's intrinsic
    /// `trap` / `trapnz`, which delivers the trap-code byte through
    /// the runtime's panic path — but holding the SigRef ready
    /// avoids a second pass for v5-beta-2 to wire in.
    #[allow(dead_code)]
    raise_trap_sig_ref: SigRef,
    /// Pre-built cranelift signature for `relon_now`.
    now_sig_ref: SigRef,
    /// Pre-built cranelift signature for `relon_cap_lookup`.
    cap_lookup_sig_ref: SigRef,
    /// Pre-built cranelift signature for `relon_glob_match_helper`
    /// (`extern "C" fn(state, s_off: i32, p_off: i32) -> i32`).
    /// Used by [`Self::emit_call_stdlib`] to route the bundled
    /// `glob_match` stdlib slot through the vtable rather than
    /// inlining the trap-sentinel IR body.
    glob_match_sig_ref: SigRef,
    pointer_ty: cranelift_codegen::ir::Type,
    /// Target frontend config (pointer width / default call conv).
    /// Threaded through so helpers that call `call_memcpy` get the
    /// right libcall signature without re-deriving it from primitives.
    frontend_config: cranelift_codegen::isa::TargetFrontendConfig,
    /// Calling-convention shape picked at compile time. Drives the
    /// `LocalGet` type (i32 vs i64), `Return` epilogue, and the
    /// buffer-protocol load / store address computation.
    entry_shape: EntryShape,
    /// `LocalGet` slot index -> cranelift `Variable`.
    locals: HashMap<u32, Variable>,
    /// `LetGet/LetSet` slot index -> cranelift `Variable`.
    let_locals: HashMap<u32, Variable>,
    arg_values: &'a [CValue],
    /// The IR's virtual operand stack, kept as live cranelift values
    /// so each `Add`/`Sub`/... pop maps to a typed `Value` directly.
    stack: Vec<CValue>,
    /// Reference back to the IR module so `Call` can look up the
    /// referenced function (in v5-beta-1 we inline stdlib bodies
    /// rather than emit per-callee cranelift functions).
    #[allow(dead_code)]
    ir: &'a IrModule,
    /// Pre-allocated "trap-and-return" block. Guards branch here
    /// when they fire; the block holds a single block param carrying
    /// the `TrapKind` code, calls `raise_trap`, and returns 0. The
    /// block is allocated unconditionally and may end up unreachable
    /// when `SandboxConfig` disables every guard, in which case
    /// cranelift's dead-block elimination removes it.
    trap_block: Option<cranelift_codegen::ir::Block>,

    /// v5-β-2 widen: label stack so `Op::Br { label_depth }` /
    /// `Op::BrIf` / `Op::BrTable` can resolve to the matching
    /// cranelift target block.
    ///
    /// Each entry carries the `(target_block, is_loop)` pair where
    /// `target_block` is:
    ///   * for `Op::Block { ... }`: the **exit** block (forward
    ///     branch — `Br N` jumps past the matching End).
    ///   * for `Op::Loop { ... }`: the **header** block (back
    ///     branch — `Br N` re-enters the loop, equivalent to
    ///     `continue`).
    ///
    /// `label_depth = 0` selects the innermost (top of stack)
    /// label; higher depths walk outwards.
    label_stack: Vec<LabelFrame>,

    /// Inline-frame stack for stdlib `Op::Call` lowering. When we
    /// inline a callee body, we push a frame here so the callee's
    /// `LocalGet(idx)` / `LetGet/LetSet` / `Op::Return` resolve
    /// against the call site rather than the entry function. See
    /// [`InlineFrame`] for fields.
    inline_frames: Vec<InlineFrame>,
    /// Pre-computed offset table for const-data records the entry
    /// references through `Op::ConstString` / `Op::ConstList*`.
    /// Cranelift emits `iconst(I32, offset)` for each reference; the
    /// const-data bytes live in the host arena's prefix (the host
    /// trampoline copies them in before each call).
    const_pool: &'a ConstPool,

    /// Cranelift `Variable` per `record_local_idx` allocated by
    /// `Op::AllocRootRecord` / `Op::AllocSubRecord`. Each variable
    /// holds an `i32` out_ptr-relative offset; subsequent
    /// `Op::StoreFieldAtRecord` / `Op::PushRecordBase` ops read it to
    /// compute the in-construction record's destination address.
    record_locals: HashMap<u32, Variable>,
    /// `true` when the entry's body touches the tail-cursor (either
    /// emits pointer-indirect StoreField or uses the
    /// AllocSubRecord / EmitTailRecordFromAbsoluteAddr dict-construction
    /// ops). Drives the prologue init (`tail_cursor = return_root_size`)
    /// and the epilogue return shape (`bytes_written = tail_cursor`
    /// vs constant `return_root_size`).
    needs_tail_cursor: bool,
    /// Pre-computed fixed-area size of the entry's return record.
    /// When `needs_tail_cursor` is `false` and the entry is buffer-
    /// protocol, the epilogue returns this as `bytes_written`. The
    /// prologue uses the same value to bias `tail_cursor` to the
    /// first byte past the fixed area when tail records are present.
    return_root_size: u32,
    /// Stage 5 Phase C.4: when this Codegen is lowering a *lambda*
    /// body (not the entry function), `captures_ptr` carries the
    /// Entry vs lambda mode. Encodes the two cases that previously
    /// lived as two separate `Option` fields (`captures_ptr` /
    /// `lambda_param_tys`) — both were always `Some` together
    /// (lambda) or both `None` (entry), making them an implicit
    /// 2-state enum. See [`CodegenMode`] for the union shape.
    mode: CodegenMode<'a>,
}

/// Lowering mode for the `Codegen` driver. Two cases:
///
/// * [`CodegenMode::Entry`] — top-level entry fn. `LocalGet(idx)`
///   reads from `arg_values` using `entry_shape` to derive types.
/// * [`CodegenMode::Lambda`] — closure body. `LocalGet(idx)` reads
///   from `arg_values` using the lambda's declared param types; an
///   extra `captures_ptr` block-param feeds `LoadField`-via-captures
///   lookups (`captures_ptr + offset`).
///
/// The `Inline` shape (stdlib body lowered through `Op::Call`) stays
/// orthogonal — it sits in `inline_frames` and overlays the active
/// `CodegenMode` for the duration of the inlined body.
enum CodegenMode<'a> {
    Entry,
    Lambda {
        captures_ptr: CValue,
        lambda_param_tys: &'a [IrType],
    },
}

impl<'a> CodegenMode<'a> {
    fn captures_ptr(&self) -> Option<CValue> {
        match self {
            CodegenMode::Entry => None,
            CodegenMode::Lambda { captures_ptr, .. } => Some(*captures_ptr),
        }
    }

    fn lambda_param_tys(&self) -> Option<&'a [IrType]> {
        match self {
            CodegenMode::Entry => None,
            CodegenMode::Lambda {
                lambda_param_tys, ..
            } => Some(lambda_param_tys),
        }
    }
}

/// One inline-frame entry for a stdlib body lowered through
/// `Op::Call`. See `Codegen::inline_frames` for usage.
struct InlineFrame {
    /// Cranelift values bound to the callee's declared parameters.
    /// `LocalGet(idx)` reads from this slice while the frame is
    /// active.
    params: Vec<CValue>,
    /// Block the callee's `Op::Return` jumps to. The exit block has
    /// one block-param carrying the typed return value.
    exit_block: cranelift_codegen::ir::Block,
    /// Result type of the callee. Informational today (block-param
    /// already carries the cranelift type); kept for the future
    /// trace-recorder hook that wants the IR-side tag for guard
    /// emission.
    #[allow(dead_code)]
    ret_ty: IrType,
    /// Caller's `let_locals` size at the moment the inline frame
    /// was pushed. The callee's `LetSet { idx }` rewrites to
    /// `let_offset + idx`, keeping each inlined frame's let
    /// bindings in a private namespace.
    let_offset: u32,
}

/// One label frame for the `Op::Br` / `Op::BrIf` / `Op::BrTable`
/// target resolution.
struct LabelFrame {
    /// The cranelift block this label resolves to (loop header for
    /// `Op::Loop`, exit block for `Op::Block`).
    target_block: cranelift_codegen::ir::Block,
    /// `true` for `Op::Loop` (back-edge); `false` for `Op::Block`
    /// (forward-edge). Used by [`Codegen::emit_loop_back_resource_check`]
    /// to recognise loop back-edges as the right site for inserting
    /// the [`crate::sandbox::RESOURCE_CHECK_INTERVAL`] cadence guard.
    is_loop: bool,
    /// When the labelled construct yields a typed value (`Op::Loop`
    /// or `Op::Block` with `result_ty = Some(_)`), this slot holds
    /// the cranelift type the matching block-param accepts. `Br` /
    /// `BrIf` / `BrTable` targeting this frame pops one operand from
    /// the virtual stack and forwards it as the block-param.
    ///
    /// For `Op::Loop` with a yield, the block-param sits on the loop
    /// header and represents the loop-carried accumulator (each back-
    /// edge supplies the next iteration's value); the loop exits by
    /// falling through to the continuation block which inherits the
    /// final value.
    ///
    /// For `Op::Block` with a yield, the block-param sits on the
    /// continuation block. `Br N` inside the body pops the yield
    /// value and forwards it as the continuation arg.
    result_cl_ty: Option<cranelift_codegen::ir::Type>,
    /// When the frame is a `Op::Loop` with `result_ty != None`, this
    /// is the continuation block that receives the loop's final
    /// value via fallthrough. `None` for blocks / yield-less loops.
    loop_cont_block: Option<cranelift_codegen::ir::Block>,
    /// Per-loop back-edge counter variable used to space the
    /// resource-deadline guard at [`crate::sandbox::RESOURCE_CHECK_INTERVAL`]
    /// cadence inside long-running loops. `None` for blocks (which
    /// have no back-edge) and when the sandbox's deadline check is
    /// disabled. The counter is an `I64` increment-and-mask Variable;
    /// `emit_loop_back_resource_check` reads / updates it on every
    /// back-edge.
    back_edge_counter: Option<Variable>,
}

impl<'a, 'b> Codegen<'a, 'b> {
    /// v5-γ stage 2: emit an indirect call to the host helper at
    /// `slot`. Loads the function pointer from
    /// `__relon_capability_vtable[slot]` and `call_indirect`s with the
    /// matching pre-built signature.
    fn emit_host_fn_call(&mut self, slot: VtableSlot, args: &[CValue]) -> Inst {
        let sig_ref = match slot {
            VtableSlot::RelonNow => self.now_sig_ref,
            VtableSlot::RelonRaiseTrap => self.raise_trap_sig_ref,
            VtableSlot::RelonCapLookup => self.cap_lookup_sig_ref,
            VtableSlot::RelonGlobMatch => self.glob_match_sig_ref,
        };
        emit_indirect_host_call(
            self.builder,
            self.vtable_gv,
            self.pointer_ty,
            slot,
            sig_ref,
            args,
        )
    }

    /// Emit the entry prologue: resource-limit check (one wall-clock
    /// read + comparison) plus any other one-shot setup. For buffer-
    /// protocol entries whose body emits pointer-indirect stores or
    /// dict-construction ops, also initialise `state.tail_cursor` to
    /// `return_root_size` so the first tail allocation lands
    /// immediately past the fixed area.
    fn emit_prologue(&mut self) {
        if self.sandbox.deadline_check {
            self.emit_resource_check();
        }
        if self.needs_tail_cursor {
            let init = self
                .builder
                .ins()
                .iconst(I32, i64::from(self.return_root_size));
            self.builder.ins().store(
                MemFlags::trusted(),
                init,
                self.state_ptr,
                STATE_OFFSET_TAIL_CURSOR,
            );
        }
    }

    /// Conditional trap: when `cond` is non-zero, jump to the trap
    /// block with the supplied `TrapKind` code as the block param.
    /// Replaces the cranelift intrinsic `trapnz`-via-`ud2` path
    /// that produced SIGILL on x86 Linux, which `catch_unwind`
    /// cannot intercept on stable Rust.
    fn cond_trap(&mut self, cond: CValue, kind: TrapKind) {
        let trap_block = self
            .trap_block
            .expect("trap_block must be pre-allocated by compile_module");
        let continue_block = self.builder.create_block();
        let code_val = self.builder.ins().iconst(I64, i64::from(kind as u8));
        self.builder
            .ins()
            .brif(cond, trap_block, &[code_val.into()], continue_block, &[]);
        self.builder.seal_block(continue_block);
        self.builder.switch_to_block(continue_block);
    }

    /// Insert a deadline guard at the current insertion point. Reads
    /// `state.epoch.elapsed().as_nanos()` via the host helper and
    /// traps when the result is past `state.deadline_ns`.
    ///
    /// The vDSO clock-gettime cost is elided host-side: `now_helper`
    /// fast-returns 0 when `deadline_ns == i64::MAX` (the "no
    /// deadline" sentinel). This keeps the cranelift IR shape as a
    /// straight-line (call + load + cond_trap) so the surrounding
    /// `Op::Loop` body retains its tight optimisation; an IR-level
    /// sentinel-skip (brif-guarded) was tried twice and rejected
    /// (see commit log on f5857d7 and the 0.132 retry attempt) — the
    /// extra basic blocks defeat a cranelift loop-opt heuristic
    /// (still present in 0.132).
    fn emit_resource_check(&mut self) {
        // call relon_now(state) -> i64 via the capability vtable.
        let inst = self.emit_host_fn_call(VtableSlot::RelonNow, &[self.state_ptr]);
        let elapsed = self.builder.inst_results(inst)[0];

        // Load deadline_ns from state. The offset lives in
        // `STATE_OFFSET_DEADLINE_NS`; the codegen and sandbox must
        // agree on it.
        let deadline = self.builder.ins().load(
            I64,
            MemFlags::trusted(),
            self.state_ptr,
            crate::sandbox::STATE_OFFSET_DEADLINE_NS,
        );

        // Trap when elapsed >= deadline.
        let cmp = self
            .builder
            .ins()
            .icmp(IntCC::SignedGreaterThanOrEqual, elapsed, deadline);
        self.cond_trap(cmp, TrapKind::ResourceExhausted);
    }

    /// Materialise a cranelift `Variable` for a `LocalGet` slot the
    /// IR references. Slot 0 corresponds to `arg_values[0]`, slot 1
    /// to `arg_values[1]`, and so on. The variable's type tracks the
    /// entry's calling convention:
    ///
    /// * `LegacyI64Args` — every local is `i64`.
    /// * `BufferProtocol` — locals 0..=3 are `i32` (the handshake
    ///   slots `in_ptr`, `in_len`, `out_ptr`, `out_cap`), local 4 is
    ///   `i64` (`caps_arg`).
    ///
    /// When an inline frame is active (we're lowering the body of a
    /// stdlib callee inlined through `Op::Call`), `LocalGet(idx)`
    /// resolves to the matching slot of the topmost inline frame
    /// instead of the entry's locals — preserving the wasm semantics
    /// where the callee sees its own `params` as locals `0..N`.
    fn get_local(&mut self, idx: u32) -> Result<CValue, CraneliftError> {
        if let Some(frame) = self.inline_frames.last() {
            let arg_idx = idx as usize;
            if arg_idx >= frame.params.len() {
                return Err(CraneliftError::Codegen(format!(
                    "LocalGet({idx}) out of range — inlined frame has {} params",
                    frame.params.len()
                )));
            }
            return Ok(frame.params[arg_idx]);
        }
        if let Some(var) = self.locals.get(&idx).copied() {
            return Ok(self.builder.use_var(var));
        }
        let arg_idx = idx as usize;
        if arg_idx >= self.arg_values.len() {
            return Err(CraneliftError::Codegen(format!(
                "LocalGet({idx}) out of range — entry has {} args",
                self.arg_values.len()
            )));
        }
        let cr_ty = if let Some(param_tys) = self.mode.lambda_param_tys() {
            // Lambda mode: types come from the IR-declared param list.
            let ir_ty = param_tys.get(arg_idx).copied().ok_or_else(|| {
                CraneliftError::Codegen(format!(
                    "LocalGet({idx}) out of range — lambda has {} declared params",
                    param_tys.len()
                ))
            })?;
            ir_ty_to_cl(ir_ty)?
        } else {
            match self.entry_shape {
                EntryShape::LegacyI64Args => I64,
                EntryShape::BufferProtocol => match idx {
                    0..=3 => I32,
                    4 => I64,
                    _ => {
                        return Err(CraneliftError::Codegen(format!(
                            "LocalGet({idx}) out of range for buffer-protocol entry (5 locals)"
                        )));
                    }
                },
            }
        };
        // Mirror the arg value into a Variable so future LocalSet
        // (if we ever support it) writes go through SSA cleanly.
        let var = self.builder.declare_var(cr_ty);
        self.builder.def_var(var, self.arg_values[arg_idx]);
        self.locals.insert(idx, var);
        Ok(self.builder.use_var(var))
    }

    /// Translate a callee `LetGet/LetSet` index into the caller's
    /// flat let-locals namespace. Each inline frame reserves a
    /// fresh window `let_offset..` so concurrent inlined frames
    /// don't clobber each other's bindings.
    fn remap_let_idx(&self, idx: u32) -> u32 {
        match self.inline_frames.last() {
            Some(frame) => frame.let_offset + idx,
            None => idx,
        }
    }

    /// Resolve / create a `let`-binding slot.
    fn get_let(&mut self, idx: u32, ty: IrType) -> Result<CValue, CraneliftError> {
        let var = match self.let_locals.get(&idx).copied() {
            Some(v) => v,
            None => {
                return Err(CraneliftError::Codegen(format!(
                    "LetGet({idx}) read before LetSet"
                )))
            }
        };
        let _ = ty; // typing handled when the Variable was declared
        Ok(self.builder.use_var(var))
    }

    fn set_let(&mut self, idx: u32, ty: IrType, value: CValue) {
        // The let-slot's declared cranelift type comes from the
        // IR-declared slot `ty` (string/list/closure are i32 arena
        // offsets, not i64 pointers). Falling back to I64 here
        // previously panicked `FunctionBuilder::def_var` when a
        // `LetSet` carrying a String slot landed in the AOT path
        // (W4 pipeline).
        let cr_ty = ir_ty_to_cl(ty).unwrap_or(I64);
        let var = if let Some(v) = self.let_locals.get(&idx).copied() {
            v
        } else {
            let v = self.builder.declare_var(cr_ty);
            self.let_locals.insert(idx, v);
            v
        };
        // Coerce the operand-stack value to the slot's declared width
        // before `def_var`. The IR can hand an `I64`-typed value to a
        // slot the lowering pass declared `I32` (e.g. the W16 list-
        // materialise path: `If { result_ty: I64 }` computes the
        // `range(n)` element count, then `LetSet { ty: I32 }` stores it
        // into the i32 length slot). Without this coercion cranelift's
        // SSA frontend panics with `declared type of variable varN
        // doesn't match type of value vM`. This mirrors the LLVM AOT
        // emitter's `coerce_to_let_ty` (zero-extend when narrower,
        // truncate when wider) so the two backends agree on the stored
        // width. Float slots only ever receive `F64` values, so the
        // integer narrow/widen path is unreachable for them.
        let stored = self.coerce_to_cl_ty(value, cr_ty);
        self.builder.def_var(var, stored);
    }

    /// Coerce an operand-stack value to a target cranelift type so it
    /// can flow into a `def_var` / typed slot whose declared width may
    /// differ from the value's. Only integer narrow/widen is performed
    /// (`ireduce` / `uextend`, matching the LLVM backend's
    /// zero-extend-on-widen semantics); any other shape (already
    /// matching, or a non-integer mismatch we don't model) is returned
    /// unchanged so a genuine type error still surfaces as the
    /// frontend's `def_var` panic rather than being silently masked.
    fn coerce_to_cl_ty(&mut self, value: CValue, target: cranelift_codegen::ir::Type) -> CValue {
        let actual = self.builder.func.dfg.value_type(value);
        if actual == target {
            return value;
        }
        // Only reconcile integer-width mismatches; leave float / vector
        // / reference shapes to the frontend's own type check.
        if actual.is_int() && target.is_int() {
            if target.bits() < actual.bits() {
                return self.builder.ins().ireduce(target, value);
            }
            if target.bits() > actual.bits() {
                return self.builder.ins().uextend(target, value);
            }
        }
        value
    }

    fn push(&mut self, v: CValue) {
        self.stack.push(v);
    }

    fn pop(&mut self) -> Result<CValue, CraneliftError> {
        self.stack
            .pop()
            .ok_or_else(|| CraneliftError::Codegen("stack underflow".into()))
    }

    fn emit_body(&mut self, body: &[TaggedOp]) -> Result<(), CraneliftError> {
        for tagged in body {
            self.emit_op(&tagged.op)?;
        }
        Ok(())
    }

    /// Per-op driver. The cranelift backend previously maintained a
    /// hand-rolled 78-arm `match op { ... }` here; that pattern drifted
    /// silently when new [`Op`] variants landed in the bytecode + wasm
    /// backends. Switching to [`relon_ir::walk_op`] dispatched against
    /// the [`op_visitor`] impl on [`Codegen`] gives the cranelift
    /// surface the same compile-time exhaustiveness guarantee — adding
    /// an [`Op`] variant now fails this crate's build until a matching
    /// `visit_*` method lands.
    fn emit_op(&mut self, op: &Op) -> Result<(), CraneliftError> {
        relon_ir::walk_op(op, self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_ir::ir::{Func, Module as IrModule, Op, TaggedOp};
    use relon_parser::TokenRange;

    /// Helper: synthesise a minimal IR module that returns
    /// `arg0 + arg1` (both `Int`).
    fn synth_add_module() -> IrModule {
        let body = vec![
            TaggedOp {
                op: Op::LocalGet(0),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::LocalGet(1),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::Add(IrType::I64),
                range: TokenRange::default(),
            },
            TaggedOp {
                op: Op::Return,
                range: TokenRange::default(),
            },
        ];
        let func = Func {
            name: "run_main".to_string(),
            params: vec![IrType::I64, IrType::I64],
            ret: IrType::I64,
            body,
            range: TokenRange::default(),
        };
        IrModule {
            imports: vec![],
            funcs: vec![func],
            entry_func_index: Some(0),
            closure_table: vec![],
        }
    }

    #[test]
    fn compile_module_rejects_non_i64_param() {
        let mut ir = synth_add_module();
        ir.funcs[0].params[0] = IrType::Bool;
        let cfg = SandboxConfig::default();
        let result = compile_module(&ir, &cfg);
        assert!(matches!(
            result,
            Err(CraneliftError::UnsupportedSignature(_))
        ));
    }

    #[test]
    fn compile_module_rejects_non_i64_return() {
        let mut ir = synth_add_module();
        ir.funcs[0].ret = IrType::Bool;
        let cfg = SandboxConfig::default();
        let result = compile_module(&ir, &cfg);
        assert!(matches!(
            result,
            Err(CraneliftError::UnsupportedSignature(_))
        ));
    }

    #[test]
    fn compile_module_emits_runnable_entry_for_add() {
        let ir = synth_add_module();
        let cfg = SandboxConfig::unchecked();
        let result = compile_module(&ir, &cfg);
        assert!(result.is_ok(), "compile failed: {:?}", result.err());
    }
}
