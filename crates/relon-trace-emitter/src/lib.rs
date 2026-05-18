//! `relon-trace-emitter` -- v6-gamma trace JIT emitter (skeleton).
//!
//! Third piece of the v6-gamma trace JIT. After `relon-trace-jit`
//! provides the self-contained trace IR + optimiser and
//! `relon-trace-recorder` lowers Relon-IR execution history into a
//! [`relon_trace_jit::OptimizedTrace`], this crate translates that
//! frozen trace into a [`cranelift_codegen::ir::Function`] suitable
//! for the host backend to compile + install in a hot function's
//! dispatch slot.
//!
//! This commit lands the workspace registration + crate skeleton.
//! The ABI surface, emitter, and guard-emission paths arrive in the
//! follow-up commits.
