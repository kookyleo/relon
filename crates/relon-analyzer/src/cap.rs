//! Capability types used by the analyzer's static reachability check.
//!
//! These were historically a field-for-field mirror of the evaluator
//! API's `Capabilities` / `NativeFnGate`, duplicated here to avoid a
//! dependency cycle (the analyzer sits *below* `relon-eval-api` in the
//! dep graph, so it could not reach back into it). The canonical
//! definitions now live in the zero-dependency [`relon_cap`] leaf crate,
//! which both this crate and `relon-eval-api` depend on; the mirror is
//! gone. This module re-exports them at the historical
//! `relon_analyzer::cap::{Capabilities, NativeFnGate}` path so every
//! call site keeps resolving unchanged.
//!
//! Hosts that drive the analyzer feed the evaluator's `Capabilities`
//! into [`Capabilities`] — now the *same* type, so the former
//! field-by-field copy at the facade layer collapses to a move/clone.

pub use relon_cap::{Capabilities, NativeFnGate};
