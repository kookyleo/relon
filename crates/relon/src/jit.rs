//! Dart-style canonical JIT entry — [`JitEvaluator`].
//!
//! Pairs with [`relon_codegen_native::AotEvaluator`] to expose a
//! two-mode user-facing surface (JIT vs AOT) over the three internal
//! tiers Relon already ships:
//!
//! * **`JitTier::TreeWalk`** — initial interpretation + fallback for
//!   the four non-`run_main` `Evaluator` methods. Always present
//!   because every other tier (bytecode VM, trace JIT) can only
//!   answer `run_main`, and `eval` / `eval_root` / `force_thunk` /
//!   `invoke_closure` need an AST-aware backend.
//! * **`JitTier::Bytecode`** — the M2-A scalar-envelope stack VM
//!   (`relon_bytecode::BytecodeEvaluator`). Populated lazily on the
//!   first `run_main` if `BytecodeEvaluator::from_source` accepts the
//!   shape; rejected sources transparently fall through to the
//!   tree-walker so the dispatcher never panics on out-of-envelope
//!   workloads.
//! * **`JitTier::Trace`** — the cranelift-emitted hot-trace JIT.
//!   Activated automatically once the bytecode tier's per-`fn_id`
//!   hot-counter saturates: the wrapper wires
//!   [`relon_codegen_native::CraneliftHotTrigger`] (recorder kick-off)
//!   and [`relon_codegen_native::CraneliftTraceLookup`] (dispatcher
//!   switch) onto the bytecode evaluator at construction time so the
//!   first `run_main` past the threshold (the bytecode VM's default
//!   [`relon_bytecode::DEFAULT_HOT_THRESHOLD`] = 1000, picked to keep
//!   warm-up cycles bounded for the cold caller while leaving any
//!   real hot loop ample headroom to trip) records, the recorder
//!   lowers + the cranelift backend emits, and subsequent
//!   invocations route through the installed trace via the
//!   dispatcher-switch bypass.
//!
//! ## Tier escalation lifecycle
//!
//! 1. `JitEvaluator::new` allocates a unique `fn_id` from the JIT
//!    wrapper's slot range and stamps it on the bytecode evaluator's
//!    compiled function. It also re-runs the parse + analyze + IR
//!    lowering pipeline to recover the IR op stream and parameter
//!    types, then calls
//!    [`relon_codegen_native::register_recording`] so the recorder
//!    has a body to walk when the trigger fires.
//! 2. The bytecode evaluator runs normally for the first
//!    [`relon_bytecode::DEFAULT_HOT_THRESHOLD`] invocations
//!    (`active_tier` reports `Bytecode`).
//! 3. On the threshold-crossing invocation the bytecode dispatch
//!    prologue trips the `CraneliftHotTrigger`, which drives the
//!    recorder + optimiser + emitter pipeline; the resulting
//!    [`relon_codegen_native::JITedTraceFn`] is installed in the
//!    process-global [`relon_codegen_native::TraceJitState`].
//! 4. Subsequent invocations consult the dispatcher switch via the
//!    `CraneliftTraceLookup` adapter on the same evaluator; a hit
//!    bypasses the bytecode dispatch loop entirely and returns the
//!    trace fn's `result_slot` (`active_tier` flips to `Trace`).
//!    Guard failure routes through the bytecode VM's partial-resume
//!    path; the trace stays installed so steady-state workloads keep
//!    the bypass.
//! 5. On drop, the wrapper releases the `fn_id` (clears the recorder
//!    registration + invalidates the installed trace) so a re-built
//!    evaluator over the same source doesn't observe stale state.
//!
//! Mirrors the canonical tier-escalation shape from LuaJIT /
//! Dart VM (interpreter → bytecode → tracing JIT) and lines up with
//! the `naming-refactor-completion.md` Open Followups §1 mandate:
//! "make `JitEvaluator::run_main` auto-escalate via the hot-counter
//! threshold so the `relon_jit` bench row catches up to the
//! `relon_trace_jit` row on hot-loop workloads."
//!
//! Hosts that want the auto-tier flavour pair this wrapper with
//! [`crate::Backend::Auto`] / [`crate::AutoEvaluator`], which already
//! routes `run_main` through cranelift-AOT lazily. The two surfaces
//! coexist: AOT is a "compile once up-front" path; JIT is a "warm up
//! through tiers as the workload turns out to be hot."

use std::collections::HashMap;
use std::sync::Arc;

use relon_eval_api::{ClosureData, Evaluator, RuntimeError, Scope, Thunk, Value};
use relon_evaluator::TreeWalkEvaluator;
use relon_parser::Node;

use crate::BackendError;

