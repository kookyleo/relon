#![forbid(unsafe_code)]
//! Canonical capability data types, deduplicated into a zero-dependency
//! leaf crate.
//!
//! These pure-data types were historically defined in `relon-eval-api`
//! (`CapabilityBit`, `NativeFnGate`, `Capabilities`) and mirrored
//! field-for-field in `relon-analyzer` to avoid a dependency cycle (the
//! analyzer sits *below* the evaluator API in the dep graph, so it could
//! not reach back into it). Hosting them here lets **both** crates depend
//! on a single definition and re-export it at their historical public
//! paths, so every `relon_eval_api::CapabilityBit` /
//! `relon_analyzer::cap::NativeFnGate` reference keeps resolving while the
//! mirror is gone.
//!
//! The enforcement machinery (`CapabilityGate`, `GatedNativeFn`,
//! `NativeFnCaps`) deliberately stays in `relon-eval-api`: it references
//! eval-api types and is not pure data. Only the bit/grant/requirement
//! data lives here.

/// Canonical assignment of capability bits to stable bit positions.
///
/// Each variant's discriminant is the bit index the compiled backends
/// key on: the cranelift `CapabilityVtable` slots a host fn at
/// `cap_bit`, the bytecode VM consults the same index, and the wasm
/// `__relon_check_cap` import receives it. Hosts registering a
/// `#native` function tag the registration with the matching bit.
///
/// Discriminants are stable: adding a new capability appends a new
/// variant rather than reshuffling existing values, so previously
/// emitted modules keep validating against the same bit positions.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapabilityBit {
    /// Filesystem reads. Mirrors `Capabilities::reads_fs` /
    /// `NativeFnGate::reads_fs`.
    ReadsFs = 0,
    /// Filesystem writes. Mirrors `Capabilities::writes_fs` /
    /// `NativeFnGate::writes_fs`.
    WritesFs = 1,
    /// Network access (sockets, HTTP, DNS). Mirrors
    /// `Capabilities::network` / `NativeFnGate::network`.
    Network = 2,
    /// Wall / monotonic clock reads. Mirrors
    /// `Capabilities::reads_clock` / `NativeFnGate::reads_clock`.
    ReadsClock = 3,
    /// Process environment reads. Mirrors `Capabilities::reads_env` /
    /// `NativeFnGate::reads_env`.
    ReadsEnv = 4,
    /// Random-number / non-deterministic source reads. Mirrors
    /// `Capabilities::uses_rng` / `NativeFnGate::uses_rng`.
    UsesRng = 5,
}

impl CapabilityBit {
    /// Stable bit index this capability claims. Used by the cranelift
    /// vtable, the bytecode VM consult, and the wasm `__relon_check_cap`
    /// import to key the same capability across backends.
    pub fn bit_index(self) -> u32 {
        self as u32
    }

    /// Stable, audit-visible string label for this capability.
    /// Mirrors the `NativeFnGate::missing_bits` field-name strings
    /// so historical diagnostics keep the same wording.
    pub fn as_str(self) -> &'static str {
        match self {
            CapabilityBit::ReadsFs => "reads_fs",
            CapabilityBit::WritesFs => "writes_fs",
            CapabilityBit::Network => "network",
            CapabilityBit::ReadsClock => "reads_clock",
            CapabilityBit::ReadsEnv => "reads_env",
            CapabilityBit::UsesRng => "uses_rng",
        }
    }

    /// Human-readable denial message for the `reason` field of
    /// `RuntimeError::CapabilityDenied`. The dominant (and only
    /// `Capabilities`-produced) case: the fn declared this bit but the
    /// caller never granted it.
    pub fn deny_message(self) -> String {
        format!(
            "function declared `{}` but caller did not grant it",
            self.as_str()
        )
    }
}

/// Capability requirements declared *per native function* at registration
/// time. The gate compares these against the context-wide
/// [`Capabilities`] grant when the function is invoked under sandbox.
///
/// A pure function (no host capability needed) carries
/// `NativeFnGate::default()` — every bit zero. The gate check is
/// trivially satisfied by any `Capabilities` value, including a
/// fully-sandboxed [`Capabilities::default`].
///
/// `#[non_exhaustive]`: future capability bits are added here without a
/// breaking semver bump. External callers should construct via
/// `NativeFnGate::default()` and set the bits they need rather than
/// relying on positional struct literals.
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
    /// `caps`. Iteration order is the field-declaration order; runtime
    /// uses the first entry as the failure reason, analyzer emits one
    /// diagnostic per entry. The returned strings are the canonical
    /// [`CapabilityBit::as_str`] labels (`"reads_fs"`, `"writes_fs"`,
    /// `"network"`, `"reads_clock"`, `"reads_env"`, `"uses_rng"`).
    pub fn missing_bits(&self, caps: &Capabilities) -> Vec<&'static str> {
        let mut out = Vec::with_capacity(6);
        if self.reads_fs && !caps.reads_fs {
            out.push(CapabilityBit::ReadsFs.as_str());
        }
        if self.writes_fs && !caps.writes_fs {
            out.push(CapabilityBit::WritesFs.as_str());
        }
        if self.network && !caps.network {
            out.push(CapabilityBit::Network.as_str());
        }
        if self.reads_clock && !caps.reads_clock {
            out.push(CapabilityBit::ReadsClock.as_str());
        }
        if self.reads_env && !caps.reads_env {
            out.push(CapabilityBit::ReadsEnv.as_str());
        }
        if self.uses_rng && !caps.uses_rng {
            out.push(CapabilityBit::UsesRng.as_str());
        }
        out
    }

    /// Capability bit indices this gate requires, in field-declaration
    /// order, **regardless of any grant**. The IR lowering pass emits
    /// one [`CapabilityBit`]-tagged `Op::CheckCap` per entry ahead of
    /// the guarded `Op::CallNative`, so the runtime consult fires on
    /// every required bit (the grant is checked at dispatch time, not
    /// here). Mirrors [`Self::missing_bits`]'s ordering but drops the
    /// grant filter — lowering doesn't know the host's runtime posture,
    /// only the static requirement. Indices match
    /// [`CapabilityBit::bit_index`] (ReadsFs=0 … UsesRng=5).
    pub fn required_bit_indices(&self) -> Vec<u32> {
        let mut out = Vec::with_capacity(6);
        if self.reads_fs {
            out.push(CapabilityBit::ReadsFs.bit_index());
        }
        if self.writes_fs {
            out.push(CapabilityBit::WritesFs.bit_index());
        }
        if self.network {
            out.push(CapabilityBit::Network.bit_index());
        }
        if self.reads_clock {
            out.push(CapabilityBit::ReadsClock.bit_index());
        }
        if self.reads_env {
            out.push(CapabilityBit::ReadsEnv.bit_index());
        }
        if self.uses_rng {
            out.push(CapabilityBit::UsesRng.bit_index());
        }
        out
    }
}

