//! M2-B phase 4c: hot-counter prologue + trace-JIT bridge hook.
//!
//! ## Design
//!
//! Mirrors the cranelift entry-fn prologue in
//! [`relon_codegen_cranelift::codegen::hot_counter`] (folded directly here
//! as Rust because the bytecode VM dispatches in match-arm code rather
//! than emitted machine instructions):
//!
//! 1. Every [`crate::BcFunction`] that carries a `fn_id` owns one
//!    [`HotCounter`] slot.
//! 2. The VM bumps the slot once per `invoke` — i.e. on function
//!    entry. M2-A's compile envelope is straight-line + label-based
//!    branching, so there is no separate back-edge increment yet; the
//!    "loop iteration count" we observe equals the entry count when
//!    the host driver replays the same function in a tight loop, which
//!    matches the bench / corpus harness shapes the phase-4c
//!    end-to-end test pins.
//! 3. When the post-increment value reaches
//!    [`DEFAULT_HOT_THRESHOLD`] the VM calls the installed
//!    [`HotTraceTrigger`] hook with the slot's `fn_id` and the packed
//!    arg vector the dispatcher would otherwise feed
//!    [`crate::vm::BytecodeVm::invoke_from_with_stack`]. The hook is
//!    typically an adapter over `__relon_jump_to_recorder` (cranelift
//!    backend) or a test double that simply observes the call shape.
//! 4. After the helper returns, the dispatch loop continues as
//!    normal — the bytecode VM still runs the current invocation so
//!    the host gets a real return value while the recorder is busy
//!    building the trace. Future iterations either find an installed
//!    trace and bypass the bytecode dispatch entirely (phase 4c-cont)
//!    or fall through this same path (counter saturated → hook still
//!    called but the helper is idempotent on the install side).
//!
//! ## Non-atomicity
//!
//! Same rationale as
//! [`relon_trace_jit::counter::HotCounter`]: a torn read / write on
//! the `u32` slot at worst delays a trigger by one iteration, never
//! introduces UB (the slot is wrapped in `Cell<u32>`, which is
//! `!Sync`; callers either keep one [`HotCounter`] per thread or wrap
//! the surrounding [`crate::BcFunction`] in a thread-local). The
//! bytecode VM holds a single [`BytecodeVm`](crate::BytecodeVm) per
//! thread today, so this is the natural shape.
//!
//! ## Wasm32 compatibility
//!
//! The hook lives behind a trait object so the bytecode crate can
//! still target wasm32 (no cranelift dependency). The cranelift
//! adapter lives in `relon_codegen_cranelift` (native-only); the
//! wasm32 build leaves the trigger unconfigured and the prologue
//! becomes inert.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use crate::vm::VmValue;

/// Default trigger threshold. Picked higher than the LuaJIT / cranelift
/// default (10) because the bytecode VM dispatches a single tick per
/// op, so the recorder has more samples per invocation; tripping the
/// hook on the very first hot iteration is fine for tests but a
/// production-ish default of 1000 stays out of the way of cold-path
/// callers that never tip past the threshold.
///
/// The constant is `pub` so the host that wires the trigger can choose
/// to tighten / loosen it via [`HotCounter::with_threshold`].
pub const DEFAULT_HOT_THRESHOLD: u32 = 1000;

/// Sentinel value indicating the counter has tripped and the
/// host-supplied trigger has already been notified. Picked at the top
/// of the `u32` range so it stands out in debugger dumps and never
/// collides with a normal increment.
pub const COUNTER_SATURATED: u32 = u32::MAX;

/// Outcome of a [`HotCounter::record`] call.
///
/// Mirrors the [`relon_trace_jit::counter::RecordResult`] variants so
/// callers porting code between the two surfaces don't need an
/// adapter. The dispatch loop maps the result directly onto whether
/// the trigger hook gets invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotCounterResult {
    /// Counter is still well below the threshold; no action.
    Cold,
    /// Counter is climbing but the trigger hasn't fired yet.
    Heating(u32),
    /// Counter just hit the threshold and the slot is now saturated;
    /// the caller must invoke [`HotTraceTrigger::on_hot`] **exactly
    /// once** for this slot. Subsequent calls return [`Self::AlreadyHot`].
    HotTrigger,
    /// Counter was already saturated. The caller skips the hook (the
    /// trigger has already been notified) — future iterations land
    /// here until the slot is explicitly [`HotCounter::reset`].
    AlreadyHot,
}

/// Single-slot hot counter living on a [`BytecodeVm`](crate::BytecodeVm).
///
/// The bytecode VM owns one of these per function it dispatches. Each
/// `invoke` ticks the slot; threshold crossings notify the installed
/// [`HotTraceTrigger`] without taking any locks (the slot is a plain
/// `Cell<u32>`).
#[derive(Debug)]
pub struct HotCounter {
    value: Cell<u32>,
    threshold: u32,
}

