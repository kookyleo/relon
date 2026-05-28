//! Stack-based bytecode VM with 4-prong sandbox engagement.
//!
//! Dispatch is `match`-based — computed-goto would require nightly
//! rust + the unstable `naked_functions` feature, and the M2-A target
//! is not perf (that's M2-C). The single tick-per-op resource counter
//! lives on [`BcVmConfig::max_steps`]; bounds / trap / capability
//! prongs trip through [`BcVmError`] variants that lift cleanly into
//! `relon_eval_api::RuntimeError`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Instant;

use ordered_float::OrderedFloat;
use relon_eval_api::{
    CapabilityBit, CapabilityError, CapabilityGate, NativeArgs, NativeFnCaps, RelonFunction,
    RuntimeError, Value,
};
use relon_ir::IrType;
use relon_parser::TokenRange;
use thiserror::Error;

use crate::arena::{ArenaError, ClosureSlot, VmMemory};
use crate::hot_counter::{HotCounterResult, HotTraceTriggerHandle};
use crate::op::{BcFunction, BcOp, BcStdlibKind, BcTrapKind, ExternalPc};
use crate::trace_dispatch::InstalledTraceLookupHandle;

/// Raw VM-value slot. The bytecode VM is homogeneous on `u64` so
/// the dispatch arms don't switch on a tagged enum on every op.
/// Comparison / arithmetic ops decode the slot per IR type — `i64`
/// uses the slot directly; `f64` interprets via `f64::from_bits`;
/// `Bool` / `Null` take the low 32 bits.
pub type VmValue = u64;

/// VM configuration. Mirrors the sandbox knobs the tree-walker /
/// cranelift backends respect, on a per-call basis.
#[derive(Debug, Clone)]
pub struct BcVmConfig {
    /// Hard cap on dispatched bytecode ops. `None` disables resource
    /// accounting (used by perf benchmarks). Tree-walker parity:
    /// matches `Capabilities::max_steps`.
    pub max_steps: Option<u64>,
    /// Optional wall-clock deadline for the call. Mirrors the
    /// cranelift `SandboxState::deadline_ns` knob. v6-δ M2-A reads
    /// this on every tick when set; M2-C will sample less frequently
    /// to claw back overhead.
    pub deadline: Option<Instant>,
    /// Capability vtable indexed by `cap_bit`. Empty slots cause a
    /// guarded call to trip [`BcVmError::CapabilityDenied`]; non-empty
    /// slots are reserved for M2-B when host-fn dispatch lands.
    pub cap_vtable: CapabilityVtable,
    /// M2-B phase 4c: optional trace-JIT recording hook. When set, the
    /// VM bumps a [`crate::HotCounter`] slot once per `invoke` and,
    /// when the slot crosses the configured threshold, calls
    /// `trigger.on_hot(fn_id, args)` so the host can drive a trace
    /// recording (cranelift backend: indirects through
    /// `__relon_jump_to_recorder`).
    ///
    /// `None` leaves the prologue inert — that's the wasm32 / unit-test
    /// default. The hook lives behind a trait object so the bytecode
    /// crate stays cranelift-free; the native adapter lives in
    /// `relon_codegen_cranelift::bytecode_bridge` (added alongside the
    /// recording-registry wire-up).
    pub hot_trigger: Option<HotTraceTriggerHandle>,
    /// M2-B phase 4c: per-`invoke` hot-counter threshold. Defaults to
    /// [`crate::DEFAULT_HOT_THRESHOLD`] (1000). Hosts that want eager
    /// recording (smoke tests / corpus harnesses that need the trigger
    /// to fire after a single iteration) lower this to `1`. Threshold
    /// 0 is rejected at [`HotCounter::with_threshold`] time.
    pub hot_threshold: u32,
    /// M2-B phase 4c-cont: optional installed-trace lookup. When set,
    /// the evaluator consults this hook at the top of every
    /// `run_main` invocation; a hit bypasses the bytecode dispatch
    /// loop entirely and routes through the JIT'd trace (mirror of
    /// the cranelift backend's entry-fn prologue + inline cache).
    ///
    /// `None` leaves the path inert — the VM behaves as before phase
    /// 4c-cont. The native adapter
    /// (`relon_codegen_cranelift::bytecode_bridge::CraneliftTraceLookup`)
    /// wraps `TraceJitState::invoke_with_resume` so a successful
    /// trace returns its `result_slot` value and a guard-failed trace
    /// surfaces the [`relon_trace_abi::DeoptStateSnapshot`] for the
    /// evaluator's `resume_from_snapshot` path to absorb.
    ///
    /// Stored on the config (not directly on the VM) so each
    /// `run_main` invocation clones the `Arc` once at entry — the VM
    /// itself stays cranelift-free.
    pub trace_lookup: Option<InstalledTraceLookupHandle>,
}

impl Default for BcVmConfig {
    fn default() -> Self {
        // 1M instruction cap by default — generous enough that the
        // legacy-i64 corpus is uncapped in practice but tight enough
        // that an accidental infinite loop in a unit test surfaces
        // before the process hangs.
        Self {
            max_steps: Some(1_000_000),
            deadline: None,
            cap_vtable: CapabilityVtable::default(),
            hot_trigger: None,
            hot_threshold: crate::hot_counter::DEFAULT_HOT_THRESHOLD,
            trace_lookup: None,
        }
    }
}

/// P2-7: pick the dispatch-loop sampling mask for the deadline /
/// `max_steps` resource gates. The mask is `0` (check every op) when
/// `max_steps` is tight enough that a 64-op slip could let the program
/// return before the gate sees the over-budget tick; otherwise it's
/// `63` (sample once per 64 ops). The threshold doubles the sample
/// period to leave headroom — at `max_steps == 64` the first sample
/// after step 1 lands at step 65, which already exceeds the budget by
/// one op, so the per-op fall-back kicks in for any limit below 128.
#[inline]
fn step_sample_mask(config: &BcVmConfig) -> u64 {
    match config.max_steps {
        Some(limit) if limit < 128 => 0,
        _ => 63,
    }
}

/// Per-call capability table. The slot at index `cap_bit` carries
/// `Some(_)` when the host has granted access; the actual host-fn
/// pointer payload is M2-B work. Today the table tracks **grants
/// only** so the capability prong can fire in the M2-A sandbox tests.
///
/// ## M2-B phase 1 — `CapabilityGate` hook
///
/// The optional `gate` field carries the unified
/// [`CapabilityGate`] policy (`relon_eval_api::capability`). When set,
/// it becomes the canonical authority for "is this capability bit
/// granted?" questions a future `BcOp::CallNative` / `BcOp::CheckCap`
/// op asks at dispatch time. The grant-bool vector stays as the
/// fallback for code paths that haven't been migrated yet — phase 2
/// will land the dispatch-side consult and trim the fallback's reach.
///
/// ## M2-B phase 4a — host-fn registry
///
/// The `host_fns` map keys host-supplied [`RelonFunction`] entries by
/// `import_idx` (the same slot `BcOp::CallNative` carries). Phase 4a
/// scope is **scalar in / scalar out** — args travel as `Value::Int` /
/// `Value::Bool` / `Value::Float` / `Value::Null` (decoded from the
/// VM's `u64` slots through the lane convention shared with the
/// cranelift backend); return values follow `BcOp::CallNative::ret_ty`
/// back into a `u64`. List / dict / string return shapes need the
/// buffer-protocol memory model and ship in phase 4b.
#[derive(Clone, Default)]
pub struct CapabilityVtable {
    grants: Vec<bool>,
    /// M2-B phase 1: optional shared gate consulted on every guarded
    /// op (phase 2 wires the actual `BcOp::CheckCap` consult). `None`
    /// preserves the M2-A grant-table-only behaviour so existing
    /// callers don't observe a change.
    gate: Option<Arc<dyn CapabilityGate>>,
    /// M2-B phase 4a: host-fn registry keyed by the `import_idx`
    /// carried on `BcOp::CallNative`. An entry here unfreezes the
    /// phase-3 `NativeNotImplemented` trap — the dispatcher pops
    /// `arg_count` slots, decodes them into a positional `Vec<Value>`,
    /// invokes the host fn, and re-encodes the returned [`Value`] back
    /// into the operand stack per `ret_ty`. An absent slot keeps the
    /// historical `NativeNotImplemented` envelope so the differential
    /// harness still has a stable bounce shape for un-registered
    /// imports.
    host_fns: HashMap<u32, Arc<dyn RelonFunction>>,
}

impl fmt::Debug for CapabilityVtable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // `dyn CapabilityGate` / `dyn RelonFunction` don't carry
        // `Debug`; print presence + cardinality only so the surrounding
        // `BcVmConfig` debug stays cheap and doesn't force the trait
        // surfaces wider than necessary.
        f.debug_struct("CapabilityVtable")
            .field("grants", &self.grants)
            .field("has_gate", &self.gate.is_some())
            .field("host_fn_count", &self.host_fns.len())
            .finish()
    }
}

impl CapabilityVtable {
    /// Grant capability `cap_bit`. Subsequent guarded calls against
    /// the slot see `Some` and proceed.
    pub fn grant(&mut self, cap_bit: u32) {
        let idx = cap_bit as usize;
        if idx >= self.grants.len() {
            self.grants.resize(idx + 1, false);
        }
        self.grants[idx] = true;
    }

    /// Inspect whether `cap_bit` is currently granted.
    pub fn is_granted(&self, cap_bit: u32) -> bool {
        self.grants.get(cap_bit as usize).copied().unwrap_or(false)
    }

    /// M2-B phase 1: install the unified [`CapabilityGate`] policy.
    /// Phase 2 wires this into the dispatch path; phase 1 only parks
    /// the slot so callers can opt in ahead of time.
    pub fn set_gate(&mut self, gate: Arc<dyn CapabilityGate>) {
        self.gate = Some(gate);
    }

    /// M2-B phase 1: read the installed gate, if any. Returns `None`
    /// when the caller hasn't opted into the unified policy boundary —
    /// the phase-2 dispatch consult treats this as "fall back to the
    /// grant-bool table".
    pub fn gate(&self) -> Option<&Arc<dyn CapabilityGate>> {
        self.gate.as_ref()
    }

    /// M2-B phase 2: consult the installed [`CapabilityGate`] for a
    /// single `cap_bit`. Returns `Ok(())` when:
    ///
    /// - no gate is installed (legacy fallback — grant table remains
    ///   the only check), OR
    /// - the gate explicitly grants the bit.
    ///
    /// Returns `Err(CapabilityError)` when the gate denies the bit.
    /// `cap_bit` outside the [`CapabilityBit`] enum's lower range is
    /// also treated as "no gate authority" — the legacy host-supplied
    /// `BcOp::Trap(CapabilityDenied)` carries `u32::MAX` which falls
    /// into this bucket and surfaces as the historical unbit-tagged
    /// denial.
    ///
    /// This helper is the dispatch-side single consult entry point
    /// future `BcOp::CallNative` / `BcOp::CheckCap` ops will use; it
    /// also drives the pre-dispatch `consult_all_granted_bits` sweep
    /// run from [`BytecodeVm::invoke_from_with_stack`].
    pub fn consult_gate(&self, cap_bit: u32) -> Result<(), CapabilityError> {
        let Some(gate) = self.gate.as_ref() else {
            return Ok(());
        };
        let Some(bit) = decode_cap_bit(cap_bit) else {
            return Ok(());
        };
        gate.check(bit)
    }

    /// M2-B phase 4a: register a host fn under `import_idx`. Overwrites
    /// any existing slot so a host can rebind between `run_main` calls
    /// without rebuilding the vtable. The `import_idx` is the same slot
    /// the [`crate::op::BcOp::CallNative`] op carries — typically the
    /// position of the corresponding `relon_ir::NativeImport` entry in
    /// the module's imports table.
    pub fn register_host_fn(&mut self, import_idx: u32, func: Arc<dyn RelonFunction>) {
        self.host_fns.insert(import_idx, func);
    }