/// Internal tier classification surfaced via [`JitEvaluator::active_tier`].
/// Mirrors the design-doc taxonomy so observability / test hooks can
/// assert the dispatcher chose the expected backend without poking at
/// concrete evaluator types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitTier {
    /// Tree-walking interpreter. Initial tier; also the fallback for
    /// the four `Evaluator` methods that aren't `run_main` (`eval` /
    /// `eval_root` / `force_thunk` / `invoke_closure`).
    TreeWalk,
    /// Stack-based bytecode VM. Selected when the source survives
    /// `BytecodeEvaluator::from_source`'s M2-A scalar envelope check
    /// and either no trace has been installed yet, or the build was
    /// compiled without the `cranelift-aot` feature so the trace tier
    /// was never wired in the first place.
    Bytecode,
    /// Cranelift-emitted hot-trace JIT. Reported once the bytecode
    /// tier's hot-counter trips, the trace recorder lowers the IR
    /// body, and the cranelift backend publishes the compiled trace
    /// into the process-global registry. Subsequent `run_main` calls
    /// take the dispatcher-switch bypass (no bytecode dispatch-loop
    /// ticks). Falling back to `Bytecode` on this tier is observable
    /// via [`relon_codegen_native::global_trace_jit_state`] —
    /// `active_tier` re-reads the install state every call so
    /// invalidation (deopt → trace evicted) is reflected
    /// immediately.
    Trace,
}

/// JIT-wrapper-reserved `fn_id` slot range. Picked to avoid the slots
/// the bench harness (`MAX_FN_ID - 10..-17`) and the existing trace
/// e2e tests (1..200, 850, 1001..1004) take. Stepping the allocator
/// monotonically through this range covers a few hundred unique
/// `JitEvaluator` instances per process run; the recycle path on
/// drop returns the slot to the free list so steady-state hosts
/// never exhaust it. The range was sized for a several-hundred-eval
/// process (bench panel + LSP smoke tests run a few dozen each); if
/// a host exceeds it the wrapper degrades gracefully by skipping the
/// trace install (the bytecode + tree-walk tiers stay live).
#[cfg(feature = "cranelift-aot")]
const JIT_FN_ID_MIN: u32 = 300;
#[cfg(feature = "cranelift-aot")]
const JIT_FN_ID_MAX: u32 = 768;

/// Process-global allocator for [`JIT_FN_ID_MIN`]..[`JIT_FN_ID_MAX`].
/// We use a `Mutex<Vec<u32>>` free list rather than a monotonic
/// `AtomicU32` so dropped instances recycle their slot — important
/// for hosts that build / discard many `JitEvaluator`s over a
/// process lifetime (e.g. an LSP server materialising one per
/// document).
#[cfg(feature = "cranelift-aot")]
fn fn_id_pool() -> &'static std::sync::Mutex<Vec<u32>> {
    use std::sync::OnceLock;
    static POOL: OnceLock<std::sync::Mutex<Vec<u32>>> = OnceLock::new();
    POOL.get_or_init(|| {
        // Seed with the reserved range in reverse so `pop()` returns
        // ids in ascending order — gives test failure messages stable
        // ids when only a handful of evaluators have been built.
        let mut v: Vec<u32> = (JIT_FN_ID_MIN..JIT_FN_ID_MAX).collect();
        v.reverse();
        std::sync::Mutex::new(v)
    })
}

/// Pop the next free `fn_id` from the wrapper-reserved pool; returns
/// `None` if the pool is exhausted (the caller falls back to the
/// trace-less hot path).
#[cfg(feature = "cranelift-aot")]
fn alloc_jit_fn_id() -> Option<u32> {
    fn_id_pool().lock().expect("jit fn_id pool poisoned").pop()
}

/// Return a previously-allocated `fn_id` to the free pool. Called
/// from `JitEvaluator::drop` after clearing the recorder registration
/// and invalidating any installed trace, so the next allocator hit
/// for this id starts from a clean registry state.
#[cfg(feature = "cranelift-aot")]
fn release_jit_fn_id(id: u32) {
    if let Ok(mut pool) = fn_id_pool().lock() {
        pool.push(id);
    }
}

/// Dart-style canonical JIT entry. Wraps the tree-walker (always
/// present) plus an optional bytecode VM (populated when the source
/// survives the M2-A envelope check). When the `cranelift-aot`
/// feature is on, the bytecode tier is also wired with the trace
/// recorder kick-off + dispatcher switch so `run_main` auto-escalates
/// to the cranelift-emitted trace JIT once the per-`fn_id`
/// hot-counter saturates — see the module-level docs for the full
/// lifecycle.
///
/// Construct via [`JitEvaluator::new`] or
/// [`crate::new_evaluator`] with [`crate::Backend::Jit`].
pub struct JitEvaluator {
    /// Tree-walking interpreter — always live. Boxed so the wrapper
    /// stays `Send + Sync` without bleeding `TreeWalkEvaluator`'s
    /// generics outward.
    tree_walk: Box<TreeWalkEvaluator>,
    /// Optional bytecode-VM tier. `None` when the source falls outside
    /// the M2-A envelope (closures / list / dict / stdlib) or when
    /// the bytecode setup raised a non-envelope error; either way the
    /// wrapper transparently routes `run_main` back through the
    /// tree-walker.
    bytecode: Option<Box<dyn Evaluator>>,
    /// Per-instance `fn_id` allocated from the JIT wrapper pool when
    /// the bytecode tier was built **and** the `cranelift-aot`
    /// feature is enabled. `None` means either the bytecode tier
    /// rejected the source, or the feature is off, or the pool was
    /// exhausted at construction time — in any of those cases the
    /// wrapper behaves identically to the v1 "no auto-tier" shape.
    #[cfg(feature = "cranelift-aot")]
    fn_id: Option<u32>,
}

