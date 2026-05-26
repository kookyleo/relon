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
//! - **Atomic counters** (#173 review fix): each slot is an
//!   [`AtomicU32`] and the cranelift prologue emits a single
//!   `atomic_rmw add` against it. Earlier drafts used a
//!   non-atomic `load/iadd/store` triple inside an `UnsafeCell`
//!   on the rationale that races would only delay a hot trigger;
//!   that reasoning is incorrect under the Rust memory model —
//!   concurrent Rust readers / JIT writers on the same `u32`
//!   slot form a data race and are UB. Switching to `AtomicU32`
//!   keeps the slot lock-free (on x86 the lowering is a single
//!   `LOCK XADD` / ~1 cycle) and makes the storage naturally
//!   `Sync` without an `unsafe impl`.
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

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use arc_swap::ArcSwap;

use cranelift_codegen::settings::{self, Configurable};
use cranelift_codegen::Context as CodegenContext;
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module as _};

use relon_ir::{IrType, Op, TaggedOp};
use relon_trace_abi::{HostHookTable, ObservedType, TraceContext, TraceEntryStatus};
use relon_trace_emitter::{HostHookFuncIds, TraceEmitter};
use relon_trace_jit::{OptimizedTrace, OptimizerPipeline, SsaVar, TraceOp};
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

/// Op-count gate for the runtime trace dispatcher. Traces whose
/// optimised body is **strictly fewer** ops than this threshold skip
/// the trace-entry prologue entirely and route straight to the
/// caller's fallback closure.
///
/// ## Rationale (W12 p99 tail)
///
/// The trace-entry path pays a fixed prologue per invoke (~80ns on
/// a 2.1 GHz Broadwell: TraceContext init, extern call boundary,
/// `result_slot` readback, deopt-state branch). For micro-traces
/// like W12's 4-op `LocalGet + ConstI64 + Add + Return` body, that
/// prologue dwarfs the body itself; the fallback closure
/// (`|args| *args + 1`) runs in <10ns. The gate exists so the
/// dispatcher can give those workloads the obviously-cheaper path
/// without invalidating or never-installing the trace (the
/// inline-emit path still benefits from it).
///
/// Empirically calibrated against the cmp_lua corpus on 2026-05-25
/// (RELON_TRACE_GATE_DEBUG instrumentation): W12's optimised trace
/// lands at exactly 5 ops, so the original `< 5` threshold missed
/// it. The next-smallest production trace observed (W3 string
/// concat) sits at 17 ops; bumping the threshold to 8 leaves a
/// 9-op safety margin against accidentally gating any real loop
/// body while comfortably capturing W12's 5-op body plus any
/// 6-7-op "single expression" recordings the recorder may emit
/// once the closure-call lowering work lands.
pub const TINY_TRACE_OP_THRESHOLD: usize = 8;

/// Symbol the cranelift codegen would use if it imported the counters
/// table by name. v6-γ M2 inlines the table base as an `iconst.i64`
/// since the address is known at compile time; the symbol name is
/// kept here so future revisions (object-cache cold start) can rebind
/// the address by name at link time.
pub const HOT_COUNTERS_SYMBOL: &str = "__relon_hot_counters";

/// Global counter table. Each slot is an [`AtomicU32`] indexed by
/// `fn_id`; the cranelift prologue derives the slot address as
/// `RELON_HOT_COUNTERS_BASE + fn_id * 4` and increments via
/// `atomic_rmw add`.
///
/// `AtomicU32` has the same layout / alignment as `u32`, so the JIT
/// continues to treat each cell as a 4-byte integer location. The
/// array is naturally `Sync` without an `unsafe impl`.
static RELON_HOT_COUNTERS: [AtomicU32; MAX_FN_ID] = [const { AtomicU32::new(0) }; MAX_FN_ID];

/// Raw pointer to the first counter slot. The cranelift prologue
/// folds this into an `iconst.i64` so each entry-fn invocation does:
///
/// ```text
/// %base = iconst.i64 <hot_counters_base()>
/// %slot = iadd_imm %base, fn_id * 4
/// %v    = atomic_rmw.i32 add %slot, 1     ; returns the OLD value
/// %v1   = iadd_imm.i32 %v, 1               ; reconstruct the NEW value
/// %hot  = icmp_imm.i32 uge %v1, RELON_HOT_THRESHOLD
/// brif %hot, hot_block, normal_block
/// ```
///
/// Returning `*mut u32` (rather than `*mut AtomicU32`) is sound:
/// `AtomicU32` and `u32` share layout and alignment, and every
/// caller — cranelift-emitted `atomic_rmw` plus the
/// [`AtomicU32::from_ptr`] helpers in this module's accessors —
/// goes through atomic memory operations, so there is no
/// non-atomic access in flight.
pub fn hot_counters_base() -> *mut u32 {
    RELON_HOT_COUNTERS.as_ptr() as *mut u32
}

/// Read the current counter value for `fn_id` (for tests).
pub fn hot_counter_peek(fn_id: u32) -> u32 {
    assert!((fn_id as usize) < MAX_FN_ID, "fn_id out of range");
    RELON_HOT_COUNTERS[fn_id as usize].load(Ordering::Relaxed)
}