    /// M2-B phase 4a: resolve a host fn by `import_idx`. Returns `None`
    /// when no host has registered an entry for the slot — the
    /// dispatcher then keeps the legacy `NativeNotImplemented` trap so
    /// the differential harness's bounce shape stays stable.
    pub fn resolve_host_fn(&self, import_idx: u32) -> Option<&Arc<dyn RelonFunction>> {
        self.host_fns.get(&import_idx)
    }

    /// M2-B phase 4a: total number of registered host-fn slots. Used by
    /// the dispatch-side instrumentation tests + the four-way harness's
    /// readiness check — `0` means "no `CallNative` will succeed; route
    /// through cranelift / tree-walker". The number is **not** part of
    /// the public ABI; hosts should not condition behaviour on it.
    pub fn host_fn_count(&self) -> usize {
        self.host_fns.len()
    }

    /// M2-B phase 2: sweep every grant-table bit through the installed
    /// gate. Used as a dispatch-time pre-check so a gate that denies a
    /// previously-granted bit (e.g. trust-level downgrade between
    /// `grant` and `invoke`) trips the capability prong before any
    /// guarded op runs. No-op when no gate is installed; no-op when no
    /// bits are granted (the scaffold's default-empty-vtable case).
    pub fn consult_all_granted_bits(&self) -> Result<(), CapabilityError> {
        if self.gate.is_none() {
            return Ok(());
        }
        for (idx, granted) in self.grants.iter().enumerate() {
            if *granted {
                self.consult_gate(idx as u32)?;
            }
        }
        Ok(())
    }

    /// #166 M2-B from_source full cap-gate activation: sweep every
    /// declared [`CapabilityBit`] through the installed gate.
    ///
    /// Unlike [`Self::consult_all_granted_bits`] (which only asks about
    /// bits the host explicitly granted on the grant-bool vector), this
    /// sweep asks about every bit the API knows about — `ReadsFs`,
    /// `WritesFs`, `Network`, `ReadsClock`, `ReadsEnv`, `UsesRng`. The
    /// first denial short-circuits with the corresponding
    /// [`CapabilityError`].
    ///
    /// Used as the entry-time consult for functions whose op stream
    /// contains a capability-sensitive op (`BcOp::CallNative` /
    /// `BcOp::CheckCap`). A deny-everything gate paired with such a
    /// function trips the prong before any op observes state — the
    /// per-op consult emitted by those ops still fires independently,
    /// so the entry sweep is defense in depth that catches the case
    /// where the host installed the gate but the source happens to use
    /// an op the per-op consult was already going to allow (today's
    /// scalar `from_source` envelope has no sensitive ops yet; this
    /// sweep is the activation point for the future widening).
    ///
    /// No-op when no gate is installed.
    pub fn consult_all_declared_bits(&self) -> Result<(), CapabilityError> {
        let Some(gate) = self.gate.as_ref() else {
            return Ok(());
        };
        for bit in [
            CapabilityBit::ReadsFs,
            CapabilityBit::WritesFs,
            CapabilityBit::Network,
            CapabilityBit::ReadsClock,
            CapabilityBit::ReadsEnv,
            CapabilityBit::UsesRng,
        ] {
            gate.check(bit)?;
        }
        Ok(())
    }
}

/// M2-B phase 2: walk every declared [`CapabilityBit`] and return the
/// first one the supplied gate denies. Used to give the static
/// `BcOp::Trap(CapabilityDenied)` site a meaningful `cap_bit` to
/// report when a gate is installed but the trap fires from a
/// hand-built BcFunction that didn't carry the bit through the IR.
fn first_denied_bit(gate: &Arc<dyn CapabilityGate>) -> Option<u32> {
    for bit in [
        CapabilityBit::ReadsFs,
        CapabilityBit::WritesFs,
        CapabilityBit::Network,
        CapabilityBit::ReadsClock,
        CapabilityBit::ReadsEnv,
        CapabilityBit::UsesRng,
    ] {
        if gate.check(bit).is_err() {
            return Some(bit.bit_index());
        }
    }
    None
}

/// M2-B phase 4a: encode a returned [`Value`] into the VM's `u64`
/// slot according to the call site's declared `ret_ty`. The phase-4a
/// envelope was scalar only; phase 4b-continuation widens it to the
/// `String` / `ListInt` lanes by materialising the host-side `Value`
/// into the VM's per-call arenas and returning the freshly minted
/// handle. Anything outside that envelope (`ListFloat` / `ListBool` /
/// `ListString` / `Closure`) still surfaces as
/// [`BcVmError::HostFnReturnTypeMismatch`].
fn encode_value_for_ret(
    value: &Value,
    ret_ty: IrType,
    import_idx: u32,
    memory: &mut VmMemory,
) -> Result<VmValue, BcVmError> {
    match (value, ret_ty) {
        (Value::Int(v), IrType::I64) | (Value::Int(v), IrType::I32) => Ok(*v as u64),
        (Value::Bool(b), IrType::Bool) => Ok(if *b { 1 } else { 0 }),
        (Value::Bool(b), IrType::I64) | (Value::Bool(b), IrType::I32) => {
            // Host fns sometimes upcast a bool return through the i64
            // lane (matches the cranelift backend's bool-as-i32-as-i64
            // convention). Mirror that so the harness round-trip works
            // even when the IR-level decl narrows to Bool.
            Ok(if *b { 1 } else { 0 })
        }
        (Value::Null, IrType::Null) | (Value::Null, IrType::I64) | (Value::Null, IrType::I32) => {
            Ok(0)
        }
        (Value::Float(OrderedFloat(f)), IrType::F64) => Ok(f.to_bits()),
        // M2-B phase 4b-continuation: lift a host-returned `Value::String`
        // into the bytecode VM's string arena. The fresh handle lives
        // for the rest of the `invoke` call; downstream ops that
        // consume the slot (StrLen / StrConcat / StrEq / DictLookupStr)
        // resolve it through the same arena.
        (Value::String(s), IrType::String) => {
            let handle = memory.strings.alloc(s.as_str());
            Ok(handle as u64)
        }
        // M2-B phase 4b-continuation: lift a host-returned `Value::List`
        // of integers into the bytecode VM's list arena. Phase 4b only
        // models type-uniform i64 lists; a heterogeneous list (or a
        // non-int list) surfaces as `HostFnReturnTypeMismatch` so the
        // host gets a clear "route through tree-walker" envelope.
        (Value::List(items), IrType::ListInt) => {
            let mut packed: Vec<u64> = Vec::with_capacity(items.len());
            for elem in items.iter() {
                match elem {
                    Value::Int(v) => packed.push(*v as u64),
                    other => {
                        return Err(BcVmError::HostFnReturnTypeMismatch {
                            import_idx,
                            expected: ret_ty,
                            found: format!("List<{}>", other.type_name()),
                        });
                    }
                }
            }
            let handle = memory.lists.alloc(packed);
            Ok(handle as u64)
        }
        (other, _) => Err(BcVmError::HostFnReturnTypeMismatch {
            import_idx,
            expected: ret_ty,
            found: other.type_name().to_string(),
        }),
    }
}

/// M2-B phase 4a stub [`NativeFnCaps`] handed to host fns the bytecode
/// VM dispatches.
///
/// The scaffold envelope does not support Relon-level callbacks
/// (`call_relon`) or `Iter`-cursor tracking, so the impl leans on the
/// trait's default implementations for everything except presence —
/// host fns that rely on those callbacks must keep routing through the
/// tree-walker / cranelift backends until the bytecode VM grows
/// frame-stack + cursor surface (M3 or later).
///
/// `Send + Sync` because the underlying caps handle ends up shared
/// across the host fn's execution; the empty stub is trivially safe.
struct BytecodeNativeFnCaps;

/// Cached single-instance `Arc<dyn NativeFnCaps>` for the bytecode VM.
/// The struct is a zero-sized unit type with no per-call state, so
/// every dispatch can clone the same `Arc` (refcount bump) instead of
/// allocating a fresh `Arc::new(BytecodeNativeFnCaps)` per `CallNative`.
fn bytecode_native_caps() -> Arc<dyn NativeFnCaps> {
    static CAPS: std::sync::OnceLock<Arc<dyn NativeFnCaps>> = std::sync::OnceLock::new();
    Arc::clone(CAPS.get_or_init(|| Arc::new(BytecodeNativeFnCaps) as Arc<dyn NativeFnCaps>))
}

impl NativeFnCaps for BytecodeNativeFnCaps {
    fn call_relon(
        &self,
        _func: &Value,
        _args: Vec<Value>,
        _range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        // Closure callbacks land alongside the bytecode VM's frame
        // stack — until then a host fn that tries to call back into
        // Relon logic surfaces this `Unsupported` envelope so the host
        // can route the call through the tree-walker instead. Phase 4a
        // scope is scalar-only, so the standard scalar intrinsics never
        // hit this path.
        Err(RuntimeError::Unsupported {
            reason: "bytecode VM: native fn callbacks into Relon closures land in M3".into(),
        })
    }
}

/// Map a numeric `cap_bit` to a [`CapabilityBit`] variant. Returns
/// `None` for bits outside the declared range — callers treat that as
/// "the bit predates the gate API and falls through to the legacy
/// grant-table path".
fn decode_cap_bit(cap_bit: u32) -> Option<CapabilityBit> {
    match cap_bit {
        0 => Some(CapabilityBit::ReadsFs),
        1 => Some(CapabilityBit::WritesFs),
        2 => Some(CapabilityBit::Network),
        3 => Some(CapabilityBit::ReadsClock),
        4 => Some(CapabilityBit::ReadsEnv),
        5 => Some(CapabilityBit::UsesRng),
        _ => None,
    }
}