impl HotCounter {
    /// Build a fresh counter at zero with [`DEFAULT_HOT_THRESHOLD`].
    pub fn new() -> Self {
        Self::with_threshold(DEFAULT_HOT_THRESHOLD)
    }

    /// Build a counter at zero with a custom threshold. Threshold must
    /// be greater than zero; the bytecode VM's prologue panics on `0`
    /// to keep the "no-op trigger every dispatch" footgun off the
    /// table.
    pub fn with_threshold(threshold: u32) -> Self {
        assert!(
            threshold > 0,
            "hot-counter threshold must be positive (got 0)"
        );
        Self {
            value: Cell::new(0),
            threshold,
        }
    }

    /// Active threshold.
    pub fn threshold(&self) -> u32 {
        self.threshold
    }

    /// Inspect the current slot value without modifying it. Mainly
    /// used by tests asserting the bump cadence.
    pub fn peek(&self) -> u32 {
        self.value.get()
    }

    /// Reset the slot to zero. Tests call this between iterations of
    /// the same `fn_id` so an earlier `HotTrigger` doesn't leak into
    /// the next case.
    pub fn reset(&self) {
        self.value.set(0);
    }

    /// Record one execution and report the resulting state.
    ///
    /// Semantics match [`relon_trace_jit::counter::HotCounter::record`]:
    ///
    /// 1. If the slot is `COUNTER_SATURATED`, return [`HotCounterResult::AlreadyHot`].
    /// 2. Otherwise increment (saturating at `threshold`).
    /// 3. If the new value crossed the threshold, switch the slot to
    ///    `COUNTER_SATURATED` and return [`HotCounterResult::HotTrigger`].
    /// 4. Else if the new value is 1, return [`HotCounterResult::Cold`].
    /// 5. Else return [`HotCounterResult::Heating`]`(new_value)`.
    pub fn record(&self) -> HotCounterResult {
        let cur = self.value.get();
        if cur == COUNTER_SATURATED {
            return HotCounterResult::AlreadyHot;
        }
        let next = cur.saturating_add(1);
        if next >= self.threshold {
            self.value.set(COUNTER_SATURATED);
            return HotCounterResult::HotTrigger;
        }
        self.value.set(next);
        if next == 1 {
            HotCounterResult::Cold
        } else {
            HotCounterResult::Heating(next)
        }
    }
}

impl Default for HotCounter {
    fn default() -> Self {
        Self::new()
    }
}

/// Host-supplied bridge invoked by the bytecode VM when a function's
/// hot counter trips the threshold.
///
/// Implementations adapt the bytecode-side trigger event into whatever
/// recording driver the host already owns. The canonical native impl
/// lives in `relon_codegen_cranelift` and forwards to
/// `__relon_jump_to_recorder` — mirroring the path the cranelift
/// entry-fn prologue takes. Hosts targeting wasm32 (or unit tests)
/// install a mock that simply records the `(fn_id, args)` pair.
///
/// The trait is `Send + Sync` so callers can share a single trigger
/// across worker threads; concrete impls are usually `Arc<…>`.
pub trait HotTraceTrigger: Send + Sync {
    /// Called exactly once per `fn_id` when its hot counter first
    /// crosses the threshold. `args` is the same value-packed slice
    /// the dispatcher was about to feed
    /// [`crate::vm::BytecodeVm::invoke_from_with_stack`]; the helper
    /// inherits the bytecode VM's calling convention (one `u64` per
    /// arg, IR-typed downstream via the call-site
    /// `RecordingRegistration` registered alongside the trigger).
    ///
    /// Implementations MUST NOT panic — the bytecode VM has no
    /// catch_unwind boundary around the call. A failure on the
    /// recording side should be swallowed (logged via tracing at
    /// `warn`) so the dispatch loop continues with the original
    /// bytecode body.
    fn on_hot(&self, fn_id: u32, args: &[VmValue]);
}

impl fmt::Debug for dyn HotTraceTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HotTraceTrigger").finish_non_exhaustive()
    }
}

/// Shared handle to a host-installed trigger. Stored on
/// [`crate::vm::BcVmConfig`] so each `BytecodeVm` clones the `Arc`
/// independently — concurrent invocations don't share the
/// `HotCounter` slot (each `BcFunction` owns its own counter), but
/// they do share the trigger.
pub type HotTraceTriggerHandle = Arc<dyn HotTraceTrigger>;