impl JitEvaluator {
    /// Build a [`JitEvaluator`] over `source`. The tree-walker tier is
    /// constructed eagerly (cheap, ~1 ms). The bytecode tier is also
    /// built eagerly today — `BytecodeEvaluator::from_source` runs the
    /// same parse / analyse / lower pipeline the tree-walker already
    /// drove, so a separate lazy slot would just add bookkeeping
    /// without saving cold-start cycles for the hosts the auto-tier
    /// path optimises for. Sources outside the M2-A envelope skip the
    /// bytecode build entirely and leave the slot at `None`.
    ///
    /// When the `cranelift-aot` feature is enabled and the bytecode
    /// tier survived setup, this also wires the trace-recorder
    /// trigger + dispatcher-switch adapter against a per-instance
    /// `fn_id`, and registers the lowered IR body with the recorder
    /// registry so the hot-counter promotion can produce a real
    /// trace. The trace install / dispatch is transparent to the
    /// `Evaluator` surface — only `active_tier` shifts to
    /// [`JitTier::Trace`] once the trace publishes.
    pub fn new(source: &str) -> std::result::Result<Self, BackendError> {
        let node =
            relon_parser::parse_document(source).map_err(|e| BackendError::Parse(e.to_string()))?;
        let tree_walk = crate::build_tree_walk_evaluator_from_parsed(node)?;
        let bytecode_evaluator = relon_bytecode::BytecodeEvaluator::from_source(source).ok();

        #[cfg(feature = "cranelift-aot")]
        let (bytecode_boxed, fn_id) = wire_trace_tier(source, bytecode_evaluator);

        #[cfg(not(feature = "cranelift-aot"))]
        let bytecode_boxed: Option<Box<dyn Evaluator>> =
            bytecode_evaluator.map(|ev| Box::new(ev) as Box<dyn Evaluator>);

        Ok(Self {
            tree_walk: Box::new(tree_walk),
            bytecode: bytecode_boxed,
            #[cfg(feature = "cranelift-aot")]
            fn_id,
        })
    }

    /// Returns the tier the dispatcher would currently route a
    /// `run_main` call through. The classification is *live* — it
    /// re-reads the trace registry on every call, so a deopt that
    /// invalidates the installed trace flips the report back to
    /// `Bytecode` on the next observation without any host action.
    ///
    /// Tier ordering (highest-priority first):
    ///
    /// 1. [`JitTier::Trace`] — only when the `cranelift-aot` feature
    ///    is on, an `fn_id` was allocated, and
    ///    [`relon_codegen_native::TraceJitState::lookup_trace`]
    ///    returns a hit. The hot-counter promotion is the only path
    ///    that publishes a trace at this `fn_id`, so a `Trace` report
    ///    is also evidence the recorder + emitter pipeline succeeded.
    /// 2. [`JitTier::Bytecode`] — bytecode tier built; either the
    ///    counter hasn't tripped yet, the recorder bailed out, or the
    ///    build doesn't have `cranelift-aot`.
    /// 3. [`JitTier::TreeWalk`] — bytecode tier rejected the source's
    ///    shape; the wrapper falls back to the tree-walker for
    ///    `run_main` as well as the four non-`run_main` methods.
    pub fn active_tier(&self) -> JitTier {
        #[cfg(feature = "cranelift-aot")]
        if let Some(id) = self.fn_id {
            let state = relon_codegen_native::global_trace_jit_state();
            if state.lookup_trace(id).is_some() {
                return JitTier::Trace;
            }
        }
        if self.bytecode.is_some() {
            JitTier::Bytecode
        } else {
            JitTier::TreeWalk
        }
    }

    /// Whether the bytecode tier survived setup. Mirrors
    /// [`Self::active_tier`] for the common boolean question hosts ask
    /// in smoke tests. Note this stays `true` after a trace promotes —
    /// the bytecode tier is the trace's deopt landing pad, so it
    /// never goes away while the JIT entry is alive.
    pub fn has_bytecode_tier(&self) -> bool {
        self.bytecode.is_some()
    }
}

#[cfg(feature = "cranelift-aot")]
impl Drop for JitEvaluator {
    /// Release the per-instance `fn_id` back to the wrapper pool, but
    /// only after evicting both the recorder registration and any
    /// installed trace. Dropping the evaluator while the registry
    /// still pointed at this id would let a future allocator hit
    /// observe stale recording state (or, worse, a stale trace whose
    /// IR pointer + slot count belonged to a freed bytecode body).
    ///
    /// The two cleanup calls are best-effort: a panicking recorder
    /// driver could in theory leave the registry in a non-empty
    /// state, and we don't want `Drop` to panic over it. Both
    /// underlying APIs are infallible from this side anyway
    /// (insert-then-remove map ops).
    fn drop(&mut self) {
        if let Some(id) = self.fn_id.take() {
            let _ = relon_codegen_native::clear_recording(id);
            let state = relon_codegen_native::global_trace_jit_state();
            let _ = state.invalidate_trace(id);
            release_jit_fn_id(id);
        }
    }
}

