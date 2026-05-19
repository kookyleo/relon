//! v6-ε-0-C: trace-entry calling convention selection.
//!
//! The trace entry historically used [`cranelift_codegen::isa::CallConv::SystemV`]
//! across the board. v6-δ M2-C bench accounting (see
//! `docs/internal/v6-delta-m2c-stage-report-2026-05-19.md` §5) showed
//! that the 9.5 ns/iter hot-loop floor is dominated by the per-trace
//! SystemV ABI prologue + epilogue, not by the Rust-side IC dispatch
//! layer. v6-ε-0-C trials the cheapest mitigation: switch the trace
//! entry's call conv to [`CallConv::Tail`].
//!
//! ## Why Tail is wire-compatible with our `extern "C"` callers
//!
//! The trace entry's [`relon_trace_abi::TRACE_ENTRY_SIG`] is
//! `(*mut TraceContext, *const u64) -> i32`: two pointer args + one
//! `i32` return. On x86_64 + aarch64 the [`CallConv::Tail`] register
//! allocation for this shape is identical to [`CallConv::SystemV`]:
//!
//! - **x86_64**: arg0 in `rdi`, arg1 in `rsi`, return in `rax`. The
//!   callee-save set (`rbx`, `rbp`, `r12`-`r14`) matches SystemV.
//!   Clobbered-by-call set is SystemV's `SYSV_CLOBBERS` (see
//!   `cranelift-codegen/src/isa/x64/abi.rs:898`). The wire-level
//!   difference reduces to "callee pops stack args" — moot here
//!   because the signature has zero stack args.
//! - **aarch64**: same arg/return registers, same callee-save set,
//!   same clobber set in non-exception paths. Tail starts the GPR
//!   allocator at a different index but for two args the resulting
//!   register pair is the same (`x0`, `x1`).
//!
//! This means a Rust caller declaring the entry pointer as
//! `unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32`
//! can invoke a Tail-conv callee with no marshalling shim. The
//! cranelift verifier accepts the cross-conv `call` because the
//! `supports_tail_calls` check (`cranelift-codegen/src/verifier/mod.rs:1683`)
//! only fires on the `return_call` opcode — a regular non-tail
//! `call` to a Tail-conv callee is fine.
//!
//! ## Target gating
//!
//! `CallConv::Tail` lowers reliably on x86_64 + aarch64 in cranelift
//! 0.131. On riscv64 the conv exists but the integration tests don't
//! cover our combination of leaf trace fn + indirect-call deopt
//! helper. On every other target (s390x, x86, riscv32, pulley, ...)
//! we fall back to [`CallConv::SystemV`] to preserve M2-C behaviour
//! exactly.
//!
//! The gate is a `cfg(target_arch)` choice resolved at host compile
//! time, not at runtime: the cranelift module ISA the host installs
//! is also pinned to that host triple via `cranelift_native::builder`,
//! so the two pieces always agree.
//!
//! ## Why this is "Tail-as-an-ABI" rather than `return_call`
//!
//! v6-ε plan §3 lists three options for ε-0:
//!
//! - **A. At-call-site inline** — splices the trace body into the
//!   host fn. Future ε-0-A work; needs cranelift-module patch-point
//!   API.
//! - **B. Trace-to-trace fall-through** — uses cranelift's
//!   `return_call` opcode for tail calls between traces. Future
//!   ε-0-B work; deferred per the plan.
//! - **C. `CallConv::Tail`** — this module. We use the Tail calling
//!   convention's *register-saving discipline* even though every
//!   call into a trace is still a regular `call`. The savings come
//!   from Tail's tighter prologue/epilogue, not from doing a tail
//!   call from the caller.
//!
//! The naming is a Cranelift quirk: `CallConv::Tail` was designed to
//! support tail-call-from-trace-to-trace chains, but the ABI itself
//! works for any caller pattern. ε-0-C exploits exactly that: cheap
//! prologue/epilogue without yet wiring the tail-call infrastructure.

use cranelift_codegen::isa::CallConv;

/// Returns the calling convention used for trace entry functions on
/// the current host architecture.
///
/// - x86_64 / aarch64: [`CallConv::Tail`] — wire-compatible with the
///   `unsafe extern "C"` ABI Rust callers use, but with a tighter
///   prologue/epilogue than SystemV.
/// - All other targets: [`CallConv::SystemV`] — preserves the v6-δ
///   M2-C baseline behaviour.
///
/// The host's cranelift module is pinned to the same target triple
/// (via `cranelift_native::builder`) so the conv this function picks
/// is always the conv the produced machine code actually exercises.
pub fn trace_entry_call_conv() -> CallConv {
    #[cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]
    {
        CallConv::Tail
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        CallConv::SystemV
    }
}

/// Returns `true` when [`trace_entry_call_conv`] selects
/// [`CallConv::Tail`] for the current host. Mainly for tests / docs
/// that want to assert which path is live; production callers should
/// use [`trace_entry_call_conv`] directly.
pub fn trace_entry_uses_tail() -> bool {
    matches!(trace_entry_call_conv(), CallConv::Tail)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pick_matches_target_arch() {
        let cc = trace_entry_call_conv();
        if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
            assert_eq!(cc, CallConv::Tail);
            assert!(trace_entry_uses_tail());
        } else {
            assert_eq!(cc, CallConv::SystemV);
            assert!(!trace_entry_uses_tail());
        }
    }
}