/// Reset a counter slot to zero (for tests).
pub fn hot_counter_reset(fn_id: u32) {
    assert!((fn_id as usize) < MAX_FN_ID, "fn_id out of range");
    RELON_HOT_COUNTERS[fn_id as usize].store(0, Ordering::Relaxed);
}

/// Reset every counter slot. Used by test harness setup to isolate
/// individual cases; production paths never call this.
pub fn hot_counter_reset_all() {
    for slot in RELON_HOT_COUNTERS.iter() {
        slot.store(0, Ordering::Relaxed);
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

fn with_trace_string_reclaim<R>(f: impl FnOnce() -> R) -> R {
    struct ReclaimOnDrop;
    impl Drop for ReclaimOnDrop {
        fn drop(&mut self) {
            // SAFETY: callers arrange for any arena-backed StringRef
            // pointers to be consumed before leaving the scope.
            unsafe { relon_trace_jit::runtime::reclaim_trace_strings() };
        }
    }

    let _reclaim = ReclaimOnDrop;
    f()
}

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

    fn return_observed_type(&self) -> Option<ObservedType> {
        self.inline_trace.ops.iter().rev().find_map(|op| {
            if let TraceOp::Return { value } = op {
                self.inline_trace.type_info.get(value).copied()
            } else {
                None
            }
        })
    }

    fn success_result_allows_string_reclaim(&self) -> bool {
        matches!(
            self.return_observed_type(),
            Some(ObservedType::I32 | ObservedType::I64 | ObservedType::F64 | ObservedType::Bool)
        )
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

    /// Number of ops in the installed trace body — same value as
    /// [`OptimizedTrace::op_count`] on the retained IR. Used by the
    /// runtime dispatcher to gate out micro-traces whose body is too
    /// short to amortise the trace-entry prologue (TraceContext
    /// setup + extern call + result-slot read-back). See
    /// [`TINY_TRACE_OP_THRESHOLD`].
    pub fn op_count(&self) -> usize {
        self.inline_trace.op_count()
    }

    /// v6-ε-0-A: convenience helper that wraps
    /// [`relon_trace_emitter::should_inline_trace`] on this trace's
    /// retained IR. Returns `true` when the trace is small enough to
    /// be inlined at a host fn call site (per
    /// [`relon_trace_emitter::MAX_INLINE_OPS`]).
    pub fn inline_candidate(&self) -> bool {
        relon_trace_emitter::should_inline_trace(&self.inline_trace)
    }

    /// Review #178 P2: scoped invoke that exposes the raw trace return
    /// to a caller closure **before** the trace string arena is
    /// reclaimed. Power-user API for callers that need a fine-grained
    /// look at the raw `ctx.result_slot` (e.g. dispatchers that
    /// branch on the status and want to materialise different
    /// representations per status).
    ///
    /// The closure receives a [`RawInvokeResult`] carrying the raw
    /// `i32` status code and the `ctx.result_slot` value. After the
    /// closure returns, [`reclaim_trace_strings`] runs unconditionally
    /// — any `*const StringRef` the closure read out of
    /// `result_slot` MUST be materialised (i.e. its payload bytes
    /// copied) before the closure returns. Callers that only want an
    /// owned representation should reach for
    /// [`Self::invoke_materialised`] instead; this helper exists for
    /// the rare case where the caller needs custom dispatch logic.
    ///
    /// # Safety
    ///
    /// Same contract as [`Self::invoke`] / [`Self::invoke_raw`]:
    /// `ctx` is an exclusive `*mut TraceContext`, `args` is either
    /// null or a sized `u64` array. The closure MUST NOT retain any
    /// `*const StringRef` derived from `result.result` past its own
    /// return — those pointers become dangling once
    /// `reclaim_trace_strings` runs.
    pub unsafe fn invoke_with_string_reclaim<R, F>(
        &self,
        ctx: *mut TraceContext,
        args: *const u64,
        f: F,
    ) -> R
    where
        F: FnOnce(RawInvokeResult) -> R,
    {
        // Read the raw status first; the `ctx.result_slot` is a
        // `u64` field on `TraceContext` populated by the trace's
        // `Return` lowering. Reading it via the raw pointer keeps
        // the borrow contract obvious (the caller owns `ctx`; we
        // only read one field through it).
        let status = unsafe { self.invoke_raw(ctx, args) };
        let result = unsafe { (*ctx).result_slot };
        let out = f(RawInvokeResult { status, result });
        // Drain every per-trace allocation on the calling thread.
        // SAFETY: by the closure contract above, no caller-held
        // `*const StringRef` derived from `result` survives past
        // `f`'s return.
        unsafe { relon_trace_jit::runtime::reclaim_trace_strings() };
        out
    }

    /// Review #178 P2: high-level invoke that returns an **owned**
    /// representation of the trace's success value, sourced from the
    /// declared [`ReturnKind`] hint, and reclaims the trace string
    /// arena before returning. Callers never see the raw
    /// `*const StringRef` pointer the arena owns.
    ///
    /// ## Dispatch shape
    ///
    /// - `ReturnKind::I64` / `ReturnKind::Bool` / `ReturnKind::F64`:
    ///   bit-cast `ctx.result_slot` into the corresponding scalar.
    ///   No arena pointer is touched and the reclaim pass is still
    ///   issued (it's a no-op when the trace did not allocate).
    /// - `ReturnKind::String`: treat `ctx.result_slot` as a
    ///   `*const StringRef`, copy the payload bytes into a
    ///   [`SmolStr`] (inline if `≤ SMOL_STR_INLINE_CAP`, else
    ///   `Arc<str>`), then reclaim. The returned `SmolStr` outlives
    ///   the reclaim and is the only handle the caller needs to
    ///   carry forward.
    ///
    /// On `GuardFailed` / `Aborted` status the trace's success value
    /// is undefined; the helper surfaces those as
    /// [`MaterialisedInvokeError::GuardFailed`] /
    /// [`MaterialisedInvokeError::Aborted`] respectively. The reclaim
    /// pass still runs so partial allocations made before the guard
    /// fire are released.
    ///
    /// # Safety
    ///
    /// Same contract as [`Self::invoke`]: `ctx` is an exclusive
    /// `*mut TraceContext`, `args` is either null or a sized `u64`
    /// array.
    ///
    /// When `return_kind == ReturnKind::String` the trace MUST tail-
    /// `Return` a `*const StringRef` previously handed out by the
    /// `relon_trace_jit::runtime::str_ops` family — that is the only
    /// pointer shape the materialiser dereferences. Passing a
    /// `ReturnKind::String` to a non-string-returning trace is UB.
    pub unsafe fn invoke_materialised(
        &self,
        ctx: *mut TraceContext,
        args: *const u64,
        return_kind: ReturnKind,
    ) -> Result<MaterialisedValue, MaterialisedInvokeError> {
        unsafe {
            self.invoke_with_string_reclaim(ctx, args, |raw| match raw.status {
                0 => Ok(materialise_success_slot(raw.result, return_kind)),
                1 => Err(MaterialisedInvokeError::GuardFailed),
                _ => Err(MaterialisedInvokeError::Aborted),
            })
        }
    }
}

/// Raw output of a trace invoke handed into the
/// [`JITedTraceFn::invoke_with_string_reclaim`] closure. Carries the
/// status code and the `ctx.result_slot` value as a `u64`; the
/// caller-side closure interprets the bit pattern per the trace's
/// declared return shape.
#[derive(Debug, Clone, Copy)]
pub struct RawInvokeResult {
    /// Raw status code (`0 == Success`, `1 == GuardFailed`,
    /// `2 == Aborted`). See [`TraceEntryStatus`].
    pub status: i32,
    /// `ctx.result_slot` value at the moment the trace returned.
    pub result: u64,
}

/// Caller-supplied hint that drives the high-level
/// [`JITedTraceFn::invoke_materialised`] dispatch.
///
/// The trace itself does not carry a self-describing return type tag
/// at the ABI boundary — `result_slot` is a `u64` slot the trace
/// writes a `Return`ed SSA value into. The host knows the trace's IR
/// return shape (it built the recorder + ran the optimiser) so the
/// kind is supplied at invoke time rather than re-derived from the
/// trace IR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReturnKind {
    /// Bit-cast `result_slot` into `i64`.
    I64,
    /// Bit-cast `result_slot` into `f64` (via
    /// `f64::from_bits(u64)`).
    F64,
    /// Treat `result_slot` as a `u64` boolean (0 = false, non-zero
    /// = true). The cranelift `Return` lowering for a `Bool` value
    /// emits a 0/1 result, so the canonical shape is met.
    Bool,
    /// Treat `result_slot` as a `*const StringRef` (the
    /// trace-runtime arena-owned shape). The materialiser copies the
    /// payload into a [`SmolStr`] before the reclaim pass invalidates
    /// the arena pointer.
    String,
}

/// Owned representation of a trace's success value produced by
/// [`JITedTraceFn::invoke_materialised`].
///
/// String-shaped traces hand back a [`SmolStr`] (≤ 22 byte payloads
/// inline, longer payloads behind an `Arc<str>`) so the caller never
/// has to reach into the arena pointer; numeric / boolean traces
/// hand back the unwrapped scalar directly.
#[derive(Debug, Clone, PartialEq)]
pub enum MaterialisedValue {
    /// 64-bit signed integer return.
    Int(i64),
    /// IEEE-754 double-precision float return.
    Float(f64),
    /// Boolean return — `result_slot` interpreted as a 0/1
    /// indicator.
    Bool(bool),
    /// String return: an SSO-friendly owned copy of the trace's
    /// arena-allocated payload.
    String(relon_eval_api::SmolStr),
}

/// Error variants surfaced by [`JITedTraceFn::invoke_materialised`].
/// The success path always yields a [`MaterialisedValue`]; deopt /
/// abort signal through this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum MaterialisedInvokeError {
    /// Trace hit a guard and returned `TraceEntryStatus::GuardFailed`.
    /// The caller's bytecode-side resume path must run; the trace's
    /// `result_slot` value is undefined here.
    #[error("trace guard failed; resume on bytecode")]
    GuardFailed,
    /// Trace returned `TraceEntryStatus::Aborted` (typically: a
    /// runtime invariant the trace cannot honour, e.g. a deopt that
    /// did not snapshot). The caller should invalidate the install
    /// and re-run from scratch.
    #[error("trace aborted; invalidate + re-run")]
    Aborted,
}

