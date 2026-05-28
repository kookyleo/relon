//! v6-δ M2-B phase 4c: adapter that wires the bytecode VM's
//! `HotTraceTrigger` hook into the same `__relon_jump_to_recorder`
//! helper the cranelift entry-fn prologue uses on threshold crossing.
//!
//! ## Why a separate adapter?
//!
//! The bytecode crate is dependency-free (no cranelift) so the wasm32
//! build still compiles. The trigger surface is a trait
//! (`relon_bytecode::HotTraceTrigger`), and this module supplies the
//! native impl that:
//!
//! 1. forwards the bytecode-side `(fn_id, args)` event to
//!    `__relon_jump_to_recorder`, which drives the
//!    `RecorderState → TraceRecordingEvaluator → JITed install`
//!    pipeline already wired up for the cranelift backend, AND
//! 2. swallows panics with `std::panic::catch_unwind` so the bytecode
//!    dispatch loop never observes a recorder-side fault. The
//!    [`HotTraceTrigger`] trait docs require panic-free impls; the
//!    catch_unwind belt-and-braces matches the design's "recorder is
//!    advisory" stance.
//!
//! ## Wire-up
//!
//! Hosts that want bytecode hot loops to drive trace recording build
//! the adapter once per evaluator and stamp it onto the
//! `BytecodeEvaluator`'s default `BcVmConfig`:
//!
//! ```ignore
//! use std::sync::Arc;
//! use relon_codegen_cranelift::bytecode_bridge::CraneliftHotTrigger;
//! let trigger: Arc<dyn relon_bytecode::HotTraceTrigger> =
//!     Arc::new(CraneliftHotTrigger);
//! let bc_eval = relon_bytecode::BytecodeEvaluator::from_source(src)?
//!     .with_hot_trigger(trigger);
//! ```
//!
//! The host is expected to register a `RecordingRegistration` for the
//! matching `fn_id` ahead of time (see
//! `crate::trace_install::register_recording`); without one the
//! `__relon_jump_to_recorder` helper logs a debug message and returns
//! immediately, so the bytecode prologue stays inert until the wiring
//! is complete on both sides.

use std::panic;

use relon_bytecode::vm::VmValue;
use relon_bytecode::{HotTraceTrigger, InstalledTraceLookup, TraceInvokeOutcome};

use crate::trace_install::{__relon_jump_to_recorder, global_trace_jit_state};

/// `HotTraceTrigger` impl that forwards bytecode-side hot-counter
/// trigger events to the cranelift recording helper.
///
/// Zero-sized: callers wrap it in `Arc<dyn HotTraceTrigger>` (or
/// `Arc::new(CraneliftHotTrigger)`) before installing on a
/// `BcVmConfig`.
#[derive(Debug, Default, Clone, Copy)]
pub struct CraneliftHotTrigger;

impl HotTraceTrigger for CraneliftHotTrigger {
    fn on_hot(&self, fn_id: u32, args: &[VmValue]) {
        // Wrap in `catch_unwind` so a panic inside the recorder /
        // optimiser / emitter never aborts the bytecode dispatch
        // loop. The bytecode VM has no convenient unwind boundary
        // around `on_hot` (the trait docs spell this out), so the
        // adapter owns the safety net.
        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| unsafe {
            // SAFETY: `args.as_ptr()` is a packed `u64` array with
            // `args.len()` elements; the helper's contract accepts a
            // null ptr (when args.is_empty()) or a non-null ptr that
            // points at `param_tys.len() <= args.len()` slots. The
            // bytecode VM's calling convention puts one `u64` per
            // declared param, matching the registry's `param_tys`.
            let ptr = if args.is_empty() {
                core::ptr::null()
            } else {
                args.as_ptr()
            };
            __relon_jump_to_recorder(fn_id, ptr);
        }));
        if let Err(panic_payload) = result {
            // Log via tracing rather than re-panicking — the bytecode
            // VM still has a usable invocation in-flight and the cold
            // path will keep handling correctness while the recorder
            // is offline.
            let msg = panic_payload
                .downcast_ref::<&'static str>()
                .copied()
                .or_else(|| panic_payload.downcast_ref::<String>().map(|s| s.as_str()))
                .unwrap_or("<non-string panic payload>");
            tracing::warn!(
                target: "relon::bytecode_bridge",
                fn_id,
                args_len = args.len(),
                panic_msg = msg,
                "CraneliftHotTrigger swallowed a panic from the recorder pipeline"
            );
        }
    }
}

