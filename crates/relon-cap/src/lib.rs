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
/// `cap_bit`, the LLVM / wasm host boundaries consult the same index,
/// and the wasm `__relon_check_cap` import receives it. Hosts registering a
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
    /// vtable and LLVM / wasm host-boundary checks to key the same
    /// capability across backends.
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
    /// Declare a required capability bit. Strictly additive: it flips
    /// the matching per-bit boolean to `true` and never clears other
    /// bits. Because the struct is `#[non_exhaustive]`, external
    /// crates cannot use struct literals; this is the supported way
    /// to build a gate: start from [`NativeFnGate::default`] and
    /// `require` each bit the fn needs. Mirrors
    /// [`Capabilities::grant`] on the grant side.
    pub fn require(&mut self, bit: CapabilityBit) {
        match bit {
            CapabilityBit::ReadsFs => self.reads_fs = true,
            CapabilityBit::WritesFs => self.writes_fs = true,
            CapabilityBit::Network => self.network = true,
            CapabilityBit::ReadsClock => self.reads_clock = true,
            CapabilityBit::ReadsEnv => self.reads_env = true,
            CapabilityBit::UsesRng => self.uses_rng = true,
        }
    }

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

/// Evaluator-side resource-budget presets.
///
/// These profiles cover limits the in-process evaluator can enforce today.
/// Host/VM limits such as wall-clock time, process memory, Wasmtime fuel, and
/// final-output bytes live at their respective host boundaries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum ResourceBudgetProfile {
    /// Preserve historical behavior: no evaluator-side resource limit.
    #[default]
    Off,
    /// Developer guardrails for local runs.
    Dev,
    /// Tighter guardrails for externally supplied source. This is not a VM
    /// security boundary; use a wasm engine for hard untrusted execution.
    Untrusted,
}

/// Evaluator-side resource budget.
///
/// `ResourceBudget` is deliberately separate from [`Capabilities`]:
/// capabilities answer "may the program use this host authority?", while a
/// budget answers "how much evaluator work/value growth is this host willing
/// to pay for?". The current implementation still stores these two fields on
/// [`Capabilities`] for compatibility; call [`Self::apply_to_capabilities`] to
/// bridge the new model into the existing evaluator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ResourceBudget {
    /// Maximum evaluator steps. `None` is unbounded.
    pub max_steps: Option<u64>,
    /// Maximum number of elements in a single List/Tuple/Dict. `None` is
    /// unbounded.
    pub max_value_elements: Option<usize>,
}

impl ResourceBudget {
    pub const DEV_MAX_STEPS: u64 = 5_000_000;
    pub const DEV_MAX_VALUE_ELEMENTS: usize = 100_000;
    pub const UNTRUSTED_MAX_STEPS: u64 = 1_000_000;
    pub const UNTRUSTED_MAX_VALUE_ELEMENTS: usize = 10_000;

    /// No evaluator-side budget.
    pub fn off() -> Self {
        Self::default()
    }

    /// Local-development guardrails.
    pub fn dev() -> Self {
        Self {
            max_steps: Some(Self::DEV_MAX_STEPS),
            max_value_elements: Some(Self::DEV_MAX_VALUE_ELEMENTS),
        }
    }

    /// Tighter evaluator guardrails for externally supplied source.
    pub fn untrusted() -> Self {
        Self {
            max_steps: Some(Self::UNTRUSTED_MAX_STEPS),
            max_value_elements: Some(Self::UNTRUSTED_MAX_VALUE_ELEMENTS),
        }
    }

    pub fn from_profile(profile: ResourceBudgetProfile) -> Self {
        match profile {
            ResourceBudgetProfile::Off => Self::off(),
            ResourceBudgetProfile::Dev => Self::dev(),
            ResourceBudgetProfile::Untrusted => Self::untrusted(),
        }
    }

    pub fn has_evaluator_limits(self) -> bool {
        self.max_steps.is_some() || self.max_value_elements.is_some()
    }

    pub fn apply_to_capabilities(self, caps: &mut Capabilities) {
        if let Some(max_steps) = self.max_steps {
            caps.max_steps = Some(max_steps);
        }
        if let Some(max_value_elements) = self.max_value_elements {
            caps.max_value_elements = Some(max_value_elements);
        }
    }
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

