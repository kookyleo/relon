//! v6-ε-0-A — at-call-site trace IR inline pipeline (host side).
//!
//! `relon-trace-emitter::inline_emit` provides the cranelift-IR-level
//! splice primitive. This module wires it through the
//! `cranelift_jit::JITModule` pipeline so a host fn whose body is
//! "the inlined trace body, no inner call" can be built and called
//! the same way the trampoline entry from
//! [`crate::trace_install::JITedTraceFn`] is.
//!
//! ## API surface
//!
//! - [`compile_inline_host_fn`] — compiles a stand-alone JIT module
//!   whose single exported function has the supplied
//!   [`relon_trace_jit::OptimizedTrace`] inlined as the function
//!   body. Returns an opaque [`InlineHostFn`] retainer that owns the
//!   module + exposes the typed entry pointer.
//! - [`InlineHostFn`] — typed-entry holder. Shares
//!   [`crate::trace_install::TraceEntryFn`]'s wire shape
//!   (`(*mut TraceContext, *const u64) -> i32`) so the same caller
//!   code can drive both the trampoline path and the inline path.
//!
//! ## Honest framing (per stage report)
//!
//! On its own this primitive does **not** reduce
//! `trace_jit_warm_inline` below `trace_jit_warm_ic` — the bench
//! still pays one Rust → JIT call boundary per iter. The win shows
//! up when a *cranelift-AOT-compiled* outer fn embeds the inline
//! path: the inner `call trace_fn_ptr` disappears and the trace
//! body becomes part of the outer fn's regalloc + scheduling. The
//! infrastructure here is the prerequisite for that real-world
//! integration.
//!
//! ## Code-bloat constraint
//!
//! [`compile_inline_host_fn`] consults
//! [`relon_trace_emitter::should_inline_trace`] before emitting. A
//! trace above the cap surfaces
//! [`InlineHostFnError::TraceTooLarge`]; the caller is expected to
//! fall back to the trampoline-call path
//! ([`crate::trace_install::TraceJitState::jit_compile_buffer_for_fn`]).

use std::sync::Arc;

use cranelift_codegen::ir::types::{I32, I64};
use cranelift_codegen::ir::{self, AbiParam, BlockArg, InstBuilder, MemFlags, Signature};
use cranelift_codegen::isa::CallConv;
use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context as CodegenContext;
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module as _};

use relon_trace_abi::TraceEntryStatus;
use relon_trace_emitter::{
    emit_trace_inline, host_hook_slot_offset, host_hooks_offset, should_inline_trace, HostHookId,
    InlineEmitError, InlineEmitHandles, MAX_INLINE_OPS,
};
use relon_trace_jit::OptimizedTrace;

use crate::trace_install::{register_trace_runtime_symbols, TraceEntryFn};

/// Errors returned by [`compile_inline_host_fn`]. Mirrors
/// [`crate::trace_install::TraceJitError`] but with a dedicated
/// `TraceTooLarge` variant so callers can distinguish "fall back to
/// trampoline" from "the cranelift module pipeline blew up".
#[derive(Debug, thiserror::Error)]
pub enum InlineHostFnError {
    /// Trace is above [`relon_trace_emitter::MAX_INLINE_OPS`]. Caller
    /// should fall back to
    /// [`crate::trace_install::TraceJitState::jit_compile_buffer_for_fn`]
    /// (the regular trampoline-call path).
    #[error("trace too large to inline: {op_count} ops, cap {cap}")]
    TraceTooLarge { op_count: usize, cap: usize },
    /// Inline emit rejected the trace (e.g. `TraceOp::Call` in body).
    #[error("inline emit failed: {0}")]
    InlineEmit(String),
    /// Cranelift module step failed (declare / define / finalize).
    #[error("cranelift module error: {0}")]
    Module(String),
}

impl From<InlineEmitError> for InlineHostFnError {
    fn from(e: InlineEmitError) -> Self {
        match e {
            InlineEmitError::TraceTooLarge { op_count, cap } => {
                InlineHostFnError::TraceTooLarge { op_count, cap }
            }
            other => InlineHostFnError::InlineEmit(other.to_string()),
        }
    }
}

