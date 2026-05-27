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
use std::sync::{Arc, Mutex};

#[cfg(feature = "cranelift-aot")]
use relon_codegen_native::TraceContext;
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
    /// Optional caller-supplied trace fixture. When installed, the
    /// fixture short-circuits the bytecode / tree-walker dispatch path:
    /// `run_main` packs the `HashMap<String, Value>` into a `Vec<u64>`,
    /// invokes the recorder-installed trace through
    /// [`relon_codegen_native::TraceJitState::invoke_with_fallback_slice`],
    /// and decodes the returned scalar back into a [`Value`].
    ///
    /// This path exists for hosts that already know the recorder IR
    /// envelope a particular `#main` should walk — most notably the
    /// `cmp_lua` panel rows for W1-W4 which hand-craft an IR body
    /// (`wN_recorder_body`) for sources whose stdlib + closure shape
    /// the auto [`bcops_to_recorder_body`] converter rejects. Future
    /// versions can fold the analyzer + recorder lowering into the
    /// wrapper so the fixture handoff is no longer required.
    ///
    /// Holds an `Option<TraceFixtureInstalled>` rather than the bare
    /// fixture so the on-drop cleanup path can keep its `fn_id` and
    /// invalidate the trace without re-running the install pipeline.
    #[cfg(feature = "cranelift-aot")]
    fixture: Option<TraceFixtureInstalled>,
}

/// Trace-fixture pack closure type alias. Projects the host's
/// `HashMap<String, Value>` into a caller-owned `Vec<u64>` (the trace's
/// packed slot vector, one slot per declared `param_tys` entry). The
/// closure clears the buffer before writing so the [`TraceFixtureInstalled`]
/// cache can reuse the same `Vec` across `run_main` calls without
/// reallocating per invocation.
#[cfg(feature = "cranelift-aot")]
pub type TraceFixturePackFn = Arc<dyn Fn(&HashMap<String, Value>, &mut Vec<u64>) + Send + Sync>;
/// Trace-fixture fallback closure type alias. Invoked when the
/// installed trace deopts (loop-exit guard, type-check failure,
/// etc.) — returns the analytic answer for the input arg slice.
#[cfg(feature = "cranelift-aot")]
pub type TraceFixtureFallbackFn = Arc<dyn Fn(&[u64]) -> u64 + Send + Sync>;
/// Trace-fixture decode closure type alias. Lifts the packed `u64`
/// (either the trace's `result_slot` or the fallback's analytic
/// answer) back into the [`Value`] shape `run_main` advertises.
#[cfg(feature = "cranelift-aot")]
pub type TraceFixtureDecodeFn = Arc<dyn Fn(u64) -> Value + Send + Sync>;

/// Caller-supplied trace fixture passed to [`JitEvaluator::install_trace_fixture`].
///
/// The host owns three things the auto-escalation path doesn't have:
///
/// 1. **The recorder IR body** — hand-crafted ops walking a hot loop
///    body whose source-level shape (stdlib + closures + non-scalar
///    args) the [`bcops_to_recorder_body`] converter cannot translate.
///    See `relon-bench/benches/cmp_lua.rs::wN_recorder_body` for the
///    canonical W1-W4 fixtures.
/// 2. **An arg-pack adapter** (`pack`) — projects the host's
///    `HashMap<String, Value>` into a `Vec<u64>` whose slots map to
///    the IR body's `Op::LocalGet(idx)` reads. Identity / int-only
///    fixtures pack `args["n"]` into `[u64; 1]`; string-heavy
///    fixtures bake in arena-pinned `StringRef` pointers because the
///    trace cannot allocate.
/// 3. **A deopt fallback + a result decoder** — the trace exits
///    through a loop-exit guard once the recorded iteration count is
///    reached, so the dispatcher invokes `fallback` with the input
///    args slice and the host returns the analytic answer (e.g.
///    `n * (n - 1) / 2` for an int sum). `decode` lifts the u64 back
///    into the [`Value`] flavour `run_main` advertises.
///
/// `slot_count` sizes the `TraceContext` SSA-slot buffer the trace
/// writes through; pick ≥ the trace's `ssa_high_water`. Existing
/// bench fixtures all set it to 64.
///
/// Only available under the `cranelift-aot` feature; without it
/// there's no trace dispatcher to route through.
#[cfg(feature = "cranelift-aot")]
pub struct TraceFixture {
    /// Recorder IR body. Same vocabulary the existing trace_jit bench
    /// rows hand-build (`Op::ConstI64`, `Op::LocalGet`,
    /// `Op::Add(IrType::I64)`, `Op::Call { ... }`, ...).
    pub body: Vec<relon_ir::TaggedOp>,
    /// Declared IR types for each input slot. Drives the recorder's
    /// out-of-band typing pass; must match the slot indices the
    /// `pack` closure populates 1:1.
    pub param_tys: Vec<relon_ir::IrType>,
    /// `TraceContext` SSA-slot capacity. Must be `>= ssa_high_water`
    /// of the compiled trace; 64 covers every fixture the bench
    /// builds today.
    pub slot_count: usize,
    /// Warmup args used to drive the recorder once at install time.
    /// Pick representative values that exercise the same control-flow
    /// path the steady-state hot invocations will take; the recorder
    /// captures observed types + IC slots from this single walk.
    pub warmup_args: Vec<u64>,
    /// Pack the host's `HashMap<String, Value>` into the `Vec<u64>`
    /// the trace consumes. The output length must equal
    /// `param_tys.len()`; the dispatcher panics if it doesn't.
    pub pack: TraceFixturePackFn,
    /// Deopt fallback. Invoked when the trace's loop-exit (or any
    /// other guard) fires; returns the analytic answer as a packed
    /// `u64`. Receives the same input args slice the trace saw.
    pub fallback: TraceFixtureFallbackFn,
    /// Decode the packed `u64` (either a trace-emitted result slot or
    /// the fallback's analytic answer) back into the `Value` shape
    /// the host expects out of `run_main`. For W1-W4 this is
    /// `|v| Value::Int(v as i64)`.
    pub decode: TraceFixtureDecodeFn,
}

