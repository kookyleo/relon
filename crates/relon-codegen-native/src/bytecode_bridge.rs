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
//! use relon_codegen_native::bytecode_bridge::CraneliftHotTrigger;
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
use relon_bytecode::HotTraceTrigger;

use crate::trace_install::__relon_jump_to_recorder;

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
        let trigger: std::sync::Arc<dyn HotTraceTrigger> =
            std::sync::Arc::new(CraneliftHotTrigger);
        trigger.on_hot(65_535, &[]);
    }
}
