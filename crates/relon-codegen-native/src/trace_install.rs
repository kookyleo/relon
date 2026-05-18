//! v6-γ M2 + M3: HotCounter-driven trace install pipeline.
//!
//! This module bridges the cranelift AOT backend with the trace JIT
//! components (`relon-trace-recorder` + `relon-trace-jit` +
//! `relon-trace-emitter`). The flow is:
//!
//! 1. Each cranelift-compiled entry function has a prologue (injected
//!    by `codegen::emit_hot_counter_inject`) that increments a per-fn
//!    counter slot. When the slot reaches `RELON_HOT_THRESHOLD`, the
//!    prologue calls [`__relon_jump_to_recorder`].
//! 2. [`__relon_jump_to_recorder`] consults the thread-local
//!    [`TraceJitState`] for the current evaluator. If a trace function
//!    is already installed for that fn_id, the call falls through
//!    (the generic path continues — the next hot-trigger has been
//!    saturated so this branch should not re-fire normally).
//!    Otherwise the state machine starts a recording pass driven by
//!    the host (driver feeds `Op` instances; see
//!    [`record_program_into_state`] used by the smoke tests).
//! 3. After recording, [`TraceJitState::jit_compile_trace_for_fn`]
//!    runs the OptimizerPipeline, hands the result to TraceEmitter,
//!    and finalises a fresh JITModule. The returned [`JITedTraceFn`]
//!    can be invoked via [`JITedTraceFn::invoke`].
//!
//! ## Layout decisions
//!
//! - **Non-atomic counters** (per v6-γ design §3): the cranelift
//!   prologue emits `load.i32 / iadd_imm / store.i32` for cheap
//!   warm-path overhead. Multi-thread races may delay a hot trigger
//!   by one or two iterations, but never cause UB — the storage is
//!   plain `u32` cells inside an [`UnsafeCell`] wrapped for `Sync`.
//! - **Threshold = 10** (LuaJIT default; see design §1.2).
//! - **Counter capacity = 1024** fn ids — generous for v6-γ's
//!   single-entry-function workloads. Excess fn ids saturate via a
//!   range check in the helper.
//!
//! ## Status
//!
//! - M2: HotCounter inject — DONE.
//! - M3: jit_compile_trace_for_fn pipeline — DONE.
//! - The full IR-walker → recorder lifting is left to a follow-up
//!   stage; the public surface used by the smoke tests today feeds
//!   the recorder an explicit `Op` stream.

use std::cell::{RefCell, UnsafeCell};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context as CodegenContext;
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module as _};

use relon_ir::{IrType, Op, TaggedOp};
use relon_trace_abi::{ObservedType, TraceContext, TraceEntryStatus};
use relon_trace_emitter::TraceEmitter;
use relon_trace_jit::{OptimizerPipeline, SsaVar};
use relon_trace_recorder::{RecordResult, RecorderState};

use crate::trace_recording::{RecordingOutcome, TraceRecordingEvaluator};

/// Counter table capacity. Each entry is a `u32` cell indexed by
/// `fn_id`; the cranelift prologue derives the slot address as
/// `RELON_HOT_COUNTERS_BASE + fn_id * 4`.
///
/// 1024 slots is generous for the v6-γ corpus (every test program has
/// at most a single `#main` plus stdlib helpers); future phases can
/// either bump the constant or move to a dynamically-sized table.
pub const MAX_FN_ID: usize = 1024;

/// Default hot-trigger threshold. Matches LuaJIT's trace-recorder
/// default (10) per `docs/internal/v6-gamma-trace-jit-design.md` §1.2.
pub const RELON_HOT_THRESHOLD: u32 = 10;

/// Symbol the cranelift codegen would use if it imported the counters
/// table by name. v6-γ M2 inlines the table base as an `iconst.i64`
/// since the address is known at compile time; the symbol name is
/// kept here so future revisions (object-cache cold start) can rebind
/// the address by name at link time.
pub const HOT_COUNTERS_SYMBOL: &str = "__relon_hot_counters";

/// Wrapper around the global counter table so it can be `static` and
/// also mutable from non-`Sync` cranelift-emitted code. Each slot is a
/// raw `u32` — torn writes are tolerated (multi-thread races at worst
/// delay a hot trigger by one iteration; design decision).
struct HotCountersTable {
    inner: UnsafeCell<[u32; MAX_FN_ID]>,
}