/// VM-side error. Lifted to [`RuntimeError`] via
/// [`BcVmError::into_runtime_error`] so the surrounding evaluator
/// matches the tree-walker / cranelift `Evaluator::run_main` shape.
#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum BcVmError {
    /// Resource-prong trip: instruction count exceeded
    /// `BcVmConfig::max_steps`. Mirrors
    /// `RuntimeError::WasmStepLimitExceeded`.
    #[error("bytecode VM step limit exceeded after {steps} ops")]
    StepLimitExceeded {
        /// Total ops dispatched before the trip.
        steps: u64,
    },
    /// Resource-prong trip: wall-clock deadline elapsed.
    #[error("bytecode VM deadline exceeded")]
    DeadlineExceeded,
    /// Trap-prong trip: integer divide-by-zero / mod-by-zero.
    #[error("integer division by zero")]
    DivisionByZero,
    /// Trap-prong trip: signed integer overflow on Add / Sub / Mul.
    /// Tree-walker parity: matches `RuntimeError::NumericOverflow`.
    #[error("signed integer overflow")]
    NumericOverflow,
    /// Bounds-prong trip: a bytecode jump landed past the op stream
    /// (compiler bug / malformed bytecode).
    #[error("bytecode jump target {target} out of range (op count {ops})")]
    JumpOutOfRange {
        /// Requested target index.
        target: usize,
        /// Total op count.
        ops: usize,
    },
    /// Bounds-prong trip: an explicit `Op::Trap(IndexOutOfBounds)`
    /// fired (typically from stdlib substring / list index).
    #[error("index out of bounds")]
    IndexOutOfBounds,
    /// Trap-prong trip: empty-list reducer trap.
    #[error("operation on empty list")]
    EmptyList,
    /// Trap-prong trip: malformed UTF-8 trap.
    #[error("invalid utf-8")]
    InvalidUtf8,
    /// Capability-prong trip: a guarded call against an empty
    /// vtable slot.
    #[error("capability denied: cap_bit {cap_bit}")]
    CapabilityDenied {
        /// The denied capability bit.
        cap_bit: u32,
    },
    /// Defensive: the operand stack underflowed. Symptom of a
    /// compiler bug (the IR-level vstack validator should catch
    /// this); flagged loudly rather than silently propagating zero.
    #[error("operand stack underflow at bc_idx {bc_idx}")]
    StackUnderflow {
        /// Bytecode index of the offending op.
        bc_idx: usize,
    },
    /// M2-B phase 3: `BcOp::CallNative` passed the capability prong
    /// but the bytecode VM has no host-fn registry to invoke. The
    /// phase-3 dispatch shape lands the consult sites; phase 4a wires
    /// the actual registry so this envelope now fires only for
    /// `import_idx` values the host has not registered. Callers that
    /// observe this envelope today should either register a host fn
    /// for the slot or route the source through cranelift /
    /// tree-walker.
    #[error("bytecode VM has no host-fn registry for import_idx {import_idx}")]
    NativeNotImplemented {
        /// The `NativeImport` slot the call targeted.
        import_idx: u32,
    },
    /// M2-B phase 4a: the host-fn registry entry for `import_idx`
    /// resolved, the gate consult passed, but the host fn itself
    /// returned an error. The reason string carries the underlying
    /// `RuntimeError`'s display form for diagnostics; the surrounding
    /// envelope lifts to `RuntimeError::Unsupported` today so the
    /// four-way differential harness keeps a stable shape. Phase 4b
    /// can widen this to a richer carrier when typed-arg lanes land.
    #[error("bytecode VM host-fn (import_idx {import_idx}) failed: {reason}")]
    HostFnError {
        /// The `NativeImport` slot the failing call targeted.
        import_idx: u32,
        /// Display form of the underlying `RuntimeError`.
        reason: String,
    },
    /// M2-B phase 4a: a registered host fn returned a [`Value`] whose
    /// shape doesn't match the [`BcOp::CallNative::ret_ty`] declared
    /// at the call site. Surfaces as `Unsupported` after the lift —
    /// the scaffold envelope is scalar-only so this typically means
    /// the host fn would need the phase-4b list / dict / string
    /// memory model.
    #[error("bytecode VM host-fn (import_idx {import_idx}) returned unsupported {found} for {expected:?}")]
    HostFnReturnTypeMismatch {
        /// The `NativeImport` slot the call targeted.
        import_idx: u32,
        /// Declared IR return type from the call site.
        expected: IrType,
        /// Display-friendly form of the returned `Value` type.
        found: String,
    },
}

impl BcVmError {
    /// Lift a VM error into the public [`RuntimeError`] surface. The
    /// caller's `entry_range` is the `#main` declaration range, used
    /// when the trap envelope has no narrower source location to
    /// attach.
    pub fn into_runtime_error(self, entry_range: TokenRange) -> RuntimeError {
        match self {
            BcVmError::StepLimitExceeded { .. } => RuntimeError::WasmStepLimitExceeded {
                range: Some(entry_range),
            },
            BcVmError::DeadlineExceeded => RuntimeError::WasmStepLimitExceeded {
                range: Some(entry_range),
            },
            BcVmError::DivisionByZero => RuntimeError::DivisionByZero(entry_range),
            BcVmError::NumericOverflow => RuntimeError::NumericOverflow(entry_range),
            BcVmError::JumpOutOfRange { .. }
            | BcVmError::IndexOutOfBounds
            | BcVmError::EmptyList
            | BcVmError::InvalidUtf8 => RuntimeError::WasmIndexOutOfBounds { range: entry_range },
            BcVmError::CapabilityDenied { cap_bit } => RuntimeError::WasmCapabilityDenied {
                cap_bit,
                range: entry_range,
            },
            BcVmError::StackUnderflow { bc_idx } => RuntimeError::Unsupported {
                reason: format!("bytecode VM stack underflow at bc_idx {bc_idx}"),
            },
            BcVmError::NativeNotImplemented { import_idx } => RuntimeError::Unsupported {
                reason: format!(
                    "bytecode VM has no host-fn registry for native import_idx {import_idx}; \
                     route through cranelift/tree-walker until phase 4 wiring lands"
                ),
            },
            BcVmError::HostFnError { import_idx, reason } => RuntimeError::Unsupported {
                reason: format!("bytecode VM host-fn (import_idx {import_idx}) failed: {reason}"),
            },
            BcVmError::HostFnReturnTypeMismatch {
                import_idx,
                expected,
                found,
            } => RuntimeError::Unsupported {
                reason: format!(
                    "bytecode VM host-fn (import_idx {import_idx}) returned {found} \
                     for declared ret_ty {expected:?} — phase 4a scope is scalar-only"
                ),
            },
        }
    }
}

/// Outcome of a single bytecode run: either a return value or a
/// trap. The `last_bc_idx` field tells the partial-resume path which
/// bytecode op tripped the trap so it can re-derive the matching IR
/// PC for diagnostics.
#[derive(Debug, Default)]
pub struct BcRunOutcome {
    /// Successful return value, when the run completed via `Return`.
    /// For buffer-protocol entries this is the IR-level `bytes_written`
    /// stand-in; the actual return data lives in `final_locals` at the
    /// schema-defined return-field slot indices.
    pub value: Option<VmValue>,
    /// Trap, when any sandbox prong fired.
    pub error: Option<BcVmError>,
    /// Bytecode index of the last op the VM saw before exit. Maps
    /// back to IR PC via `BcFunction::ir_pc_map[last_bc_idx]`.
    pub last_bc_idx: usize,
    /// Total ops dispatched.
    pub steps: u64,
    /// Local-slot snapshot at exit. The buffer-protocol return
    /// unpacks return-field values from this; the legacy-i64 path
    /// ignores it.
    pub final_locals: Vec<VmValue>,
    /// Bytecode-coverage-expansion B-2: copy-out of string-arena
    /// payloads for slots the caller wants to recover after the per-
    /// invoke `StringArena` drops. Indexed `local_slot_idx ->
    /// String_payload`. Empty when the caller didn't request any
    /// string slot lift-outs — preserves the previous outcome
    /// posture for the scalar-only paths.
    ///
    /// Populated by [`BytecodeVm::invoke_from_with_string_args`] when
    /// `string_return_slots` is non-empty: each `local_slot_idx` is
    /// looked up in the just-exited arena and the payload is cloned
    /// into a fresh `String` that survives `VmMemory` drop.
    pub final_strings: std::collections::HashMap<usize, String>,
}

/// Stack-based VM. Stateful across calls (counters reset per
/// [`BytecodeVm::invoke`]).
pub struct BytecodeVm {
    config: Arc<BcVmConfig>,
    /// M2-C lever 2: per-`BytecodeVm` inline cache for `BcOp::CallNative`
    /// host-fn resolution.
    ///
    /// `BcOp::CallNative { import_idx, .. }` previously walked the
    /// per-call `CapabilityVtable::host_fns: HashMap<u32, Arc<dyn>>`
    /// on every dispatch. Loops that hit the same host fn back-to-back
    /// now consult this single-slot cache first — a hit skips the
    /// HashMap probe and re-uses the already-resolved `Arc` without a
    /// fresh refcount bump (the `Arc::clone` still happens on consume,
    /// but the cache absorbs the lookup).
    ///
    /// `RefCell` so `dispatch_one(&self, ...)` keeps its existing
    /// signature; the cache reads + writes are single-threaded per
    /// `invoke_*` (`BytecodeVm` is not `Sync` in practice). Cleared
    /// on every `invoke` entry via [`Self::reset_call_cache`] so a
    /// stale entry can't leak across VM reuses.
    ///
    /// W12 doesn't exercise `CallNative` so this cache doesn't shift
    /// the cmp_lua W12 row's number; the benefit shows up on workloads
    /// with hot `CallNative` sites (today: phase-4a stdlib-driven
    /// fixtures inside the bytecode envelope). Future fixtures that
    /// dispatch the same `import_idx` from a hot loop are the primary
    /// motivation for keeping the cache wired even though the bench
    /// dashboard's W12 row reads "no delta".
    call_native_cache: std::cell::RefCell<CallNativeCache>,
}

/// M2-C lever 2 cache slot. Single-entry "last resolved" cache —
/// matches the LuaJIT / V8 monomorphic IC shape: the common case
/// (loop bodies dispatching one stdlib slot repeatedly) hits, the
/// polymorphic / megamorphic case still goes through the HashMap
/// but doesn't degrade beyond the pre-cache baseline.
#[derive(Default)]
struct CallNativeCache {
    last: Option<(u32, Arc<dyn RelonFunction>)>,
}

// M2-C lever 7: per-thread scratch buffers reused across the typed-i64
// fast path. Each `invoke_pooled_typed_i64` call borrows + clears the
// `locals` / `stack` buffers, then resizes them in place — no per-call
// heap allocation when the buffer is already large enough (the common
// case after the first warm-up invoke).
//
// The buffers are thread-local because the bytecode VM is not `Sync`
// in practice — `BytecodeVm` carries a `RefCell<CallNativeCache>`
// which forces single-threaded access regardless. The pooled buffers
// stay coherent with that invariant: each thread that drives the
// typed-i64 fast path owns its own pair.
thread_local! {
    static POOLED_LOCALS: RefCell<Vec<VmValue>> = const { RefCell::new(Vec::new()) };
    static POOLED_STACK: RefCell<Vec<VmValue>> = const { RefCell::new(Vec::new()) };
}

impl BytecodeVm {
    /// Build a new VM with the supplied config. Accepts either an
    /// owned `BcVmConfig` (auto-wrapped in `Arc`) or a pre-shared
    /// `Arc<BcVmConfig>` (refcount bump on the caller side, deep
    /// clone avoided).
    pub fn new(config: impl Into<Arc<BcVmConfig>>) -> Self {
        Self {
            config: config.into(),
            call_native_cache: std::cell::RefCell::new(CallNativeCache::default()),
        }
    }

    /// M2-C lever 2: reset the per-VM inline cache. Called at the top
    /// of every `invoke_*` so a previously-resolved host fn from a
    /// prior invocation can't shadow a config swap between calls.
    /// The cache is intentionally `Option<...>` rather than `Vec<...>`
    /// because the common dispatch loop hits the same slot on every
    /// iteration — a single-entry cache is enough for the monomorphic
    /// case, and the slow path still consults the HashMap.
    fn reset_call_cache(&self) {
        self.call_native_cache.borrow_mut().last = None;
    }

    /// M2-C lever 2: resolve a host fn via the inline cache. Hit
    /// re-uses the cached `Arc` (one refcount bump on the consumer
    /// side); miss falls through to the HashMap probe and primes the
    /// cache for the next iteration.
    fn resolve_host_fn_cached(&self, import_idx: u32) -> Option<Arc<dyn RelonFunction>> {
        if let Some((cached_idx, ref f)) = self.call_native_cache.borrow().last {
            if cached_idx == import_idx {
                return Some(Arc::clone(f));
            }
        }
        // Miss — go to the HashMap, then prime the cache. A `None`
        // resolve (un-registered slot) does NOT update the cache; the
        // next dispatch hits the same slow path so registration races
        // surface immediately instead of being hidden by a stale
        // `None`.
        let resolved = self.config.cap_vtable.resolve_host_fn(import_idx).cloned();
        if let Some(ref f) = resolved {
            self.call_native_cache.borrow_mut().last = Some((import_idx, Arc::clone(f)));
        }
        resolved
    }

