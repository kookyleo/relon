//! v6-δ M2-B phase 4c-cont: dispatcher-switch hook for the bytecode VM.
//!
//! ## Background
//!
//! Phase 4c wired the bytecode VM's hot-counter prologue +
//! [`crate::HotTraceTrigger`] so a hot loop drives the recorder ➜ JIT
//! install pipeline (`__relon_jump_to_recorder`). What was missing was
//! the **read-side**: once a trace is installed for a given `fn_id`,
//! the bytecode VM still walked its full dispatch loop on every
//! subsequent invocation. The cranelift backend bypasses its own body
//! by jumping straight to the installed trace fn via the entry-fn
//! prologue + IC; the bytecode VM needs the equivalent.
//!
//! This module introduces the trait surface — kept cranelift-free so
//! the bytecode crate still compiles for wasm32 — that the
//! [`crate::evaluator::BytecodeEvaluator`] consults on every
//! `run_main`. The native impl (`relon_codegen_native::bytecode_bridge`)
//! wraps `TraceJitState::invoke_with_resume` and maps its three exit
//! shapes (no trace / success / guard-failed) onto
//! [`TraceInvokeOutcome`].
//!
//! ## Why a trait (not a direct call into trace_install)
//!
//! 1. Wasm32 builds: the bytecode crate is dependency-free of
//!    cranelift / trace-jit. A trait object behind an `Arc` keeps the
//!    coupling at the host wiring level.
//! 2. Testability: phase 4c's mock-based unit tests pin the dispatch
//!    shape (counter saturation, exactly-one-fire) without needing a
//!    cranelift install pipeline. The same pattern applies here —
//!    bytecode-side tests install a `TestTraceLookup` mock that returns
//!    canned `TraceInvokeOutcome` variants to assert the routing.
//! 3. `forbid(unsafe_code)`: the trace fn ABI is `unsafe extern "C"`,
//!    which can't be invoked from the bytecode crate directly. The
//!    bridge in `relon_codegen_native` owns the `unsafe` block; the
//!    trait surface trades `*mut TraceContext` for owned safe values.
//!
//! ## Lookup vs. invoke
//!
//! The trait collapses lookup + invoke into a single
//! [`InstalledTraceLookup::try_invoke`] call so the bridge can hold the
//! `TraceJitState`'s read lock for the minimum window. Splitting them
//! would require the trait surface to expose a `JITedTraceFn` handle,
//! which would re-introduce the cranelift coupling we're trying to
//! avoid.

use std::fmt;
use std::sync::Arc;

use relon_trace_abi::DeoptStateSnapshot;

use crate::vm::VmValue;

/// Outcome of an [`InstalledTraceLookup::try_invoke`] call.
///
/// Mirrors the three exit shapes the cranelift backend's
/// `TraceJitState::invoke_with_resume` produces, lifted to safe owned
/// values so the bytecode crate can route them without touching the
/// trace ABI:
///
/// - [`TraceInvokeOutcome::NoTrace`] — no installed trace for `fn_id`;
///   the evaluator should fall through to the regular bytecode dispatch
///   loop. The hot-counter prologue still runs; if the loop is hot the
///   next invocation may find a trace freshly installed.
/// - [`TraceInvokeOutcome::Success`] — the trace ran to completion and
///   wrote `result` into `TraceContext::result_slot`. The evaluator
///   places this value into the schema's return slot and skips the
///   bytecode dispatch entirely (the M2-C user-visible win on hot
///   loops).
/// - [`TraceInvokeOutcome::Deopt`] — a guard inside the trace fired,
///   producing a populated [`DeoptStateSnapshot`]. The evaluator hands
///   the snapshot to
///   [`crate::BytecodeEvaluator::resume_from_snapshot`] so the bytecode
///   VM picks up exactly where the trace bailed.
#[derive(Debug)]
pub enum TraceInvokeOutcome {
    /// No installed trace for the supplied `fn_id`. The caller should
    /// continue normal dispatch.
    NoTrace,
    /// Trace ran to completion. `result` is the value written into
    /// `TraceContext::result_slot` — the evaluator decodes it via the
    /// declared return type the way the bytecode VM's regular `Return`
    /// op would.
    Success {
        /// The raw `u64` the trace placed in `result_slot`.
        result: u64,
    },
    /// A guard inside the trace fired. The snapshot carries
    /// `external_pc` (the IR PC of the bailing op), `ssa_slots_copy`
    /// (locals state), and `value_stack_copy` (operand-stack state) so
    /// the bytecode VM can pick up the partial computation.
    Deopt {
        /// Boxed because [`DeoptStateSnapshot`] is large and the cold
        /// path doesn't need the inline storage; matches the
        /// `invoke_with_resume` closure's `Option<&DeoptStateSnapshot>`
        /// shape (we own the value here).
        snapshot: Box<DeoptStateSnapshot>,
    },
}