/// Construct the dispatcher-wired bytecode evaluator + matching
/// `fn_id`. Returns `(boxed_evaluator, fn_id)` where `fn_id` is
/// `Some` only when the recorder hooks were successfully installed.
/// Falls back to a trace-less boxing whenever any step trips
/// (param-type recovery failed, BcOp → IR Op conversion bailed,
/// pool exhaustion) so the wrapper never panics over a hookup
/// detail.
#[cfg(feature = "cranelift-aot")]
fn wire_trace_tier(
    source: &str,
    bytecode_evaluator: Option<relon_bytecode::BytecodeEvaluator>,
) -> (Option<Box<dyn Evaluator>>, Option<u32>) {
    let Some(ev) = bytecode_evaluator else {
        return (None, None);
    };

    // Convert the bytecode VM's compiled op stream back into the
    // recorder-friendly IR `TaggedOp` form. The lowered IR we'd get
    // straight out of `lower_workspace_single` is wasm-handshake
    // shaped (params `[i32, i32, i32, i32, i64]`, body uses
    // `LoadField` against an `in_ptr` the wasm host materialises) —
    // the bytecode VM's call ABI passes args as a packed `u64`
    // slice instead, so the recorder body needs to read user args
    // through `Op::LocalGet(slot_idx)`. The bytecode compile pass
    // already did exactly that translation when it produced
    // `BcOp::LocalGet(i)` / `BcOp::LocalSet(i)`; we just project the
    // BcOp stream into the recorder's IR-Op vocabulary and re-use
    // the recorder/emitter pipeline.
    //
    // The converter only handles the M2-A scalar straight-line
    // envelope (arith / cmp / const / local / return). Anything
    // with a jump or call bails out so the wrapper degrades to
    // bytecode-only without installing a trace whose recorder walk
    // would abort anyway.
    let recorder_body = match bcops_to_recorder_body(&ev.function().ops) {
        Some(b) => b,
        None => {
            tracing::debug!(
                target: "relon::jit_evaluator",
                "tier escalation: bytecode body contains ops outside the trace-recorder envelope (jumps / calls / etc); skipping trace install"
            );
            return (Some(Box::new(ev) as Box<dyn Evaluator>), None);
        }
    };

    // Sympathetic-gate against the runtime trace dispatcher's
    // `TINY_TRACE_OP_THRESHOLD` (= 8): bodies smaller than the
    // threshold install a trace the runtime always routes past
    // (because the per-invoke trace-entry prologue dwarfs the body)
    // — wiring the dispatcher switch in that case is pure overhead
    // (one extra `lookup_trace` per `run_main`, plus the bytecode
    // VM's hot-counter prologue cost). The W12 workload sits exactly
    // at this size, so the bench panel showed a ~38% slowdown on
    // the row before this gate landed. Skipping the trace install
    // for trivial bodies keeps those rows at bytecode-equivalent
    // speed instead of paying for a trace that never dispatches.
    //
    // Loops + multi-statement bodies clear the gate easily once
    // recorder support widens past straight-line; until then the
    // gate makes the JIT escalation a pure win (no slowdown on
    // bodies it can't help) at the cost of leaving W12-shape
    // single-expression mains on the bytecode tier.
    if recorder_body.len() < relon_codegen_native::TINY_TRACE_OP_THRESHOLD {
        tracing::debug!(
            target: "relon::jit_evaluator",
            body_len = recorder_body.len(),
            threshold = relon_codegen_native::TINY_TRACE_OP_THRESHOLD,
            "tier escalation: recorder body below TINY_TRACE_OP_THRESHOLD; skipping trace install to avoid dispatcher-switch overhead"
        );
        return (Some(Box::new(ev) as Box<dyn Evaluator>), None);
    }

    let param_tys = match user_param_tys_from_source(source) {
        Some(tys) => tys,
        None => {
            tracing::debug!(
                target: "relon::jit_evaluator",
                "tier escalation: user-param type recovery failed; skipping trace install"
            );
            return (Some(Box::new(ev) as Box<dyn Evaluator>), None);
        }
    };
    let body = recorder_body;

    let Some(fn_id) = alloc_jit_fn_id() else {
        // Pool exhausted: enough live JitEvaluator instances are
        // already holding ids that we can't escalate this one. Log
        // at `warn` because it's a real loss of tier coverage, but
        // keep the bytecode tier live so the host still gets a
        // correct (just not auto-promoting) `run_main`.
        tracing::warn!(
            target: "relon::jit_evaluator",
            jit_pool_range = ?(JIT_FN_ID_MIN..JIT_FN_ID_MAX),
            "tier escalation: fn_id pool exhausted; bytecode tier stays without trace hooks"
        );
        return (Some(Box::new(ev) as Box<dyn Evaluator>), None);
    };

    // Register the recorder body on the **current thread** — the
    // registry is thread_local (see
    // `relon_codegen_native::trace_install::RECORDING_REGISTRY` docs:
    // per-thread recorder state machines mirror the design's
    // §3.4 stance). Multi-threaded hosts that dispatch `run_main`
    // off a different thread than `new` will silently skip the
    // recorder path until they touch `run_main` from the thread
    // that owns the registration — this is a documented limitation
    // of v1; if it becomes a pain point the registry can move to a
    // process-global `DashMap` keyed by `(thread_id, fn_id)`.
    let prior = relon_codegen_native::register_recording(
        fn_id,
        relon_codegen_native::RecordingRegistration {
            body,
            param_tys,
            ..Default::default()
        },
    );
    if prior.is_some() {
        // Sanity: a pool-allocated id should never already be in the
        // registry. If we see one, surface it loud — it means the
        // pool's free-list invariant is broken (double-release? race
        // on a non-Mutex op?). We carry on; the new registration
        // overwrote the stale one.
        tracing::warn!(
            target: "relon::jit_evaluator",
            fn_id,
            "tier escalation: recorder registry already held an entry for our pool-allocated fn_id; overwrote it"
        );
    }

    let trigger: relon_bytecode::HotTraceTriggerHandle =
        Arc::new(relon_codegen_native::CraneliftHotTrigger);
    let lookup: relon_bytecode::InstalledTraceLookupHandle =
        Arc::new(relon_codegen_native::CraneliftTraceLookup);
    let ev_wired = ev
        .with_fn_id(fn_id)
        .with_hot_trigger(trigger)
        .with_trace_lookup(lookup);

    (Some(Box::new(ev_wired) as Box<dyn Evaluator>), Some(fn_id))
}