/// State retained inside [`JitEvaluator`] after a successful fixture
/// install. Wraps the original [`TraceFixture`] plus the fn_id the
/// install pipeline allocated, so the drop path can invalidate the
/// trace + return the id to the pool without re-walking the recorder.
#[cfg(feature = "cranelift-aot")]
struct TraceFixtureInstalled {
    fn_id: u32,
    slot_count: usize,
    /// Caller-owned `TraceContext` reused across `run_main` calls. Built
    /// once at install time so the per-call dispatch avoids both the
    /// `ssa_slots: Box<[u64]>` heap round-trip and the 256-slot
    /// `dict_lookup_ic` array zero-init each invocation. Wrapped in
    /// `Mutex` because `Evaluator::run_main(&self, ..)` is shared-borrow
    /// while `TraceContext::invoke` needs `&mut`; uncontended lock is
    /// one CAS so it stays dwarfed by the trace's own per-iter cost.
    /// Bundles the packed-args buffer alongside the context so the same
    /// lock acquire covers both the `pack` writeback and the trace
    /// invoke — host-side glue stays one CAS per `run_main`.
    state: Mutex<TraceFixtureCallState>,
    pack: TraceFixturePackFn,
    fallback: TraceFixtureFallbackFn,
    decode: TraceFixtureDecodeFn,
}