    /// Mutable accessor on the active config — used by tests that
    /// flip a single knob (cap grant, max_steps) between invocations.
    /// Routes through `Arc::make_mut`: if this VM has the only handle
    /// the mutation is in-place; if the config is still shared (e.g.
    /// the evaluator that built the VM still holds an `Arc`) it
    /// clones once and detaches.
    pub fn config_mut(&mut self) -> &mut BcVmConfig {
        Arc::make_mut(&mut self.config)
    }

    /// Read-only accessor on the active config.
    pub fn config(&self) -> &BcVmConfig {
        &self.config
    }

    /// M2-B phase 2: pre-dispatch consult of the installed
    /// [`CapabilityGate`]. Convenience pass-through to
    /// [`CapabilityVtable::consult_all_granted_bits`] — exposed on the
    /// VM so callers that hold a [`BytecodeVm`] can opt into the
    /// pre-check without reaching into the config. Returns `Ok(())`
    /// when no gate is installed or every granted bit passes.
    pub fn consult_capability_gate(&self) -> Result<(), CapabilityError> {
        self.config.cap_vtable.consult_all_granted_bits()
    }

    /// P1-19: shared dispatch-entry capability prologue. Both
    /// [`Self::invoke_from_with_stack`] and
    /// [`Self::invoke_pooled_typed_i64`] ran the same two-stage
    /// consult sequence (`consult_all_granted_bits` then, when the
    /// function carries cap-sensitive ops, `consult_all_declared_bits`)
    /// inline; the helper unifies the sequencing so future cap-gate
    /// changes can't drift between the two entry points. Marked
    /// `#[inline]` so the call sites stay branch-equivalent to the
    /// hand-inlined original — the dispatch loop body itself
    /// (`feedback_bench_methodology_first`) remains untouched.
    #[inline]
    fn precheck_capabilities(&self, func: &BcFunction) -> Result<(), BcVmError> {
        if let Err(err) = self.config.cap_vtable.consult_all_granted_bits() {
            return Err(BcVmError::CapabilityDenied {
                cap_bit: err.cap.bit_index(),
            });
        }
        if func.requires_cap_consult {
            if let Err(err) = self.config.cap_vtable.consult_all_declared_bits() {
                return Err(BcVmError::CapabilityDenied {
                    cap_bit: err.cap.bit_index(),
                });
            }
        }
        Ok(())
    }

    /// P1-19: shared hot-counter trigger prologue. The trigger
    /// records one tick against the hot counter and, when the
    /// configured threshold is crossed for the first time, hands the
    /// args slice to the installed `HotTrigger` so the trace recorder
    /// can prime against the same view the VM is about to execute.
    /// Saturates after `HotTrigger` so subsequent invocations pay
    /// only the Option probe. Inlined so neither call site grows a
    /// new fn boundary inside the per-invoke prologue.
    #[inline]
    fn maybe_trigger_hot(&self, func: &BcFunction, args: &[VmValue]) {
        if let (Some(fn_id), Some(trigger)) = (func.fn_id, self.config.hot_trigger.as_ref()) {
            let outcome = crate::hot_counter::record_hot(fn_id, self.config.hot_threshold);
            if outcome == HotCounterResult::HotTrigger {
                trigger.on_hot(fn_id, args);
            }
        }
    }

    /// Invoke `func` with the supplied `args` filling locals
    /// `0..args.len()`. The dispatch loop ticks against
    /// `max_steps`, watches `deadline`, and surfaces trap-prong
    /// failures via [`BcRunOutcome::error`].
    pub fn invoke(&self, func: &BcFunction, args: &[VmValue]) -> BcRunOutcome {
        self.invoke_from(
            func,
            args,
            /*start_bc_idx=*/ 0,
            /*extra_locals=*/ &[],
        )
    }

    /// Invoke `func` starting at a non-zero bytecode index, with the
    /// supplied `extra_locals` overlaid past the args slot. This is
    /// the partial-resume entry point — M2-B will widen it to also
    /// rehydrate the operand stack from the deopt snapshot.
    pub fn invoke_from(
        &self,
        func: &BcFunction,
        args: &[VmValue],
        start_bc_idx: usize,
        extra_locals: &[VmValue],
    ) -> BcRunOutcome {
        self.invoke_from_with_locals(
            func,
            args,
            start_bc_idx,
            extra_locals,
            /*return_slot_count=*/ 0,
        )
    }

    /// Invoke `func` while reserving `return_slot_count` virtual
    /// locals past the args slot for the buffer-protocol epilogue's
    /// `StoreField` lowerings. Returns the full local-slot snapshot
    /// in [`BcRunOutcome::final_locals`] so the evaluator can lift
    /// the return-field values back into a `Value`.
    pub fn invoke_from_with_locals(
        &self,
        func: &BcFunction,
        args: &[VmValue],
        start_bc_idx: usize,
        extra_locals: &[VmValue],
        return_slot_count: u32,
    ) -> BcRunOutcome {
        self.invoke_from_with_stack(
            func,
            args,
            start_bc_idx,
            extra_locals,
            return_slot_count,
            /*initial_stack=*/ &[],
        )
    }

    /// v6-δ M2-B partial-resume entry: same as
    /// [`Self::invoke_from_with_locals`] but pre-seeds the operand
    /// stack with `initial_stack` (bottom-up) before dispatching.
    /// This is the path
    /// `crate::evaluator::BytecodeEvaluator::resume_from_pc` uses
    /// to rehydrate mid-expression deopts: the recipe in
    /// `BcFunction::stack_recipe[start_bc_idx]` tells the evaluator
    /// what values to push (locals / consts / snapshot slots), the
    /// evaluator materialises them into a `Vec<u64>`, and the VM
    /// continues dispatch at `start_bc_idx` exactly where the trace
    /// would have been.
    pub fn invoke_from_with_stack(
        &self,
        func: &BcFunction,
        args: &[VmValue],
        start_bc_idx: usize,
        extra_locals: &[VmValue],
        return_slot_count: u32,
        initial_stack: &[VmValue],
    ) -> BcRunOutcome {
        // Bytecode-coverage-expansion B-2: thin wrapper for callers
        // that don't need to lift host-supplied `Value::String` args
        // into the VM's per-invoke `StringArena`. See
        // [`Self::invoke_from_with_string_args`] for the lift-aware
        // entry point.
        self.invoke_from_with_string_args(
            func,
            args,
            start_bc_idx,
            extra_locals,
            return_slot_count,
            initial_stack,
            /*string_arg_slots=*/ &[],
        )
    }

    /// Bytecode-coverage-expansion B-2: invocation entry that lifts
    /// host-supplied `Value::String` args into the VM's per-invoke
    /// `StringArena` so the dispatch loop's string-shaped ops
    /// (`StrConcat` / `StrContains` / `StrSubstring` / …) can resolve
    /// the slot handles. Each entry in `string_arg_slots` is
    /// `(local_idx, string_payload)`: the helper allocates the payload
    /// in `memory.strings` after `memory` is constructed, then
    /// overwrites `locals[local_idx]` with the resulting handle.
    ///
    /// `string_return_slots` flips the lift direction: each listed
    /// local slot holds a string handle the host wants back; the
    /// helper reads the payload through the arena before drop and
    /// stores it in [`BcRunOutcome::final_strings`].
    ///
    /// Pre-condition: `local_idx` must be one of the args / let /
    /// return slots the dispatch loop reads; callers using
    /// `pack_args` already match this. Strings travel through the
    /// same u64 lane as the rest of the dispatch loop — the handle
    /// is the arena's 32-bit slot index zero-extended into the slot,
    /// identical to what `BcOp::StrConst` would push for an inline
    /// literal.
    #[allow(clippy::too_many_arguments)]
    pub fn invoke_from_with_string_args(
        &self,
        func: &BcFunction,
        args: &[VmValue],
        start_bc_idx: usize,
        extra_locals: &[VmValue],
        return_slot_count: u32,
        initial_stack: &[VmValue],
        string_arg_slots: &[(usize, &str)],
    ) -> BcRunOutcome {
        // Forward to the broader entry; the broader entry handles
        // both string args (in) and string return slots (out). Use
        // the empty-out variant here so the existing test surface
        // doesn't change behaviour.
        self.invoke_from_with_string_io(
            func,
            args,
            start_bc_idx,
            extra_locals,
            return_slot_count,
            initial_stack,
            string_arg_slots,
            /*string_return_slots=*/ &[],
        )
    }