thread_local! {
    /// Per-thread map `fn_id -> HotCounter`. The bytecode VM's
    /// dispatch loop builds a fresh `BytecodeVm` per `invoke` call (see
    /// `crate::evaluator::BytecodeEvaluator::run_main_inner`), so the
    /// counter has to live somewhere other than the VM itself for the
    /// "tick on every entry" semantics to hold across calls. A
    /// thread-local map mirrors the cranelift backend's
    /// `RELON_HOT_COUNTERS` global table while keeping the bytecode
    /// crate dependency-free (no cranelift, wasm32-safe).
    ///
    /// The map is `RefCell` so the dispatch path can lazily insert
    /// new slots without taking a lock. Concurrent threads each see
    /// their own table — this matches the recorder state machine's
    /// per-thread shape (`relon_codegen_cranelift` keeps
    /// `RECORDING_REGISTRY` thread-local for the same reason).
    static HOT_COUNTERS: RefCell<HashMap<u32, HotCounter>> =
        RefCell::new(HashMap::new());
}

/// Bump the hot counter for `fn_id`, creating the slot with
/// `threshold` on first touch. Returns the [`HotCounterResult`] the
/// dispatcher should act on.
///
/// Cheap fast path: when the slot is already `COUNTER_SATURATED` the
/// helper returns `AlreadyHot` with a single `HashMap::get` + load.
/// Cold path (new slot) costs one `HashMap::entry` insert.
pub fn record_hot(fn_id: u32, threshold: u32) -> HotCounterResult {
    HOT_COUNTERS.with(|cell| {
        let mut map = cell.borrow_mut();
        let slot = map
            .entry(fn_id)
            .or_insert_with(|| HotCounter::with_threshold(threshold));
        slot.record()
    })
}

/// Inspect the current value of the `fn_id` counter. Returns `None`
/// when no slot has been touched yet. Mainly used by tests verifying
/// the bump cadence.
pub fn peek_hot(fn_id: u32) -> Option<u32> {
    HOT_COUNTERS.with(|cell| cell.borrow().get(&fn_id).map(|c| c.peek()))
}

/// Reset (or remove) the counter for `fn_id`. Used by test harness
/// setup to isolate individual cases; production code typically
/// leaves saturated slots alone so `AlreadyHot` keeps short-circuiting
/// the helper.
pub fn reset_hot(fn_id: u32) {
    HOT_COUNTERS.with(|cell| {
        let map = cell.borrow();
        if let Some(slot) = map.get(&fn_id) {
            slot.reset();
        }
    });
}

/// Drop every slot on the current thread. Test harness setup calls
/// this between cases so a stale `HotTrigger` from a previous test
/// doesn't bleed through.
pub fn reset_hot_all() {
    HOT_COUNTERS.with(|cell| cell.borrow_mut().clear());
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Pin the threshold math against the
    /// `relon_trace_jit::counter::HotCounter` reference impl.
    #[test]
    fn record_climbs_then_saturates() {
        let hc = HotCounter::with_threshold(3);
        assert_eq!(hc.record(), HotCounterResult::Cold);
        assert_eq!(hc.record(), HotCounterResult::Heating(2));
        assert_eq!(hc.record(), HotCounterResult::HotTrigger);
        // Saturated.
        assert_eq!(hc.record(), HotCounterResult::AlreadyHot);
        assert_eq!(hc.peek(), COUNTER_SATURATED);
    }

    #[test]
    fn reset_restarts() {
        let hc = HotCounter::with_threshold(2);
        let _ = hc.record();
        let _ = hc.record(); // HotTrigger
        assert_eq!(hc.record(), HotCounterResult::AlreadyHot);
        hc.reset();
        assert_eq!(hc.peek(), 0);
        assert_eq!(hc.record(), HotCounterResult::Cold);
    }

    #[test]
    #[should_panic]
    fn zero_threshold_panics() {
        let _ = HotCounter::with_threshold(0);
    }

    /// Sanity-check the trait shape. The test mock pushes every
    /// trigger event into a shared `Vec` so phase-4c integration
    /// tests can assert dispatch invariants.
    struct MockTrigger {
        log: Mutex<Vec<(u32, Vec<VmValue>)>>,
    }

    impl HotTraceTrigger for MockTrigger {
        fn on_hot(&self, fn_id: u32, args: &[VmValue]) {
            self.log.lock().unwrap().push((fn_id, args.to_vec()));
        }
    }

    #[test]
    fn trait_can_record_calls() {
        let mock = Arc::new(MockTrigger {
            log: Mutex::new(Vec::new()),
        });
        let handle: HotTraceTriggerHandle = mock.clone();
        handle.on_hot(7, &[1, 2, 3]);
        handle.on_hot(7, &[]);
        let log = mock.log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0], (7, vec![1, 2, 3]));
        assert_eq!(log[1], (7, Vec::<VmValue>::new()));
    }
}
