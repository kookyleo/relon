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
use relon_trace_abi::{HostHookTable, ObservedType, TraceContext, TraceEntryStatus};
use relon_trace_emitter::{HostHookFuncIds, TraceEmitter};
use relon_trace_jit::{OptimizedTrace, OptimizerPipeline, SsaVar};
use relon_trace_recorder::{RecordResult, RecorderState};

use crate::trace_recording::{RecordingOutcome, TraceRecordingEvaluator};

/// Construct a [`HostHookTable`] pre-populated with all three v6-γ
/// runtime helpers wired through.
///
/// Hosts that want to invoke a JIT-emitted trace and observe deopt /
/// call-resolution / IC telemetry through the table indirection
/// (rather than relying on JITBuilder's symbol resolution) build
/// their `TraceContext` with this table.
///
/// v6-γ M5 widened the `resolve_call` / `inline_cache_lookup` slots
/// to dedicated fn-pointer types ([`relon_trace_abi::TraceResolveCallFn`] /
/// [`relon_trace_abi::TraceIcLookupFn`]) carrying the helpers'
/// **real** signatures — `(*mut TraceContext, u64) -> *const u8` and
/// `(*mut u8, u8) -> i32`. The uniform `TraceHookFn` shape stays in
/// use for `on_trap` / `save_deopt`; the new typed slots accept the
/// non-void-return helpers directly so the host doesn't have to ship
/// a lossy shim.
///
/// The cranelift emitter still resolves the three canonical extern
/// symbols via `JITBuilder::symbol` (direct call, no extra
/// indirection); the table is kept populated in parallel so:
///
/// 1. Hosts that want a stable handle on "is this helper installed?"
///    can inspect the table without re-deriving the symbol from the
///    JIT module.
/// 2. A future emitter revision can switch one or more helpers to
///    `call_indirect` through `ctx.host_hooks.<slot>` without an
///    ABI break.
pub fn default_host_hooks() -> HostHookTable {
    use relon_trace_abi::{TraceIcLookupFn, TraceResolveCallFn, TraceSaveDeoptFn};

    HostHookTable {
        on_trap: None,
        // v6-δ M1 R5: save_deopt carries its full 3-arg signature now
        // (TraceSaveDeoptFn). The emitter's deopt block dispatches via
        // `call_indirect` through `ctx.host_hooks.save_deopt`, so the
        // historical void-returning shim that dropped `external_pc` is
        // gone — hosts get the real resume PC.
        save_deopt: Some(relon_trace_jit::runtime::__relon_trace_save_deopt as TraceSaveDeoptFn),
        resolve_call: Some(
            relon_trace_jit::runtime::__relon_trace_resolve_call as TraceResolveCallFn,
        ),
        inline_cache_lookup: Some(
            relon_trace_jit::runtime::__relon_trace_inline_cache_lookup as TraceIcLookupFn,
        ),
    }
}

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
    /// v6-δ M2-C: per-guard SSA-index snapshots, indexed by `trace_pc`.
    /// Each entry holds the list of SSA-index ints the recorder's
    /// operand-stack mirror carried at the moment the guard was
    /// emitted (oldest first / top last). Used by the host-side
    /// `invoke_with_resume` path to render `value_stack_copy` into the
    /// `DeoptStateSnapshot` AFTER the cranelift-emitted save_deopt
    /// helper has written `ssa_slots_copy`. Empty for guards that
    /// emitted before any operand was pushed (rare).
    ///
    /// Keyed by `trace_pc` (the guard op's index in the optimised
    /// trace), not `external_pc`. The save_deopt helper passes
    /// `guard_pc = trace_pc` so this is a direct index.
    guard_ssa_stacks: Box<[Box<[u32]>]>,
    /// v6-δ M2-C: parallel external_pc table — useful for tests that
    /// want to verify the trace's deopt routing without reaching for
    /// the full guard list.
    #[allow(dead_code)]
    guard_external_pcs: Box<[u64]>,
    /// Owning module — Drop'd after every installed trace fn for the
    /// fn_id has been removed. Kept inside an `Arc` so concurrent
    /// callers can share the same trace fn without lock contention.
    _module: JITModule,
    /// v6-ε-0-A: optimised trace IR retained so a host fn can re-emit
    /// the same body inline at its trace dispatch call site, skipping
    /// the cranelift call/ret + arg-marshall boundary entirely.
    ///
    /// Set unconditionally on the install path; consumers consult
    /// [`relon_trace_emitter::should_inline_trace`] (or the
    /// `MAX_INLINE_OPS` cap directly) to decide between the inline
    /// path and the standard trampoline-call (`fn_ptr`) path.
    ///
    /// Wrapped in `Arc` so the host can cheaply hand the same trace to
    /// multiple inline call sites without cloning the op stream each
    /// time.
    inline_trace: Arc<OptimizedTrace>,
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
        let raw = unsafe { self.invoke_raw(ctx, args) };
        match raw {
            0 => TraceEntryStatus::Success,
            1 => TraceEntryStatus::GuardFailed,
            _ => TraceEntryStatus::Aborted,
        }
    }

    /// v6-δ M2-C: invoke the trace entry skipping the
    /// `i32 → TraceEntryStatus` enum mapping. The caller compares the
    /// raw return code (`0 == Success`, `1 == GuardFailed`,
    /// `2 == Aborted`) directly — useful in tight hot loops where the
    /// enum match adds a branch + cmov pair.
    ///
    /// Marked `#[inline]` so call sites that have a hot-loop-shaped
    /// usage (e.g. the bench's `trace_jit_warm_ic` row, or eventually
    /// the cranelift IC dispatch stub) can have the indirect call
    /// lower straight into a `call rax` with no Rust-side wrapper
    /// frame in between.
    ///
    /// # Safety
    ///
    /// Same contract as [`Self::invoke`]: `ctx` is an exclusive,
    /// properly aligned `*mut TraceContext` and `args` is either null
    /// or points to a `u64` array sized for the trace's LocalGet
    /// accesses.
    #[inline]
    pub unsafe fn invoke_raw(&self, ctx: *mut TraceContext, args: *const u64) -> i32 {
        let entry: TraceEntryFn = unsafe { std::mem::transmute(self.fn_ptr) };
        unsafe { entry(ctx, args) }
    }

    /// Raw entry pointer (mainly for tests verifying install
    /// dispatch behaviour, and for the v6-δ M2-C IC stub that wants
    /// to bypass the [`JITedTraceFn`] indirection entirely).
    pub fn raw_fn_ptr(&self) -> *const u8 {
        self.fn_ptr
    }

    /// v6-δ M2-C: typed function pointer cast of the trace entry. The
    /// IC dispatch stub stores this in a per-callsite `Cell` and
    /// calls it directly — one indirect call, no `Arc` deref, no
    /// `transmute` per invocation, no status-enum mapping.
    ///
    /// Returns the same address as [`Self::raw_fn_ptr`], just typed.
    ///
    /// # Safety
    ///
    /// The returned function pointer's lifetime is bound by this
    /// `JITedTraceFn`'s `_module`. Callers MUST keep the `Arc<Self>`
    /// alive for as long as they may call through the typed pointer.
    /// The IC slot in `trace_ic` enforces this by holding the `Arc`
    /// alongside the typed pointer.
    pub unsafe fn typed_entry(&self) -> TraceEntryFn {
        unsafe { std::mem::transmute(self.fn_ptr) }
    }

    /// v6-δ M2-C: expose the per-guard SSA-stack table for tests that
    /// want to verify the recorder mirror landed on the install path.
    /// Indexed by `trace_pc`; entries are SSA-index lists (oldest
    /// first / top last). Empty `Box<[u32]>` for non-guard trace_pcs.
    pub fn guard_ssa_stack(&self, trace_pc: u32) -> Option<&[u32]> {
        self.guard_ssa_stacks.get(trace_pc as usize).map(|s| &s[..])
    }

    /// v6-δ M2-C: number of slots in the per-guard SSA-stack table.
    /// Equal to the trace's op_count — most entries are empty (only
    /// guard ops populate them).
    pub fn guard_table_len(&self) -> usize {
        self.guard_ssa_stacks.len()
    }

    /// v6-ε-0-A: shared handle on the trace's [`OptimizedTrace`] IR.
    ///
    /// Host fn compilers consult this when deciding between the
    /// inline path (re-emit the trace body straight into the host fn
    /// IR — see [`relon_trace_emitter::emit_trace_inline`]) and the
    /// trampoline-call path (jump to [`Self::raw_fn_ptr`]).
    ///
    /// Cloning the returned `Arc` is the cheap way for a caller to
    /// pin the IR alive across the inline emit pass without holding
    /// the [`TraceJitState`] write lock.
    pub fn inline_trace(&self) -> Arc<OptimizedTrace> {
        Arc::clone(&self.inline_trace)
    }

    /// v6-ε-0-A: convenience helper that wraps
    /// [`relon_trace_emitter::should_inline_trace`] on this trace's
    /// retained IR. Returns `true` when the trace is small enough to
    /// be inlined at a host fn call site (per
    /// [`relon_trace_emitter::MAX_INLINE_OPS`]).
    pub fn inline_candidate(&self) -> bool {
        relon_trace_emitter::should_inline_trace(&self.inline_trace)
    }
}