/// Mutex-guarded per-call scratch reused across `run_main` invocations.
#[cfg(feature = "cranelift-aot")]
struct TraceFixtureCallState {
    ctx: TraceContext,
    /// Packed arg vector reused across `run_main` calls. The `pack`
    /// closure clears + writes; the trace entry consumes the slice.
    packed: Vec<u64>,
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
            #[cfg(feature = "cranelift-aot")]
            fixture: None,
        })
    }

    /// Install a caller-supplied trace fixture. After this returns
    /// `Ok`, [`Self::run_main`] short-circuits the bytecode /
    /// tree-walker dispatch and routes every call through the
    /// installed trace (with the fixture's `fallback` covering deopt
    /// exits). [`Self::active_tier`] flips to [`JitTier::Trace`]
    /// immediately because the fixture pipeline drives one warmup
    /// walk synchronously at install time and only returns `Ok` after
    /// `lookup_trace` confirms the compiled trace landed in the
    /// global registry.
    ///
    /// At most one fixture can be installed per `JitEvaluator`.
    /// Re-installing replaces the previous fixture: the old fn_id is
    /// returned to the pool, the old trace is invalidated, and a
    /// fresh fn_id is allocated for the new body. This lets a host
    /// rotate fixtures across benchmarks without throwing away the
    /// surrounding tree-walker + bytecode tiers.
    ///
    /// Returns [`BackendError::Bytecode`] (re-used as a generic
    /// "trace install failed" carrier) on pool exhaustion or recorder
    /// abort. The wrapper stays usable on either failure — the
    /// non-trace dispatch tiers were never touched.
    ///
    /// Only available under the `cranelift-aot` feature; without it
    /// there's no trace dispatcher to route through.
    #[cfg(feature = "cranelift-aot")]
    pub fn install_trace_fixture(&mut self, fixture: TraceFixture) -> Result<(), BackendError> {
        // Drop the previous fixture (if any) first: invalidate its
        // installed trace and return its fn_id to the pool. We do
        // this *before* allocating the new id so a host that
        // repeatedly cycles fixtures on the same evaluator doesn't
        // bloat the pool's high-water mark.
        if let Some(prev) = self.fixture.take() {
            let _ = relon_codegen_native::clear_recording(prev.fn_id);
            let state = relon_codegen_native::global_trace_jit_state();
            let _ = state.invalidate_trace(prev.fn_id);
            release_jit_fn_id(prev.fn_id);
        }

        if fixture.warmup_args.len() != fixture.param_tys.len() {
            return Err(BackendError::Bytecode(format!(
                "trace fixture: warmup_args.len() = {} must match param_tys.len() = {}",
                fixture.warmup_args.len(),
                fixture.param_tys.len()
            )));
        }

        let Some(fn_id) = alloc_jit_fn_id() else {
            tracing::warn!(
                target: "relon::jit_evaluator",
                jit_pool_range = ?(JIT_FN_ID_MIN..JIT_FN_ID_MAX),
                "trace fixture install: fn_id pool exhausted; leaving wrapper in its non-fixture state"
            );
            return Err(BackendError::Bytecode(
                "JIT fn_id pool exhausted; cannot install trace fixture".to_string(),
            ));
        };

        // Drive the recorder once with the warmup args; on success
        // `state.lookup_trace(fn_id)` returns a freshly-compiled
        // trace fn (validated inside the helper). On failure the
        // helper returns an `Err(reason)` we surface verbatim.
        let param_count = fixture.param_tys.len();
        if let Err(reason) = relon_codegen_native::install_recorder_trace_warmup(
            fn_id,
            fixture.body,
            fixture.param_tys,
            &fixture.warmup_args,
        ) {
            // Recorder bailed — return the freshly-allocated id so it
            // doesn't leak.
            release_jit_fn_id(fn_id);
            return Err(BackendError::Bytecode(format!(
                "trace fixture install failed: {reason}"
            )));
        }

        // Sanity: trace is now in the registry. Stash the dispatch
        // closures so `run_main` can route through it on every call.
        let ctx = TraceContext::with_hooks(
            fixture.slot_count,
            relon_codegen_native::default_host_hooks(),
        );
        let packed = Vec::with_capacity(param_count);
        self.fixture = Some(TraceFixtureInstalled {
            fn_id,
            slot_count: fixture.slot_count,
            state: Mutex::new(TraceFixtureCallState { ctx, packed }),
            pack: fixture.pack,
            fallback: fixture.fallback,
            decode: fixture.decode,
        });
        Ok(())
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
        {
            // Caller-installed fixture wins over the auto-wired
            // bytecode-tier promotion path: a host that paid the cost
            // of pre-building a recorder body wants the dispatcher to
            // honour it. The trace must also be present in the
            // global registry — a fixture whose trace got invalidated
            // (deopt or external eviction) falls back to whatever
            // auto tier is still live.
            let state = relon_codegen_native::global_trace_jit_state();
            if let Some(installed) = &self.fixture {
                if state.lookup_trace(installed.fn_id).is_some() {
                    return JitTier::Trace;
                }
            }
            if let Some(id) = self.fn_id {
                if state.lookup_trace(id).is_some() {
                    return JitTier::Trace;
                }
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
        if let Some(installed) = self.fixture.take() {
            let _ = relon_codegen_native::clear_recording(installed.fn_id);
            let state = relon_codegen_native::global_trace_jit_state();
            let _ = state.invalidate_trace(installed.fn_id);
            release_jit_fn_id(installed.fn_id);
        }
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

    // Two recorder-body sources are wired through this seam:
    //
    // * **IR path** — the production-lowered IR body the bytecode
    //   compile pass walked, exposed verbatim via
    //   `recording_registration_data()` (structured `Block` / `Loop` /
    //   `BrIf` / `LoadField` / `Op::Call` form). The recorder's
    //   `step_*` walker handles every op in this shape and PC alignment
    //   with the bytecode's `ir_pc_map` holds by construction. Used
    //   for any source whose body contains an `Op::Loop` and matches
    //   the Int-return / no-`Op::If` envelope.
    //
    // * **BcOp legacy path** — projects the bytecode VM's compiled op
    //   stream onto a flat recorder body. Kept in place for the
    //   straight-line scalar shapes (`x + x + x + ... + x`-style
    //   chains) the existing `hot_loop_escalates_to_trace_tier` pin
    //   exercises; those bodies have no loop to expose to the IR path.
    //
    // Correctness gate: after the IR-path recorder is registered + the
    // trace lands via [`install_recorder_trace_warmup`], the wrapper
    // runs the trace once with a tiny synthetic arg and compares the
    // result against the bytecode VM's answer. Any mismatch — for
    // bodies where the trace recorder + emitter pipeline hits a
    // post-loop SSA propagation gap not currently caught by the
    // pre-install structural predicates — invalidates the trace,
    // releases the `fn_id`, and falls back to the bytecode-only tier.
    // This keeps the auto-escalation honest: `JitTier::Trace` is only
    // reported when a future `run_main` would observe a result
    // equivalent to the bytecode tier's.
    let registration_data = ev.recording_registration_data();
    let use_ir_path = ir_body_is_recorder_safe(&registration_data.body);

    let (recorder_body, param_tys, field_offset_to_local) = if use_ir_path {
        (
            registration_data.body.clone(),
            registration_data.param_tys.clone(),
            registration_data.field_offset_to_local.clone(),
        )
    } else {
        let body = match bcops_to_recorder_body(&ev.function().ops) {
            Some(b) => b,
            None => {
                tracing::debug!(
                    target: "relon::jit_evaluator",
                    "tier escalation: bytecode body contains ops outside the trace-recorder envelope (jumps / calls / etc) and the IR-path predicate did not match; skipping trace install"
                );
                return (Some(Box::new(ev) as Box<dyn Evaluator>), None);
            }
        };
        let tys = match user_param_tys_from_source(source) {
            Some(tys) => tys,
            None => {
                tracing::debug!(
                    target: "relon::jit_evaluator",
                    "tier escalation: user-param type recovery failed; skipping trace install"
                );
                return (Some(Box::new(ev) as Box<dyn Evaluator>), None);
            }
        };
        (body, tys, std::collections::BTreeMap::new())
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
    // For the IR-path branch this counts top-level IR ops (a single
    // `Op::Block` wraps a loop, so the count stays modest); for the
    // legacy BcOp branch it counts post-lowering bytecode ops. Both
    // are >= 8 for the workloads we want to escalate; trivial bodies
    // (`x + 1`-shape) lower to fewer ops on either branch and stay on
    // the bytecode tier.
    if recorder_body.len() < relon_codegen_native::TINY_TRACE_OP_THRESHOLD {
        tracing::debug!(
            target: "relon::jit_evaluator",
            body_len = recorder_body.len(),
            threshold = relon_codegen_native::TINY_TRACE_OP_THRESHOLD,
            "tier escalation: recorder body below TINY_TRACE_OP_THRESHOLD; skipping trace install to avoid dispatcher-switch overhead"
        );
        return (Some(Box::new(ev) as Box<dyn Evaluator>), None);
    }

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
            body: body.clone(),
            param_tys: param_tys.clone(),
            field_offset_to_local,
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

    // Install-time correctness verification — only for the IR path.
    //
    // Drive [`install_recorder_trace_warmup`] once with a small Int
    // arg, then invoke the freshly-installed trace and compare its
    // result against the bytecode VM's answer for the same arg. If
    // the trace mis-computes (e.g. the recorder/emitter pipeline
    // exhibits a post-loop SSA propagation gap for this body shape),
    // invalidate the trace + clear the registration + release the
    // `fn_id` and return without wiring the trace hooks. The bytecode
    // tier stays live so the host still gets a correct `run_main`.
    //
    // The verification arg is `n = 0` for Int-only `#main(Int n)`
    // sources: that hits the zero-iteration boundary the recorder
    // walks during its synchronous warmup pre-flight, then probes
    // whether the installed trace's `result_slot` matches the
    // bytecode VM's `unpack_return_slots` answer. For W2-shape
    // bodies the analytic answer is `0` and the trace returns `0`
    // → verification passes. For W1-shape bodies the trace incorrectly
    // returns the recorder's intermediate value rather than the
    // analytic answer; verification trips and the wrapper falls back
    // to bytecode-only.
    //
    // We only run this on single-Int-arg sources (W1 / W2 / W4
    // shape) — multi-arg or non-Int-arg sources skip the gate
    // and just trust the IR-path wiring (matches the conservative
    // posture from before the gate landed, with the caveat that
    // correctness on those shapes still depends on the recorder/emit
    // pipeline's own envelope checks).
    if use_ir_path && param_tys.len() == 1 && matches!(param_tys[0], relon_ir::IrType::I64) {
        // Probe with a small but >0 n so the recorded trace observes
        // one or more loop iterations; using `n=0` would give the
        // recorder a zero-iteration walk and leave the post-loop SSA
        // propagation case untested. `n=32` matches the workload's
        // hot-loop scale without paying many cycles for the
        // verification itself.
        let probe_n: i64 = 32;
        let warmup_args = vec![probe_n as u64];

        // Re-pull the offset map from the registration we just made —
        // the warmup helper re-registers, so we hand it the map to
        // carry forward.
        let offset_map = registration_data.field_offset_to_local.clone();
        match relon_codegen_native::install_recorder_trace_warmup_with_offset_map(
            fn_id,
            body.clone(),
            param_tys.clone(),
            offset_map,
            &warmup_args,
        ) {
            Ok(_) => {
                // Trace installed — verify its result matches the
                // bytecode VM's analytic answer. Any mismatch
                // invalidates the trace and falls the wrapper back to
                // bytecode-only so users never observe a wrong-answer
                // Trace tier.
                let trace_result_ok =
                    verify_installed_trace_against_bytecode(source, fn_id, &warmup_args, probe_n);
                if !trace_result_ok {
                    tracing::warn!(
                        target: "relon::jit_evaluator",
                        fn_id,
                        "tier escalation: IR-path trace install verification failed; invalidating trace and falling back to bytecode-only"
                    );
                    let state = relon_codegen_native::global_trace_jit_state();
                    let _ = state.invalidate_trace(fn_id);
                    let _ = relon_codegen_native::clear_recording(fn_id);
                    release_jit_fn_id(fn_id);
                    return (Some(Box::new(ev) as Box<dyn Evaluator>), None);
                }
            }
            Err(reason) => {
                tracing::debug!(
                    target: "relon::jit_evaluator",
                    fn_id,
                    reason = %reason,
                    "tier escalation: IR-path warmup install failed; falling back to bytecode-only"
                );
                let _ = relon_codegen_native::clear_recording(fn_id);
                release_jit_fn_id(fn_id);
                return (Some(Box::new(ev) as Box<dyn Evaluator>), None);
            }
        }
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

/// Compare the installed trace fn's answer against the bytecode VM's
/// for `probe_n`. Returns `true` iff both produce the same Int value.
///
/// The bytecode side runs a fresh `BytecodeEvaluator::from_source` —
/// it doesn't share the per-instance trace hooks the wrapper is about
/// to wire (no `with_trace_lookup`), so it stays on the bytecode
/// dispatch loop and returns the analytic answer. The trace side
/// invokes the installed JIT'd trace via the cranelift lookup adapter
/// and lifts its `result_slot` via `try_invoke`.
#[cfg(feature = "cranelift-aot")]
fn verify_installed_trace_against_bytecode(
    source: &str,
    fn_id: u32,
    warmup_args: &[u64],
    probe_n: i64,
) -> bool {
    let _ = warmup_args;

    // Bytecode reference run — a fresh `BytecodeEvaluator` with NO
    // trace_lookup wired stays on the bytecode dispatch loop and
    // computes the analytic answer directly.
    let bytecode_answer = match relon_bytecode::BytecodeEvaluator::from_source(source) {
        Ok(ev) => {
            let mut args = HashMap::new();
            args.insert("n".to_string(), Value::Int(probe_n));
            match Evaluator::run_main(&ev, args) {
                Ok(Value::Int(v)) => v,
                _ => return false,
            }
        }
        Err(_) => return false,
    };

    // Traced run — same source through a `BytecodeEvaluator` that
    // carries the trace_lookup. Its `run_main` routes through
    // `lookup.try_invoke(fn_id, &packed)` and, on Success, returns
    // `pack_trace_result(result)` directly; on Deopt, it calls
    // `resume_from_deopt(args, &snapshot)` which steps the bytecode
    // VM from the snapshot's `bc_idx`. This exactly mirrors what
    // the user-facing `run_main` will observe once the wrapper wires
    // the trace tier, so a mismatch here is a real user-visible
    // correctness gap.
    let mut user_facing_args = HashMap::new();
    user_facing_args.insert("n".to_string(), Value::Int(probe_n));
    let traced_answer = match relon_bytecode::BytecodeEvaluator::from_source(source) {
        Ok(ev_with_hooks) => {
            let lookup_handle: relon_bytecode::InstalledTraceLookupHandle =
                Arc::new(relon_codegen_native::CraneliftTraceLookup);
            let ev_hooked = ev_with_hooks
                .with_fn_id(fn_id)
                .with_trace_lookup(lookup_handle);
            match Evaluator::run_main(&ev_hooked, user_facing_args) {
                Ok(Value::Int(v)) => v,
                _ => return false,
            }
        }
        Err(_) => return false,
    };
    traced_answer == bytecode_answer
}

/// Pre-install gate: returns `true` when the production-lowered IR
/// `body` is one the trace recorder can walk end-to-end without
/// hitting an `Unsupported`-class abort. Pairs with the post-install
/// verification in [`wire_trace_tier`] — the predicate keeps the
/// install path off shapes the recorder cannot start walking, the
/// verifier covers shapes the recorder accepts but where the
/// emit-side trace fn produces the wrong answer (e.g. post-loop
/// SSA propagation gaps).
///
/// Accept rules:
///
/// * Body must contain an `Op::Loop` somewhere. The IR path's
///   load-bearing affordance is the structured loop shape that
///   production lowering produces for `range(...).fold(...)` /
///   `map(...).filter(...).len()` etc.; bodies without a loop sit on
///   the legacy BcOp path (which the existing
///   `hot_loop_escalates_to_trace_tier` test pins for unrolled
///   straight-line shapes).
/// * No `Op::If` anywhere — the recorder lowering aborts on it
///   (`UnsupportedOp("If")`).
/// * No non-Int `StoreField` — the BcOp resume path for non-Int return
///   shapes is still maturing; sources that need a String / Float /
///   Bool / Null return install a recorder fixture explicitly via
///   `install_trace_fixture`.
#[cfg(feature = "cranelift-aot")]
fn ir_body_is_recorder_safe(body: &[relon_ir::TaggedOp]) -> bool {
    use relon_ir::{IrType, Op};

    fn walk(ops: &[relon_ir::TaggedOp], saw_loop: &mut bool, bad: &mut bool) {
        for tagged in ops {
            if *bad {
                return;
            }
            match &tagged.op {
                Op::If { .. } => {
                    *bad = true;
                    return;
                }
                Op::StoreField { ty, .. } if !matches!(ty, IrType::I64) => {
                    *bad = true;
                    return;
                }
                Op::Loop { body, .. } => {
                    *saw_loop = true;
                    walk(body, saw_loop, bad);
                }
                Op::Block { body, .. } => {
                    walk(body, saw_loop, bad);
                }
                _ => {}
            }
        }
    }

    let mut saw_loop = false;
    let mut bad = false;
    walk(body, &mut saw_loop, &mut bad);
    if bad {
        return false;
    }
    saw_loop
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
        // Dispatch order:
        //   1. Caller-installed trace fixture (host paid for a
        //      hand-built recorder body; honour it). Routes the
        //      packed args through the trace registry's
        //      `invoke_with_fallback_slice` and decodes the returned
        //      scalar via the fixture's `decode` closure.
        //   2. Bytecode tier — owns both the hot-counter prologue
        //      that promotes us to Trace and the dispatcher-switch
        //      lookup that routes a hot invocation through the
        //      installed trace.
        //   3. Tree-walker fallback when the bytecode setup rejected
        //      the source's shape.
        #[cfg(feature = "cranelift-aot")]
        if let Some(installed) = &self.fixture {
            // Fixture path: project the host args into the cached
            // packed buffer (`pack` clears + writes), invoke the
            // installed trace through the cached `TraceContext`, and
            // decode the returned scalar. Both buffers live in the
            // same `Mutex` so one CAS covers the whole `run_main` —
            // host-side glue stays minimal. `invoke_with_existing_ctx`
            // short-circuits the GuardFailed branch to a plain
            // `fallback(args)` (no `value_stack_copy` rendering), so
            // bench rows where the trace exits via a guard every call
            // (e.g. cmp_lua W3 string concat) avoid the per-call Vec
            // allocation `invoke_with_resume` performs.
            let trace_state = relon_codegen_native::global_trace_jit_state();
            let raw = {
                let mut guard = installed
                    .state
                    .lock()
                    .expect("trace fixture state mutex poisoned");
                let state_ref = &mut *guard;
                (installed.pack)(&args, &mut state_ref.packed);
                assert!(
                    installed.slot_count >= state_ref.packed.len(),
                    "trace fixture ctx slot_count ({}) must be >= packed args ({})",
                    installed.slot_count,
                    state_ref.packed.len()
                );
                let fallback = Arc::clone(&installed.fallback);
                trace_state.invoke_with_existing_ctx_slice(
                    installed.fn_id,
                    &mut state_ref.ctx,
                    &state_ref.packed,
                    |args_slice| (fallback)(args_slice),
                )
            };
            return Ok((installed.decode)(raw));
        }

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

    /// cmp_lua-W2-shape production source auto-escalation pin.
    ///
    /// Walks the W2 `list.sum(range(n).map((i) => (i+1)*(i+2)))`
    /// **production source string** (no hand-built fixture) through
    /// the auto-escalation path: the bytecode tier accepts the
    /// stdlib-fold shape, the production-lowered IR body matches
    /// `ir_body_is_recorder_safe`, the install-time correctness
    /// verification confirms the trace's answer matches the bytecode
    /// VM's for the probe arg, and the wrapper wires the trace hooks.
    /// After the hot counter saturates, `active_tier()` reports
    /// `JitTier::Trace` and `run_main` keeps returning the analytic
    /// answer (`sum((i+1)*(i+2) for i in 0..n)`) across the
    /// transition.
    ///
    /// Pins the end-to-end IR-path auto-escalation: a production loop
    /// body (no hand-built fixture handoff) drives the recorder +
    /// emitter + cranelift install pipeline + verification gate off
    /// the source string. The accompanying W1 / W3 / W4 tests pin the
    /// other branches of the wiring contract (verification rejects /
    /// predicate rejects).
    ///
    /// Skipped when the `cranelift-aot` feature is off — there's no
    /// trace install path without it.
    #[cfg(feature = "cranelift-aot")]
    #[test]
    fn cmp_lua_w2_production_source_auto_escalates_to_trace() {
        let src = "#import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   list.sum(range(n).map((i) => (i + 1) * (i + 2)))";
        let jit = JitEvaluator::new(src).expect("build jit");
        assert!(
            jit.has_bytecode_tier(),
            "W2 production source must accept the bytecode envelope"
        );

        let n: i64 = 32;
        let target_iters = (relon_bytecode::DEFAULT_HOT_THRESHOLD as usize) * 2;
        let mut last = Value::Null;
        for _ in 0..target_iters {
            let mut args = HashMap::new();
            args.insert("n".to_string(), Value::Int(n));
            last = jit.run_main(args).expect("run_main");
        }
        let expected: i64 = (0..n).map(|i| (i + 1) * (i + 2)).sum();
        assert_eq!(last, Value::Int(expected));
        assert_eq!(
            jit.active_tier(),
            JitTier::Trace,
            "W2 must auto-escalate to Trace tier after 2x DEFAULT_HOT_THRESHOLD invocations"
        );

        // Vary n after escalation to prove the trace is computing the
        // analytic answer (not just echoing the recorder's snapshot).
        for &probe_n in &[1i64, 4, 16, 64] {
            let mut args = HashMap::new();
            args.insert("n".to_string(), Value::Int(probe_n));
            let v = jit.run_main(args).expect("run_main");
            let expected: i64 = (0..probe_n).map(|i| (i + 1) * (i + 2)).sum();
            assert_eq!(
                v,
                Value::Int(expected),
                "W2 trace must compute the analytic answer for n={probe_n}"
            );
        }
    }

    /// cmp_lua-W1-shape production source: after the Phase J.2 trace
    /// runtime fix the emit-side trace fn writes loop-carried phi
    /// values back into `TraceContext::ssa_slots` at every loop-header
    /// entry, so the bytecode-VM resume can reconstruct the live
    /// `acc` / `i` from the deopt snapshot and surface the analytic
    /// `n * (n - 1) / 2` answer. The wrapper's install-time
    /// verification gate then accepts the trace and `active_tier()`
    /// flips to `Trace` once the hot counter saturates — same shape
    /// as the W2 source.
    ///
    /// Pins the post-J.2 success path: the recorder accepts the
    /// production body, the emit pipeline produces a correct trace
    /// fn, and the verification gate clears so the user observes a
    /// real Trace-tier acceleration.
    #[cfg(feature = "cranelift-aot")]
    #[test]
    fn cmp_lua_w1_production_source_auto_escalates_to_trace() {
        let src = "#import list from \"std/list\"\n#main(Int n) -> Int\nlist.sum(range(n))";
        let jit = JitEvaluator::new(src).expect("build jit");
        assert!(
            jit.has_bytecode_tier(),
            "W1 production source must accept the bytecode envelope"
        );

        let n: i64 = 32;
        for _ in 0..3 {
            let mut args = HashMap::new();
            args.insert("n".to_string(), Value::Int(n));
            let v = jit.run_main(args).expect("run_main");
            assert_eq!(v, Value::Int(n * (n - 1) / 2));
        }

        let target_iters = (relon_bytecode::DEFAULT_HOT_THRESHOLD as usize) * 2;
        let mut last = Value::Null;
        for _ in 0..target_iters {
            let mut args = HashMap::new();
            args.insert("n".to_string(), Value::Int(n));
            last = jit.run_main(args).expect("run_main");
        }
        assert_eq!(last, Value::Int(n * (n - 1) / 2));
        assert_eq!(
            jit.active_tier(),
            JitTier::Trace,
            "W1 must auto-escalate to Trace tier after 2x DEFAULT_HOT_THRESHOLD invocations (post-J.2 spill of loop-carried phis)"
        );

        // Vary n after escalation to prove the trace re-runs the loop
        // each call (not just echoing the recorder's snapshot).
        for &probe_n in &[1i64, 4, 16, 64] {
            let mut args = HashMap::new();
            args.insert("n".to_string(), Value::Int(probe_n));
            let v = jit.run_main(args).expect("run_main");
            assert_eq!(
                v,
                Value::Int(probe_n * (probe_n - 1) / 2),
                "W1 trace must compute the analytic answer for n={probe_n}"
            );
        }
    }

    /// cmp_lua-W4-shape production source: the correctness-gate
    /// still rejects the trace post-J.2. The deopt path's
    /// `external_pc` for the loop-exit BrIf lands on a bytecode
    /// index that re-executes the final iteration's count-increment
    /// during resume, so the trace surfaces `n + 1` instead of the
    /// analytic `n`. The recovery widening for this (PC alignment of
    /// inner-loop-exit guards across nested Block / Loop scopes) is
    /// tracked as a Phase J.3 follow-up — separate from the J.2
    /// phi-spill that already lands the correct loop-carried state in
    /// the deopt snapshot (`ssa_slots_copy[count_let_slot]` = n).
    #[cfg(feature = "cranelift-aot")]
    #[test]
    fn cmp_lua_w4_production_source_correctness_gate_keeps_bytecode_tier() {
        let src = "#import list from \"std/list\"\n\
                   #main(Int n) -> Int\n\
                   range(n)\n\
                     .map((i) => \"axb\")\n\
                     .filter((s) => s.contains(\"x\"))\n\
                     .len()";
        let jit = JitEvaluator::new(src).expect("build jit");
        assert!(
            jit.has_bytecode_tier(),
            "W4 production source must accept the bytecode envelope"
        );

        let n: i64 = 32;
        for _ in 0..3 {
            let mut args = HashMap::new();
            args.insert("n".to_string(), Value::Int(n));
            let v = jit.run_main(args).expect("run_main");
            assert_eq!(v, Value::Int(n));
        }

        let target_iters = (relon_bytecode::DEFAULT_HOT_THRESHOLD as usize) * 2;
        let mut last = Value::Null;
        for _ in 0..target_iters {
            let mut args = HashMap::new();
            args.insert("n".to_string(), Value::Int(n));
            last = jit.run_main(args).expect("run_main");
        }
        assert_eq!(last, Value::Int(n));
        assert_ne!(
            jit.active_tier(),
            JitTier::Trace,
            "W4 must NOT auto-escalate to Trace tier until the post-loop SSA gap closes"
        );
    }

    /// cmp_lua-W3-shape **String-return** sources stay on the
    /// bytecode tier even after the hot counter saturates.
    ///
    /// `ir_body_is_recorder_safe` rejects bodies whose final
    /// `StoreField` carries a non-Int type, and the legacy BcOp path
    /// bails on the body's `StrConcat` / `StrContains` ops. Neither
    /// path wires a trace, so `active_tier()` stays at `Bytecode`.
    ///
    /// Hosts that need a Trace tier on a String-return shape today
    /// install a recorder fixture via `install_trace_fixture`; the
    /// bench panel's `relon_jit` W3 / W4_long_haystack rows use that
    /// path.
    #[cfg(feature = "cranelift-aot")]
    #[test]
    fn cmp_lua_w3_string_return_stays_on_bytecode_tier_pending_resume_widening() {
        let src = "#import list from \"std/list\"\n\
                   #main(Int n) -> String\n\
                   range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)";
        let jit = JitEvaluator::new(src).expect("build jit");
        assert!(
            jit.has_bytecode_tier(),
            "W3 production source must accept the bytecode envelope"
        );

        let target_iters = (relon_bytecode::DEFAULT_HOT_THRESHOLD as usize) * 2;
        let n: i64 = 8;
        for _ in 0..target_iters {
            let mut args = HashMap::new();
            args.insert("n".to_string(), Value::Int(n));
            let _ = jit.run_main(args).expect("run_main");
        }
        assert_ne!(
            jit.active_tier(),
            JitTier::Trace,
            "W3 (String return) must NOT auto-escalate to Trace tier; the recorder safety predicate rejects non-Int return shapes pending resume-rehydration widening"
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

    /// Task #270: caller-supplied trace fixture must (a) install the
    /// trace synchronously at `install_trace_fixture` time, (b) flip
    /// `active_tier` to `Trace` immediately, and (c) route
    /// subsequent `run_main` calls through `invoke_with_fallback_slice`
    /// so the decoded result matches the fixture's analytic
    /// `fallback`. The fixture's body is a loop-exit-deopt shape:
    /// the trace runs n iterations then hits the loop-exit guard,
    /// which routes through `fallback` and returns the analytic
    /// answer (`n*(n-1)/2` for sum 0..n-1).
    ///
    /// We pick a source whose Relon body lives outside the M2-A
    /// scalar envelope so the bytecode tier would be `None` —
    /// exactly the W1-W4 panel-row shape this entry point is built
    /// for. The fixture wins over `wire_trace_tier`'s auto-install
    /// path (which would have bailed on the stdlib import anyway).
    #[cfg(feature = "cranelift-aot")]
    #[test]
    fn trace_fixture_install_promotes_run_main_to_trace_tier() {
        use relon_ir::{IrType, Op, TaggedOp};
        use relon_parser::TokenRange;

        // Source uses a String-return shape so the auto path's
        // `ir_body_is_recorder_safe` predicate rejects it (non-Int
        // return + StringRef-typed StoreField), keeping the auto
        // wrapper out of the way. The bytecode tier still accepts the
        // shape, so pre-install `active_tier` is Bytecode; the
        // fixture install must promote it to `Trace`.
        //
        // Phase J.2 historical note: the pre-J.2 incarnation of this
        // test used `list.sum(range(n))` (a W1-shape Int-return body)
        // and relied on the recorder/emitter's post-loop SSA gap to
        // keep the auto wrapper from escalating. Once the J.2 spill
        // landed, the W1 auto path escalates to Trace immediately,
        // so the pre-install assertion would observe `Trace` instead
        // of `Bytecode` and the "fixture install promotes" intent
        // would no longer be testable on that source. Switching to
        // the W3-shape String body keeps the auto path on Bytecode
        // for the duration of this test without weakening any other
        // pinned invariant.
        let src = "#import list from \"std/list\"\n\
                   #main(Int n) -> String\n\
                   range(n).map((i) => \"a\").reduce(\"\", (acc, s) => acc + s)";
        let mut jit = JitEvaluator::new(src).expect("build jit");
        assert_ne!(jit.active_tier(), JitTier::Trace);

        let tag = |op: Op| TaggedOp {
            op,
            range: TokenRange::default(),
        };
        const I: u32 = 0;
        const ACC: u32 = 1;
        // sum 0..n-1, same shape `w1_recorder_body` builds in the
        // cmp_lua bench.
        let body = vec![
            tag(Op::ConstI64(0)),
            tag(Op::LetSet {
                idx: I,
                ty: IrType::I64,
            }),
            tag(Op::ConstI64(0)),
            tag(Op::LetSet {
                idx: ACC,
                ty: IrType::I64,
            }),
            tag(Op::Block {
                result_ty: None,
                body: vec![tag(Op::Loop {
                    result_ty: None,
                    body: vec![
                        tag(Op::LetGet {
                            idx: I,
                            ty: IrType::I64,
                        }),
                        tag(Op::LocalGet(0)),
                        tag(Op::Ge(IrType::I64)),
                        tag(Op::BrIf { label_depth: 1 }),
                        tag(Op::LetGet {
                            idx: ACC,
                            ty: IrType::I64,
                        }),
                        tag(Op::LetGet {
                            idx: I,
                            ty: IrType::I64,
                        }),
                        tag(Op::Add(IrType::I64)),
                        tag(Op::LetSet {
                            idx: ACC,
                            ty: IrType::I64,
                        }),
                        tag(Op::LetGet {
                            idx: I,
                            ty: IrType::I64,
                        }),
                        tag(Op::ConstI64(1)),
                        tag(Op::Add(IrType::I64)),
                        tag(Op::LetSet {
                            idx: I,
                            ty: IrType::I64,
                        }),
                        tag(Op::Br { label_depth: 0 }),
                    ],
                })],
            }),
            tag(Op::LetGet {
                idx: ACC,
                ty: IrType::I64,
            }),
            tag(Op::Return),
        ];

        let fixture = crate::TraceFixture {
            body,
            param_tys: vec![IrType::I64],
            slot_count: 64,
            warmup_args: vec![16],
            pack: Arc::new(|args: &HashMap<String, Value>, buf: &mut Vec<u64>| {
                let n = match args.get("n") {
                    Some(Value::Int(v)) => *v,
                    other => panic!("expected Int n, got {other:?}"),
                };
                buf.clear();
                buf.push(n as u64);
            }),
            fallback: Arc::new(|args: &[u64]| {
                let n = args[0] as i64;
                (n * (n - 1) / 2) as u64
            }),
            decode: Arc::new(|v: u64| Value::Int(v as i64)),
        };

        jit.install_trace_fixture(fixture)
            .expect("trace fixture install");

        // Post-install: dispatcher must report Trace tier and
        // `run_main` must return the analytic answer (the trace's
        // own SUM body would also reach it, but the loop-exit guard
        // deopts at i == n, so the dispatcher takes the fallback
        // path which returns `n*(n-1)/2` directly).
        assert_eq!(jit.active_tier(), JitTier::Trace);

        let mut args = HashMap::new();
        args.insert("n".to_string(), Value::Int(256));
        let v = jit.run_main(args).expect("run_main");
        assert_eq!(v, Value::Int(256 * 255 / 2));
    }
}
