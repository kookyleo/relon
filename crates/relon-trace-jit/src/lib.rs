//! `relon-trace-jit` -- v6-gamma trace-JIT scaffolding.
//!
//! This crate is a **pre-integration** drop. It defines the
//! self-contained data structures + algorithms the v6-gamma trace JIT
//! will need, without yet depending on `relon-ir` or
//! `relon-codegen-native`. The goal is to let the trace IR, hot
//! counter, deopt protocol, and optimiser passes be designed and
//! unit-tested in isolation; the v6-gamma phase then bolts on a
//! lowering layer (`relon_ir::Op -> TraceOp`) and a cranelift IR
//! emitter for the optimised trace.
//!
//! ## Public surface
//!
//! The intent is for the v6-gamma integration code to depend only on
//! the exports below. Internal modules are visible inside the crate
//! for unit tests but are NOT considered stable surface.
//!
//! - [`TraceBuffer`] / [`OptimizedTrace`] -- the recorder's primary
//!   data structure and its frozen form.
//! - [`HotCounter`] / [`RecordResult`] -- entry-point counter
//!   bookkeeping (design doc §1.2).
//! - [`EffectClass`] -- side-effect classification used to validate
//!   trace recording (§3 red lines).
//! - [`GuardSite`] / [`GuardKind`] / [`DeoptState`] /
//!   [`RecoverableWrite`] -- the deopt-protocol structures (§3).
//! - [`TraceOp`] / [`SsaVar`] / [`ExternalPc`] / [`ExternalSlot`] /
//!   [`ExternalAddr`] -- the low-level trace IR.
//! - [`OptimizerPass`] / [`OptimizerPipeline`] -- the optimiser
//!   pipeline plumbing.

pub mod buffer;
pub mod counter;
pub mod effect;
pub mod guard;
pub mod inline_cache;
pub mod optimizer;
pub mod trace_ir;

pub use buffer::{OptimizedTrace, SerializableSideTables, TraceBuffer};
pub use counter::{HotCounter, RecordResult, COUNTER_SATURATED};
pub use effect::EffectClass;
pub use guard::{DeoptState, GuardSite, RecoverableWrite};
pub use inline_cache::{CacheResult, InlineCache};
pub use optimizer::{
    const_fold::ConstFold, dead_store::DeadStoreElim, licm::LICM, load_forward::LoadForwarding,
    type_spec::TypeSpec, OptimizerPass, OptimizerPipeline, PassReport,
};
pub use trace_ir::{
    CmpKind, ExternalAddr, ExternalPc, ExternalSlot, FuncId, GuardKind, ObservedType, Offset,
    SsaVar, TraceConst, TraceOp,
};