// SAFETY: We accept torn reads/writes on the raw u32 slots; no
// invariant requires atomicity. The trace-install path's correctness
// is guarded downstream by `TraceJitState`'s RwLock.
unsafe impl Sync for HotCountersTable {}

static RELON_HOT_COUNTERS: HotCountersTable = HotCountersTable {
    inner: UnsafeCell::new([0u32; MAX_FN_ID]),
};

/// Raw pointer to the first counter slot. The cranelift prologue
/// folds this into an `iconst.i64` so each entry-fn invocation does:
///
/// ```text
/// %base = iconst.i64 <hot_counters_base()>
/// %slot = iadd_imm %base, fn_id * 4
/// %v    = load.i32 %slot
/// %v1   = iadd_imm.i32 %v, 1
/// store.i32 %v1, %slot
/// %hot  = icmp_imm.i32 uge %v1, RELON_HOT_THRESHOLD
/// brif %hot, hot_block, normal_block
/// ```
pub fn hot_counters_base() -> *mut u32 {
    RELON_HOT_COUNTERS.inner.get() as *mut u32
}

/// Read the current counter value for `fn_id` (for tests).
pub fn hot_counter_peek(fn_id: u32) -> u32 {
    assert!((fn_id as usize) < MAX_FN_ID, "fn_id out of range");
    // SAFETY: in-bounds; torn reads are explicitly tolerated.
    unsafe { *hot_counters_base().add(fn_id as usize) }
}

/// Reset a counter slot to zero (for tests).
pub fn hot_counter_reset(fn_id: u32) {
    assert!((fn_id as usize) < MAX_FN_ID, "fn_id out of range");
    // SAFETY: in-bounds; tests run sequentially per fn_id.
    unsafe { *hot_counters_base().add(fn_id as usize) = 0 };
}

/// Reset every counter slot. Used by test harness setup to isolate
/// individual cases; production paths never call this.
pub fn hot_counter_reset_all() {
    // SAFETY: we hold the only writer (test code, single-threaded).
    let p = hot_counters_base();
    for i in 0..MAX_FN_ID {
        unsafe { *p.add(i) = 0 };
    }
}

/// A JIT-finalised trace function backed by its own cranelift JIT
/// module. The host holds an `Arc<JITedTraceFn>` per installed
/// fn_id so the module stays mapped for as long as the function
/// pointer is reachable.
pub struct JITedTraceFn {
    /// fn_id this trace was compiled for. Round-tripped through the
    /// install path so test assertions don't have to track it
    /// separately.
    pub fn_id: u32,
    /// Raw entry pointer obeying [`relon_trace_abi::TRACE_ENTRY_SIG`]:
    /// `(*mut TraceContext, *const u64) -> i32`.
    fn_ptr: *const u8,
    /// Owning module — Drop'd after every installed trace fn for the
    /// fn_id has been removed. Kept inside an `Arc` so concurrent
    /// callers can share the same trace fn without lock contention.
    _module: JITModule,
}

// SAFETY: the entry fn pointer is owned by `_module`; sharing the
// `JITedTraceFn` across threads is safe as long as the host honours
// the `TRACE_ENTRY_SIG` contract (single mutable `TraceContext`).
unsafe impl Send for JITedTraceFn {}
unsafe impl Sync for JITedTraceFn {}

impl JITedTraceFn {
    /// Invoke the trace entry with the supplied [`TraceContext`] and
    /// argument slot pointer. The return value is the raw status code
    /// the trace tail-returned (0 = Success, 1 = GuardFailed,
    /// 2 = Aborted), matching [`TraceEntryStatus`].
    ///
    /// # Safety
    ///
    /// `ctx` must point at an exclusive `TraceContext` allocated with
    /// `ssa_slots.len() >= optimized_trace.ssa_high_water`. `args` may
    /// be null when the trace ignores its second arg (the v6-γ M3
    /// emitter doesn't materialise input args yet).
    pub unsafe fn invoke(&self, ctx: *mut TraceContext, args: *const u64) -> TraceEntryStatus {
        let entry: unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32 =
            unsafe { std::mem::transmute(self.fn_ptr) };
        let raw = unsafe { entry(ctx, args) };
        match raw {
            0 => TraceEntryStatus::Success,
            1 => TraceEntryStatus::GuardFailed,
            _ => TraceEntryStatus::Aborted,
        }
    }

