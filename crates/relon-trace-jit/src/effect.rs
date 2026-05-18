//! Effect classification of trace ops.
//!
//! v6-γ M1 promotes [`EffectClass`] to the shared `relon-trace-abi`
//! crate so the trace-emitter, trace-jit runtime helpers, and the
//! recorder all see one source of truth. This module re-exports the
//! canonical definition; reviewers should **not** add a parallel
//! definition here.
//!
//! ## Variant cheatsheet
//!
//! - [`EffectClass::Pure`] — referentially transparent. Safe to inline,
//!   dedup, and reorder freely across guards.
//! - [`EffectClass::ReadOnly`] — reads external state but never mutates
//!   it. Within a single trace the result is deterministic, so the op
//!   may be hoisted across non-store ops.
//! - [`EffectClass::RecoverableWrite`] — mutates state but the
//!   pre-image is captured so a deopt can restore it (scratch arena
//!   cursor, list_append, etc.). The trace recorder must save the
//!   before-value into the enclosing [`crate::DeoptState`].
//! - [`EffectClass::Unrecoverable`] — irreversible external effect
//!   (network send, host function with hidden state). Trace JIT must
//!   **abort** the moment it sees an op with this class.

pub use relon_trace_abi::EffectClass;
