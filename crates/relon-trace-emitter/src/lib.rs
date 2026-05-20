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
//!    cranelift module pipeline (same path `relon-codegen-native` uses
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

pub mod abi;
pub mod call_conv;
pub mod emitter;
pub mod guard_emit;
pub mod inline_emit;
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
    emit_str_contains_inline, emit_str_contains_inline_preloaded, load_string_ref_payload,
    needle_fits_inline, StrPayload, MAX_INLINE_NEEDLE_LEN,
};