    /// Raw entry pointer (mainly for tests verifying install
    /// dispatch behaviour).
    pub fn raw_fn_ptr(&self) -> *const u8 {
        self.fn_ptr
    }
}

/// Error wrapper for [`TraceJitState::jit_compile_trace_for_fn`].
#[derive(Debug, thiserror::Error)]
pub enum TraceJitError {
    /// The recorder finalised with an abort (e.g. UnsupportedOp).
    #[error("recorder aborted: {0:?}")]
    RecorderAbort(relon_trace_recorder::AbortReason),
    /// The trace emitter rejected the optimised trace.
    #[error("trace emit failed: {0:?}")]
    Emit(relon_trace_emitter::EmitError),
    /// A cranelift module step failed (declare / define / finalize).
    #[error("cranelift module error: {0}")]
    Module(String),
    /// fn_id outside the counter table's range.
    #[error("fn_id {0} >= MAX_FN_ID {MAX_FN_ID}")]
    FnIdOutOfRange(u32),
}

/// Trace-install registry tied to one cranelift evaluator instance.
///
/// `TraceJitState` is `Send + Sync` and keeps a map of installed
/// `JITedTraceFn` instances indexed by `fn_id`. The plan calls for
/// per-thread instances in production hosts, but a single shared
/// registry suffices for v6-γ smoke tests and for the integration
/// path: every install holds the writer lock just long enough to
/// insert the new fn, lookups take only a reader lock.
pub struct TraceJitState {
    trace_fns: RwLock<HashMap<u32, Arc<JITedTraceFn>>>,
}

impl TraceJitState {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self {
            trace_fns: RwLock::new(HashMap::new()),
        }
    }

    /// Look up the installed trace fn for `fn_id`, returning a cloned
    /// `Arc` so the caller can outlive the lock window.
    pub fn lookup_trace(&self, fn_id: u32) -> Option<Arc<JITedTraceFn>> {
        let guard = self.trace_fns.read().expect("trace_fns lock poisoned");
        guard.get(&fn_id).cloned()
    }

    /// Number of installed traces (mainly for tests).
    pub fn installed_count(&self) -> usize {
        self.trace_fns
            .read()
            .expect("trace_fns lock poisoned")
            .len()
    }

    /// Install a freshly-compiled trace. Returns the previous trace
    /// for the same `fn_id` if any (caller may keep the `Arc` alive
    /// to drain in-flight invocations).
    pub fn install_trace(&self, fn_id: u32, trace_fn: JITedTraceFn) -> Option<Arc<JITedTraceFn>> {
        let mut guard = self.trace_fns.write().expect("trace_fns lock poisoned");
        guard.insert(fn_id, Arc::new(trace_fn))
    }

    /// Drive the full pipeline `recorder → optimizer → emitter →
    /// cranelift JIT` for a single fn_id and return the installable
    /// trace fn. The caller decides whether to install it via
    /// [`TraceJitState::install_trace`].
    pub fn jit_compile_trace_for_fn(
        &self,
        fn_id: u32,
        recorder_state: RecorderState,
    ) -> Result<JITedTraceFn, TraceJitError> {
        // 1. Finalise recorder → TraceBuffer.
        let buffer = recorder_state
            .finalize()
            .map_err(TraceJitError::RecorderAbort)?;
        self.jit_compile_buffer_for_fn(fn_id, buffer)
    }

    /// Compile a pre-built [`TraceBuffer`] (skipping the recorder
    /// finalize step). Used by tests that need to construct a trace
    /// without going through the recorder lowering rules — useful for
    /// pinning emitter / optimiser behaviour on synthetic ops. The
    /// production pipeline always goes through
    /// [`Self::jit_compile_trace_for_fn`].
    pub fn jit_compile_buffer_for_fn(
        &self,
        fn_id: u32,
        mut buffer: relon_trace_jit::TraceBuffer,
    ) -> Result<JITedTraceFn, TraceJitError> {
        if (fn_id as usize) >= MAX_FN_ID {
            return Err(TraceJitError::FnIdOutOfRange(fn_id));
        }

        // 2. Run the optimizer pipeline (six passes).
        let _reports = OptimizerPipeline::default_pipeline().run(&mut buffer);

        // 3. Freeze + emit cranelift IR.
        let optimized = buffer.into_optimized();
        let mut module = build_trace_jit_module()?;
        let pointer_ty = module.target_config().pointer_type();

        let mut ctx = CodegenContext::new();
        TraceEmitter::emit_with_pointer_ty(&optimized, &mut ctx, pointer_ty)
            .map_err(TraceJitError::Emit)?;

        // 4. Declare + define the function inside the trace JIT
        //    module, then finalize and resolve the function pointer.
        let trace_fn_name = format!("relon_trace_fn_{fn_id}");
        let func_id = module
            .declare_function(&trace_fn_name, Linkage::Local, &ctx.func.signature)
            .map_err(|e| TraceJitError::Module(format!("declare {trace_fn_name}: {e}")))?;
        module
            .define_function(func_id, &mut ctx)
            .map_err(|e| TraceJitError::Module(format!("define {trace_fn_name}: {e}")))?;
        module
            .finalize_definitions()
            .map_err(|e| TraceJitError::Module(format!("finalize: {e}")))?;
        let fn_ptr = module.get_finalized_function(func_id);

        Ok(JITedTraceFn {
            fn_id,
            fn_ptr,
            _module: module,
        })
    }
}

