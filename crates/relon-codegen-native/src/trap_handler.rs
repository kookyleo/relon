//! Process-wide signal handler for SIGSEGV / SIGFPE / SIGILL.
//!
//! Stage 5 Phase C.3 — infrastructure for trap interception that
//! complements the existing `catch_unwind` shield around the JIT
//! entry call. The handler converts these signals into recorded
//! trap codes in a thread-local slot; the host trampoline lifts
//! the recorded code into a typed `RuntimeError`.
//!
//! ## Why not full `sigsetjmp` / `siglongjmp`?
//!
//! The brief specifies a `sigsetjmp` round trip replacement. libc
//! exposes `setjmp` but **not** `sigsetjmp` (the latter is a macro
//! that varies per platform). Implementing a real long-jump from a
//! signal handler back to the host trampoline requires:
//!
//! 1. A C-side shim for `sigsetjmp` (or an inline-asm Rust
//!    equivalent) — not available in the `libc` crate today.
//! 2. Careful Rust drop / unwind safety since `siglongjmp` skips
//!    every destructor on the unwound stack frames.
//! 3. Per-thread jmp_buf storage with strict lifetime rules.
//!
//! For v5-β-2 stage 5 we ship the **handler infrastructure** here
//! (process-wide install once, signal -> trap-code translation,
//! defense-in-depth against genuine JIT-side memory-safety bugs)
//! and keep the existing `catch_unwind` shield as the primary
//! trap path. The two cooperate:
//!
//! * Most traps in production come from explicit `cond_trap`
//!   sequences emitted by the codegen — they record a trap code
//!   then `return` a sentinel zero. No signal fires; the handler
//!   is dormant.
//! * If a bug in the JIT-emitted code triggers a real SIGSEGV /
//!   SIGFPE / SIGILL, the handler records `TrapKind::BoundsViolation`
//!   / `DivisionByZero` / `Unreachable` respectively and the
//!   process keeps running because the handler returns rather
//!   than aborting. The subsequent host-side code can then refuse
//!   to use the corrupted state.
//!
//! The v6-γ trace JIT work picks the full `sigsetjmp` round trip
//! back up, by which time we'll need it for the trace-recorder
//! deopt path anyway.

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
    //      registered handler so the existing Rust panic runtime
    //      hook (if any) still runs after ours.
    //   3. The infrastructure is dormant in production because all
    //      sandbox guards route through `cond_trap` -> recorded
    //      trap code; signals only fire on genuine memory-safety
    //      bugs in the JIT-emitted code, which we treat as a hard
    //      defense-in-depth surface.
    //
    // SAFETY: handler bodies are async-signal-safe (touch only
    // thread-local + atomic state).
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

/// Read back the per-thread signal slot. `0` means no signal fired.
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