/// Host-supplied bridge the bytecode VM consults on every `run_main`
/// to decide whether a hot-installed trace is available for the
/// `fn_id` it would otherwise dispatch.
///
/// The canonical native impl lives in
/// `relon_codegen_native::bytecode_bridge::CraneliftTraceLookup` and
/// wraps `TraceJitState::invoke_with_resume`. Hosts targeting wasm32 (or
/// unit tests) install a no-op (or a `TestTraceLookup` mock) so the
/// bytecode dispatch loop runs unchanged.
///
/// ## Threading
///
/// `Send + Sync`: a single shared lookup can serve every evaluator on
/// every worker thread. The native impl holds an
/// `&'static TraceJitState`; tests usually wrap a `Mutex<Vec<...>>` to
/// record dispatch shape and assert exactly-once / exactly-N call
/// counts.
///
/// ## Calling convention
///
/// `args` is the same packed-`u64` view the bytecode VM passes to its
/// dispatch loop (one slot per declared `param_ty`, in declaration
/// order). The native bridge forwards `args.as_ptr()` to the trace fn's
/// `*const u64` second argument — the bytecode VM's calling convention
/// matches the cranelift trace ABI by design (phase 4c lined them up).
pub trait InstalledTraceLookup: Send + Sync {
    /// Look up the trace for `fn_id` and invoke it if one is installed.
    ///
    /// Implementations MUST NOT panic — a recorder-side fault should
    /// surface as [`TraceInvokeOutcome::NoTrace`] (after a tracing-log
    /// at `warn`) so the bytecode VM stays a usable fallback.
    fn try_invoke(&self, fn_id: u32, args: &[VmValue]) -> TraceInvokeOutcome;
}

impl fmt::Debug for dyn InstalledTraceLookup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InstalledTraceLookup").finish_non_exhaustive()
    }
}

/// Shared handle to a host-installed trace lookup. Stored on
/// [`crate::vm::BcVmConfig::trace_lookup`] so each `BytecodeVm` clones
/// the `Arc` independently.
pub type InstalledTraceLookupHandle = Arc<dyn InstalledTraceLookup>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Sanity-check the trait shape + outcome enum carry through an
    /// `Arc<dyn …>`. Concrete dispatch wiring is covered by the
    /// evaluator-level tests in
    /// `crates/relon-bytecode/tests/trace_dispatch_switch.rs`.
    struct MockLookup {
        log: Mutex<Vec<(u32, Vec<VmValue>)>>,
        canned: Mutex<Vec<TraceInvokeOutcome>>,
    }

    impl InstalledTraceLookup for MockLookup {
        fn try_invoke(&self, fn_id: u32, args: &[VmValue]) -> TraceInvokeOutcome {
            self.log.lock().unwrap().push((fn_id, args.to_vec()));
            self.canned
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(TraceInvokeOutcome::NoTrace)
        }
    }

    #[test]
    fn outcome_variants_round_trip_through_handle() {
        let mock = Arc::new(MockLookup {
            log: Mutex::new(Vec::new()),
            canned: Mutex::new(vec![
                TraceInvokeOutcome::Success { result: 42 },
                TraceInvokeOutcome::NoTrace,
            ]),
        });
        let handle: InstalledTraceLookupHandle = mock.clone();
        // Pop order: last-in-first-out. First call gets NoTrace.
        assert!(matches!(
            handle.try_invoke(9, &[1, 2]),
            TraceInvokeOutcome::NoTrace
        ));
        assert!(matches!(
            handle.try_invoke(9, &[3]),
            TraceInvokeOutcome::Success { result: 42 }
        ));
        let log = mock.log.lock().unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0], (9, vec![1, 2]));
        assert_eq!(log[1], (9, vec![3]));
    }

    #[test]
    fn deopt_variant_carries_snapshot() {
        let snap = DeoptStateSnapshot::new(7, 0xdead_beef);
        let outcome = TraceInvokeOutcome::Deopt {
            snapshot: Box::new(snap),
        };
        match outcome {
            TraceInvokeOutcome::Deopt { snapshot } => {
                assert_eq!(snapshot.guard_pc, 7);
                assert_eq!(snapshot.external_pc, 0xdead_beef);
            }
            _ => panic!("expected Deopt"),
        }
    }
}