/// Owned JIT module whose exported entry function is the inlined
/// version of the supplied [`OptimizedTrace`].
///
/// Drop semantics: dropping the `InlineHostFn` unmaps the JIT module
/// and invalidates the entry pointer. The bench / test holds the
/// `InlineHostFn` for the duration of the call loop.
pub struct InlineHostFn {
    entry: TraceEntryFn,
    raw_fn_ptr: *const u8,
    inline_trace: Arc<OptimizedTrace>,
    /// Owning module — retained so the entry pointer stays callable.
    /// Drop order is intentional: `_module` after `entry` per the
    /// rust drop-glue rules (last declared = first dropped) so
    /// callers can rely on the module outliving any in-flight entry
    /// dispatch.
    _module: JITModule,
}

// SAFETY: same contract as `JITedTraceFn` — the entry pointer is
// owned by `_module`; sharing across threads is safe as long as
// callers respect TRACE_ENTRY_SIG (single exclusive TraceContext).
unsafe impl Send for InlineHostFn {}
unsafe impl Sync for InlineHostFn {}

impl InlineHostFn {
    /// Typed entry pointer suitable for direct `call rax` dispatch.
    ///
    /// # Safety
    ///
    /// The returned pointer is bound to the lifetime of `self`.
    /// Callers must keep the `InlineHostFn` alive for the duration of
    /// any invocation.
    pub unsafe fn typed_entry(&self) -> TraceEntryFn {
        self.entry
    }

    /// Raw entry pointer (cast to `*const u8`). Use [`Self::typed_entry`]
    /// for direct invocation; this accessor is reserved for IC slot
    /// tooling that wants the address as an opaque value.
    pub fn raw_fn_ptr(&self) -> *const u8 {
        self.raw_fn_ptr
    }

    /// Shared handle on the trace IR that was inlined into the entry.
    /// Mirrors [`crate::trace_install::JITedTraceFn::inline_trace`].
    pub fn inline_trace(&self) -> Arc<OptimizedTrace> {
        Arc::clone(&self.inline_trace)
    }

    /// Convenience wrapper that builds a fresh [`relon_trace_abi::TraceContext`]
    /// sized for the trace, calls the inline entry, and returns the
    /// `(status, result_slot)` pair. Mainly used by tests and bench
    /// warm-up paths that don't want to manage a `TraceContext`
    /// directly.
    ///
    /// # Safety
    ///
    /// `args` must point to a `u64[]` sized for the trace's
    /// `LocalGet` accesses, or be null when the trace has no
    /// `LocalGet` ops.
    pub unsafe fn invoke_owned(
        &self,
        args: *const u64,
        slot_count: usize,
    ) -> (TraceEntryStatus, u64) {
        let mut ctx = relon_trace_abi::TraceContext::with_capacity(slot_count);
        let raw = unsafe { (self.entry)(&mut ctx as *mut _, args) };
        let status = match raw {
            0 => TraceEntryStatus::Success,
            1 => TraceEntryStatus::GuardFailed,
            _ => TraceEntryStatus::Aborted,
        };
        (status, ctx.result_slot)
    }
}

