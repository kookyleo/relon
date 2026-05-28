//! Trap-signal mapping helper.
//!
//! Relon's typed trap path does not depend on OS signal handling:
//! guarded operations lower to structured branches that call
//! `relon_raise_trap(state, code)`, and the host trampoline reads
//! `SandboxState::trap_code` after the JIT call returns. This module
//! only keeps the libc-signal-to-`TrapKind` mapping so platform crash
//! integrations can render a familiar error shape if they implement a
//! real recovery trampoline.
//!
//! No process-wide SIGSEGV / SIGFPE / SIGILL handler is installed from
//! Rust: a handler body writing into a Rust `thread_local!` is not
//! POSIX async-signal-safe, and a process-wide handler could run on
//! threads unrelated to Relon's JIT. Hardware faults remain fail-fast
//! process crashes until a `sigsetjmp` / landing-pad shim is added
//! host-side.

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
