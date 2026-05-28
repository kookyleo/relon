//! `relon-trace-emitter` -- v6-gamma trace JIT emitter.
//!
//! Third piece of the v6-gamma trace JIT. After `relon-trace-jit`
//! provides the self-contained trace IR + optimiser and
//! `relon-trace-recorder` lowers Relon-IR execution history into a
//! [`relon_trace_jit::OptimizedTrace`], this crate translates that
//! frozen trace into a [`cranelift_codegen::ir::Function`] suitable
//! for the host backend to compile + install in a hot function's
//! dispatch slot.
//!
//! ## Surface
//!
//! Two entry points matter:
//!
//! 1. [`TraceEmitter::emit`] — drains an [`relon_trace_jit::OptimizedTrace`]
//!    into a pre-built [`cranelift_codegen::Context`]. After a successful
//!    emit the caller can hand the context off to its existing
//!    cranelift module pipeline (same path `relon-codegen-cranelift` uses
//!    for non-trace functions).
//! 2. [`TRACE_ENTRY_SIG`] / [`TraceContext`] — the fixed ABI every
//!    trace entry obeys. The host installs trace pointers through this
//!    signature so dispatch is a uniform indirect call.
//!
//! ## Status
//!
//! Pre-integration: the emitter produces cranelift IR but does **not**
//! finalise a JIT module or execute machine code. Tests use cranelift's
//! [`cranelift_codegen::verifier::verify_function`] to confirm the
//! emitted IR is well-formed. Integration with the JIT module + ISA
//! lives in the v6-gamma phase, alongside the deopt-dispatch host
//! helper and the hot-counter inject pass.
//!
//! ## inline_emit / emitter sync
//!
//! [`emitter::TraceEmitter`] (standalone trampoline entry) and
//! [`inline_emit::emit_trace_inline`] (at-call-site embed) historically
//! carried line-for-line copies of the per-op lowering rules. P1-12
//! collapsed the genuinely identical helpers (`Add`/`Sub`/`Mul`/`Div`/
//! `Cmp`/`Load`/`Store`/`Const*`/`LocalGet`) into [`op_lower`], so both
//! emit paths now drive the shared lowering via the
//! [`op_lower::OpLowerer`] trait. Divergent ops — `Return`,
//! `MarkLoopHead`/`MarkLoopBack`, `Mod` (the standalone path has
//! magic-mod + preheader-hoist), and the str / dict / list / Call host
//! helpers (standalone-only; the inline path returns
//! `CallNotSupportedInInline`) — stay per-path.
//!
//! The sync invariant is still: every `TraceOp` variant matched in one
//! emit path's `emit_op` MUST also be matched in the other — as a real
//! helper call (potentially the same `lower_*` free function) OR an
//! explicit `Err(InlineEmitError::CallNotSupportedInInline)` route.
//! Adding a new op means touching both files.
//!
//! Guards:
//!
//! * `tests/inline_emit_sync_lint.rs` — source-level lint that scrapes
//!   both `fn emit_op` bodies and asserts the `TraceOp::<Variant>` sets
//!   they reference are equal AND cover every variant declared in
//!   `relon_trace_jit::TraceOp`. Drift fails `cargo test` at compile +
//!   test time.
//! * `crates/relon-codegen-cranelift/tests/trace_jit_inline_smoke.rs`
//!   `inline_matches_standalone_result` — runtime equivalence check
//!   on a small canned trace (the original sync guard; still useful
//!   for catching divergent codegen on shared variants).

pub mod abi;
pub mod call_conv;
#[cfg(test)]
pub mod dict_inline;
pub mod emitter;
pub mod guard_emit;
pub mod inline_emit;
pub mod op_lower;
pub mod str_inline;

pub use abi::{
    abi_type_to_cranelift, host_hook_slot_offset, host_hooks_offset, result_slot_offset,
    AbiSignature, AbiSignatureExt, AbiType, CraneliftType, DeoptStateSnapshot, EffectClass,
    ExternalAddr, ExternalAddrRepr, ExternalPc, ExternalPcRepr, ExternalSlot, ExternalSlotRepr,
    HostHookId, HostHookTable, ObservedType, RecoverableWriteRecord, TraceContext,
    TraceEntryStatus, TRACE_ENTRY_SIG,
};
pub use call_conv::{trace_entry_call_conv, trace_entry_uses_tail};
pub use emitter::{EmitError, HostHookFuncIds, TraceEmitter};
pub use guard_emit::{emit_guard, GuardEmitCtx};
pub use inline_emit::{
    emit_trace_inline, should_inline_trace, InlineEmitError, InlineEmitHandles, MAX_INLINE_OPS,
};
pub use str_inline::{
    concat_rhs_fits_inline, emit_str_concat_inline_short_rhs, emit_str_contains_inline,
    load_string_ref_payload, needle_fits_inline, HaystackHandle, StrPayload,
    MAX_INLINE_CONCAT_RHS_LEN, MAX_INLINE_NEEDLE_LEN,
};