    /// Bytecode-coverage-expansion B-2: full string-aware invocation
    /// entry. See [`Self::invoke_from_with_string_args`] for the args
    /// lift; this entry adds `string_return_slots` for lifting string
    /// payloads back out of the per-invoke `StringArena` before it
    /// drops.
    #[allow(clippy::too_many_arguments)]
    pub fn invoke_from_with_string_io(
        &self,
        func: &BcFunction,
        args: &[VmValue],
        start_bc_idx: usize,
        extra_locals: &[VmValue],
        return_slot_count: u32,
        initial_stack: &[VmValue],
        string_arg_slots: &[(usize, &str)],
        string_return_slots: &[usize],
    ) -> BcRunOutcome {
        // M2-C lever 2: reset the inline cache at the top of every
        // outer invocation so a previously-resolved host fn from an
        // earlier call can't shadow a `register_host_fn` swap that
        // happened between calls.
        self.reset_call_cache();
        let needed = (func.locals as usize)
            .max(args.len() + return_slot_count as usize + extra_locals.len());
        let mut locals = vec![0u64; needed];
        for (i, v) in args.iter().enumerate() {
            if i < locals.len() {
                locals[i] = *v;
            }
        }
        // Overlay extra locals past the args + return slots. The
        // trace-JIT deopt snapshot stores let-bound slots that the
        // recorder observed; let-locals sit in the bytecode VM past
        // the input + return reservations.
        let overlay_base = args.len() + return_slot_count as usize;
        for (i, v) in extra_locals.iter().enumerate() {
            let idx = overlay_base + i;
            if idx < locals.len() {
                locals[idx] = *v;
            }
        }
        let mut stack: Vec<VmValue> = Vec::with_capacity(16.max(initial_stack.len()));
        stack.extend_from_slice(initial_stack);
        // M2-B phase 4b: per-invoke arena state. Each invocation gets
        // a fresh memory bag — handles minted here drop with the
        // outcome so no value escapes the call. The memory state lives
        // on the stack frame (not inside `BytecodeVm`) because a
        // `&BytecodeVm` is shared across calls and the arenas must not
        // leak between them.
        let mut memory = VmMemory::default();
        // Bytecode-coverage-expansion B-2: lift host-supplied string
        // args into the fresh `StringArena` and stash the resulting
        // handles into the matching local slots. Has to happen here
        // (not in `pack_args`) because the arena is per-invoke and
        // wouldn't survive across calls. Slots outside `locals` are
        // silently dropped — same defensive posture as the `args` /
        // `extra_locals` overlays above.
        for (slot_idx, payload) in string_arg_slots {
            if *slot_idx < locals.len() {
                let handle = memory.strings.alloc(*payload);
                locals[*slot_idx] = handle as u64;
            }
        }
        let mut pc = start_bc_idx;
        let mut steps: u64 = 0;
        let mut last_bc_idx = pc;
        // Bytecode-coverage-expansion B-2: capture the return-slot
        // string list before the closure takes ownership so each
        // `exit(...)` call can lift the live arena payload into the
        // outcome before `memory` drops.
        let want_return_strings: Vec<usize> = string_return_slots.to_vec();
        let exit = |value: Option<VmValue>,
                    error: Option<BcVmError>,
                    last_bc_idx: usize,
                    steps: u64,
                    locals: Vec<VmValue>,
                    memory: &VmMemory|
         -> BcRunOutcome {
            // Lift requested return-slot strings into a host-owned
            // map before the arena drops. Slots out of range or
            // pointing at an invalid handle are skipped silently so a
            // mis-pred return slot doesn't crash the dispatch loop.
            let mut final_strings = std::collections::HashMap::new();
            for slot in &want_return_strings {
                if let Some(handle_u64) = locals.get(*slot) {
                    let handle = *handle_u64 as u32;
                    if let Ok(arc_s) = memory.strings.get(handle) {
                        final_strings.insert(*slot, arc_s.as_ref().to_string());
                    }
                }
            }
            BcRunOutcome {
                value,
                error,
                last_bc_idx,
                steps,
                final_locals: locals,
                final_strings,
            }
        };
        // M2-B phase 2 dispatch-time pre-check: if a `CapabilityGate`
        // is installed, every grant-table bit must still pass the
        // gate before the first op runs. Mirrors the cranelift
        // backend's vtable-build-time consult — the bytecode VM
        // doesn't have a vtable-build phase, so the pre-check runs
        // once at invoke entry (cost: one virtual call per granted
        // bit, paid by callers that opted into the gate). The scaffold
        // grant table is empty for the standard `from_source` path so
        // this is a no-op there; hand-built BcFunctions that grant
        // bits get the consult for free.
        //
        // Note: this is enforcement, not advisory — a denial here
        // surfaces as `RuntimeError::WasmCapabilityDenied` exactly
        // like a `BcOp::Trap(CapabilityDenied)` would. Phase 3 IR
        // coverage expansion will widen this into per-call-site
        // consults via the new `BcOp::CheckCap` / `BcOp::CallNative`
        // ops; phase 2 keeps the consult at the dispatch boundary
        // where the existing scaffold ops live.
        // P1-19: capability prologue runs through the shared helper so
        // the two-stage consult sequence (`consult_all_granted_bits`
        // then, when `requires_cap_consult` is set, the declared-bit
        // sweep — #166 M2-B from_source full cap-gate activation)
        // can't drift between this entry and `invoke_pooled_typed_i64`.
        if let Err(err) = self.precheck_capabilities(func) {
            return exit(None, Some(err), last_bc_idx, steps, locals, &memory);
        }
        // M2-B phase 4c: hot-counter prologue. Mirrors the cranelift
        // entry-fn prologue (`crates/relon-codegen-cranelift/src/codegen/hot_counter.rs`)
        // but as Rust on the dispatch path — no machine-code emit.
        // Only the **outer** invocation entry runs the trigger —
        // `start_bc_idx == 0` filters out partial-resume re-entries
        // routed through `BytecodeEvaluator::resume_from_pc` so the
        // recorder doesn't get retriggered for every deopt bounce.
        if start_bc_idx == 0 {
            self.maybe_trigger_hot(func, args);
        }
        // P2-7: sample the deadline / max_steps gates every 64 ops to
        // shave the per-op `Instant::now()` + `Option` branch from the
        // hot dispatch loop. The mask collapses to 0 (per-op) when
        // `max_steps` is tight enough that a ±64 op slip could let a
        // small-budget program slip past the trap; otherwise we
        // tolerate the ±64 op fuzz the task brief allows.
        let step_sample_mask: u64 = step_sample_mask(&self.config);
        loop {
            // Resource prong: tick.
            steps += 1;
            if (steps & step_sample_mask) == (1 & step_sample_mask) {
                if let Some(limit) = self.config.max_steps {
                    if steps > limit {
                        return exit(
                            None,
                            Some(BcVmError::StepLimitExceeded { steps }),
                            last_bc_idx,
                            steps,
                            locals,
                            &memory,
                        );
                    }
                }
                if let Some(d) = self.config.deadline {
                    if Instant::now() >= d {
                        return exit(
                            None,
                            Some(BcVmError::DeadlineExceeded),
                            last_bc_idx,
                            steps,
                            locals,
                            &memory,
                        );
                    }
                }
            }
            if pc >= func.ops.len() {
                return exit(
                    None,
                    Some(BcVmError::JumpOutOfRange {
                        target: pc,
                        ops: func.ops.len(),
                    }),
                    last_bc_idx,
                    steps,
                    locals,
                    &memory,
                );
            }
            last_bc_idx = pc;
            match self.dispatch_one(
                func,
                &func.ops[pc],
                &mut stack,
                &mut locals,
                &mut memory,
                pc,
                /*current_captures=*/ None,
                /*closures_pool=*/ &func.closure_bodies,
            ) {
                Ok(StepOutcome::Advance) => pc += 1,
                Ok(StepOutcome::Jump(target)) => {
                    if target > func.ops.len() {
                        return exit(
                            None,
                            Some(BcVmError::JumpOutOfRange {
                                target,
                                ops: func.ops.len(),
                            }),
                            last_bc_idx,
                            steps,
                            locals,
                            &memory,
                        );
                    }
                    pc = target;
                }
                Ok(StepOutcome::Return(v)) => {
                    return exit(Some(v), None, last_bc_idx, steps, locals, &memory);
                }
                Err(e) => {
                    return exit(None, Some(e), last_bc_idx, steps, locals, &memory);
                }
            }
        }
    }

    /// M2-C lever 7: typed-i64 pooled fast path.
    ///
    /// A bespoke entry for the scalar `#main(Int...) -> Int` envelope
    /// the `BytecodeEvaluator::run_main_i64` API serves. Differences
    /// from [`Self::invoke_from_with_stack`] (the general entry):
    ///
    /// * Reuses [`POOLED_LOCALS`] / [`POOLED_STACK`] thread-local
    ///   scratch buffers — no per-call `vec![0u64; N]` / `Vec::with_capacity`
    ///   alloc. After the first invoke warms the buffer the hot path
    ///   pays only a `Vec::clear` + a memset for the locals span.
    /// * Returns a single [`VmValue`] result + a `BcVmError` envelope,
    ///   skipping [`BcRunOutcome`]'s `final_locals` `Vec` move. The
    ///   caller passes the return slot index it wants to read directly.
    /// * No `extra_locals` / `initial_stack` overlay — the fast path
    ///   is for outer-entry invokes only (partial-resume still routes
    ///   through `invoke_from_with_stack`).
    ///
    /// All other dispatch semantics (cap-vtable consult, hot-counter
    /// prologue, dispatcher-switch / trace-lookup consult, step/deadline
    /// ticks, full `BcOp` coverage) match the general path bit-for-bit.
    /// This is purely an allocation + return-shape micro-optimisation;
    /// it does not change the W12 envelope's correctness surface.
    ///
    /// Returns `Ok(VmValue)` with the value at `return_slot_idx` on
    /// successful `Return`, or `Err(BcVmError)` for any sandbox prong
    /// trip. The caller is responsible for ensuring `return_slot_idx`
    /// is in range — typically `BytecodeEvaluator::return_field_base`
    /// for schema-driven returns, or `0` for the legacy-i64 envelope.
    pub fn invoke_pooled_typed_i64(
        &self,
        func: &BcFunction,
        args: &[VmValue],
        return_slot_count: u32,
        return_slot_idx: u32,
    ) -> Result<VmValue, BcVmError> {
        // Reset the inline cache so a previously-resolved host fn from
        // an earlier call can't shadow a `register_host_fn` swap that
        // happened between calls. Matches the general path's discipline.
        self.reset_call_cache();
        let needed = (func.locals as usize).max(args.len() + return_slot_count as usize);
        POOLED_LOCALS.with(|locals_cell| {
            POOLED_STACK.with(|stack_cell| {
                let mut locals = locals_cell.borrow_mut();
                let mut stack = stack_cell.borrow_mut();
                // Resize-and-zero the locals span. `resize` keeps any
                // already-allocated capacity from a previous call and
                // only grows the heap span when `needed` exceeds it.
                // Once-warmed buffers stay warm — the W12 row pays the
                // memset only.
                locals.clear();
                locals.resize(needed, 0u64);
                // Seed args into the bottom of the locals span.
                for (i, v) in args.iter().enumerate() {
                    locals[i] = *v;
                }
                stack.clear();
                // Pre-emptively reserve a reasonable stack depth so the
                // typical scalar workload doesn't grow mid-dispatch.
                if stack.capacity() < 16 {
                    stack.reserve(16);
                }

                let mut memory = VmMemory::default();
                let mut pc: usize = 0;
                let mut steps: u64 = 0;

                // P1-19: capability + hot-counter prologue routed
                // through the shared helpers, identical to the general
                // `invoke_from_with_stack` path. The W12-style inert
                // scaffold still pays only the per-helper short-circuit
                // (`consult_all_granted_bits` / `consult_all_declared_bits`
                // each return early when no gate is installed), and the
                // `#[inline]` annotation keeps the call sites equivalent
                // to the hand-inlined original.
                self.precheck_capabilities(func)?;
                self.maybe_trigger_hot(func, args);

                // P2-7: deadline / max_steps sampling — see
                // `invoke_from_with_stack` for the rationale.
                let step_sample_mask: u64 = step_sample_mask(&self.config);
                loop {
                    steps += 1;
                    if (steps & step_sample_mask) == (1 & step_sample_mask) {
                        if let Some(limit) = self.config.max_steps {
                            if steps > limit {
                                return Err(BcVmError::StepLimitExceeded { steps });
                            }
                        }
                        if let Some(d) = self.config.deadline {
                            if Instant::now() >= d {
                                return Err(BcVmError::DeadlineExceeded);
                            }
                        }
                    }
                    if pc >= func.ops.len() {
                        return Err(BcVmError::JumpOutOfRange {
                            target: pc,
                            ops: func.ops.len(),
                        });
                    }
                    match self.dispatch_one(
                        func,
                        &func.ops[pc],
                        &mut stack,
                        &mut locals,
                        &mut memory,
                        pc,
                        /*current_captures=*/ None,
                        /*closures_pool=*/ &func.closure_bodies,
                    ) {
                        Ok(StepOutcome::Advance) => pc += 1,
                        Ok(StepOutcome::Jump(target)) => {
                            if target > func.ops.len() {
                                return Err(BcVmError::JumpOutOfRange {
                                    target,
                                    ops: func.ops.len(),
                                });
                            }
                            pc = target;
                        }
                        Ok(StepOutcome::Return(v)) => {
                            // For the legacy-i64 path the schema slot
                            // is irrelevant — the return value `v` is
                            // already what the caller wants. For schema-
                            // driven returns the value lives at
                            // `return_slot_idx` of the locals span; the
                            // top-of-stack `v` is the buffer-protocol
                            // `bytes_written` placeholder (the bytecode
                            // compiler synthesises it but never reads
                            // it back). Read the slot if the schema
                            // reserved one, otherwise return `v`.
                            if return_slot_count > 0 {
                                return Ok(locals
                                    .get(return_slot_idx as usize)
                                    .copied()
                                    .unwrap_or(0));
                            }
                            return Ok(v);
                        }
                        Err(e) => return Err(e),
                    }
                }
            })
        })
    }