impl Default for TraceJitState {
    fn default() -> Self {
        Self::new()
    }
}

/// Process-wide singleton registry. The cranelift evaluator (M4
/// follow-up) will install per-evaluator instances; v6-γ M2/M3
/// keeps a single global slot wired to the `__relon_jump_to_recorder`
/// helper so the test harness can drive recording without threading
/// a separate context handle through every JIT-emitted call.
static GLOBAL_TRACE_JIT_STATE: OnceLock<TraceJitState> = OnceLock::new();

/// Access the global registry, creating it on first use. The
/// `__relon_jump_to_recorder` host helper indirects through this
/// when the cranelift-emitted prologue triggers a hot crossing.
pub fn global_trace_jit_state() -> &'static TraceJitState {
    GLOBAL_TRACE_JIT_STATE.get_or_init(TraceJitState::new)
}

/// IR registration entry the [`__relon_jump_to_recorder`] helper
/// consults when its counter saturates. The cranelift evaluator
/// installs one of these per fn_id before invoking the entry
/// function so the recording driver can find the IR body + arg
/// layout it needs to drive a real trace recording.
#[derive(Debug, Clone)]
pub struct RecordingRegistration {
    /// Cloned IR op stream for the function. Cheaper to clone than
    /// to thread an Arc through the recording driver — the body is
    /// only the Phase-1 hot subset and the install path is a slow
    /// path anyway.
    pub body: Vec<TaggedOp>,
    /// Parameter types, in declaration order. The helper combines
    /// these with the slot values read from `args_ptr` to produce
    /// the `(u64, IrType)` pairs the [`TraceRecordingEvaluator`]
    /// expects.
    pub param_tys: Vec<IrType>,
}

thread_local! {
    /// Per-thread map `fn_id -> RecordingRegistration`. The
    /// cranelift evaluator installs an entry before calling its JIT
    /// entry function so the helper can fall back into a real
    /// recording driver when the prologue's counter saturates.
    ///
    /// Thread-local rather than process-global because the design
    /// (`v6-gamma-integration-plan-2026-05-18.md` §3.4) explicitly
    /// keeps the recorder state machine per-thread; using a thread-
    /// local registry mirrors that decision without needing an
    /// additional lock.
    pub static RECORDING_REGISTRY: RefCell<HashMap<u32, RecordingRegistration>> =
        RefCell::new(HashMap::new());
}

/// Register an IR body for `fn_id` so the next
/// [`__relon_jump_to_recorder`] call can drive a real recording
/// pass. Returns the previous registration if any.
pub fn register_recording(fn_id: u32, reg: RecordingRegistration) -> Option<RecordingRegistration> {
    RECORDING_REGISTRY.with(|r| r.borrow_mut().insert(fn_id, reg))
}

