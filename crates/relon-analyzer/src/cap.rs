//! Capability mirrors used by the analyzer's static reachability check.
//!
//! These types intentionally duplicate the shape of
//! [`relon_evaluator::eval::Capabilities`] / `NativeFnGate` rather than
//! depending on the evaluator crate. The analyzer sits *below* the
//! evaluator in the dependency graph (the evaluator pulls the analyzer's
//! workspace tree at startup); reaching into the evaluator from here
//! would create a cycle. The mirror is small and stable — every
//! capability bit lands in both crates simultaneously.
//!
//! Hosts that drive the analyzer feed the evaluator's `Capabilities`
//! into [`Capabilities`] via field-by-field copy at the facade layer
//! (e.g. `relon`'s `Context::analyze_workspace` adapter). Field names
//! match exactly so that copy is a one-liner.

/// Per-fn capability requirement declared at registration time. Mirrors
/// `relon_evaluator::eval::NativeFnGate` field-for-field. A pure fn
/// carries `NativeFnGate::default()` (every bit zero) and is trivially
/// satisfied by any [`Capabilities`].
///
/// `#[non_exhaustive]`: future capability bits are added here without a
/// breaking semver bump. External callers should construct via
/// `NativeFnGate::default()` and set the bits they need.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct NativeFnGate {
    /// Function reads from the filesystem.
    pub reads_fs: bool,
    /// Function writes to or mutates the filesystem.
    pub writes_fs: bool,
    /// Function makes network requests.
    pub network: bool,
    /// Function reads wall / monotonic clocks.
    pub reads_clock: bool,
    /// Function reads process environment.
    pub reads_env: bool,
    /// Function consumes randomness from a non-deterministic source.
    pub uses_rng: bool,
}

impl NativeFnGate {
    /// Capability bits required by this gate that are *not* granted in
    /// `caps`. Iteration order is the field-declaration order; the
    /// analyzer emits one diagnostic per entry, runtime stops at the
    /// first.
    pub fn missing_bits(&self, caps: &Capabilities) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.reads_fs && !caps.reads_fs {
            out.push("reads_fs");
        }
        if self.writes_fs && !caps.writes_fs {
            out.push("writes_fs");
        }
        if self.network && !caps.network {
            out.push("network");
        }
        if self.reads_clock && !caps.reads_clock {
            out.push("reads_clock");
        }
        if self.reads_env && !caps.reads_env {
            out.push("reads_env");
        }
        if self.uses_rng && !caps.uses_rng {
            out.push("uses_rng");
        }
        out
    }
}

/// Context-wide grant the host hands the evaluator. Mirrors the bit
/// grants from `relon_evaluator::eval::Capabilities`. Resource budgets
/// (`max_steps`, `max_value_elements`) are excluded — they affect
/// runtime evaluation, not static reachability.
///
/// `#[non_exhaustive]`: same rationale as the evaluator twin.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Capabilities {
    /// Filesystem reads permitted.
    pub reads_fs: bool,
    /// Filesystem writes permitted.
    pub writes_fs: bool,
    /// Network access permitted.
    pub network: bool,
    /// Clock reads permitted.
    pub reads_clock: bool,
    /// Process environment reads permitted.
    pub reads_env: bool,
    /// Randomness (non-deterministic) permitted.
    pub uses_rng: bool,
}

impl Default for Capabilities {
    /// Zero-trust default — every bit off. Matches the evaluator's
    /// `Capabilities::default()` shape.
    fn default() -> Self {
        Self {
            reads_fs: false,
            writes_fs: false,
            network: false,
            reads_clock: false,
            reads_env: false,
            uses_rng: false,
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
            reads_fs: true,
            writes_fs: true,
            network: true,
            reads_clock: true,
            reads_env: true,
            uses_rng: true,
        }
    }
}