    /// Grant a single capability bit. Strictly additive: it flips the
    /// matching per-bit boolean to `true` and never clears other bits
    /// or touches the budget fields (`max_steps` /
    /// `max_value_elements`). This is the canonical
    /// [`CapabilityBit`]-to-field mapping; keep it in sync with
    /// [`Self::is_granted`] when a new bit is appended.
    pub fn grant(&mut self, bit: CapabilityBit) {
        match bit {
            CapabilityBit::ReadsFs => self.reads_fs = true,
            CapabilityBit::WritesFs => self.writes_fs = true,
            CapabilityBit::Network => self.network = true,
            CapabilityBit::ReadsClock => self.reads_clock = true,
            CapabilityBit::ReadsEnv => self.reads_env = true,
            CapabilityBit::UsesRng => self.uses_rng = true,
        }
    }

    /// Whether the given capability bit is granted. Reads the same
    /// per-bit boolean [`Self::grant`] writes.
    pub fn is_granted(&self, bit: CapabilityBit) -> bool {
        match bit {
            CapabilityBit::ReadsFs => self.reads_fs,
            CapabilityBit::WritesFs => self.writes_fs,
            CapabilityBit::Network => self.network,
            CapabilityBit::ReadsClock => self.reads_clock,
            CapabilityBit::ReadsEnv => self.reads_env,
            CapabilityBit::UsesRng => self.uses_rng,
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

    #[test]
    fn grant_is_per_bit_and_additive() {
        const ALL_BITS: [CapabilityBit; 6] = [
            CapabilityBit::ReadsFs,
            CapabilityBit::WritesFs,
            CapabilityBit::Network,
            CapabilityBit::ReadsClock,
            CapabilityBit::ReadsEnv,
            CapabilityBit::UsesRng,
        ];
        for granted_bit in ALL_BITS {
            let mut caps = Capabilities::default();
            caps.grant(granted_bit);
            for probe in ALL_BITS {
                assert_eq!(
                    caps.is_granted(probe),
                    probe == granted_bit,
                    "grant({granted_bit:?}) must flip exactly its own bit (probe {probe:?})"
                );
            }
            assert_eq!(caps.max_steps, None);
            assert_eq!(caps.max_value_elements, None);
        }
        // Additive: granting a second bit keeps the first.
        let mut caps = Capabilities::default();
        caps.grant(CapabilityBit::ReadsClock);
        caps.grant(CapabilityBit::Network);
        assert!(caps.is_granted(CapabilityBit::ReadsClock));
        assert!(caps.is_granted(CapabilityBit::Network));
    }

    #[test]
    fn require_and_grant_round_trip_through_missing_bits() {
        let mut gate = NativeFnGate::default();
        gate.require(CapabilityBit::ReadsClock);
        assert_eq!(
            gate.missing_bits(&Capabilities::default()),
            vec!["reads_clock"]
        );
        let mut caps = Capabilities::default();
        caps.grant(CapabilityBit::ReadsClock);
        assert!(gate.missing_bits(&caps).is_empty());
    }

    #[test]
    fn resource_budget_profiles_are_stable() {
        assert_eq!(
            ResourceBudget::from_profile(ResourceBudgetProfile::Off),
            ResourceBudget::off()
        );
        assert_eq!(
            ResourceBudget::from_profile(ResourceBudgetProfile::Dev),
            ResourceBudget {
                max_steps: Some(ResourceBudget::DEV_MAX_STEPS),
                max_value_elements: Some(ResourceBudget::DEV_MAX_VALUE_ELEMENTS),
            }
        );
        assert_eq!(
            ResourceBudget::from_profile(ResourceBudgetProfile::Untrusted),
            ResourceBudget {
                max_steps: Some(ResourceBudget::UNTRUSTED_MAX_STEPS),
                max_value_elements: Some(ResourceBudget::UNTRUSTED_MAX_VALUE_ELEMENTS),
            }
        );
    }

    #[test]
    fn resource_budget_does_not_grant_capabilities() {
        let mut caps = Capabilities::default();
        ResourceBudget::untrusted().apply_to_capabilities(&mut caps);
        assert_eq!(caps.max_steps, Some(ResourceBudget::UNTRUSTED_MAX_STEPS));
        assert_eq!(
            caps.max_value_elements,
            Some(ResourceBudget::UNTRUSTED_MAX_VALUE_ELEMENTS)
        );
        assert_eq!(
            NativeFnGate {
                reads_fs: true,
                writes_fs: true,
                network: true,
                reads_clock: true,
                reads_env: true,
                uses_rng: true,
            }
            .missing_bits(&caps)
            .len(),
            6
        );
    }
}