    /// M3 closure-body sub-dispatch. Runs the supplied closure body
    /// against a fresh locals frame seeded with `args` (positional)
    /// and with `captures` exposed via [`BcOp::CaptureGet`]. The
    /// outer VM's [`VmMemory`] is shared (allocations made by the
    /// closure body — list pushes, string concats, nested closure
    /// constructions — live alongside the caller's handles so
    /// closure-returned values stay observable to the caller).
    ///
    /// The sub-loop reuses the parent VM's resource accounting style
    /// (step ticks + deadline check) but on a fresh local counter,
    /// then returns the single value the body pushed on its operand
    /// stack via `BcOp::Return`. A trap inside the body propagates
    /// unchanged.
    fn invoke_closure_body(
        &self,
        body: &BcFunction,
        args: &[VmValue],
        captures: &[VmValue],
        memory: &mut VmMemory,
        closures_pool: &[BcFunction],
    ) -> Result<VmValue, BcVmError> {
        let needed = (body.locals as usize).max(args.len());
        let mut locals: Vec<VmValue> = vec![0u64; needed];
        for (i, v) in args.iter().enumerate() {
            if i < locals.len() {
                locals[i] = *v;
            }
        }
        let mut stack: Vec<VmValue> = Vec::with_capacity(8);
        let mut pc: usize = 0;
        // Local step budget: keep the closure body bounded by the
        // outer VM's `max_steps` cap (closure bodies inherit the
        // resource budget). If `max_steps` is `None` the loop runs
        // unbounded (matches the outer dispatch).
        let mut steps: u64 = 0;
        // P2-7: deadline / max_steps sampling — see
        // `invoke_from_with_stack` for the rationale.
        let step_sample_mask: u64 = step_sample_mask(&self.config);
        loop {
            steps += 1;
            if (steps & step_sample_mask) == (1 & step_sample_mask) {
                if let Some(limit) = self.config.max_steps {
                    if steps > limit {
                        return Err(BcVmError::StepLimitExceeded { steps });
                    }
                }
                if let Some(d) = self.config.deadline {
                    if Instant::now() >= d {
                        return Err(BcVmError::DeadlineExceeded);
                    }
                }
            }
            if pc >= body.ops.len() {
                return Err(BcVmError::JumpOutOfRange {
                    target: pc,
                    ops: body.ops.len(),
                });
            }
            match self.dispatch_one(
                body,
                &body.ops[pc],
                &mut stack,
                &mut locals,
                memory,
                pc,
                Some(captures),
                closures_pool,
            )? {
                StepOutcome::Advance => pc += 1,
                StepOutcome::Jump(target) => {
                    if target > body.ops.len() {
                        return Err(BcVmError::JumpOutOfRange {
                            target,
                            ops: body.ops.len(),
                        });
                    }
                    pc = target;
                }
                StepOutcome::Return(v) => return Ok(v),
            }
        }
    }