/// v6-δ M2-B phase 4c-cont: native adapter that lets the bytecode VM
/// bypass its own dispatch loop and invoke an installed trace fn.
///
/// ## Why this lives in `relon_codegen_cranelift`
///
/// Same reason as [`CraneliftHotTrigger`]: the bytecode crate stays
/// cranelift-free so the wasm32 build keeps compiling. The unsafe
/// `TraceJitState::invoke_with_resume` indirection lives here; the
/// trait surface ([`InstalledTraceLookup`]) trades raw pointers for
/// safe owned values ([`TraceInvokeOutcome`]).
///
/// ## Dispatch shape
///
/// 1. Look up the trace for `fn_id` via
///    [`global_trace_jit_state`]. If absent →
///    [`TraceInvokeOutcome::NoTrace`].
/// 2. Build a `TraceContext` sized to the trace's `ssa_high_water`,
///    pass the bytecode VM's packed `args` slice through as the
///    `*const u64` second argument, invoke the trace fn.
/// 3. On `TraceEntryStatus::Success` → return
///    [`TraceInvokeOutcome::Success`] carrying `result_slot`.
/// 4. On `TraceEntryStatus::GuardFailed` → return
///    [`TraceInvokeOutcome::Deopt`] carrying the populated
///    [`relon_trace_abi::DeoptStateSnapshot`] (with `value_stack_copy`
///    already rendered from the per-guard SSA-stack table, courtesy of
///    `invoke_with_resume`).
///
/// The `slot_count` we hand to `invoke_with_resume` is the trace's
/// SSA high-water mark, pulled from the installed `JITedTraceFn` —
/// the trait surface doesn't expose it because the bytecode VM
/// doesn't need to know it; the bridge handles the bookkeeping.
///
/// Wrapped in `catch_unwind` so a recorder / install pipeline panic
/// never aborts the bytecode dispatch loop — the surface degrades
/// gracefully to [`TraceInvokeOutcome::NoTrace`] after logging via
/// tracing.
#[derive(Debug, Default, Clone, Copy)]
pub struct CraneliftTraceLookup;

