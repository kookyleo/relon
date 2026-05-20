//! `relon-trace-recorder` — v6-gamma trace recorder (pre-integration).
//!
//! Given a stream of Relon IR ops the cranelift-generic backend is
//! executing, the recorder produces:
//!
//! * a [`relon_trace_jit::TraceBuffer`] whose [`relon_trace_jit::TraceOp`]
//!   stream mirrors the hot-path observations,
//! * per-SSA observed-type metadata + emitted `TypeCheck` guards,
//! * abort decisions whenever an op falls outside the trace-safe
//!   subset (unrecoverable effect, unsupported op, hetero typed
//!   reuse).
//!
//! The recorder is fully **self-contained**: callers feed it
//! `relon_ir::Op` instances paired with the SSA ids of the inputs
//! they have already mapped, the recorder returns the SSA id of the
//! produced value (or an abort reason). The lower-level
//! [`relon_trace_jit::TraceBuffer`] is the single shared state.
//!
//! See `docs/internal/v6-gamma-trace-jit-design.md` §1.3 for the
//! state-machine contract.

pub mod abort;
pub mod lowering;
pub mod recorder;
pub mod type_obs;

pub use abort::AbortReason;
pub use lowering::{
    lower_op, map_effect_class, LookupKind, OpLoweringContext, STDLIB_IDX_CONTAINS,
};
pub use recorder::{LoopCarry, RecordResult, RecorderState, SsaAllocator};
pub use type_obs::{infer_observed_type, ObservedType};

/// Re-exported convenience aliases so downstream callers do not have
/// to depend on `relon-trace-jit` directly when all they want is the
/// recorder façade.
pub use relon_trace_jit::{
    CmpKind, EffectClass as TraceEffectClass, FuncId, GuardKind, Offset, SsaVar, TraceBuffer,
    TraceOp,
};