/// v6-δ M2-C: typed entry-function pointer matching
/// [`relon_trace_abi::TRACE_ENTRY_SIG`]
/// (`(*mut TraceContext, *const u64) -> i32`). Stored inline in
/// [`crate::trace_ic::TraceIcSlot`] so the IC dispatch path can do a
/// single `call rax` indirect call.
pub type TraceEntryFn = unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32;

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

    /// Drop the installed trace for `fn_id`, returning it if any. A
    /// caller may invoke this when a deopt makes the trace
    /// unsalvageable (e.g. a type-check guard fired and the recorder's
    /// observed-type assumption is no longer valid).
    pub fn invalidate_trace(&self, fn_id: u32) -> Option<Arc<JITedTraceFn>> {
        let mut guard = self.trace_fns.write().expect("trace_fns lock poisoned");
        guard.remove(&fn_id)
    }

    /// Invoke the installed trace for `fn_id` if any, falling back to
    /// `fallback` on guard failure / abort / no-trace-installed.
    ///
    /// Convenience wrapper around
    /// [`Self::invoke_with_fallback_at_pc`] for callers that don't
    /// care about partial-resume. The fallback closure receives the
    /// raw `args_ptr` only; the deopt `external_pc` (if any) is
    /// ignored. Pre-v6-γ-M5 callers go through this; new ones use
    /// the `_at_pc` variant.
    ///
    /// # Safety
    ///
    /// Same contract as [`Self::invoke_with_fallback_at_pc`].
    pub unsafe fn invoke_with_fallback<F>(
        &self,
        fn_id: u32,
        args_ptr: *const u64,
        slot_count: usize,
        fallback: F,
    ) -> u64
    where
        F: FnOnce(*const u64) -> u64,
    {
        unsafe {
            self.invoke_with_fallback_at_pc(fn_id, args_ptr, slot_count, |args, _resume_pc| {
                fallback(args)
            })
        }
    }

    /// Invoke the installed trace for `fn_id` if any, falling back to
    /// `fallback` on guard failure / abort / no-trace-installed.
    ///
    /// ## Deopt protocol (v6-γ M5 cut)
    ///
    /// 1. Look up the trace fn. If absent, return `fallback(args_ptr,
    ///    None)`.
    /// 2. Build a `TraceContext` sized to fit the trace's
    ///    `ssa_high_water` (caller supplies `slot_count`).
    /// 3. Invoke the trace via [`JITedTraceFn::invoke`].
    /// 4. On `TraceEntryStatus::Success`: return `ctx.result_slot`.
    /// 5. On `TraceEntryStatus::GuardFailed`: read
    ///    `ctx.deopt_state.external_pc` and pass it to the fallback
    ///    as `Some(external_pc)`. v6-γ M5 threads the resume PC
    ///    through so callers that have an IR-side op-index table
    ///    can partial-resume rather than re-running from the entry.
    ///    A `None` `resume_pc` means "trace was not installed /
    ///    snapshot missing"; fallback should re-run from the top.
    /// 6. On `TraceEntryStatus::Aborted`: invalidate the trace +
    ///    fallback with `resume_pc = None`.
    ///
    /// ## Why two arities
    ///
    /// The original signature ([`Self::invoke_with_fallback`]) stayed
    /// stable for pre-M5 callers that never inspect the deopt PC.
    /// The new `_at_pc` form gives the partial-resume code path an
    /// explicit handle on the snapshot's resume PC without forcing
    /// every test fixture to spell out the extra arg.
    ///
    /// ## Safety
    ///
    /// `args_ptr` is forwarded unchanged into both the trace and the
    /// fallback. The caller pins its lifetime + validity.
    ///
    /// `slot_count` must be ≥ the trace's `ssa_high_water`; passing
    /// a smaller value yields undefined behaviour (cranelift writes
    /// past the slot array). The trace install pipeline records the
    /// high-water mark on the `JITedTraceFn`; this API takes the
    /// number as an explicit param so the caller can size the
    /// context once per fn and re-use it.
    pub unsafe fn invoke_with_fallback_at_pc<F>(
        &self,
        fn_id: u32,
        args_ptr: *const u64,
        slot_count: usize,
        fallback: F,
    ) -> u64
    where
        F: FnOnce(*const u64, Option<u64>) -> u64,
    {
        unsafe {
            self.invoke_with_resume(fn_id, args_ptr, slot_count, |args, resume_pc, _snapshot| {
                fallback(args, resume_pc)
            })
        }
    }

    /// v6-δ M1 R3: invoke the installed trace, falling back through a
    /// closure that receives the **full** deopt-state snapshot when a
    /// guard fires.
    ///
    /// Compared to [`Self::invoke_with_fallback_at_pc`] the fallback
    /// closure takes a third arg `snapshot: Option<&DeoptStateSnapshot>`
    /// so it can:
    ///
    /// 1. Read the captured `ssa_slots_copy` to feed
    ///    [`relon_eval_api::Evaluator::resume_from_pc`] —
    ///    backends that maintain an IR-side PC table get pixel-perfect
    ///    partial-resume rather than running from `#main` entry.
    /// 2. Drain `recoverable_writes` to undo writes the trace
    ///    speculatively performed before the deopt.
    /// 3. Inspect `guard_pc` for telemetry / log-trace correlation.
    ///
    /// Hosts that don't need the snapshot can keep using the existing
    /// `_at_pc` variant; the trait surface above (`Evaluator::resume_from_pc`)
    /// has a sensible default that ignores `local_snapshot`.
    ///
    /// ## Safety
    ///
    /// Same contract as [`Self::invoke_with_fallback_at_pc`].
    pub unsafe fn invoke_with_resume<F>(
        &self,
        fn_id: u32,
        args_ptr: *const u64,
        slot_count: usize,
        fallback: F,
    ) -> u64
    where
        F: FnOnce(*const u64, Option<u64>, Option<&relon_trace_abi::DeoptStateSnapshot>) -> u64,
    {
        let trace_fn = match self.lookup_trace(fn_id) {
            Some(t) => t,
            None => return fallback(args_ptr, None, None),
        };

        let mut ctx = TraceContext::with_hooks(slot_count, default_host_hooks());
        let status = unsafe { trace_fn.invoke(&mut ctx as *mut _, args_ptr) };
        match status {
            TraceEntryStatus::Success => ctx.result_slot,
            TraceEntryStatus::GuardFailed => {
                // v6-δ M2-C: render `value_stack_copy` from the
                // recorder's per-guard ssa_stack snapshot. The
                // cranelift-emitted `save_deopt` helper only knows
                // `ssa_slots_copy` (the full SSA state); the
                // operand-stack view lives in the install-time
                // `guard_ssa_stacks` table on the trace_fn. Walking
                // the table host-side is one indexed lookup + one
                // `ssa_slots_copy[i]` per stack entry — typical
                // trace bodies push at most 2-4 values mid-expression,
                // so the cost is bounded and dwarfed by the bytecode
                // VM resume path that consumes the result.
                let mut snapshot = ctx.deopt_state.take();
                if let Some(ref mut snap) = snapshot {
                    let guard_pc = snap.guard_pc as usize;
                    if guard_pc < trace_fn.guard_ssa_stacks.len() {
                        let stack_ssas = &trace_fn.guard_ssa_stacks[guard_pc];
                        let mut value_stack: Vec<u64> = Vec::with_capacity(stack_ssas.len());
                        for &ssa_idx in stack_ssas.iter() {
                            // SAFETY-equiv: out-of-range SSAs are
                            // silently filled with 0 — matches the
                            // bytecode-side `materialise_stack`
                            // fall-back convention so a sloppy
                            // recorder mirror doesn't panic the host.
                            let val = snap
                                .ssa_slots_copy
                                .get(ssa_idx as usize)
                                .copied()
                                .unwrap_or(0);
                            value_stack.push(val);
                        }
                        snap.value_stack_copy = value_stack.into_boxed_slice();
                    }
                }
                let resume_pc = snapshot.as_ref().map(|d| d.external_pc);
                tracing::debug!(
                    target: "relon::trace_install",
                    fn_id,
                    deopt_recorded = snapshot.is_some(),
                    ?resume_pc,
                    slots = snapshot.as_ref().map(|s| s.ssa_slots_copy.len()).unwrap_or(0),
                    value_stack_len = snapshot.as_ref().map(|s| s.value_stack_copy.len()).unwrap_or(0),
                    "trace GuardFailed; partial-resume snapshot ready (value_stack_copy rendered)"
                );
                fallback(args_ptr, resume_pc, snapshot.as_ref())
            }
            TraceEntryStatus::Aborted => {
                tracing::warn!(
                    target: "relon::trace_install",
                    fn_id,
                    "trace Aborted; invalidating + falling back"
                );
                self.invalidate_trace(fn_id);
                fallback(args_ptr, None, None)
            }
        }
    }

    /// Drive the full pipeline `recorder → optimizer → emitter →
    /// cranelift JIT` for a single fn_id and return the installable
    /// trace fn. The caller decides whether to install it via
    /// [`TraceJitState::install_trace`].
    ///
    /// Uses the host's default trace entry calling convention picked
    /// by [`relon_trace_emitter::trace_entry_call_conv`] — i.e.
    /// `CallConv::Tail` on x86_64 / aarch64 (v6-ε-0-C) and
    /// `CallConv::SystemV` elsewhere. Tests / benches that need a
    /// specific conv go through
    /// [`Self::jit_compile_buffer_for_fn_with_call_conv`] directly.
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

    /// Compile a pre-built [`relon_trace_jit::TraceBuffer`] (skipping the recorder
    /// finalize step). Used by tests that need to construct a trace
    /// without going through the recorder lowering rules — useful for
    /// pinning emitter / optimiser behaviour on synthetic ops. The
    /// production pipeline always goes through
    /// [`Self::jit_compile_trace_for_fn`].
    ///
    /// Uses the host's default trace entry calling convention; see
    /// [`Self::jit_compile_buffer_for_fn_with_call_conv`] for an
    /// explicit-conv variant.
    pub fn jit_compile_buffer_for_fn(
        &self,
        fn_id: u32,
        buffer: relon_trace_jit::TraceBuffer,
    ) -> Result<JITedTraceFn, TraceJitError> {
        self.jit_compile_buffer_for_fn_with_call_conv(
            fn_id,
            buffer,
            relon_trace_emitter::trace_entry_call_conv(),
        )
    }

    /// v6-ε-0-C: same as [`Self::jit_compile_buffer_for_fn`] but with
    /// an explicit cranelift calling convention for the trace entry
    /// function.
    ///
    /// Production callers should use [`Self::jit_compile_buffer_for_fn`]
    /// (which picks the optimal conv per host arch via
    /// [`relon_trace_emitter::trace_entry_call_conv`]). This variant
    /// exists so the v6-ε-0-C bench can install a SystemV-conv trace
    /// and a Tail-conv trace side-by-side for direct comparison
    /// (`trace_jit_warm_ic` vs `trace_jit_warm_tail` rows).
    ///
    /// **Note on `extern "C"` interop**: Rust callers that go through
    /// [`JITedTraceFn::invoke`] / the typed-entry helpers declare the
    /// entry pointer as `unsafe extern "C" fn(...) -> i32`. On
    /// x86_64 + aarch64 the wire-level register layout for the
    /// `TRACE_ENTRY_SIG` shape (2 ptr args + i32 return) is identical
    /// between `CallConv::Tail` and `CallConv::SystemV`, so the
    /// cross-conv call works. See
    /// `relon-trace-emitter/src/call_conv.rs` for the analysis.
    pub fn jit_compile_buffer_for_fn_with_call_conv(
        &self,
        fn_id: u32,
        mut buffer: relon_trace_jit::TraceBuffer,
        entry_call_conv: cranelift_codegen::isa::CallConv,
    ) -> Result<JITedTraceFn, TraceJitError> {
        if (fn_id as usize) >= MAX_FN_ID {
            return Err(TraceJitError::FnIdOutOfRange(fn_id));
        }

        // 2. Run the optimizer pipeline (six passes).
        let _reports = OptimizerPipeline::default_pipeline().run(&mut buffer);

        // 3. Freeze + emit cranelift IR.
        let optimized = buffer.into_optimized();

        // v6-δ M2-C: build the per-guard SSA-stack lookup table the
        // host-side `invoke_with_resume` uses to render
        // `value_stack_copy` on deopt. The table is indexed by
        // `trace_pc` (guard op index in the optimised trace); the
        // entry is the SSA-id snapshot the recorder mirror captured
        // at emit time. Stored as `Box<[u32]>` per entry (vs
        // `Vec<SsaVar>`) so the host-side rendering loop is a tight
        // index → `ssa_slots_copy[idx]` lookup.
        //
        // Use `optimized.ops.len()` as the upper bound — guards only
        // ever sit at trace_pc slots within that range, so we size
        // the table to op_count and leave non-guard slots as empty.
        let table_len = optimized.ops.len();
        let mut guard_ssa_stacks_vec: Vec<Box<[u32]>> =
            (0..table_len).map(|_| Box::<[u32]>::from([])).collect();
        let mut guard_external_pcs_vec: Vec<u64> = vec![0u64; table_len];
        for site in optimized.guards.iter() {
            let idx = site.trace_pc as usize;
            if idx >= table_len {
                continue;
            }
            let stack: Vec<u32> = site.ssa_stack_snapshot.iter().map(|s| s.raw()).collect();
            guard_ssa_stacks_vec[idx] = stack.into_boxed_slice();
            guard_external_pcs_vec[idx] = site.deopt_pc.0;
        }
        let guard_ssa_stacks = guard_ssa_stacks_vec.into_boxed_slice();
        let guard_external_pcs = guard_external_pcs_vec.into_boxed_slice();

        let mut module = build_trace_jit_module()?;
        let pointer_ty = module.target_config().pointer_type();

        // v6-δ M1: pre-declare the three host helpers as `Linkage::Import`
        // functions BEFORE the trace fn itself. The cranelift-module
        // crate uses `FuncId.as_u32()` for the `UserExternalName.index`
        // when serialising calls in the function IR. If we let the
        // trace fn be FuncId 0 (the historical default), then the
        // emitter's `call save_deopt(...)` would compile down to
        // calling FuncId 0 (the trace fn itself) — instant
        // self-recursion → SIGSEGV the moment any guard fires.
        //
        // By pre-declaring the helpers we get a deterministic
        // FuncId-to-symbol mapping; the trace fn then takes the next
        // free slot. The `HostHookFuncIds` block is passed to the
        // emitter so `declare_imported_user_function` writes the
        // right indices into the function IR.
        let save_deopt_sig = build_host_helper_signature(
            &[
                pointer_ty,
                cranelift_codegen::ir::types::I32,
                cranelift_codegen::ir::types::I64,
            ],
            &[],
        );
        let resolve_call_sig = build_host_helper_signature(
            &[pointer_ty, cranelift_codegen::ir::types::I32],
            &[pointer_ty],
        );
        let ic_lookup_sig = build_host_helper_signature(
            &[
                pointer_ty,
                cranelift_codegen::ir::types::I32,
                cranelift_codegen::ir::types::I64,
            ],
            &[cranelift_codegen::ir::types::I64],
        );
        let save_deopt_id = module
            .declare_function(
                relon_trace_emitter::HostHookId::SaveDeopt.symbol(),
                Linkage::Import,
                &save_deopt_sig,
            )
            .map_err(|e| TraceJitError::Module(format!("declare save_deopt: {e}")))?;
        let resolve_call_id = module
            .declare_function(
                relon_trace_emitter::HostHookId::ResolveCall.symbol(),
                Linkage::Import,
                &resolve_call_sig,
            )
            .map_err(|e| TraceJitError::Module(format!("declare resolve_call: {e}")))?;
        let ic_lookup_id = module
            .declare_function(
                relon_trace_emitter::HostHookId::InlineCacheLookup.symbol(),
                Linkage::Import,
                &ic_lookup_sig,
            )
            .map_err(|e| TraceJitError::Module(format!("declare ic_lookup: {e}")))?;
        // F-D7 string shims: import the four `__relon_str_*` helpers
        // up-front so the emitter can target them by FuncId. All four
        // use `(ptr,ptr) -> ptr`-shaped signatures except contains
        // (returns i32), find (returns i64), and substring (takes an
        // extra start+length pair).
        let str_concat_sig = build_host_helper_signature(&[pointer_ty, pointer_ty], &[pointer_ty]);
        let str_contains_sig = build_host_helper_signature(
            &[pointer_ty, pointer_ty],
            &[cranelift_codegen::ir::types::I32],
        );
        let str_find_sig = build_host_helper_signature(
            &[pointer_ty, pointer_ty],
            &[cranelift_codegen::ir::types::I64],
        );
        let str_substring_sig = build_host_helper_signature(
            &[
                pointer_ty,
                cranelift_codegen::ir::types::I64,
                cranelift_codegen::ir::types::I64,
            ],
            &[pointer_ty],
        );
        let str_concat_id = module
            .declare_function(
                relon_trace_emitter::HostHookId::StrConcat.symbol(),
                Linkage::Import,
                &str_concat_sig,
            )
            .map_err(|e| TraceJitError::Module(format!("declare str_concat: {e}")))?;
        let str_contains_id = module
            .declare_function(
                relon_trace_emitter::HostHookId::StrContains.symbol(),
                Linkage::Import,
                &str_contains_sig,
            )
            .map_err(|e| TraceJitError::Module(format!("declare str_contains: {e}")))?;
        let str_find_id = module
            .declare_function(
                relon_trace_emitter::HostHookId::StrFind.symbol(),
                Linkage::Import,
                &str_find_sig,
            )
            .map_err(|e| TraceJitError::Module(format!("declare str_find: {e}")))?;
        let str_substring_id = module
            .declare_function(
                relon_trace_emitter::HostHookId::StrSubstring.symbol(),
                Linkage::Import,
                &str_substring_sig,
            )
            .map_err(|e| TraceJitError::Module(format!("declare str_substring: {e}")))?;
        // F-D7-I: optional alloc-only helper for the inline `StrConcat`
        // short-rhs lowering. `(lhs: *const StringRef, total_len: usize)
        // -> *mut StringRef`.
        let str_concat_alloc_sig =
            build_host_helper_signature(&[pointer_ty, pointer_ty], &[pointer_ty]);
        let str_concat_alloc_id = module
            .declare_function(
                relon_trace_emitter::HostHookId::StrConcatAlloc.symbol(),
                Linkage::Import,
                &str_concat_alloc_sig,
            )
            .map_err(|e| TraceJitError::Module(format!("declare str_concat_alloc: {e}")))?;
        // F-D8: pre-declare the dict/list helpers so traces emitted
        // with `TraceOp::ListGet` / `TraceOp::DictLookup` link
        // cleanly. Same Linkage::Import rationale as save_deopt /
        // resolve_call — the symbols are registered via
        // `register_trace_runtime_symbols` on the JIT builder before
        // we finalise the module.
        let list_get_sig = build_host_helper_signature(
            &[pointer_ty, cranelift_codegen::ir::types::I64, pointer_ty],
            &[cranelift_codegen::ir::types::I64],
        );
        let dict_lookup_sig = build_host_helper_signature(
            &[
                pointer_ty,
                pointer_ty,
                cranelift_codegen::ir::types::I64,
                pointer_ty,
            ],
            &[cranelift_codegen::ir::types::I64],
        );
        let list_get_id = module
            .declare_function(
                relon_trace_emitter::HostHookId::ListGet.symbol(),
                Linkage::Import,
                &list_get_sig,
            )
            .map_err(|e| TraceJitError::Module(format!("declare list_get: {e}")))?;
        let dict_lookup_id = module
            .declare_function(
                relon_trace_emitter::HostHookId::DictLookup.symbol(),
                Linkage::Import,
                &dict_lookup_sig,
            )
            .map_err(|e| TraceJitError::Module(format!("declare dict_lookup: {e}")))?;
        // F-D8-E.2: pre-declare the "shape already checked" variant
        // of the dict lookup helper. The optimizer's `dict_ic_hoist`
        // pass rewrites in-loop `TraceOp::DictLookup` whose
        // `dict_ptr` is loop-invariant into a `DictShapeGuard` (LICM
        // hoists out of the loop) + a `DictLookupPrechecked` (stays
        // in the body) pair. The prechecked op routes here; the
        // shape compare it skips was made redundant by the hoisted
        // guard. Signature mirrors `dict_lookup` minus the
        // `shape_hash: i64` arg.
        let dict_lookup_prechecked_sig = build_host_helper_signature(
            &[pointer_ty, pointer_ty, pointer_ty],
            &[cranelift_codegen::ir::types::I64],
        );
        let dict_lookup_prechecked_id = module
            .declare_function(
                relon_trace_emitter::HostHookId::DictLookupPrechecked.symbol(),
                Linkage::Import,
                &dict_lookup_prechecked_sig,
            )
            .map_err(|e| TraceJitError::Module(format!("declare dict_lookup_prechecked: {e}")))?;
        let hook_func_ids = HostHookFuncIds {
            save_deopt: save_deopt_id.as_u32(),
            resolve_call: resolve_call_id.as_u32(),
            inline_cache_lookup: ic_lookup_id.as_u32(),
            str_concat: str_concat_id.as_u32(),
            str_contains: str_contains_id.as_u32(),
            str_find: str_find_id.as_u32(),
            str_substring: str_substring_id.as_u32(),
            str_concat_alloc: Some(str_concat_alloc_id.as_u32()),
            list_get: Some(list_get_id.as_u32()),
            dict_lookup: Some(dict_lookup_id.as_u32()),
            dict_lookup_prechecked: Some(dict_lookup_prechecked_id.as_u32()),
        };

        let mut ctx = CodegenContext::new();
        TraceEmitter::emit_with_hooks_and_call_conv(
            &optimized,
            &mut ctx,
            pointer_ty,
            hook_func_ids,
            entry_call_conv,
        )
        .map_err(TraceJitError::Emit)?;
        tracing::trace!(
            target: "relon::trace_install",
            fn_id,
            call_conv = %entry_call_conv,
            ir = %ctx.func.display(),
            "trace cranelift IR ready for module install"
        );
        if std::env::var_os("RELON_DUMP_TRACE_IR").is_some() {
            eprintln!(
                "=== trace fn_id={fn_id} IR ===\n{}=== end ===",
                ctx.func.display()
            );
        }

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
            guard_ssa_stacks,
            guard_external_pcs,
            _module: module,
            // v6-ε-0-A: clone the optimised trace into an Arc so host
            // fn compilers that pick the inline path can re-emit the
            // body without re-running the optimiser. The trace was
            // moved into `module.define_function` via `ctx.func`; the
            // OptimizedTrace itself is independent of the cranelift
            // Function value, so cloning is cheap (Box<[TraceOp]> +
            // small side tables).
            inline_trace: Arc::new(optimized),
        })
    }
}