/// Recover the user-declared `#main` parameter IR types from a
/// freshly-parsed `source`. The bytecode VM's calling convention
/// passes args as one packed `u64` per declared `#main` parameter
/// (see `BytecodeEvaluator::pack_args`); the recorder reads them
/// back through `Op::LocalGet(slot)` against the same array, so the
/// `param_tys` vector handed to `register_recording` must match the
/// declared-arg type list (and **not** the wasm-handshake
/// `[i32, i32, i32, i32, i64]` shape the IR module's entry function
/// carries).
///
/// Returns `None` when any pipeline step before the type extraction
/// fails — the caller treats this as "no trace hookup", same as a
/// recorder-envelope BcOp bail.
#[cfg(feature = "cranelift-aot")]
fn user_param_tys_from_source(source: &str) -> Option<Vec<relon_ir::IrType>> {
    use relon_eval_api::schema_canonical::TypeRepr;
    let ast = relon_parser::parse_document(source).ok()?;
    let analyzed = relon_analyzer::analyze(&ast);
    if analyzed.has_errors() {
        return None;
    }
    let lowered = relon_ir::lower_workspace_single(&analyzed, &ast).ok()?;
    let tys = lowered
        .main_schema
        .fields
        .iter()
        .map(|f| match &f.ty {
            TypeRepr::Int => relon_ir::IrType::I64,
            TypeRepr::Float => relon_ir::IrType::F64,
            TypeRepr::Bool => relon_ir::IrType::Bool,
            TypeRepr::Null => relon_ir::IrType::Null,
            TypeRepr::String => relon_ir::IrType::String,
            // The recorder doesn't know how to walk non-scalar
            // args; the bytecode tier would have rejected the source
            // before we got here, but keep the fallback honest.
            _ => relon_ir::IrType::I64,
        })
        .collect();
    Some(tys)
}