    // M3: the captures parameter pushes the arg count past clippy's
    // default seven-arg threshold; the alternative (bundle the dispatch
    // state into a struct) buys nothing here because the fields are
    // already pulled apart for borrow-discipline reasons (operand
    // stack borrowed mutably alongside locals + arena).
    #[allow(clippy::too_many_arguments)]
    fn dispatch_one(
        &self,
        func: &BcFunction,
        op: &BcOp,
        stack: &mut Vec<VmValue>,
        locals: &mut [VmValue],
        memory: &mut VmMemory,
        bc_idx: usize,
        // M3: captures of the currently executing closure body, if
        // any. Outer (non-closure) dispatch passes `None`; the closure
        // call site (`BcOp::CallClosure`) re-enters a sub-dispatch loop
        // with `Some(&closure_slot.captures)` so `BcOp::CaptureGet`
        // resolves against the matching frame.
        current_captures: Option<&[VmValue]>,
        // Phase D: shared closure-body pool. The top-level invoker
        // passes `&func.closure_bodies`; nested `BcOp::CallClosure`
        // invocations forward the same pool unchanged. This decouples
        // the body resolution from the currently-executing
        // `BcFunction`, so a closure body whose `closure_bodies` field
        // is empty (the common shape for non-nesting lambdas — they
        // never emit their own `MakeClosure`) can still dispatch
        // self-referential calls: the slot's `body_idx` keys into the
        // top-level pool, which contains the lambda itself.
        closures_pool: &[BcFunction],
    ) -> Result<StepOutcome, BcVmError> {
        match op {
            BcOp::ConstI64(v) => {
                stack.push(*v as u64);
            }
            BcOp::ConstI32(v) => {
                stack.push(*v as u32 as u64);
            }
            BcOp::LocalGet(idx) => {
                let i = *idx as usize;
                if i >= locals.len() {
                    return Err(BcVmError::StackUnderflow { bc_idx });
                }
                stack.push(locals[i]);
            }
            BcOp::LocalSet(idx) => {
                let v = pop(stack, bc_idx)?;
                let i = *idx as usize;
                if i >= locals.len() {
                    return Err(BcVmError::StackUnderflow { bc_idx });
                }
                locals[i] = v;
            }
            // M2-C lever 3: per-op specialization — typed arith / cmp
            // ops dispatch through monomorphic arms that read the
            // operand-stack slots through the i64 / f64 lane directly,
            // skipping the inner `match ty` the typed-IrType umbrella
            // used to go through. The i64 arms cover both `IrType::I64`
            // and `IrType::I32` (the bytecode VM has no I32 storage of
            // its own — both ride the same u64 slot).
            BcOp::AddI64 => {
                let rhs = pop(stack, bc_idx)? as i64;
                let lhs = pop(stack, bc_idx)? as i64;
                let r = lhs.checked_add(rhs).ok_or(BcVmError::NumericOverflow)?;
                stack.push(r as u64);
            }
            BcOp::SubI64 => {
                let rhs = pop(stack, bc_idx)? as i64;
                let lhs = pop(stack, bc_idx)? as i64;
                let r = lhs.checked_sub(rhs).ok_or(BcVmError::NumericOverflow)?;
                stack.push(r as u64);
            }
            BcOp::MulI64 => {
                let rhs = pop(stack, bc_idx)? as i64;
                let lhs = pop(stack, bc_idx)? as i64;
                let r = lhs.checked_mul(rhs).ok_or(BcVmError::NumericOverflow)?;
                stack.push(r as u64);
            }
            BcOp::DivI64 => {
                let rhs = pop(stack, bc_idx)? as i64;
                let lhs = pop(stack, bc_idx)? as i64;
                if rhs == 0 {
                    return Err(BcVmError::DivisionByZero);
                }
                let r = lhs.checked_div(rhs).ok_or(BcVmError::NumericOverflow)?;
                stack.push(r as u64);
            }
            BcOp::ModI64 => {
                let rhs = pop(stack, bc_idx)? as i64;
                let lhs = pop(stack, bc_idx)? as i64;
                if rhs == 0 {
                    return Err(BcVmError::DivisionByZero);
                }
                let r = lhs.checked_rem(rhs).ok_or(BcVmError::NumericOverflow)?;
                stack.push(r as u64);
            }
            BcOp::AddF64 => {
                let rhs = f64::from_bits(pop(stack, bc_idx)?);
                let lhs = f64::from_bits(pop(stack, bc_idx)?);
                stack.push((lhs + rhs).to_bits());
            }
            BcOp::SubF64 => {
                let rhs = f64::from_bits(pop(stack, bc_idx)?);
                let lhs = f64::from_bits(pop(stack, bc_idx)?);
                stack.push((lhs - rhs).to_bits());
            }
            BcOp::MulF64 => {
                let rhs = f64::from_bits(pop(stack, bc_idx)?);
                let lhs = f64::from_bits(pop(stack, bc_idx)?);
                stack.push((lhs * rhs).to_bits());
            }
            BcOp::DivF64 => {
                let rhs = f64::from_bits(pop(stack, bc_idx)?);
                let lhs = f64::from_bits(pop(stack, bc_idx)?);
                stack.push((lhs / rhs).to_bits());
            }
            BcOp::ModF64 => {
                let rhs = f64::from_bits(pop(stack, bc_idx)?);
                let lhs = f64::from_bits(pop(stack, bc_idx)?);
                stack.push((lhs % rhs).to_bits());
            }
            BcOp::EqI64 => {
                let rhs = pop(stack, bc_idx)? as i64;
                let lhs = pop(stack, bc_idx)? as i64;
                stack.push(if lhs == rhs { 1 } else { 0 });
            }
            BcOp::NeI64 => {
                let rhs = pop(stack, bc_idx)? as i64;
                let lhs = pop(stack, bc_idx)? as i64;
                stack.push(if lhs != rhs { 1 } else { 0 });
            }
            BcOp::LtI64 => {
                let rhs = pop(stack, bc_idx)? as i64;
                let lhs = pop(stack, bc_idx)? as i64;
                stack.push(if lhs < rhs { 1 } else { 0 });
            }
            BcOp::LeI64 => {
                let rhs = pop(stack, bc_idx)? as i64;
                let lhs = pop(stack, bc_idx)? as i64;
                stack.push(if lhs <= rhs { 1 } else { 0 });
            }
            BcOp::GtI64 => {
                let rhs = pop(stack, bc_idx)? as i64;
                let lhs = pop(stack, bc_idx)? as i64;
                stack.push(if lhs > rhs { 1 } else { 0 });
            }
            BcOp::GeI64 => {
                let rhs = pop(stack, bc_idx)? as i64;
                let lhs = pop(stack, bc_idx)? as i64;
                stack.push(if lhs >= rhs { 1 } else { 0 });
            }
            BcOp::EqF64 => {
                let rhs = f64::from_bits(pop(stack, bc_idx)?);
                let lhs = f64::from_bits(pop(stack, bc_idx)?);
                stack.push(if lhs == rhs { 1 } else { 0 });
            }
            BcOp::NeF64 => {
                let rhs = f64::from_bits(pop(stack, bc_idx)?);
                let lhs = f64::from_bits(pop(stack, bc_idx)?);
                stack.push(if lhs != rhs { 1 } else { 0 });
            }
            BcOp::LtF64 => {
                let rhs = f64::from_bits(pop(stack, bc_idx)?);
                let lhs = f64::from_bits(pop(stack, bc_idx)?);
                stack.push(if lhs < rhs { 1 } else { 0 });
            }
            BcOp::LeF64 => {
                let rhs = f64::from_bits(pop(stack, bc_idx)?);
                let lhs = f64::from_bits(pop(stack, bc_idx)?);
                stack.push(if lhs <= rhs { 1 } else { 0 });
            }
            BcOp::GtF64 => {
                let rhs = f64::from_bits(pop(stack, bc_idx)?);
                let lhs = f64::from_bits(pop(stack, bc_idx)?);
                stack.push(if lhs > rhs { 1 } else { 0 });
            }
            BcOp::GeF64 => {
                let rhs = f64::from_bits(pop(stack, bc_idx)?);
                let lhs = f64::from_bits(pop(stack, bc_idx)?);
                stack.push(if lhs >= rhs { 1 } else { 0 });
            }
            BcOp::Jump(target) => return Ok(StepOutcome::Jump(*target)),
            BcOp::JumpIfTrue(target) => {
                let cond = pop(stack, bc_idx)? as u32;
                if cond != 0 {
                    return Ok(StepOutcome::Jump(*target));
                }
            }
            BcOp::JumpIfFalse(target) => {
                let cond = pop(stack, bc_idx)? as u32;
                if cond == 0 {
                    return Ok(StepOutcome::Jump(*target));
                }
            }
            BcOp::Return => {
                // The buffer-protocol IR ends `run_main` with
                // `StoreField + Return` — the codegen synthesises a
                // trailing `bytes_written` push, but the bytecode VM
                // skips that since the return-field values are
                // unpacked from the virtual locals slot. Tolerate an
                // empty stack by returning `0`; tests that genuinely
                // depend on the popped value (legacy-i64 direct-IR
                // path) leave a single operand and we lift it
                // through.
                let v = stack.pop().unwrap_or(0);
                return Ok(StepOutcome::Return(v));
            }
            BcOp::CallNative {
                import_idx,
                arg_count,
                cap_bit,
                ret_ty,
            } => {
                // M2-B phase 3: per-call-site capability consult.
                //
                // Order of operations matters — the gate consult runs
                // **before** we touch the operand stack so a denial
                // surfaces with the correct `cap_bit` regardless of
                // arg-count mismatches. This matches the cranelift
                // `check_cap` prologue's discipline: the host fn never
                // observes any state when the capability prong fires.
                if *cap_bit != u32::MAX {
                    if let Err(err) = self.config.cap_vtable.consult_gate(*cap_bit) {
                        return Err(BcVmError::CapabilityDenied {
                            cap_bit: err.cap.bit_index(),
                        });
                    }
                    // Belt-and-braces: when no gate is installed the
                    // legacy grant-table path enforces the bit. An
                    // ungranted bit on a `cap_bit`-tagged call surfaces
                    // as the same `CapabilityDenied` shape.
                    if self.config.cap_vtable.gate().is_none()
                        && !self.config.cap_vtable.is_granted(*cap_bit)
                    {
                        return Err(BcVmError::CapabilityDenied { cap_bit: *cap_bit });
                    }
                }
                // M2-B phase 4a: real host-fn dispatch.
                //
                // Resolve the registry slot keyed by `import_idx`. A
                // hit pops `arg_count` operands (in declaration order —
                // the top-of-stack is the last arg), decodes them per
                // the phase-4a scalar lane convention (i64 for the
                // numeric / bool / null slots, f64 via bit cast for the
                // float lane), invokes the host fn, and re-encodes the
                // [`Value`] return back into the operand stack per
                // `ret_ty`. A miss falls through to the legacy
                // `NativeNotImplemented` envelope so the differential
                // harness's bounce shape stays stable.
                //
                // We still drain `arg_count` operands on the miss path
                // so the surrounding op stream's stack discipline
                // matches what a real dispatch would leave behind —
                // this keeps the `stack_recipe` table valid in the
                // deopt-resume path.
                // M2-C lever 2: consult the per-VM inline cache before
                // walking the per-call HashMap. Hot loops dispatching
                // the same `import_idx` repeatedly take the fast path;
                // the polymorphic case still falls through cleanly.
                let host_fn = self.resolve_host_fn_cached(*import_idx);
                if let Some(func) = host_fn {
                    // Pop in declaration order: stack top is the last
                    // arg, so we collect then reverse.
                    let mut packed: Vec<Value> = Vec::with_capacity(*arg_count as usize);
                    for _ in 0..*arg_count {
                        let slot = pop(stack, bc_idx)?;
                        // Phase 4a scope: all args travel through the
                        // i64 lane as `Value::Int`. The wider
                        // arg-type-tagged decode (per-slot `IrType`)
                        // ships with the buffer-protocol envelope in
                        // phase 4b — until then host fns that need
                        // anything richer than `Value::Int` should
                        // route through the tree-walker / cranelift
                        // backends.
                        packed.push(Value::Int(slot as i64));
                    }
                    packed.reverse();
                    let caps = bytecode_native_caps();
                    let args = NativeArgs::from_positional(packed, caps);
                    let returned = func.call(args, TokenRange::default()).map_err(|e| {
                        // Host-fn failure surfaces as `Unsupported` —
                        // the bytecode VM's error envelope doesn't
                        // carry a `NativeFnError` variant today, and
                        // the lift through `into_runtime_error` already
                        // routes `Unsupported` cleanly. Phase 4b can
                        // widen this when richer error shapes land.
                        BcVmError::HostFnError {
                            import_idx: *import_idx,
                            reason: e.to_string(),
                        }
                    })?;
                    // Encode the return value back into the VM's u64
                    // slot per `ret_ty`. Phase 4b-continuation: the
                    // encoder reaches into `memory` for the String /
                    // ListInt lift lanes, so it has to thread the
                    // arena state through.
                    stack.push(encode_value_for_ret(
                        &returned,
                        *ret_ty,
                        *import_idx,
                        memory,
                    )?);
                } else {
                    for _ in 0..*arg_count {
                        pop(stack, bc_idx)?;
                    }
                    return Err(BcVmError::NativeNotImplemented {
                        import_idx: *import_idx,
                    });
                }
            }
            BcOp::CheckCap { cap_bit } => {
                // M2-B phase 3: standalone capability consult. The
                // wasm `Op::CheckCap` lower target — fires the gate
                // consult without dispatching a call. `u32::MAX`
                // (`NO_CAPABILITY_BIT`) is a no-op so the analyzer can
                // emit unconditional `CheckCap` ops without forcing
                // every backend to special-case the sentinel.
                if *cap_bit != u32::MAX {
                    if let Err(err) = self.config.cap_vtable.consult_gate(*cap_bit) {
                        return Err(BcVmError::CapabilityDenied {
                            cap_bit: err.cap.bit_index(),
                        });
                    }
                    if self.config.cap_vtable.gate().is_none()
                        && !self.config.cap_vtable.is_granted(*cap_bit)
                    {
                        return Err(BcVmError::CapabilityDenied { cap_bit: *cap_bit });
                    }
                }
            }
            BcOp::CallStdlibScalar { kind, arg_count } => {
                // M2-B phase 3: scalar-pure stdlib dispatch. Pops the
                // declared arity, evaluates the handler, pushes one
                // i64 result. Arity mismatches between `kind.arity()`
                // and `arg_count` are compile-time bugs but the
                // dispatcher honours the encoded `arg_count` so the
                // stack-recipe accounting stays consistent.
                if kind.arity() != *arg_count {
                    return Err(BcVmError::StackUnderflow { bc_idx });
                }
                match kind {
                    BcStdlibKind::IntAbs => {
                        let v = pop(stack, bc_idx)? as i64;
                        stack.push(v.wrapping_abs() as u64);
                    }
                    BcStdlibKind::IntMin => {
                        let rhs = pop(stack, bc_idx)? as i64;
                        let lhs = pop(stack, bc_idx)? as i64;
                        stack.push(lhs.min(rhs) as u64);
                    }
                    BcStdlibKind::IntMax => {
                        let rhs = pop(stack, bc_idx)? as i64;
                        let lhs = pop(stack, bc_idx)? as i64;
                        stack.push(lhs.max(rhs) as u64);
                    }
                }
            }
            BcOp::ListLen => {
                // M2-B phase 3: pre-computed length already sits on
                // the stack as an i64 (the compile-pass constant-fold
                // for `Op::ConstList*` stores the length verbatim).
                // The op is a witness slot — leave the stack untouched
                // and step over.
            }
            BcOp::MakeList { len } => {
                // M2-B phase 4b: pop `len` operands in declaration
                // order. Top-of-stack is the last element, so we
                // collect-then-reverse to match the IR-level layout.
                // An underflow surfaces as `StackUnderflow` (compiler
                // bug — `apply_stack_effect` should have caught it).
                let n = *len as usize;
                if stack.len() < n {
                    return Err(BcVmError::StackUnderflow { bc_idx });
                }
                let mut elements: Vec<u64> = Vec::with_capacity(n);
                for _ in 0..n {
                    elements.push(pop(stack, bc_idx)?);
                }
                elements.reverse();
                let handle = memory.lists.alloc(elements);
                stack.push(handle as u64);
            }
            BcOp::ListGetInt => {
                // M2-B phase 4b: `[list, idx] -> [elem]`. Pop idx
                // first (top-of-stack), then handle. Out-of-range
                // (incl. negative) trips `IndexOutOfBounds` — matches
                // the tree-walker / cranelift envelope.
                let idx = pop(stack, bc_idx)? as i64;
                let handle = pop(stack, bc_idx)? as u32;
                let elem = memory
                    .lists
                    .get_element(handle, idx)
                    .map_err(arena_to_vm_error)?;
                stack.push(elem);
            }
            BcOp::ListPush => {
                // M2-B phase 4b-continuation: `[list, elem] -> [list']`.
                // Pop element first (top-of-stack), then handle.
                // Copy-on-write semantics: if the slot's Arc has a
                // single owner the push lands in place and we re-push
                // the same handle; otherwise we clone the elements and
                // allocate a fresh slot.
                let elem = pop(stack, bc_idx)?;
                let handle = pop(stack, bc_idx)? as u32;
                let new_handle = memory
                    .lists
                    .push_cow(handle, elem)
                    .map_err(arena_to_vm_error)?;
                stack.push(new_handle as u64);
            }
            BcOp::StrConst { idx } => {
                // M2-B phase 4b-continuation: intern a per-function
                // string pool entry into the live VM's StringArena and
                // push the resulting handle.
                let pool_idx = *idx as usize;
                let value = func
                    .string_pool
                    .get(pool_idx)
                    .ok_or(BcVmError::StackUnderflow { bc_idx })?;
                let handle = memory.strings.alloc(value.as_str());
                stack.push(handle as u64);
            }
            BcOp::StrLen => {
                // `[s] -> [i64 len]`. Code-point count for tree-walker
                // parity (`String::chars().count()`).
                let handle = pop(stack, bc_idx)? as u32;
                let n = memory.strings.len_of(handle).map_err(arena_to_vm_error)?;
                stack.push(n as u64);
            }
            BcOp::StrConcat => {
                // `[s_lhs, s_rhs] -> [s_concat]`. Pop rhs first then
                // lhs; alloc a fresh slot.
                let rhs_h = pop(stack, bc_idx)? as u32;
                let lhs_h = pop(stack, bc_idx)? as u32;
                let lhs = memory
                    .strings
                    .get(lhs_h)
                    .map_err(arena_to_vm_error)?
                    .clone();
                let rhs = memory
                    .strings
                    .get(rhs_h)
                    .map_err(arena_to_vm_error)?
                    .clone();
                let mut joined = String::with_capacity(lhs.len() + rhs.len());
                joined.push_str(&lhs);
                joined.push_str(&rhs);
                let handle = memory.strings.alloc(joined.as_str());
                stack.push(handle as u64);
            }
            BcOp::StrConcatN { argc } => {
                // #165 — single-allocation N-operand string concat.
                // `[s_0, s_1, ..., s_{n-1}] -> [s_concat]`. Pops `argc`
                // handles in declaration order (the top-of-stack is
                // the outer RHS, the bottom is the deepest leaf). The
                // op was lowered from `Op::StrConcatN` so we know
                // `argc >= 2`; defensive guard keeps the dispatch
                // honest in case a malformed bytecode reaches us.
                let n = *argc as usize;
                if n < 2 {
                    return Err(BcVmError::StackUnderflow { bc_idx });
                }
                if stack.len() < n {
                    return Err(BcVmError::StackUnderflow { bc_idx });
                }
                // Pop handles into a temp buffer in reverse order then
                // restore source order so the joined payload reads
                // `s_0 || s_1 || ... || s_{n-1}` left-to-right.
                let mut handles: Vec<u32> = Vec::with_capacity(n);
                for _ in 0..n {
                    handles.push(pop(stack, bc_idx)? as u32);
                }
                handles.reverse();
                // Resolve every handle up-front, clone the underlying
                // `Arc<str>` so we don't keep the arena's per-slot
                // borrow alive across the eventual `memory.strings.alloc`
                // call. Cloning an `Arc` is a cheap refcount bump.
                let mut slots: Vec<std::sync::Arc<str>> = Vec::with_capacity(n);
                let mut total_len: usize = 0;
                for h in &handles {
                    let s = memory.strings.get(*h).map_err(arena_to_vm_error)?.clone();
                    total_len = total_len.saturating_add(s.len());
                    slots.push(s);
                }
                // Single-allocation join: one `String::with_capacity`
                // for `total_len` bytes followed by an in-place
                // `push_str` per operand. Mirrors the
                // `SmolStr::concat_many` shape that the tree-walker
                // already uses (#152) — the bytecode VM keeps its
                // own `StringArena` rather than touching `SmolStr`,
                // so we re-implement the single-alloc pattern in
                // arena terms.
                let mut joined = String::with_capacity(total_len);
                for slot in &slots {
                    joined.push_str(slot);
                }
                let handle = memory.strings.alloc(joined.as_str());
                stack.push(handle as u64);
            }
            BcOp::StrEq => {
                // `[s_lhs, s_rhs] -> [bool]`. Byte-equal compare.
                let rhs_h = pop(stack, bc_idx)? as u32;
                let lhs_h = pop(stack, bc_idx)? as u32;
                let lhs = memory
                    .strings
                    .get(lhs_h)
                    .map_err(arena_to_vm_error)?
                    .clone();
                let rhs = memory
                    .strings
                    .get(rhs_h)
                    .map_err(arena_to_vm_error)?
                    .clone();
                stack.push(if lhs.as_ref() == rhs.as_ref() { 1 } else { 0 });
            }
            BcOp::StrGlobMatch => {
                // 2026-05-21: `[s, pattern] -> [bool]`. Pop pattern
                // first (top-of-stack), then the haystack. Resolve both
                // handles in the StringArena and defer to the shared
                // `relon_ir::glob::glob_match` algorithm so the
                // bytecode backend stays behaviour-equivalent with the
                // tree-walker + cranelift paths.
                let pat_h = pop(stack, bc_idx)? as u32;
                let s_h = pop(stack, bc_idx)? as u32;
                let s = memory.strings.get(s_h).map_err(arena_to_vm_error)?.clone();
                let pat = memory
                    .strings
                    .get(pat_h)
                    .map_err(arena_to_vm_error)?
                    .clone();
                let matched = relon_ir::glob::glob_match(s.as_ref(), pat.as_ref());
                stack.push(if matched { 1 } else { 0 });
            }
            BcOp::StrContains => {
                // Bytecode-coverage-expansion B-1: `[haystack, needle]
                // -> [bool]`. Pops the needle first (top-of-stack), then
                // the haystack. Resolves both handles in the StringArena
                // and defers to Rust's stdlib `str::contains` byte-
                // substring search. Mirrors `TraceOp::StrContains` so a
                // trace-jit deopt that landed on a bytecode `.contains`
                // call can resume without the VM walking the raw-memory
                // `contains_string` body the bytecode envelope rejects.
                let needle_h = pop(stack, bc_idx)? as u32;
                let s_h = pop(stack, bc_idx)? as u32;
                let s = memory.strings.get(s_h).map_err(arena_to_vm_error)?.clone();
                let needle = memory
                    .strings
                    .get(needle_h)
                    .map_err(arena_to_vm_error)?
                    .clone();
                stack.push(if s.as_ref().contains(needle.as_ref()) {
                    1
                } else {
                    0
                });
            }
            BcOp::StrSubstring => {
                // Bytecode-coverage-expansion B-1: `[s, i64 start, i64
                // length] -> [s_substring_handle]`. Pops `length` first
                // (top-of-stack), then `start`, then the string handle.
                // Clamps `start` / `length` into `[0, len(s)]` (the
                // same clamp posture the trace-jit `__relon_str_substring`
                // shim applies) and allocates a fresh `StringArena` slot
                // for the byte-range slice.
                //
                // Byte indexing is intentional: tree-walker / cranelift
                // / trace-jit all treat `substring` as byte-indexed so
                // the bytecode path stays behaviour-equivalent. Callers
                // wanting Unicode-aware slicing live one layer up.
                let len_i64 = pop(stack, bc_idx)? as i64;
                let start_i64 = pop(stack, bc_idx)? as i64;
                let s_h = pop(stack, bc_idx)? as u32;
                let s = memory.strings.get(s_h).map_err(arena_to_vm_error)?.clone();
                let s_len = s.as_ref().len() as i64;
                let start = start_i64.clamp(0, s_len) as usize;
                let len = len_i64.clamp(0, s_len - start as i64) as usize;
                let slice = &s.as_ref().as_bytes()[start..start + len];
                // Safe: the input is valid UTF-8 and we slice on byte
                // bounds. The trace-jit shim has the same safety
                // posture (it relies on the recorder having clamped to
                // a valid UTF-8 boundary; callers passing mid-codepoint
                // offsets get the same garbled output here as there).
                let slice_str = std::str::from_utf8(slice).unwrap_or("");
                let handle = memory.strings.alloc(slice_str);
                stack.push(handle as u64);
            }
            BcOp::MakeDict { len } => {
                // `[k_0, v_0, ..., k_{n-1}, v_{n-1}] -> [dict_handle]`.
                // Pops `len * 2` slots; the keys are string handles
                // (a compile-time invariant — the lowering pass only
                // emits MakeDict after StrConst entries).
                let n = *len as usize;
                let needed = n * 2;
                if stack.len() < needed {
                    return Err(BcVmError::StackUnderflow { bc_idx });
                }
                let mut entries: Vec<(std::sync::Arc<str>, u64)> = Vec::with_capacity(n);
                // Pop in reverse, then reverse so declaration order is
                // restored.
                let mut tmp: Vec<(u32, u64)> = Vec::with_capacity(n);
                for _ in 0..n {
                    let v = pop(stack, bc_idx)?;
                    let k = pop(stack, bc_idx)? as u32;
                    tmp.push((k, v));
                }
                tmp.reverse();
                for (k_handle, v) in tmp {
                    let k = memory
                        .strings
                        .get(k_handle)
                        .map_err(arena_to_vm_error)?
                        .clone();
                    entries.push((std::sync::Arc::<str>::from(k.as_ref()), v));
                }
                let handle = memory.dicts.alloc(entries);
                stack.push(handle as u64);
            }
            BcOp::DictLookupStr => {
                // `[dict, key] -> [value]`. Pop key first (top of
                // stack), then dict handle. Miss surfaces as
                // `IndexOutOfBounds` — the tree-walker envelope.
                let key_handle = pop(stack, bc_idx)? as u32;
                let dict_handle = pop(stack, bc_idx)? as u32;
                let key = memory
                    .strings
                    .get(key_handle)
                    .map_err(arena_to_vm_error)?
                    .clone();
                let value = memory
                    .dicts
                    .lookup(dict_handle, key.as_ref())
                    .map_err(arena_to_vm_error)?
                    .ok_or(BcVmError::IndexOutOfBounds)?;
                stack.push(value);
            }
            BcOp::MakeClosure {
                body_idx,
                capture_count,
            } => {
                // M3: pop `capture_count` operands in declaration
                // order (top-of-stack is the last capture), copy into
                // a fresh closure slot, push the resulting handle.
                let n = *capture_count as usize;
                if stack.len() < n {
                    return Err(BcVmError::StackUnderflow { bc_idx });
                }
                let mut captures: Vec<VmValue> = Vec::with_capacity(n);
                for _ in 0..n {
                    captures.push(pop(stack, bc_idx)?);
                }
                captures.reverse();
                // Validate body index against the shared closure pool.
                // Phase D: pool is the top-level function's
                // `closure_bodies`; inner closure bodies pass the same
                // pool through so a nested `MakeClosure` (rare today,
                // but valid IR shape) still resolves correctly.
                if (*body_idx as usize) >= closures_pool.len() {
                    return Err(BcVmError::StackUnderflow { bc_idx });
                }
                let handle = memory.closures.alloc(*body_idx, captures);
                stack.push(handle as u64);
            }
            BcOp::CallClosure { argc } => {
                // M3: pop `argc` args (top-of-stack is the last arg)
                // then the closure handle. Look up the slot, dispatch
                // the closure body in a fresh stack/locals frame
                // populated with the popped args, return value pushed
                // back onto the caller's stack.
                let n = *argc as usize;
                if stack.len() < n + 1 {
                    return Err(BcVmError::StackUnderflow { bc_idx });
                }
                let mut args: Vec<VmValue> = Vec::with_capacity(n);
                for _ in 0..n {
                    args.push(pop(stack, bc_idx)?);
                }
                args.reverse();
                let handle = pop(stack, bc_idx)? as u32;
                // Clone the Arc<ClosureSlot> so we can release the
                // mutable borrow on `memory.closures` before re-entering
                // dispatch. Phase D: body resolution routes through the
                // shared `closures_pool` (= top-level closure_bodies),
                // not `func.closure_bodies` — so a self-recursive call
                // from inside a closure body (whose own
                // `closure_bodies` is empty) still finds the lambda.
                let slot: Arc<ClosureSlot> = memory
                    .closures
                    .get(handle)
                    .map_err(arena_to_vm_error)?
                    .clone();
                let body = closures_pool
                    .get(slot.body_idx as usize)
                    .ok_or(BcVmError::StackUnderflow { bc_idx })?;
                let ret =
                    self.invoke_closure_body(body, &args, &slot.captures, memory, closures_pool)?;
                stack.push(ret);
            }
            BcOp::CaptureGet { idx } => {
                // M3: push the value at `idx` of the currently
                // executing closure's captures vector. Outside a
                // closure body (current_captures is None) is a compiler
                // bug — surface as StackUnderflow.
                let caps = current_captures.ok_or(BcVmError::StackUnderflow { bc_idx })?;
                let i = *idx as usize;
                if i >= caps.len() {
                    return Err(BcVmError::StackUnderflow { bc_idx });
                }
                stack.push(caps[i]);
            }
            BcOp::Trap(kind) => match kind {
                BcTrapKind::IndexOutOfBounds => return Err(BcVmError::IndexOutOfBounds),
                BcTrapKind::EmptyList => return Err(BcVmError::EmptyList),
                BcTrapKind::InvalidUtf8 => return Err(BcVmError::InvalidUtf8),
                BcTrapKind::CapabilityDenied => {
                    // M2-B phase 2: when a `CapabilityGate` is
                    // installed, consult it for the first denied
                    // grant-table bit so the surfaced `cap_bit`
                    // matches the policy authority rather than the
                    // legacy `u32::MAX` sentinel. The sentinel is
                    // preserved when no gate is installed (legacy
                    // behaviour) so existing tests / hand-built
                    // BcFunctions don't observe a change.
                    //
                    // Phase 3 will replace this BC-level static trap
                    // with `BcOp::CallNative`-driven per-site
                    // consults; phase 2's role is to prove the gate
                    // is reachable from the dispatch path and to
                    // standardise the error envelope.
                    let cap_bit = match self.config.cap_vtable.gate() {
                        Some(gate) => first_denied_bit(gate).unwrap_or(u32::MAX),
                        None => u32::MAX,
                    };
                    return Err(BcVmError::CapabilityDenied { cap_bit });
                }
            },
        }
        Ok(StepOutcome::Advance)
    }
}