/// Compile a JIT module whose single exported function is the
/// supplied `OptimizedTrace` inlined inside a host fn body.
///
/// The resulting function obeys [`relon_trace_abi::TRACE_ENTRY_SIG`]
/// — `(*mut TraceContext, *const u64) -> i32` — so callers can drop
/// it into the same dispatch slots a real trace fn would occupy.
///
/// ## Control flow
///
/// The compiled function has three blocks:
/// - `entry` — picks up the two ABI params, jumps into the inlined
///   trace body.
/// - `post` — joined by the inlined `TraceOp::Return`; stores the
///   result into `ctx.result_slot` (redundant: the inline emitter
///   already wrote it for compat, but the host fn replays the
///   canonical store so passes that inspect the host fn IR keep
///   working) and returns `TraceEntryStatus::Success.as_i32()`.
/// - `deopt` — joined by every guard fire. Calls
///   `ctx.host_hooks.save_deopt` via `call_indirect` (mirroring the
///   trampoline emitter's deopt block layout) and returns
///   `TraceEntryStatus::GuardFailed.as_i32()`.
///
/// ## Limits
///
/// Returns [`InlineHostFnError::TraceTooLarge`] for traces above
/// [`MAX_INLINE_OPS`]; callers must fall back to the trampoline
/// install path.
pub fn compile_inline_host_fn(
    trace: Arc<OptimizedTrace>,
) -> Result<InlineHostFn, InlineHostFnError> {
    if !should_inline_trace(&trace) {
        return Err(InlineHostFnError::TraceTooLarge {
            op_count: trace.op_count(),
            cap: MAX_INLINE_OPS,
        });
    }

    let mut module = build_inline_host_jit_module()?;
    let pointer_ty = module.target_config().pointer_type();

    // Pre-declare the save_deopt helper as Linkage::Import so the
    // host fn can fall back to a direct call when the host hook
    // table slot is null (matches the standalone emitter's deopt
    // block layout).
    let mut save_deopt_sig = Signature::new(CallConv::SystemV);
    save_deopt_sig.params.push(AbiParam::new(pointer_ty));
    save_deopt_sig.params.push(AbiParam::new(I32));
    save_deopt_sig.params.push(AbiParam::new(I64));
    let save_deopt_id = module
        .declare_function(
            HostHookId::SaveDeopt.symbol(),
            Linkage::Import,
            &save_deopt_sig,
        )
        .map_err(|e| InlineHostFnError::Module(format!("declare save_deopt: {e}")))?;

    // Host fn body. Signature mirrors TRACE_ENTRY_SIG so call-site
    // dispatch is uniform between the trampoline and inline paths.
    let mut sig = Signature::new(CallConv::SystemV);
    sig.params.push(AbiParam::new(pointer_ty));
    sig.params.push(AbiParam::new(pointer_ty));
    sig.returns.push(AbiParam::new(I32));

    let mut ctx = CodegenContext::new();
    ctx.func = ir::Function::with_name_signature(
        ir::UserFuncName::user(0, save_deopt_id.as_u32() + 1),
        sig.clone(),
    );

    // Import save_deopt as a FuncRef inside the host fn.
    let save_deopt_sig_ref = ctx.func.import_signature(save_deopt_sig.clone());
    let save_deopt_name = ctx
        .func
        .declare_imported_user_function(ir::UserExternalName::new(0, save_deopt_id.as_u32()));
    let save_deopt_ref = ctx.func.import_function(ir::ExtFuncData {
        name: ir::ExternalName::User(save_deopt_name),
        signature: save_deopt_sig_ref,
        colocated: false,
        patchable: false,
    });

    let mut builder_ctx = FunctionBuilderContext::new();
    let mut builder = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);

    let entry_block = builder.create_block();
    builder.append_block_params_for_function_params(entry_block);
    builder.switch_to_block(entry_block);
    builder.seal_block(entry_block);
    let trace_ctx_ptr = builder.block_params(entry_block)[0];
    let input_args_ptr = builder.block_params(entry_block)[1];

    // post_block: success return path. Block param = the i64 result
    // from the inlined Return op.
    let post_block = builder.create_block();
    builder.append_block_param(post_block, I64);
    // deopt_block: guard-fire path. Block params = (guard_pc: i32,
    // external_pc: i64).
    let deopt_block = builder.create_block();
    builder.append_block_param(deopt_block, I32);
    builder.append_block_param(deopt_block, I64);

    // Inline the trace body straight into the host fn entry block.
    emit_trace_inline(
        &mut builder,
        &trace,
        pointer_ty,
        InlineEmitHandles {
            trace_ctx_ptr,
            input_args_ptr,
            post_block,
            deopt_block,
        },
    )?;

    // Fill post_block: return Success status. result is already
    // stored to ctx.result_slot by the inline emit; the post-block
    // arg carries the i64 for consumers that read the value
    // directly from the cranelift Value (regalloc keeps it in a
    // register where possible, eliding the result_slot load).
    builder.switch_to_block(post_block);
    builder.seal_block(post_block);
    let _result = builder.block_params(post_block)[0];
    let success = builder
        .ins()
        .iconst(I32, i64::from(TraceEntryStatus::Success.as_i32()));
    builder.ins().return_(&[success]);

    // Fill deopt_block: dispatch through ctx.host_hooks.save_deopt
    // (mirrors the standalone emitter's deopt block).
    builder.switch_to_block(deopt_block);
    builder.seal_block(deopt_block);
    let guard_pc = builder.block_params(deopt_block)[0];
    let external_pc = builder.block_params(deopt_block)[1];

    let hook_off = host_hooks_offset() + host_hook_slot_offset(HostHookId::SaveDeopt);
    let hook_ptr = builder
        .ins()
        .load(pointer_ty, MemFlags::trusted(), trace_ctx_ptr, hook_off);
    let null = builder.ins().iconst(pointer_ty, 0);
    let has_hook = builder
        .ins()
        .icmp(ir::condcodes::IntCC::NotEqual, hook_ptr, null);
    let indirect_block = builder.create_block();
    let direct_block = builder.create_block();
    let empty: [BlockArg; 0] = [];
    builder.ins().brif(
        has_hook,
        indirect_block,
        empty.iter(),
        direct_block,
        empty.iter(),
    );
    builder.seal_block(indirect_block);
    builder.seal_block(direct_block);

    // Indirect arm: call_indirect through the hook slot.
    builder.switch_to_block(indirect_block);
    let indirect_sig = builder.func.import_signature(save_deopt_sig.clone());
    builder.ins().call_indirect(
        indirect_sig,
        hook_ptr,
        &[trace_ctx_ptr, guard_pc, external_pc],
    );
    let failed_i = builder
        .ins()
        .iconst(I32, i64::from(TraceEntryStatus::GuardFailed.as_i32()));
    builder.ins().return_(&[failed_i]);

    // Direct arm: call save_deopt directly via the extern FuncRef.
    builder.switch_to_block(direct_block);
    builder
        .ins()
        .call(save_deopt_ref, &[trace_ctx_ptr, guard_pc, external_pc]);
    let failed_d = builder
        .ins()
        .iconst(I32, i64::from(TraceEntryStatus::GuardFailed.as_i32()));
    builder.ins().return_(&[failed_d]);

    builder.finalize();

    // Declare + define the host fn in the module.
    let fn_name = format!(
        "relon_trace_inline_host_fn_{}",
        save_deopt_id.as_u32().wrapping_add(1)
    );
    let func_id = module
        .declare_function(&fn_name, Linkage::Local, &ctx.func.signature)
        .map_err(|e| InlineHostFnError::Module(format!("declare {fn_name}: {e}")))?;
    module
        .define_function(func_id, &mut ctx)
        .map_err(|e| InlineHostFnError::Module(format!("define {fn_name}: {e}")))?;
    module
        .finalize_definitions()
        .map_err(|e| InlineHostFnError::Module(format!("finalize: {e}")))?;
    let raw_fn_ptr = module.get_finalized_function(func_id);
    // SAFETY: TRACE_ENTRY_SIG-compatible signature established above.
    let entry: TraceEntryFn = unsafe { std::mem::transmute(raw_fn_ptr) };

    tracing::debug!(
        target: "relon::trace_inline",
        op_count = trace.op_count(),
        guard_count = trace.guard_count(),
        cap = MAX_INLINE_OPS,
        "inline host fn compiled"
    );

    Ok(InlineHostFn {
        entry,
        raw_fn_ptr,
        inline_trace: trace,
        _module: module,
    })
}

