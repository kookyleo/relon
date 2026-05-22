//! Process-wide signal handler for SIGSEGV / SIGFPE / SIGILL.
//!
//! **Scope:** telemetry / fail-fast hook. This handler is **not** a
//! recovery mechanism. A synchronous hardware fault (memory access
//! violation, integer divide-by-zero, illegal instruction) inside
//! JIT-emitted code is treated as an unrecoverable defect; the
//! handler exists to (a) record a coarse signal code in a thread-
//! local slot so the trampoline can attach a typed `RuntimeError`
//! when control returns naturally, and (b) chain into any pre-
//! existing handler so the platform's default termination /
//! crash-reporting flow still runs.
//!
//! ## Why this is fail-fast, not recoverable
//!
//! A signal-hook handler that merely sets a thread-local slot and
//! returns does **not** rewrite the saved instruction pointer in
//! the trapping ucontext. When the kernel resumes the thread it
//! re-executes the faulting instruction, which faults again. With
//! `signal-hook`'s chain semantics the next handler in the chain
//! (or the platform default — typically `SIG_DFL` -> core dump /
//! `abort`) then takes over. There is no clean return to the host
//! trampoline from a synchronous hardware fault.
//!
//! Recovering for real requires either:
//!
//! * `sigsetjmp` / `siglongjmp` round-trip (a C-side shim because
//!   `libc` exposes `setjmp` but **not** `sigsetjmp`; the latter
//!   is a per-platform macro). Tracked as the Option B follow-up
//!   below.
//! * Rewriting `ucontext_t::uc_mcontext::rip` (Linux x86_64) inside
//!   the handler to a "trap landing pad" emitted into the JIT body.
//!   Cross-platform variants are non-trivial.
//!
//! Both are out of scope for the current stage; we ship the
//! infrastructure and the doc honestly admits the limits.
//!
//! ## How typed traps actually surface
//!
//! In practice, the **typed** trap path does not depend on this
//! handler at all:
//!
//! * Every guarded operation in the codegen (`cond_trap` in
//!   `codegen::mod`) is lowered to a structured branch that calls
//!   `relon_raise_trap(state, code)` and returns a sentinel `0`
//!   from the entry. The host trampoline reads
//!   `SandboxState::trap_code` after the JIT call returns and lifts
//!   the code into a typed `RuntimeError`.
//! * Helper-call symbols emitted by the codegen are audited as
//!   non-panicking on their hot paths; any panic that does leak
//!   through (e.g. a debug assertion in a misbehaving capability
//!   helper) is converted to a typed error by the `catch_unwind`
//!   shield around the JIT entry call.
//!
//! The signal slot is therefore only consulted as a
//! defense-in-depth observation: if a hardware fault did occur and
//! by some chance control returned to the trampoline (e.g. a
//! follow-on handler in the chain rewrote the IP), the trampoline
//! converts the recorded signal code into a typed `RuntimeError`
//! rather than silently returning a sentinel. In the much more
//! likely case where the process aborts on the chained default
//! handler, the slot write is harmless — by then the process is
//! already on its way out.
//!
//! ## Follow-up: Option B — real `sigsetjmp` round trip
//!
//! The v6-γ trace-recorder work needs a real long-jump path for
//! deopt anyway. When that lands, this module gets a C shim
//! exposing `sigsetjmp` + per-thread `sigjmp_buf` storage, and the
//! handler longjmps back to a setjmp point installed by the host
//! trampoline. At that point hardware faults become recoverable
//! and the module-level promise can be tightened.

use std::sync::Once;

/// Idempotent installer for the process-wide signal handlers.
/// Subsequent calls are no-ops.
///
/// SAFETY: the handlers we install only write to atomic statics and
/// the thread-local trap slot — both signal-safe operations. We
/// deliberately avoid any allocations / locks / Drop-running code
/// inside the handlers.
pub fn install_global_signal_handler() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // SAFETY: signal-hook's `register` API is documented as safe
        // when the handler is itself signal-safe. Our handler only
        // touches thread-local + atomic state.
        let _ = install_handlers_inner();
    });
}