/// Common materialisation core shared by [`JITedTraceFn::invoke_materialised`]
/// and the `cfg(test)` mirror used inside this module. Pulled out as a
/// free function so it stays a non-`unsafe` building block — the only
/// dangerous bit (dereferencing the `StringRef` pointer) is gated by
/// the caller declaring `ReturnKind::String`.
fn materialise_success_slot(result: u64, return_kind: ReturnKind) -> MaterialisedValue {
    match return_kind {
        ReturnKind::I64 => MaterialisedValue::Int(result as i64),
        ReturnKind::F64 => MaterialisedValue::Float(f64::from_bits(result)),
        ReturnKind::Bool => MaterialisedValue::Bool(result != 0),
        ReturnKind::String => {
            // SAFETY: the caller-supplied `ReturnKind::String` is the
            // ABI hint that the trace's success path tail-returned a
            // valid `*const StringRef`. A null pointer surfaces as
            // an empty SmolStr — matches the str_ops shim contract
            // for null inputs without panicking.
            let ptr = result as usize as *const relon_trace_jit::runtime::StringRef;
            let owned = unsafe { relon_trace_jit::runtime::StringRef::as_str(ptr) }
                .map(relon_eval_api::SmolStr::from_borrowed)
                .unwrap_or_else(relon_eval_api::SmolStr::new_empty);
            MaterialisedValue::String(owned)
        }
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
    /// P2-12: `ArcSwap<HashMap>` published view + `Mutex` write lock.
    /// Reads via `lookup_trace` are wait-free — one atomic load per
    /// trace dispatch instead of the prior `RwLock::read` CAS path.
    /// Writers (`install_trace` / `invalidate_trace`) hold the mutex
    /// across the load-clone-modify-store sequence so concurrent
    /// installs serialise (RwLock-equivalent for writes) and a
    /// concurrent reader can't see a torn intermediate.
    trace_fns: ArcSwap<HashMap<u32, Arc<JITedTraceFn>>>,
    write_lock: Mutex<()>,
}

impl TraceJitState {
    /// Construct an empty registry.
    pub fn new() -> Self {
        Self {
            trace_fns: ArcSwap::from_pointee(HashMap::new()),
            write_lock: Mutex::new(()),
        }
    }

    /// Look up the installed trace fn for `fn_id`, returning a cloned
    /// `Arc` so the caller can outlive the lock window.
    pub fn lookup_trace(&self, fn_id: u32) -> Option<Arc<JITedTraceFn>> {
        let snap = self.trace_fns.load();
        snap.get(&fn_id).cloned()
    }

    /// Number of installed traces (mainly for tests).
    pub fn installed_count(&self) -> usize {
        self.trace_fns.load().len()
    }

    /// Install a freshly-compiled trace. Returns the previous trace
    /// for the same `fn_id` if any (caller may keep the `Arc` alive
    /// to drain in-flight invocations).
    pub fn install_trace(&self, fn_id: u32, trace_fn: JITedTraceFn) -> Option<Arc<JITedTraceFn>> {
        let _w = self
            .write_lock
            .lock()
            .expect("trace_fns write lock poisoned");
        let mut next = (**self.trace_fns.load()).clone();
        let prev = next.insert(fn_id, Arc::new(trace_fn));
        self.trace_fns.store(Arc::new(next));
        prev
    }

    /// Drop the installed trace for `fn_id`, returning it if any. A
    /// caller may invoke this when a deopt makes the trace
    /// unsalvageable (e.g. a type-check guard fired and the recorder's
    /// observed-type assumption is no longer valid).
    pub fn invalidate_trace(&self, fn_id: u32) -> Option<Arc<JITedTraceFn>> {
        let _w = self
            .write_lock
            .lock()
            .expect("trace_fns write lock poisoned");
        let mut next = (**self.trace_fns.load()).clone();
        let prev = next.remove(&fn_id);
        self.trace_fns.store(Arc::new(next));
        prev
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

        // Tiny-trace gate: route micro-traces around the cranelift
        // entry. See [`TINY_TRACE_OP_THRESHOLD`].
        if trace_fn.op_count() < TINY_TRACE_OP_THRESHOLD {
            return fallback(args_ptr, None, None);
        }

        let mut ctx = TraceContext::with_hooks(slot_count, default_host_hooks());
        let status = unsafe { trace_fn.invoke(&mut ctx as *mut _, args_ptr) };
        match status {
            TraceEntryStatus::Success => {
                let result = ctx.result_slot;
                if trace_fn.success_result_allows_string_reclaim() {
                    // SAFETY: the installed trace's return SSA is a
                    // non-pointer observed type, so the raw result
                    // cannot be an arena-backed StringRef. Temporary
                    // StringRef allocations made along the way can be
                    // reclaimed after the result slot has been copied.
                    unsafe { relon_trace_jit::runtime::reclaim_trace_strings() };
                }
                result
            }
            TraceEntryStatus::GuardFailed => with_trace_string_reclaim(|| {
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
            }),
            TraceEntryStatus::Aborted => with_trace_string_reclaim(|| {
                tracing::warn!(
                    target: "relon::trace_install",
                    fn_id,
                    "trace Aborted; invalidating + falling back"
                );
                self.invalidate_trace(fn_id);
                fallback(args_ptr, None, None)
            }),
        }
    }

    /// Invoke the installed trace for `fn_id` using a caller-owned
    /// [`TraceContext`], avoiding the per-call allocation that
    /// [`Self::invoke_with_fallback`] performs internally.
    ///
    /// ## When to use this
    ///
    /// Hot dispatch loops that repeatedly invoke the **same** trace
    /// (criterion benches, sustained query plans, batch processors)
    /// see noticeable variance reduction by keeping the context's
    /// memory live across calls:
    ///
    /// 1. The `ssa_slots: Box<[u64]>` heap allocation is amortised
    ///    (one allocator round-trip per loop instead of one per call).
    /// 2. The [`TraceContext::dict_lookup_ic`] table stays warm —
    ///    every `(dict_ptr, key_ptr)` pair primed by one invocation
    ///    is available to the next, so the inline-IR IC probe in
    ///    `DictLookupPrechecked` hits on the **first** loop iter of
    ///    subsequent calls rather than the second.
    /// 3. The TraceContext's storage address stays stable, so the
    ///    cranelift-emitted reads / writes hit the same L1 lines
    ///    across calls — avoids the address-aliasing lottery that
    ///    makes per-call-allocated contexts produce 2× variance on
    ///    nominally idle hosts.
    ///
    /// ## Per-call reset semantics
    ///
    /// On entry the helper clears `deopt_state` (so a stale
    /// snapshot from a previous deopt doesn't bleed in) and drains
    /// `pending_recoverable_writes` (DSE-emitted writes from a prior
    /// call are not replayable across invocation boundaries). The
    /// `dict_lookup_ic` table is **intentionally** left intact so it
    /// can carry warm entries forward.
    ///
    /// If the caller intends to switch to a different `(dict_ptr,
    /// key_ptr)` workload mid-loop and wants to start the IC cold,
    /// they call [`TraceContext::reset_dict_lookup_ic`] explicitly.
    ///
    /// # Safety
    ///
    /// Same contract as [`Self::invoke_with_fallback`] except the
    /// caller is responsible for sizing the context's `ssa_slots`
    /// to at least the trace's `ssa_high_water` (otherwise cranelift
    /// writes past the slot array — undefined behaviour). The
    /// install pipeline records the high-water on the
    /// `JITedTraceFn`; callers typically pick `64` for benches with
    /// well-bounded traces, mirroring the figure used by the v6-γ
    /// recorder fixture set.
    pub unsafe fn invoke_with_existing_ctx<F>(
        &self,
        fn_id: u32,
        ctx: &mut TraceContext,
        args_ptr: *const u64,
        fallback: F,
    ) -> u64
    where
        F: FnOnce(*const u64) -> u64,
    {
        let trace_fn = match self.lookup_trace(fn_id) {
            Some(t) => t,
            None => return fallback(args_ptr),
        };

        // Tiny-trace gate: bodies shorter than the threshold cannot
        // amortise the trace-entry prologue; route straight to the
        // fallback closure. See [`TINY_TRACE_OP_THRESHOLD`] for the
        // sizing rationale.
        if trace_fn.op_count() < TINY_TRACE_OP_THRESHOLD {
            return fallback(args_ptr);
        }

        // Reset per-call state (IC table persists across calls).
        ctx.deopt_state = None;
        ctx.pending_recoverable_writes.clear();

        let status = unsafe { trace_fn.invoke(ctx as *mut _, args_ptr) };
        match status {
            TraceEntryStatus::Success => {
                let result = ctx.result_slot;
                if trace_fn.success_result_allows_string_reclaim() {
                    // SAFETY: same rationale as `invoke_with_resume`'s
                    // success arm.
                    unsafe { relon_trace_jit::runtime::reclaim_trace_strings() };
                }
                result
            }
            TraceEntryStatus::GuardFailed => with_trace_string_reclaim(|| fallback(args_ptr)),
            TraceEntryStatus::Aborted => with_trace_string_reclaim(|| {
                tracing::warn!(
                    target: "relon::trace_install",
                    fn_id,
                    "trace Aborted; invalidating + falling back"
                );
                self.invalidate_trace(fn_id);
                fallback(args_ptr)
            }),
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
        // Pre-declare every host helper the emitter may reference,
        // keyed by `HostHookId`. We declare them up-front so the JIT
        // module's FuncId-to-symbol mapping is deterministic before
        // the trace fn claims the next free slot — otherwise the
        // emitter could end up calling FuncId 0 (the trace itself)
        // and recurse the moment any guard fires.
        //
        // `Linkage::Import` is the right shape for every entry:
        // `register_trace_runtime_symbols` wires the actual address
        // through the JIT builder before finalisation. Helpers
        // disabled at emit time (e.g. `StrConcatSealHash`, which the
        // IR emitter currently skips per the W3 +245% regression
        // fix) still get declared so a future selective-emit pass
        // can re-enable them without a fresh install pipeline.
        let i32_ty = cranelift_codegen::ir::types::I32;
        let i64_ty = cranelift_codegen::ir::types::I64;
        let host_hooks: &[(
            relon_trace_emitter::HostHookId,
            &[cranelift_codegen::ir::Type],
            &[cranelift_codegen::ir::Type],
        )] = &[
            (
                relon_trace_emitter::HostHookId::SaveDeopt,
                &[pointer_ty, i32_ty, i64_ty],
                &[],
            ),
            (
                relon_trace_emitter::HostHookId::ResolveCall,
                &[pointer_ty, i32_ty],
                &[pointer_ty],
            ),
            (
                relon_trace_emitter::HostHookId::InlineCacheLookup,
                &[pointer_ty, i32_ty, i64_ty],
                &[i64_ty],
            ),
            (
                relon_trace_emitter::HostHookId::StrConcat,
                &[pointer_ty, pointer_ty],
                &[pointer_ty],
            ),
            (
                relon_trace_emitter::HostHookId::StrContains,
                &[pointer_ty, pointer_ty],
                &[i32_ty],
            ),
            (
                relon_trace_emitter::HostHookId::StrFind,
                &[pointer_ty, pointer_ty],
                &[i64_ty],
            ),
            (
                relon_trace_emitter::HostHookId::StrSubstring,
                &[pointer_ty, i64_ty, i64_ty],
                &[pointer_ty],
            ),
            (
                relon_trace_emitter::HostHookId::StrConcatAlloc,
                &[pointer_ty, pointer_ty],
                &[pointer_ty],
            ),
            (
                relon_trace_emitter::HostHookId::StrConcatSealHash,
                &[pointer_ty],
                &[],
            ),
            (
                relon_trace_emitter::HostHookId::StrConcatNAlloc,
                &[pointer_ty, pointer_ty, pointer_ty],
                &[pointer_ty],
            ),
            (
                relon_trace_emitter::HostHookId::StrGlobMatch,
                &[pointer_ty, pointer_ty],
                &[i32_ty],
            ),
            (
                relon_trace_emitter::HostHookId::ListGet,
                &[pointer_ty, i64_ty, pointer_ty],
                &[i64_ty],
            ),
            (
                relon_trace_emitter::HostHookId::DictLookup,
                &[pointer_ty, pointer_ty, pointer_ty, i64_ty, pointer_ty],
                &[i64_ty],
            ),
            (
                relon_trace_emitter::HostHookId::DictLookupPrechecked,
                &[pointer_ty, pointer_ty, pointer_ty, pointer_ty],
                &[i64_ty],
            ),
        ];
        let mut hook_id_by_hook: std::collections::HashMap<relon_trace_emitter::HostHookId, u32> =
            std::collections::HashMap::with_capacity(host_hooks.len());
        for (hook, args, rets) in host_hooks {
            let sig = build_host_helper_signature(args, rets);
            let id = module
                .declare_function(hook.symbol(), Linkage::Import, &sig)
                .map_err(|e| TraceJitError::Module(format!("declare {}: {e}", hook.symbol())))?;
            hook_id_by_hook.insert(*hook, id.as_u32());
        }
        let get = |h: relon_trace_emitter::HostHookId| -> u32 {
            *hook_id_by_hook.get(&h).expect("hook declared above")
        };
        let hook_func_ids = HostHookFuncIds {
            save_deopt: get(relon_trace_emitter::HostHookId::SaveDeopt),
            resolve_call: get(relon_trace_emitter::HostHookId::ResolveCall),
            inline_cache_lookup: get(relon_trace_emitter::HostHookId::InlineCacheLookup),
            str_concat: get(relon_trace_emitter::HostHookId::StrConcat),
            str_contains: get(relon_trace_emitter::HostHookId::StrContains),
            str_find: get(relon_trace_emitter::HostHookId::StrFind),
            str_substring: get(relon_trace_emitter::HostHookId::StrSubstring),
            str_concat_alloc: Some(get(relon_trace_emitter::HostHookId::StrConcatAlloc)),
            // 2026-05-22: disabled to fix W3 string_concat trace_jit +245%
            // regression. Per-iter unconditional seal_hash does fx_hash
            // over the full (growing) payload, making any hot-loop concat
            // O(N²). The symbol is still declared above for a future
            // selective-emit reactivation (only when the concat result is
            // statically a dict-key feed).
            str_concat_seal_hash: None,
            str_concat_n_alloc: Some(get(relon_trace_emitter::HostHookId::StrConcatNAlloc)),
            str_glob_match: Some(get(relon_trace_emitter::HostHookId::StrGlobMatch)),
            list_get: Some(get(relon_trace_emitter::HostHookId::ListGet)),
            dict_lookup: Some(get(relon_trace_emitter::HostHookId::DictLookup)),
            dict_lookup_prechecked: Some(get(
                relon_trace_emitter::HostHookId::DictLookupPrechecked,
            )),
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

impl From<relon_bytecode::RecordingRegistrationData> for RecordingRegistration {
    /// PC-alignment follow-up #3: bridge the bytecode evaluator's
    /// IR-body view into the native registration shape so hosts that
    /// drive a `BytecodeEvaluator` can register the recorder against
    /// the **same** IR the bytecode compile pass walked.
    ///
    /// The bytecode crate is dependency-free from cranelift, so it
    /// returns a parallel [`relon_bytecode::RecordingRegistrationData`]
    /// shape rather than reach into this crate. The conversion is a
    /// zero-cost field move; both shapes carry the same two
    /// `(body, param_tys)` slots.
    fn from(data: relon_bytecode::RecordingRegistrationData) -> Self {
        Self {
            body: data.body,
            param_tys: data.param_tys,
        }
    }
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
    // Tier 1b: companion seal-hash helper for the inline `StrConcat`
    // short-rhs lowering. Runs after the unrolled rhs `store.i8` tail
    // so the freshly-built `StringRef`'s cached fx_hash field matches
    // the now-complete payload — keeps the dict-lookup IC fast path
    // viable on cross-trace dict accesses against the concat result.
    builder.symbol(
        relon_trace_emitter::HostHookId::StrConcatSealHash.symbol(),
        relon_trace_jit::runtime::__relon_str_concat_seal_hash as *const u8,
    );
    // #168: N-operand single-allocation concat helper. The inline
    // `TraceOp::StrConcatN` lowering calls this once per op so hot
    // 3-/4-way string concat chains stop bouncing through `N - 1`
    // pair-wise extern shim invocations. The seal-hash helper above
    // closes the digest gap on the result `StringRef`.
    builder.symbol(
        relon_trace_emitter::HostHookId::StrConcatNAlloc.symbol(),
        relon_trace_jit::runtime::__relon_str_concat_n_alloc as *const u8,
    );
    // 2026-05-21 Tier-2: glob_match helper. Body lives in
    // `crate::trace_glob_helper` so the trace JIT runtime crate stays
    // free of a `relon-ir` dependency; the symbol still resolves via
    // the same JITBuilder symbol table the other str_* shims use.
    builder.symbol(
        relon_trace_emitter::HostHookId::StrGlobMatch.symbol(),
        crate::trace_glob_helper::__relon_str_glob_match as *const u8,
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
        relon_trace_jit::runtime::__relon_trace_dict_lookup_v2 as *const u8,
    );
    // F-D8-E.2: prechecked dict lookup helper. Paired with
    // `TraceOp::DictShapeGuard` ahead of the call site.
    builder.symbol(
        relon_trace_emitter::HostHookId::DictLookupPrechecked.symbol(),
        relon_trace_jit::runtime::__relon_trace_dict_lookup_prechecked_v2 as *const u8,
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
        RELON_HOT_COUNTERS[id as usize].store(7, Ordering::Relaxed);
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

    /// #173 regression: prior to the AtomicU32 conversion the
    /// counter slots were plain `u32` inside an `UnsafeCell`, so two
    /// threads hammering the same slot would race and lose
    /// increments (UB strictly, lost updates observably). With the
    /// `AtomicU32` + `fetch_add` lowering the final value must equal
    /// `THREADS * BUMPS_PER_THREAD` exactly.
    ///
    /// We pin to a fn_id at the top of the table so we can't
    /// collide with any other test in this module that touches
    /// low-numbered slots.
    #[test]
    fn hot_counter_multi_thread_no_lost_updates() {
        use std::sync::atomic::{AtomicBool, Ordering as ThreadOrd};
        use std::sync::Arc;
        use std::thread;

        const THREADS: usize = 8;
        const BUMPS_PER_THREAD: u32 = 5_000;
        let fn_id = (MAX_FN_ID - 2) as u32;

        hot_counter_reset(fn_id);
        assert_eq!(hot_counter_peek(fn_id), 0);

        // Release barrier so every worker starts hammering the slot
        // at roughly the same wall-clock instant, maximising the
        // chance of overlapping RMWs.
        let go = Arc::new(AtomicBool::new(false));
        let mut handles = Vec::with_capacity(THREADS);
        for _ in 0..THREADS {
            let go = Arc::clone(&go);
            handles.push(thread::spawn(move || {
                while !go.load(ThreadOrd::Acquire) {
                    std::hint::spin_loop();
                }
                for _ in 0..BUMPS_PER_THREAD {
                    RELON_HOT_COUNTERS[fn_id as usize].fetch_add(1, Ordering::Relaxed);
                }
            }));
        }
        go.store(true, ThreadOrd::Release);
        for h in handles {
            h.join().expect("worker panicked");
        }

        let expected = (THREADS as u32) * BUMPS_PER_THREAD;
        assert_eq!(
            hot_counter_peek(fn_id),
            expected,
            "atomic counter must not lose updates under contention"
        );
        hot_counter_reset(fn_id);
    }

    // ---- Review #178 P2: invoke_materialised / invoke_with_string_reclaim ----

    /// Drive a `ConstI64 + Return` trace through `invoke_materialised`
    /// (with `ReturnKind::I64`) and check the helper returns the
    /// owned `Int` variant without touching the trace string arena.
    #[test]
    fn invoke_materialised_i64_unwraps_const_return() {
        let state = TraceJitState::new();
        let recorder = make_const_return_recorder(123);
        let trace_fn = state
            .jit_compile_trace_for_fn(0, recorder)
            .expect("compile");
        let mut ctx = TraceContext::with_hooks(64, default_host_hooks());
        let val = unsafe {
            trace_fn.invoke_materialised(&mut ctx as *mut _, std::ptr::null(), ReturnKind::I64)
        }
        .expect("trace must Succeed");
        assert_eq!(val, MaterialisedValue::Int(123));
    }

    /// Drive a string-returning trace through `invoke_materialised`
    /// (with `ReturnKind::String`) and verify the returned `SmolStr`
    /// outlives the post-invoke reclaim — i.e. the payload bytes are
    /// owned by the SmolStr, not borrowed from the arena.
    #[test]
    fn invoke_materialised_string_outlives_reclaim() {
        // Build a `LocalGet(0) -> Return` trace; feed it a host-side
        // arena `StringRef::from_static("hello-relon")` and let the
        // materialiser copy the payload into a SmolStr.
        let mut buffer = relon_trace_jit::TraceBuffer::new();
        let v = buffer.fresh_ssa();
        buffer.append(relon_trace_jit::TraceOp::LocalGet {
            dst: v,
            slot_idx: 0,
        });
        buffer.append(relon_trace_jit::TraceOp::Return { value: v });
        let state = TraceJitState::new();
        let trace_fn = state
            .jit_compile_buffer_for_fn(7, buffer)
            .expect("LocalGet+Return trace must install");

        // Host-side input lives in the trace string arena; the
        // materialiser must reclaim it after copying.
        let arena_before = relon_trace_jit::runtime::trace_string_arena_len();
        let input = relon_trace_jit::runtime::StringRef::from_static("hello-relon");
        let args = [input as u64];
        let arena_after_input = relon_trace_jit::runtime::trace_string_arena_len();
        assert_eq!(
            arena_after_input,
            arena_before + 1,
            "from_static registers one allocation"
        );

        let mut ctx = TraceContext::with_hooks(64, default_host_hooks());
        let val = unsafe {
            trace_fn.invoke_materialised(&mut ctx as *mut _, args.as_ptr(), ReturnKind::String)
        }
        .expect("trace must Succeed");

        // SmolStr must own its bytes — the arena was drained, so a
        // dangling pointer would tripwire `assert_eq` here.
        match val {
            MaterialisedValue::String(s) => {
                assert_eq!(s.as_str(), "hello-relon");
                assert!(
                    s.is_inline(),
                    "payload fits in 22-byte inline slot, should stay off the heap"
                );
            }
            other => panic!("expected MaterialisedValue::String, got {other:?}"),
        }
        assert_eq!(
            relon_trace_jit::runtime::trace_string_arena_len(),
            0,
            "invoke_materialised must drain the arena after copying the payload"
        );
    }

    /// `invoke_with_string_reclaim` exposes the raw status code +
    /// `result_slot` and lets the closure render a custom shape
    /// before the reclaim pass runs.
    #[test]
    fn invoke_with_string_reclaim_passes_raw_then_reclaims() {
        let state = TraceJitState::new();
        let recorder = make_const_return_recorder(-7);
        let trace_fn = state
            .jit_compile_trace_for_fn(1, recorder)
            .expect("compile");
        let mut ctx = TraceContext::with_hooks(64, default_host_hooks());
        // Seed the arena with one allocation so we can confirm the
        // post-closure reclaim pass actually drained the recorder.
        let _seed = relon_trace_jit::runtime::StringRef::from_static("seed");
        let before = relon_trace_jit::runtime::trace_string_arena_len();
        assert!(before >= 1);
        let raw = unsafe {
            trace_fn.invoke_with_string_reclaim(&mut ctx as *mut _, std::ptr::null(), |raw| {
                (raw.status, raw.result)
            })
        };
        assert_eq!(raw.0, 0, "trace must Succeed");
        assert_eq!(raw.1 as i64, -7, "result slot must carry the ConstI64");
        assert_eq!(
            relon_trace_jit::runtime::trace_string_arena_len(),
            0,
            "post-closure reclaim must drain the arena"
        );
    }

    /// Materialise dispatch covers I64, F64, and Bool shapes without
    /// touching `unsafe` arena pointer arithmetic in the caller.
    #[test]
    fn materialise_success_slot_handles_scalar_kinds() {
        assert_eq!(
            materialise_success_slot(42u64, ReturnKind::I64),
            MaterialisedValue::Int(42)
        );
        let f_bits = (1.5_f64).to_bits();
        assert_eq!(
            materialise_success_slot(f_bits, ReturnKind::F64),
            MaterialisedValue::Float(1.5)
        );
        assert_eq!(
            materialise_success_slot(0u64, ReturnKind::Bool),
            MaterialisedValue::Bool(false)
        );
        assert_eq!(
            materialise_success_slot(1u64, ReturnKind::Bool),
            MaterialisedValue::Bool(true)
        );
    }

    /// Null `*const StringRef` returns surface as an empty
    /// `SmolStr` rather than a panic — matches the str_ops shim
    /// contract for null inputs.
    #[test]
    fn materialise_string_null_pointer_returns_empty() {
        let v = materialise_success_slot(0u64, ReturnKind::String);
        match v {
            MaterialisedValue::String(s) => assert!(s.is_empty()),
            other => panic!("expected empty MaterialisedValue::String, got {other:?}"),
        }
    }
}