/// Build a fresh JIT module configured the same way the trampoline
/// trace-install path uses ([`crate::trace_install::build_trace_jit_module`])
/// — `is_pic=false`, `opt_level=speed`, verifier on, no probestack /
/// frame pointers. Keeping the flag set identical means the inline
/// fn's machine-code shape is comparable to the trampoline fn's for
/// bench purposes.
fn build_inline_host_jit_module() -> Result<JITModule, InlineHostFnError> {
    let mut flag_builder = settings::builder();
    flag_builder
        .set("is_pic", "false")
        .map_err(|e| InlineHostFnError::Module(format!("flag is_pic: {e}")))?;
    flag_builder
        .set("opt_level", "speed")
        .map_err(|e| InlineHostFnError::Module(format!("flag opt_level: {e}")))?;
    flag_builder
        .set("enable_verifier", "true")
        .map_err(|e| InlineHostFnError::Module(format!("flag enable_verifier: {e}")))?;
    let _ = flag_builder.set("enable_probestack", "false");
    let _ = flag_builder.set("preserve_frame_pointers", "false");
    let flags = settings::Flags::new(flag_builder);

    let isa_builder = cranelift_native::builder()
        .map_err(|e| InlineHostFnError::Module(format!("isa builder: {e}")))?;
    let isa = isa_builder
        .finish(flags)
        .map_err(|e| InlineHostFnError::Module(format!("isa finish: {e}")))?;

    let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    // The inline host fn calls save_deopt on the deopt path; register
    // the trace runtime symbols so the JIT linker resolves the import.
    register_trace_runtime_symbols(&mut builder);
    Ok(JITModule::new(builder))
}