/// Context-wide sandbox policy the host hands the evaluator. The per-bit
/// booleans are the capabilities the host *grants*; per-function
/// *requirements* live on [`NativeFnGate`]. A call goes through iff every
/// bit declared on the fn's gate is also set here — there is no per-name
/// allowlist or global short-circuit, so a successful call proves that
/// every bit on its gate was granted.
///
/// Beyond the capability bits, this struct also carries the runtime
/// resource budgets (`max_steps`, `max_value_elements`) the evaluator
/// enforces. The analyzer's static reachability check only reads the
/// capability bits and ignores the budgets, but they live on the same
/// struct so the evaluator's `Context` keeps a single sandbox-policy
/// carrier (the budgets are `Option<_>` defaulting to "unbounded", so a
/// `Capabilities` built purely for the analyzer is unaffected).
///
/// `#[non_exhaustive]`: future capability bits are added here without a
/// breaking semver bump. External callers should prefer constructing via
/// [`Capabilities::default`] / [`Capabilities::all_granted`] and mutating
/// fields rather than relying on field-order struct literals.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct Capabilities {
    /// Filesystem reads (host fn that calls `std::fs::read*`, also the
    /// policy bit consulted by `FilesystemModuleResolver`).
    pub reads_fs: bool,
    /// Filesystem writes (host fn that calls `std::fs::write*` /
    /// `OpenOptions::write` / `create_dir*` / `remove_*`).
    pub writes_fs: bool,
    /// Network access (sockets, HTTP clients, DNS).
    pub network: bool,
    /// Wall / monotonic clock reads (`SystemTime::now`, `Instant::now`).
    pub reads_clock: bool,
    /// Process environment reads (`std::env::var`, `args`, etc.).
    pub reads_env: bool,
    /// Random number generation (any non-deterministic source).
    pub uses_rng: bool,
    /// Maximum number of AST nodes to process before aborting. `None`
    /// is unbounded. Consulted only by the evaluator; the analyzer
    /// ignores it.
    pub max_steps: Option<u64>,
    /// Maximum number of elements in a single List or Dict. `None` is
    /// unbounded. Consulted only by the evaluator; the analyzer ignores
    /// it.
    pub max_value_elements: Option<usize>,
}

impl Capabilities {
    /// Audit-visible "grant everything" preset: every capability bit
    /// flipped, no step / value-size budget. The spec forbids an
    /// implicit `Context::trusted()`-style shortcut; hosts that need
    /// full grant must call this and read the resulting `Capabilities`
    /// *as data*. See `docs/zh/guide/spec.md` §4.2.
    ///
    /// Note: opening filesystem reads also requires installing a
    /// non-rejecting `FilesystemModuleResolver`. The `reads_fs` flag is
    /// the policy bit; the resolver is the machinery that enforces it.
    pub fn all_granted() -> Self {
        Self {
            reads_fs: true,
            writes_fs: true,
            network: true,
            reads_clock: true,
            reads_env: true,
            uses_rng: true,
            max_steps: None,
            max_value_elements: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_bit_indices_are_stable() {
        assert_eq!(CapabilityBit::ReadsFs.bit_index(), 0);
        assert_eq!(CapabilityBit::WritesFs.bit_index(), 1);
        assert_eq!(CapabilityBit::Network.bit_index(), 2);
        assert_eq!(CapabilityBit::ReadsClock.bit_index(), 3);
        assert_eq!(CapabilityBit::ReadsEnv.bit_index(), 4);
        assert_eq!(CapabilityBit::UsesRng.bit_index(), 5);
    }

    #[test]
    fn missing_bits_uses_canonical_labels() {
        let gate = NativeFnGate {
            reads_fs: true,
            writes_fs: true,
            network: true,
            reads_clock: true,
            reads_env: true,
            uses_rng: true,
        };
        assert_eq!(
            gate.missing_bits(&Capabilities::default()),
            vec![
                "reads_fs",
                "writes_fs",
                "network",
                "reads_clock",
                "reads_env",
                "uses_rng",
            ]
        );
        assert!(gate.missing_bits(&Capabilities::all_granted()).is_empty());
    }
}