/// Remove the recording registration for `fn_id`. Used by hosts that
/// invalidate a compiled fn (e.g. on schema change).
pub fn clear_recording(fn_id: u32) -> Option<RecordingRegistration> {
    RECORDING_REGISTRY.with(|r| r.borrow_mut().remove(&fn_id))
}

/// Number of installed recording registrations on the current thread
/// (mainly for tests asserting the registry is in the expected state).
pub fn recording_registration_count() -> usize {
    RECORDING_REGISTRY.with(|r| r.borrow().len())
}

/// `extern "C"` host helper invoked from the cranelift entry-fn
/// prologue when its counter saturates. v6-γ M4 wires this into a
/// real recording driver:
///
/// 1. Bump the diagnostic counter so smoke tests can prove the
///    prologue path fired.
/// 2. Look up the IR registration for `fn_id`. If none is present
///    (e.g. the host never registered a body, or `fn_id` falls
///    outside the prepared set) the helper returns immediately and
///    the cranelift-generic backend handles the cold path.
/// 3. Unpack the IR-declared param types against the host-supplied
///    `args_ptr` (each slot is one `u64`).
/// 4. Spin up a fresh [`RecorderState`] + [`TraceRecordingEvaluator`]
///    and walk the IR body, recording every op into the buffer as
///    the walker executes it for real.
/// 5. On `RecordingOutcome::Recorded`: drive the buffer through the
///    `optimizer → emitter → JIT install` pipeline and store the
///    resulting [`JITedTraceFn`] in [`global_trace_jit_state`].
/// 6. On `RecordingOutcome::Aborted`: log the reason and bail — the
///    counter stays saturated so subsequent hot crossings keep
///    invoking the helper, but the install never happens. Future
///    iterations may add a sticky-abort bit to short-circuit this.
///
/// # Safety
///
/// `args_ptr` must point at a contiguous array of `u64`s with at
/// least `param_tys.len()` elements, **OR** be null (in which case
/// the helper synthesises a zeroed slot vector). Both shapes are
/// load-bearing: the cranelift prologue currently always passes
/// null (per `codegen::emit_hot_counter_inject`) but the registry
/// API will hand-roll real arg ptrs in a later stage.
#[no_mangle]
pub unsafe extern "C" fn __relon_jump_to_recorder(fn_id: u32, args_ptr: *const u64) {
    // Bump a debug counter so smoke tests can confirm the
    // cranelift-injected prologue actually fired.
    JUMP_HELPER_CALLS.with(|c| c.set(c.get() + 1));

    tracing::debug!(
        target: "relon::trace_install",
        fn_id,
        args_ptr = ?args_ptr,
        "hot trigger: __relon_jump_to_recorder invoked"
    );

    // Short-circuit: if a trace is already installed for this fn_id
    // we have nothing to do. The cranelift prologue keeps saturating
    // the counter and re-firing the helper because the hot-block
    // emit returns sentinel zero on every iteration — until the
    // host dispatcher learns to route through the installed trace
    // (v6-γ M5), the install is idempotent.
    let state = global_trace_jit_state();
    if state.lookup_trace(fn_id).is_some() {
        tracing::debug!(
            target: "relon::trace_install",
            fn_id,
            "hot trigger: trace already installed, skipping"
        );
        return;
    }

    let registration = RECORDING_REGISTRY.with(|r| r.borrow().get(&fn_id).cloned());
    let Some(registration) = registration else {
        tracing::debug!(
            target: "relon::trace_install",
            fn_id,
            "hot trigger: no IR registration, falling back to generic backend"
        );
        return;
    };

    // Materialise the (u64 value, IrType) pairs the walker expects.
    // When the cranelift prologue passes a null ptr (today's shape)
    // we substitute zeroed slots so the walker can still run; the
    // recorder will then abort on the first arith op that needs a
    // typed input. This is a known imprecision while the prologue
    // still ignores its arg ptr; the recording will install when
    // the host bumps the prologue to thread real args through.
    let args: Vec<(u64, IrType)> = if args_ptr.is_null() {
        registration
            .param_tys
            .iter()
            .map(|ty| (0u64, *ty))
            .collect()
    } else {
        registration
            .param_tys
            .iter()
            .enumerate()
            .map(|(i, ty)| {
                // SAFETY: caller contract — args_ptr is a packed u64 array
                // with len >= param_tys.len().
                let v = unsafe { *args_ptr.add(i) };
                (v, *ty)
            })
            .collect()
    };

    let mut recorder = RecorderState::new();
    let outcome = TraceRecordingEvaluator::record_and_run(&mut recorder, &args, &registration.body);

    match outcome {
        RecordingOutcome::Recorded {
            recorder: boxed,
            result,
        } => {
            tracing::debug!(
                target: "relon::trace_install",
                fn_id,
                result,
                "recording succeeded; driving install pipeline"
            );
            match state.jit_compile_trace_for_fn(fn_id, *boxed) {
                Ok(trace_fn) => {
                    state.install_trace(fn_id, trace_fn);
                    tracing::debug!(
                        target: "relon::trace_install",
                        fn_id,
                        "trace installed successfully"
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        target: "relon::trace_install",
                        fn_id,
                        error = %e,
                        "JIT compile failed; trace not installed"
                    );
                }
            }
        }
        RecordingOutcome::Aborted {
            reason,
            partial_result,
        } => {
            tracing::debug!(
                target: "relon::trace_install",
                fn_id,
                ?reason,
                partial_result,
                "recording aborted; trace not installed"
            );
        }
    }
}