enum StepOutcome {
    Advance,
    Jump(usize),
    Return(VmValue),
}

#[inline(always)]
fn pop(stack: &mut Vec<VmValue>, bc_idx: usize) -> Result<VmValue, BcVmError> {
    // ok_or_else defers the error struct construction to the cold
    // path — the success arm (every hot arith / cmp pop) skips it.
    stack.pop().ok_or(BcVmError::StackUnderflow { bc_idx })
}

/// M2-B phase 4b: lift an [`ArenaError`] into the dispatch-side
/// [`BcVmError`]. `OutOfRange` (compiler bug — handle the arena never
/// minted) maps to the same `IndexOutOfBounds` envelope as
/// `ElementOutOfRange` for now; both surface as
/// `RuntimeError::WasmIndexOutOfBounds` after the lift, which keeps
/// the four-way differential harness's bounce shape stable. Phase
/// 4b-continuation can widen the carrier if we ever want to
/// distinguish "compiler bug" from "runtime trap" at the public
/// surface.
fn arena_to_vm_error(err: ArenaError) -> BcVmError {
    match err {
        ArenaError::OutOfRange { .. } | ArenaError::ElementOutOfRange { .. } => {
            BcVmError::IndexOutOfBounds
        }
    }
}

/// Discover the IR PC the VM was sitting on right before it tripped
/// — used by partial-resume and diagnostics. M2-A surface keeps the
/// PC accessible without leaking the bytecode-side index.
pub fn ir_pc_at(func: &BcFunction, bc_idx: usize) -> ExternalPc {
    func.ir_pc_map.get(bc_idx).copied().unwrap_or(0)
}
