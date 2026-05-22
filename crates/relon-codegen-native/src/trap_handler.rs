//! Trap-signal mapping helpers.
//!
//! Relon's typed trap path does not depend on OS signal handling:
//! guarded operations lower to structured branches that call
//! `relon_raise_trap(state, code)`, and the host trampoline reads
//! `SandboxState::trap_code` after the JIT call returns. This module
//! only keeps the libc-signal-to-`TrapKind` mapping so platform crash
//! integrations can render a familiar error shape if they implement a
//! real recovery trampoline.
//!
//! We intentionally do **not** install a process-wide SIGSEGV /
//! SIGFPE / SIGILL handler from Rust. Earlier revisions registered a
//! fail-fast telemetry handler and wrote into a Rust `thread_local!`
//! slot from the handler body. That is not a
//! POSIX async-signal-safe operation: TLS initialization and Rust
//! runtime paths are not valid from a synchronous signal context.
//! Worse, the handler was process-wide, so it could run on threads
//! unrelated to Relon's JIT.
//!
//! Recovering from a hardware fault requires a real host-side
//! mechanism such as:
//!
//! * `sigsetjmp` / `siglongjmp` round-trip (a C-side shim because
//!   `libc` exposes `setjmp` but **not** `sigsetjmp`; the latter
//!   is a per-platform macro).
//! * Rewriting `ucontext_t::uc_mcontext::rip` (Linux x86_64) inside
//!   the handler to a "trap landing pad" emitted into the JIT body.
//!   Cross-platform variants are non-trivial.
//!
//! Until such a mechanism exists, hardware faults remain fail-fast
//! process crashes handled by the platform default signal behavior.

/// Historical idempotent installer hook.
///
/// This intentionally does nothing. Keeping the function avoids
/// churn at existing call sites while making the safety policy
/// explicit: Relon does not register process-wide signal handlers
/// from Rust.
pub fn install_global_signal_handler() {}

/// Historical per-thread signal-slot reset.
///
/// No signal slot exists in the current implementation because no
/// Rust signal handler is installed. This remains a no-op so the
/// dispatch trampoline can keep the same pre-call structure.
pub fn reset_thread_signal_slot() {}

/// Historical per-thread signal-slot read. Always returns `0`
/// because hardware faults are no longer observed through Rust TLS.
pub fn read_thread_signal_slot() -> i32 {
    0
}

/// Map a libc signal code into the matching [`crate::sandbox::TrapKind`].
/// Returns `None` for unknown / un-handled signals.
pub fn signal_to_trap_kind(sig: i32) -> Option<crate::sandbox::TrapKind> {
    #[cfg(unix)]
    {
        use libc::{SIGFPE, SIGILL, SIGSEGV};
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
            use libc::SIGFPE;
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
            use libc::SIGSEGV;
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
            use libc::SIGILL;
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