#[cfg(unix)]
fn install_handlers_inner() -> Result<(), std::io::Error> {
    // signal-hook 0.3 marks SIGSEGV / SIGFPE / SIGILL as "forbidden"
    // because installing handlers for them from library code can
    // race with the Rust panic runtime and other registry-aware
    // libraries. The `register_unchecked` API bypasses the check —
    // we use it deliberately because:
    //
    //   1. Our handler is async-signal-safe (only touches
    //      thread-locals + atomics, no allocs / locks / Drop).
    //   2. We register *additionally* — signal-hook chains every
    //      registered handler so the platform default (or the
    //      Rust panic runtime hook, if any) still runs after ours.
    //      The chain provides the actual fail-fast behaviour for
    //      synchronous hardware faults; see the module-level doc
    //      on why this is telemetry / fail-fast, not recoverable.
    //   3. The infrastructure is dormant in production because all
    //      sandbox guards route through `cond_trap` -> recorded
    //      trap code; signals only fire on genuine memory-safety
    //      bugs in the JIT-emitted code, which we treat as a hard
    //      defense-in-depth surface.
    //
    // SAFETY: handler bodies are async-signal-safe (touch only
    // thread-local + atomic state). They do not attempt to recover
    // from the fault — see the module-level doc.
    use signal_hook::consts::*;
    use signal_hook_registry::register_signal_unchecked;
    unsafe {
        register_signal_unchecked(SIGSEGV, || {
            LAST_SIGNAL_CODE.with(|c| c.set(SIGSEGV));
        })?;
        register_signal_unchecked(SIGFPE, || {
            LAST_SIGNAL_CODE.with(|c| c.set(SIGFPE));
        })?;
        register_signal_unchecked(SIGILL, || {
            LAST_SIGNAL_CODE.with(|c| c.set(SIGILL));
        })?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn install_handlers_inner() -> Result<(), std::io::Error> {
    // Non-unix targets (wasm32 / windows for non-AOT builds) don't
    // install handlers — wasm32 doesn't even compile the
    // cranelift-jit path, and windows uses SEH which is outside the
    // signal-hook surface.
    Ok(())
}

#[cfg(unix)]
thread_local! {
    /// Last signal code observed in this thread. `0` means no
    /// signal has been observed since the slot was reset. The host
    /// trampoline resets this before each JIT entry call and reads
    /// it after to decide whether to lift the result into a typed
    /// trap.
    static LAST_SIGNAL_CODE: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// Reset the per-thread signal slot. Called before every JIT entry
/// invocation so post-call reads pick up only signals fired during
/// that specific call.
pub fn reset_thread_signal_slot() {
    #[cfg(unix)]
    LAST_SIGNAL_CODE.with(|c| c.set(0));
}

/// Read back the per-thread signal slot. `0` means no signal fired
/// (or control did not return after the fault — see the module-
/// level doc for why a non-zero read is best-effort defense-in-
/// depth, not a guaranteed observation).
///
/// The returned code is a libc signal number (`SIGSEGV` / `SIGFPE` /
/// `SIGILL`); the trampoline maps each to a [`crate::TrapKind`].
pub fn read_thread_signal_slot() -> i32 {
    #[cfg(unix)]
    {
        LAST_SIGNAL_CODE.with(|c| c.get())
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// Map a libc signal code into the matching [`crate::sandbox::TrapKind`].
/// Returns `None` for unknown / un-handled signals.
pub fn signal_to_trap_kind(sig: i32) -> Option<crate::sandbox::TrapKind> {
    #[cfg(unix)]
    {
        use signal_hook::consts::*;
        match sig {
            SIGSEGV => Some(crate::sandbox::TrapKind::BoundsViolation),
            SIGFPE => Some(crate::sandbox::TrapKind::DivisionByZero),
            SIGILL => Some(crate::sandbox::TrapKind::Unreachable),
            _ => None,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = sig;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_handler_is_idempotent() {
        install_global_signal_handler();
        install_global_signal_handler();
        install_global_signal_handler();
        // No panic / abort = success.
    }

    #[test]
    fn reset_then_read_returns_zero() {
        install_global_signal_handler();
        reset_thread_signal_slot();
        assert_eq!(read_thread_signal_slot(), 0);
    }

    #[test]
    fn signal_to_trap_kind_maps_sigfpe_to_division_by_zero() {
        #[cfg(unix)]
        {
            use signal_hook::consts::SIGFPE;
            assert!(matches!(
                signal_to_trap_kind(SIGFPE),
                Some(crate::sandbox::TrapKind::DivisionByZero)
            ));
        }
    }

    #[test]
    fn signal_to_trap_kind_maps_sigsegv_to_bounds_violation() {
        #[cfg(unix)]
        {
            use signal_hook::consts::SIGSEGV;
            assert!(matches!(
                signal_to_trap_kind(SIGSEGV),
                Some(crate::sandbox::TrapKind::BoundsViolation)
            ));
        }
    }

    #[test]
    fn signal_to_trap_kind_maps_sigill_to_unreachable() {
        #[cfg(unix)]
        {
            use signal_hook::consts::SIGILL;
            assert!(matches!(
                signal_to_trap_kind(SIGILL),
                Some(crate::sandbox::TrapKind::Unreachable)
            ));
        }
    }

    #[test]
    fn signal_to_trap_kind_returns_none_for_unknown_signal() {
        // Use a signal code outside our known set.
        assert!(signal_to_trap_kind(99).is_none());
    }
}