/// Project the bytecode VM's compiled op stream into the recorder's
/// IR `TaggedOp` vocabulary. The converter is deliberately
/// conservative: any BcOp outside the recorder-friendly scalar
/// straight-line envelope (jumps, calls, traps, list / dict / string
/// ops) trips the bail-out and the caller skips trace install.
///
/// Semantic notes:
///
/// * `BcOp::LocalSet(_)` immediately followed by `BcOp::Return` is
///   the bytecode VM's "store-into-return-slot then exit" idiom; the
///   recorder doesn't model a return slot (it consumes the top of
///   stack at `Op::Return`), so the `LocalSet` is dropped. This
///   matches the hand-built recorder bodies the bench harness uses
///   (e.g. `w12_recorder_body` ends `[..., Add(I64), Return]` with
///   no `LocalSet`).
///
/// * `BcOp::AddI64` etc. carry no `IrType` payload — the bytecode VM
///   monomorphises every arith op into the per-type variant at
///   compile time. We re-attach `IrType::I64` / `IrType::F64` based
///   on the variant.
///
/// Returns `None` on any unsupported BcOp; the caller degrades to
/// bytecode-only.
#[cfg(feature = "cranelift-aot")]
fn bcops_to_recorder_body(ops: &[relon_bytecode::BcOp]) -> Option<Vec<relon_ir::TaggedOp>> {
    use relon_bytecode::BcOp;
    use relon_ir::{IrType, Op, TaggedOp};
    use relon_parser::TokenRange;

    let tag = |op: Op| TaggedOp {
        op,
        range: TokenRange::default(),
    };

    let mut out: Vec<TaggedOp> = Vec::with_capacity(ops.len());
    for (i, op) in ops.iter().enumerate() {
        // Skip the return-slot store: a `LocalSet(_)` immediately
        // before `Return` is the bytecode VM's epilogue for writing
        // the entry's scalar return slot. The recorder consumes the
        // top of stack at `Return`, so the store must NOT appear in
        // the recorder body.
        if matches!(op, BcOp::LocalSet(_)) && ops.get(i + 1) == Some(&BcOp::Return) {
            continue;
        }
        let ir_op = match op {
            BcOp::ConstI64(v) => Op::ConstI64(*v),
            BcOp::ConstI32(v) => Op::ConstI64(*v as i64),
            BcOp::LocalGet(idx) => Op::LocalGet(*idx),
            // A `LocalSet` not adjacent to `Return` is a let-binding
            // write; the recorder uses `LetSet` for that shape.
            BcOp::LocalSet(idx) => Op::LetSet {
                idx: *idx,
                ty: IrType::I64,
            },
            BcOp::AddI64 => Op::Add(IrType::I64),
            BcOp::SubI64 => Op::Sub(IrType::I64),
            BcOp::MulI64 => Op::Mul(IrType::I64),
            BcOp::DivI64 => Op::Div(IrType::I64),
            BcOp::ModI64 => Op::Mod(IrType::I64),
            BcOp::AddF64 => Op::Add(IrType::F64),
            BcOp::SubF64 => Op::Sub(IrType::F64),
            BcOp::MulF64 => Op::Mul(IrType::F64),
            BcOp::DivF64 => Op::Div(IrType::F64),
            BcOp::ModF64 => Op::Mod(IrType::F64),
            BcOp::EqI64 => Op::Eq(IrType::I64),
            BcOp::NeI64 => Op::Ne(IrType::I64),
            BcOp::LtI64 => Op::Lt(IrType::I64),
            BcOp::LeI64 => Op::Le(IrType::I64),
            BcOp::GtI64 => Op::Gt(IrType::I64),
            BcOp::GeI64 => Op::Ge(IrType::I64),
            BcOp::EqF64 => Op::Eq(IrType::F64),
            BcOp::NeF64 => Op::Ne(IrType::F64),
            BcOp::LtF64 => Op::Lt(IrType::F64),
            BcOp::LeF64 => Op::Le(IrType::F64),
            BcOp::GtF64 => Op::Gt(IrType::F64),
            BcOp::GeF64 => Op::Ge(IrType::F64),
            BcOp::Return => Op::Return,
            // Anything else (jumps, calls, traps, list / dict / str)
            // is outside the recorder's straight-line envelope for
            // v1 of this wrapper. Surface as None so the caller
            // skips the trace install and the bytecode tier stays
            // the canonical dispatch path.
            _ => return None,
        };
        out.push(tag(ir_op));
    }
    Some(out)
}

impl Evaluator for JitEvaluator {
    fn eval(&self, node: &Node, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        // Only the tree-walker exposes arbitrary-node evaluation; the
        // bytecode and trace tiers are `run_main`-only.
        self.tree_walk.eval(node, scope)
    }

    fn eval_root(&self, scope: &Arc<Scope>) -> Result<Value, RuntimeError> {
        // Library / static-config path; tree-walker always.
        self.tree_walk.eval_root(scope)
    }

    fn run_main(&self, args: HashMap<String, Value>) -> Result<Value, RuntimeError> {
        // Dispatch order: bytecode tier first (it owns both the
        // hot-counter prologue that promotes us to Trace and the
        // dispatcher-switch lookup that routes a hot invocation
        // through the installed trace), tree-walker fallback when
        // the bytecode setup rejected the source's shape.
        if let Some(bc) = &self.bytecode {
            match bc.run_main(args.clone()) {
                Ok(v) => return Ok(v),
                Err(RuntimeError::Unsupported { .. }) => {
                    // The bytecode tier surfaced an envelope-edge op
                    // it can't execute (M2-A leaves several ops as
                    // `Unsupported`). Quietly fall through to the
                    // tree-walker so the host still gets an answer.
                    tracing::debug!(
                        target: "relon::jit_evaluator",
                        "bytecode tier returned Unsupported; falling back to tree-walker"
                    );
                }
                Err(other) => return Err(other),
            }
        }
        Evaluator::run_main(self.tree_walk.as_ref(), args)
    }

    fn force_thunk(&self, thunk: &Arc<Thunk>) -> Result<Value, RuntimeError> {
        self.tree_walk.force_thunk(thunk)
    }