impl InstalledTraceLookup for CraneliftTraceLookup {
    fn try_invoke(&self, fn_id: u32, args: &[VmValue]) -> TraceInvokeOutcome {
        let state = global_trace_jit_state();
        // Cheap fast path: bail out before the heavier
        // invoke_with_resume call if no trace is installed. The
        // lookup is a single `RwLock::read + HashMap::get`, paid on
        // every `run_main`. When the trace is absent we want to
        // return immediately so the bytecode dispatch loop is the
        // only thing the cold path observes.
        let trace_fn = match state.lookup_trace(fn_id) {
            Some(t) => t,
            None => return TraceInvokeOutcome::NoTrace,
        };
        let slot_count = trace_fn.guard_table_len();
        // Detach the trace_fn handle so the lock window inside
        // invoke_with_resume is the only thing we hold.
        drop(trace_fn);

        let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
            // Buffer the outcome through a `Cell` so the unsafe
            // closure can publish either `Success` or `Deopt` without
            // making `TraceInvokeOutcome` `Copy`.
            use std::cell::Cell;
            let captured: Cell<Option<TraceInvokeOutcome>> = Cell::new(None);
            // The args ptr the bytecode VM uses for its calling
            // convention: one `u64` per declared param. Null when
            // args is empty (matches `invoke_with_resume`'s contract
            // — the trace fn ignores the pointer when the recorder
            // didn't record any LocalGet ops).
            let args_ptr = if args.is_empty() {
                core::ptr::null()
            } else {
                args.as_ptr()
            };
            let trace_result = unsafe {
                state.invoke_with_resume(
                    fn_id,
                    args_ptr,
                    slot_count,
                    |_args, _resume_pc, snapshot| {
                        // GuardFailed (or Aborted, which collapses
                        // here with `snapshot == None` after the
                        // trace is invalidated).
                        match snapshot {
                            Some(snap) => {
                                // `DeoptStateSnapshot` does not impl
                                // `Clone` (the `recoverable_writes`
                                // payload deliberately stays move-only
                                // to keep the host-write-undo invariant
                                // unambiguous). Hand-rebuild a fresh
                                // owned copy — the bytecode resume
                                // path only consults `external_pc`,
                                // `ssa_slots_copy`, and
                                // `value_stack_copy`; the recoverable-
                                // writes vector is logically already
                                // consumed by the trace runtime before
                                // we land here, so we leave it empty.
                                let owned = relon_trace_abi::DeoptStateSnapshot::with_value_stack(
                                    snap.guard_pc,
                                    snap.external_pc,
                                    snap.ssa_slots_copy.clone(),
                                    snap.value_stack_copy.clone(),
                                );
                                captured.set(Some(TraceInvokeOutcome::Deopt {
                                    snapshot: Box::new(owned),
                                }));
                            }
                            None => {
                                captured.set(Some(TraceInvokeOutcome::NoTrace));
                            }
                        }
                        // The trace fn's fallback returns a `u64`
                        // that bubbles back out of `invoke_with_resume`
                        // — we don't consult it here because the
                        // bytecode VM's resume path is what produces
                        // the actual return value. Return 0 as a
                        // benign placeholder.
                        0u64
                    },
                )
            };
            match captured.take() {
                Some(outcome) => outcome,
                // No `captured` write means the closure didn't run
                // — i.e. the trace succeeded. `trace_result` is the
                // `TraceContext::result_slot` value.
                None => TraceInvokeOutcome::Success {
                    result: trace_result,
                },
            }
        }));
        match result {
            Ok(outcome) => outcome,
            Err(panic_payload) => {
                let msg = panic_payload
                    .downcast_ref::<&'static str>()
                    .copied()
                    .or_else(|| panic_payload.downcast_ref::<String>().map(|s| s.as_str()))
                    .unwrap_or("<non-string panic payload>");
                tracing::warn!(
                    target: "relon::bytecode_bridge",
                    fn_id,
                    args_len = args.len(),
                    panic_msg = msg,
                    "CraneliftTraceLookup swallowed a panic from the trace dispatch path"
                );
                TraceInvokeOutcome::NoTrace
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relon_bytecode::HotTraceTrigger;

    /// Smoke test: dispatching against an unregistered `fn_id` falls
    /// through the helper's "no IR registration" guard cleanly — no
    /// panic, no global-state mutation observable to the caller.
    ///
    /// Full end-to-end recording-driven install lives in
    /// `relon-test-harness::tests::recorded_loop_e2e` because it needs
    /// the shared trace registry + the cranelift IR module the
    /// recorder walks.
    #[test]
    fn adapter_handles_unregistered_fn_id() {
        let trigger = CraneliftHotTrigger;
        // 65_535 is well past the corpus's used `fn_id` range; no
        // `RecordingRegistration` exists for it, so the helper logs +
        // returns without touching the install pipeline. We just
        // verify the call doesn't panic.
        trigger.on_hot(65_535, &[1, 2, 3]);
        trigger.on_hot(65_535, &[]);
    }

    /// Sanity-check the trait wiring through an `Arc<dyn …>` — same
    /// shape `BcVmConfig` stores.
    #[test]
    fn adapter_works_through_trait_object() {
        let trigger: std::sync::Arc<dyn HotTraceTrigger> = std::sync::Arc::new(CraneliftHotTrigger);
        trigger.on_hot(65_535, &[]);
    }

    /// Phase 4c-cont smoke: with no trace installed for the chosen
    /// `fn_id`, the lookup returns `NoTrace`. End-to-end coverage
    /// (real trace install → trace bypass → result_slot) lives in
    /// `relon-test-harness::tests::bytecode_trace_dispatch_switch_e2e`
    /// because it needs the shared registry + the IR module.
    #[test]
    fn trace_lookup_returns_no_trace_for_unregistered_id() {
        let lookup = CraneliftTraceLookup;
        // Same out-of-corpus-range id `CraneliftHotTrigger` uses;
        // safe because no other test installs a trace for it on this
        // thread.
        match lookup.try_invoke(65_535, &[]) {
            TraceInvokeOutcome::NoTrace => {}
            other => panic!("expected NoTrace, got {other:?}"),
        }
        match lookup.try_invoke(65_535, &[1, 2, 3]) {
            TraceInvokeOutcome::NoTrace => {}
            other => panic!("expected NoTrace, got {other:?}"),
        }
    }

    /// Trait-object plumbing through the same shape `BcVmConfig`
    /// stores. Verifies the `dyn InstalledTraceLookup` cast compiles
    /// and the call dispatches cleanly.
    #[test]
    fn trace_lookup_works_through_trait_object() {
        let lookup: std::sync::Arc<dyn InstalledTraceLookup> =
            std::sync::Arc::new(CraneliftTraceLookup);
        match lookup.try_invoke(65_535, &[]) {
            TraceInvokeOutcome::NoTrace => {}
            other => panic!("expected NoTrace, got {other:?}"),
        }
    }
}
