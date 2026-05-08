//! Capability mirrors used by the analyzer's static reachability check.
//!
//! These types intentionally duplicate the shape of
//! [`relon_evaluator::eval::Capabilities`] / `NativeFnGate` rather than
//! depending on the evaluator crate. The analyzer sits *below* the
//! evaluator in the dependency graph (the evaluator pulls the analyzer's
//! workspace tree at startup); reaching into the evaluator from here
//! would create a cycle. The mirror is small and stable — Stage 4 keeps
//! it in lock-step with the evaluator's v1 (`reads_fs` only) shape, and
//! later stages extend both crates together.
//!
//! Hosts that drive the analyzer feed the evaluator's `Capabilities`
//! into [`Capabilities`] via field-by-field copy at the facade layer
//! (e.g. `relon`'s `Context::analyze_workspace` adapter). Field names
//! match exactly so that copy is a one-liner.

use std::collections::HashSet;

/// Per-fn capability requirement declared at registration time. Mirrors
/// `relon_evaluator::eval::NativeFnGate` exactly. Stage 4 v1 carries
/// only `reads_fs`; future stages add `network`, `writes_fs`, `env`, …
/// in both crates simultaneously.
#[derive(Debug, Clone, Default)]
pub struct NativeFnGate {
    /// The function reads from the filesystem (callers must hold
    /// `Capabilities::reads_fs` to invoke it under sandbox).
    pub reads_fs: bool,
}

/// Context-wide grant the host hands the evaluator. Mirrors the
/// allow-list-shaped fields of `relon_evaluator::eval::Capabilities`.
/// Resource budgets (`max_steps`, `max_value_bytes`) are deliberately
/// excluded — they affect runtime evaluation, not static reachability.
#[derive(Debug, Clone)]
pub struct Capabilities {
    /// If true, every gated native fn is allowed regardless of `name`.
    pub allow_all_native_fn: bool,
    /// Specific native fn names that are allowed even when
    /// `allow_all_native_fn` is off.
    pub allow_native_fn: HashSet<String>,
    /// Filesystem reads permitted. Required to call any native fn whose
    /// gate sets `reads_fs: true`.
    pub reads_fs: bool,
}

impl Default for Capabilities {
    /// Zero-trust default — matches the evaluator's
    /// `Capabilities::default()` shape (no fn calls, no fs).
    fn default() -> Self {
        Self {
            allow_all_native_fn: false,
            allow_native_fn: HashSet::new(),
            reads_fs: false,
        }
    }
}

impl Capabilities {
    /// "Grant everything" preset. Mirrors the evaluator's
    /// `Capabilities::all_granted()` for the fields the analyzer cares
    /// about. Hosts that test under no-sandbox conditions feed this in
    /// and the static check stays silent.
    pub fn all_granted() -> Self {
        Self {
            allow_all_native_fn: true,
            allow_native_fn: HashSet::new(),
            reads_fs: true,
        }
    }
}