thread_local! {
    /// Per-thread counter incremented on every
    /// [`__relon_jump_to_recorder`] call. Tests reset + read this
    /// to verify the prologue path executed.
    pub static JUMP_HELPER_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Read the per-thread `__relon_jump_to_recorder` call count.
pub fn jump_helper_call_count() -> u64 {
    JUMP_HELPER_CALLS.with(|c| c.get())
}

/// Reset the per-thread `__relon_jump_to_recorder` call count.
pub fn reset_jump_helper_call_count() {
    JUMP_HELPER_CALLS.with(|c| c.set(0));
}

/// Build a freestanding JIT module pre-wired with the v6-γ trace
/// runtime helpers. Used internally by
/// [`TraceJitState::jit_compile_trace_for_fn`] for compiling trace
/// entries, and by the smoke tests as a building block.
pub fn build_trace_jit_module() -> Result<JITModule, TraceJitError> {
    let mut flag_builder = settings::builder();
    flag_builder
        .set("is_pic", "false")
        .map_err(|e| TraceJitError::Module(format!("flag is_pic: {e}")))?;
    flag_builder
        .set("opt_level", "speed")
        .map_err(|e| TraceJitError::Module(format!("flag opt_level: {e}")))?;
    let flags = settings::Flags::new(flag_builder);

    let isa_builder = cranelift_native::builder()
        .map_err(|e| TraceJitError::Module(format!("isa builder: {e}")))?;
    let isa = isa_builder
        .finish(flags)
        .map_err(|e| TraceJitError::Module(format!("isa finish: {e}")))?;

    let mut builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
    register_trace_runtime_symbols(&mut builder);
    Ok(JITModule::new(builder))
}

/// Register the four v6-γ trace runtime helpers in `builder`'s
/// internal symbol table. Cranelift resolves `extern` calls in the
/// trace IR by consulting this table before falling back to dlsym;
/// keeping every helper symbol-registered avoids surprising
/// rdynamic / strip behaviour.
///
/// Exposed `pub` so [`build_jit_module_with_runtime_helpers`] (the
/// codegen-native entry function builder) can call it without
/// duplicating the symbol list.
pub fn register_trace_runtime_symbols(builder: &mut JITBuilder) {
    builder.symbol(
        relon_trace_emitter::HostHookId::SaveDeopt.symbol(),
        relon_trace_jit::runtime::__relon_trace_save_deopt as *const u8,
    );
    builder.symbol(
        relon_trace_emitter::HostHookId::ResolveCall.symbol(),
        relon_trace_jit::runtime::__relon_trace_resolve_call as *const u8,
    );
    builder.symbol(
        relon_trace_emitter::HostHookId::InlineCacheLookup.symbol(),
        relon_trace_jit::runtime::__relon_trace_inline_cache_lookup as *const u8,
    );
    builder.symbol(
        "__relon_jump_to_recorder",
        __relon_jump_to_recorder as *const u8,
    );
}

/// Helper used by tests: feed a slice of `(Op, inputs, observed)`
/// tuples into a fresh [`RecorderState`] and return the recorder
/// when the sequence terminates (or aborts). Returns the SSA value
/// of each successful op so the caller can chain inputs across
/// ops.
///
/// Doc-private: not exported through `lib.rs`; tests reach it via
/// `crate::trace_install::record_program_into_state`.
pub fn record_program_into_state(
    ops: &[(Op, Vec<SsaVar>, Option<ObservedType>)],
) -> Result<RecorderState, relon_trace_recorder::AbortReason> {
    let mut recorder = RecorderState::new();
    for (op, inputs, observed) in ops {
        match recorder.record_op(op, inputs, *observed) {
            RecordResult::Ok { .. }
            | RecordResult::NeedsGuard { .. }
            | RecordResult::Terminated => continue,
            RecordResult::Abort(reason) => return Err(reason),
        }
    }
    Ok(recorder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_counters_base_is_stable() {
        let p1 = hot_counters_base();
        let p2 = hot_counters_base();
        assert_eq!(p1, p2, "base pointer must be process-stable");
    }

    #[test]
    fn hot_counter_peek_reset_round_trip() {
        // Use a fn_id that smoke tests don't touch.
        let id = (MAX_FN_ID - 1) as u32;
        hot_counter_reset(id);
        assert_eq!(hot_counter_peek(id), 0);
        // SAFETY: writes a single u32 slot to a process-stable buffer.
        unsafe {
            *hot_counters_base().add(id as usize) = 7;
        }
        assert_eq!(hot_counter_peek(id), 7);
        hot_counter_reset(id);
        assert_eq!(hot_counter_peek(id), 0);
    }

    #[test]
    fn jit_compile_trace_for_fn_returns_invokable_entry() {
        let state = TraceJitState::new();
        let mut recorder = RecorderState::new();
        let _ = recorder.record_op(&Op::ConstI64(42), &[], Some(ObservedType::I64));
        let last_val = match recorder.record_op(&Op::ConstI64(100), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) } => v,
            other => panic!("unexpected: {other:?}"),
        };
        let term = recorder.record_op(&Op::Return, &[last_val], None);
        assert!(matches!(term, RecordResult::Terminated));

        let trace_fn = state
            .jit_compile_trace_for_fn(0, recorder)
            .expect("trace pipeline must succeed for trivial trace");
        assert_eq!(trace_fn.fn_id, 0);
        assert!(
            !trace_fn.raw_fn_ptr().is_null(),
            "JIT must yield a non-null entry pointer"
        );
    }

    #[test]
    fn install_trace_round_trip() {
        let state = TraceJitState::new();
        let recorder = make_const_return_recorder(7);
        let trace_fn = state
            .jit_compile_trace_for_fn(42, recorder)
            .expect("compile");
        assert!(state.lookup_trace(42).is_none());
        let prev = state.install_trace(42, trace_fn);
        assert!(prev.is_none());
        assert!(state.lookup_trace(42).is_some());
        assert_eq!(state.installed_count(), 1);
    }

    fn make_const_return_recorder(v: i64) -> RecorderState {
        let mut recorder = RecorderState::new();
        let val = match recorder.record_op(&Op::ConstI64(v), &[], Some(ObservedType::I64)) {
            RecordResult::Ok { value: Some(v) } => v,
            other => panic!("ConstI64 produced no SSA value: {other:?}"),
        };
        let term = recorder.record_op(&Op::Return, &[val], None);
        assert!(matches!(term, RecordResult::Terminated));
        recorder
    }

    #[test]
    fn record_program_into_state_collects_terminated_recorder() {
        let v0 = SsaVar(0);
        let ops = vec![
            (Op::ConstI64(11), vec![], Some(ObservedType::I64)),
            (Op::Return, vec![v0], None),
        ];
        let recorder = record_program_into_state(&ops).expect("ok");
        assert!(recorder.is_terminated());
    }

    #[test]
    fn fn_id_out_of_range_errors() {
        let state = TraceJitState::new();
        let recorder = make_const_return_recorder(1);
        let err = state
            .jit_compile_trace_for_fn(MAX_FN_ID as u32 + 1, recorder)
            .err()
            .expect("must error");
        assert!(matches!(err, TraceJitError::FnIdOutOfRange(_)));
    }
}