    fn invoke_closure(&self, closure: &ClosureData, args: &[Value]) -> Result<Value, RuntimeError> {
        self.tree_walk.invoke_closure(closure, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Trivial scalar `#main` survives the bytecode envelope: the
    /// dispatcher should report `Bytecode` as the active tier
    /// initially and `run_main` must return the same value the
    /// tree-walker would.
    #[test]
    fn scalar_main_routes_through_bytecode_tier() {
        let src = "#main(Int x) -> Int\nx + 1";
        let jit = JitEvaluator::new(src).expect("build jit");
        assert_eq!(jit.active_tier(), JitTier::Bytecode);
        assert!(jit.has_bytecode_tier());

        let mut args = HashMap::new();
        args.insert("x".to_string(), Value::Int(41));
        let out = jit.run_main(args).expect("run_main");
        assert_eq!(out, Value::Int(42));
    }

    /// Non-scalar shapes (list literal body here) fall outside the
    /// M2-A envelope. The wrapper must skip the bytecode build,
    /// report `TreeWalk` as the active tier, and still answer
    /// `run_main` correctly via the tree-walker fallback.
    #[test]
    fn non_scalar_main_falls_back_to_tree_walk() {
        let src = "#main(Int n) -> List<Int>\n[n, n + 1, n + 2]";
        let jit = JitEvaluator::new(src).expect("build jit");
        assert_eq!(jit.active_tier(), JitTier::TreeWalk);
        assert!(!jit.has_bytecode_tier());

        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(10));
        let out = jit.run_main(args).expect("run_main");
        match out {
            Value::List(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], Value::Int(10));
                assert_eq!(items[1], Value::Int(11));
                assert_eq!(items[2], Value::Int(12));
            }
            other => panic!("expected List, got {other:?}"),
        }
    }

    /// `eval_root` / `force_thunk` / `invoke_closure` always go
    /// through the tree-walker. Sanity check: a no-`#main` library
    /// document evaluates the same as via [`crate::value_from_str`].
    #[test]
    fn library_mode_works_via_eval_root() {
        let src = r#"{ host: "x", port: 80 }"#;
        let jit = JitEvaluator::new(src).expect("build jit");
        let scope = Arc::new(Scope::default());
        let value = jit.eval_root(&scope).expect("eval_root");
        match value {
            Value::Dict(d) => {
                let host = d.map.get("host").expect("host");
                assert_eq!(host, &Value::String("x".into()));
            }
            other => panic!("expected Dict, got {other:?}"),
        }
    }

    /// Tier escalation smoke. With `cranelift-aot` enabled, a
    /// bytecode-envelope source whose recorder body clears the
    /// [`relon_codegen_native::TINY_TRACE_OP_THRESHOLD`] gate should
    /// pick up a `fn_id` at construction time, run through the
    /// bytecode tier for the first few invocations, and then have
    /// the dispatcher promote the dispatch shape to the Trace tier
    /// once the hot-counter saturates.
    ///
    /// The source body uses eight live `x` loads + seven adds (no
    /// constants the IR lowering would fold away) so the recorder
    /// body lands at 16 ops — comfortably above the `TINY_TRACE_OP_THRESHOLD`
    /// gate the wrapper consults to skip the install for trivial
    /// bodies (where the dispatcher switch would be pure overhead).
    /// Trivial single-expression mains like `x + 1` deliberately
    /// stay on the bytecode tier — see the body-size gate inside
    /// [`super::wire_trace_tier`].
    ///
    /// We drive enough invocations to clear
    /// [`relon_bytecode::DEFAULT_HOT_THRESHOLD`] (= 1000) with a
    /// comfortable margin, then
    /// assert `active_tier == Trace`. The numerical answer keeps
    /// matching the tree-walker reference across the transition —
    /// either the bytecode tier ran the dispatch (cold path) or the
    /// installed trace returned via `result_slot` (hot path).
    ///
    /// Skipped when the feature is off because there's no trace
    /// install path to exercise.
    #[cfg(feature = "cranelift-aot")]
    #[test]
    fn hot_loop_escalates_to_trace_tier() {
        // `x + x + x + x + x + x + x + x` — 16-op recorder body
        // after BcOp conversion (8 × LocalGet, 7 × AddI64, Return).
        let src = "#main(Int x) -> Int\nx + x + x + x + x + x + x + x";
        let jit = JitEvaluator::new(src).expect("build jit");
        // Pre-escalation: bytecode tier active, trace not yet
        // installed.
        assert_eq!(jit.active_tier(), JitTier::Bytecode);

        // Drive ~2x the bytecode VM's default hot threshold
        // (`relon_bytecode::DEFAULT_HOT_THRESHOLD = 1000`) so the
        // counter saturates with a comfortable margin. The recorder
        // also runs synchronously inside the helper that fires
        // exactly once at the threshold-crossing call, so the
        // installed trace is observable in the registry on the very
        // next iteration after the trigger fires.
        let target_iters = (relon_bytecode::DEFAULT_HOT_THRESHOLD as usize) * 2;
        let mut last = Value::Null;
        for i in 0..target_iters {
            let mut args = HashMap::new();
            args.insert("x".to_string(), Value::Int(i as i64));
            last = jit.run_main(args).expect("run_main");
        }
        // Final answer is `(target_iters - 1) * 8`.
        assert_eq!(last, Value::Int((target_iters as i64 - 1) * 8));

        // Post-escalation: the trace registry must have an entry
        // for our fn_id, and `active_tier` must report it.
        assert_eq!(
            jit.active_tier(),
            JitTier::Trace,
            "after 2x DEFAULT_HOT_THRESHOLD invocations the trace must install and dispatcher must report Trace tier"
        );
    }

    /// Trivial single-expression mains (recorder body below
    /// `TINY_TRACE_OP_THRESHOLD`) intentionally stay on the bytecode
    /// tier — the runtime trace-dispatcher gate routes past tiny
    /// traces, so installing one is pure overhead. This test pins
    /// that behaviour against the `x + 1` shape the W12 bench uses.
    #[cfg(feature = "cranelift-aot")]
    #[test]
    fn tiny_body_stays_on_bytecode_tier_no_trace_install() {
        let src = "#main(Int x) -> Int\nx + 1";
        let jit = JitEvaluator::new(src).expect("build jit");
        // Pre-escalation: bytecode tier active.
        assert_eq!(jit.active_tier(), JitTier::Bytecode);
        // The wrapper saw the recorder-body size (~4 ops) was below
        // the `TINY_TRACE_OP_THRESHOLD` gate and skipped the
        // recorder install entirely, so no `fn_id` was reserved.
        assert!(
            jit.fn_id.is_none(),
            "trivial body must not consume a fn_id pool slot"
        );

        // Drive a lot of iterations: the tier should NOT promote
        // because no trace was ever registered.
        let target_iters = (relon_bytecode::DEFAULT_HOT_THRESHOLD as usize) * 2;
        for i in 0..target_iters {
            let mut args = HashMap::new();
            args.insert("x".to_string(), Value::Int(i as i64));
            let _ = jit.run_main(args).expect("run_main");
        }
        assert_eq!(
            jit.active_tier(),
            JitTier::Bytecode,
            "trivial body must stay on Bytecode tier; no trace gets installed because the body is below TINY_TRACE_OP_THRESHOLD"
        );
    }

    /// Multiple `JitEvaluator` instances must each get their own
    /// `fn_id` so concurrent traces don't clobber each other. The
    /// pool returns ids in a deterministic order (descending from
    /// `JIT_FN_ID_MAX - 1` on first run, ascending after recycle),
    /// so we just assert distinctness here without pinning specific
    /// values. Sources must clear the `TINY_TRACE_OP_THRESHOLD` gate
    /// (≥ 8 BcOps) so the wrapper actually allocates a pool slot —
    /// trivial single-expression bodies skip the install (see
    /// [`tiny_body_stays_on_bytecode_tier_no_trace_install`]).
    #[cfg(feature = "cranelift-aot")]
    #[test]
    fn multiple_jit_evaluators_get_distinct_fn_ids() {
        let src_a = "#main(Int x) -> Int\nx + x + x + x + x + x + x + x";
        let src_b = "#main(Int y) -> Int\ny + y + y + y + y + y + y + y";
        let a = JitEvaluator::new(src_a).expect("a");
        let b = JitEvaluator::new(src_b).expect("b");
        let id_a = a.fn_id.expect("a fn_id");
        let id_b = b.fn_id.expect("b fn_id");
        assert_ne!(
            id_a, id_b,
            "distinct JitEvaluators must hold distinct fn_ids"
        );
        // Both must land inside the reserved range.
        for id in [id_a, id_b] {
            assert!(
                (JIT_FN_ID_MIN..JIT_FN_ID_MAX).contains(&id),
                "allocated fn_id {id} falls outside the JIT pool range"
            );
        }
    }

    /// Drop-released `fn_id` must come back into circulation — a
    /// fresh evaluator built right after the prior dropped one
    /// observes a clean recorder registry under its slot. We don't
    /// assert id reuse (the pool is LIFO; a long-running process
    /// with churn could see arbitrary slots), only that the count
    /// of registered recordings doesn't grow unbounded. The
    /// fixture clears the `TINY_TRACE_OP_THRESHOLD` body-size gate
    /// so the wrapper actually registers a recorder body — trivial
    /// single-expression bodies skip the install (the registration
    /// count would stay at `before` under both branches and the
    /// `before + 1` precondition would never hold).
    #[cfg(feature = "cranelift-aot")]
    #[test]
    fn dropped_jit_evaluator_releases_recorder_registration() {
        let src = "#main(Int x) -> Int\nx + x + x + x + x + x + x + x";
        let before = relon_codegen_native::recording_registration_count();
        {
            let _a = JitEvaluator::new(src).expect("a");
            // Inside this scope the registration is live: count
            // must have grown by exactly one above the baseline.
            assert_eq!(
                relon_codegen_native::recording_registration_count(),
                before + 1,
                "JitEvaluator::new must register exactly one recorder body"
            );
        }
        // After drop the registry returns to baseline.
        assert_eq!(
            relon_codegen_native::recording_registration_count(),
            before,
            "JitEvaluator::drop must clear its recorder registration"
        );
    }
}
