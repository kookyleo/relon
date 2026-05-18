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
//! This commit lands the ABI module: the fixed trace-entry
//! signature, `TraceContext` shape, and concrete bindings for the
//! opaque `ExternalPc / ExternalSlot / ExternalAddr` newtypes from
//! `relon-trace-jit`. The full emitter + guard emission paths arrive
//! in follow-up commits.

pub mod abi;

pub use abi::{
    AbiSignature, CraneliftType, ExternalAddrRepr, ExternalPcRepr, ExternalSlotRepr, HostHookTable,
    TraceContext, TraceEntryStatus, TRACE_ENTRY_SIG,
};