impl Default for TraceJitState {
    fn default() -> Self {
        Self::new()
    }
}

/// Build a cranelift `Signature` for a host helper function. Used by
/// `jit_compile_buffer_for_fn` when pre-declaring `__relon_trace_save_deopt`
/// et al. as `Linkage::Import` entries in the trace JIT module.
fn build_host_helper_signature(
    params: &[cranelift_codegen::ir::Type],
    returns: &[cranelift_codegen::ir::Type],
) -> cranelift_codegen::ir::Signature {
    use cranelift_codegen::ir::{AbiParam, Signature};
    use cranelift_codegen::isa::CallConv;
    let mut sig = Signature::new(CallConv::SystemV);
    for p in params {
        sig.params.push(AbiParam::new(*p));
    }
    for r in returns {
        sig.returns.push(AbiParam::new(*r));
    }
    sig
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
    flag_builder
        .set("enable_verifier", "true")
        .map_err(|e| TraceJitError::Module(format!("flag enable_verifier: {e}")))?;
    // v6-δ M2-C: shave a few cycles off the per-call invoke cost.
    // Cranelift's default `enable_probestack=true` injects a probe
    // sequence at the function prologue that adds branch + stack
    // access — useless for trace bodies that allocate zero stack
    // space. The trace entries we emit today are leaf functions
    // (no call_indirect outside the deopt block), so probestack is
    // pure overhead. Same logic for `preserve_frame_pointers`: the
    // bench loop never unwinds through a trace, and `gdb` /
    // profilers prefer DWARF info anyway.
    //
    // Disabling these saves ~1-2 cycles / iter in the bench hot
    // loop and reduces the trace entry's instruction footprint —
    // both micro-optimisations help the trace_jit_warm_ic row
    // approach the LuaJIT trace-tier range.
    let _ = flag_builder.set("enable_probestack", "false");
    let _ = flag_builder.set("preserve_frame_pointers", "false");
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
/// Exposed `pub` so `build_jit_module_with_runtime_helpers` (the
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
    // F-D7 string shims. Symbols match `HostHookId::Str*.symbol()`.
    builder.symbol(
        relon_trace_emitter::HostHookId::StrConcat.symbol(),
        relon_trace_jit::runtime::__relon_str_concat as *const u8,
    );
    builder.symbol(
        relon_trace_emitter::HostHookId::StrContains.symbol(),
        relon_trace_jit::runtime::__relon_str_contains as *const u8,
    );
    builder.symbol(
        relon_trace_emitter::HostHookId::StrFind.symbol(),
        relon_trace_jit::runtime::__relon_str_find as *const u8,
    );
    builder.symbol(
        relon_trace_emitter::HostHookId::StrSubstring.symbol(),
        relon_trace_jit::runtime::__relon_str_substring as *const u8,
    );
    // F-D7-I: alloc-only helper paired with the inline `StrConcat`
    // short-rhs lowering. Symbol resolution mirrors the other str
    // shims; the emitter only emits the call when a trace's const-byte
    // side table carries a ≤ 16-byte rhs payload.
    builder.symbol(
        relon_trace_emitter::HostHookId::StrConcatAlloc.symbol(),
        relon_trace_jit::runtime::__relon_str_concat_alloc as *const u8,
    );
    // F-D8: dict/list helpers. Symbol resolution happens via
    // `JITBuilder::symbol`; the cranelift emitter's
    // `declare_imported_user_function` call carries the matching
    // `HostHookId::symbol()` string so the linker pairs both ends.
    builder.symbol(
        relon_trace_emitter::HostHookId::ListGet.symbol(),
        relon_trace_jit::runtime::__relon_trace_list_get as *const u8,
    );
    builder.symbol(
        relon_trace_emitter::HostHookId::DictLookup.symbol(),
        relon_trace_jit::runtime::__relon_trace_dict_lookup as *const u8,
    );
    // F-D8-E.2: prechecked dict lookup helper. Paired with
    // `TraceOp::DictShapeGuard` ahead of the call site.
    builder.symbol(
        relon_trace_emitter::HostHookId::DictLookupPrechecked.symbol(),
        relon_trace_jit::runtime::__relon_trace_dict_lookup_prechecked as *const u8,
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
