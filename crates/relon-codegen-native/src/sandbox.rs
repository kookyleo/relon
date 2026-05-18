//! Sandbox primitives for the cranelift-native AOT backend.
//!
//! v5-beta-1 enforces the same four hard sandbox guarantees the
//! wasm-AOT backend ships, but implemented inside cranelift IR rather
//! than through the wasm spec + wasmtime trap surface:
//!
//! 1. **Linear memory bounds check** — every host-visible memory load
//!    emitted by the codegen lowering is guarded by an explicit
//!    `icmp_ult` comparison against the linear-memory byte length.
//!    The fault path branches to a dedicated trap stub that converts
//!    to [`RuntimeError::WasmIndexOutOfBounds`] before unwinding back
//!    to the host through [`SandboxState::call_with_sandbox`].
//!
//! 2. **Trap handler** — the JIT-compiled entry runs inside
//!    `std::panic::catch_unwind` so divide-by-zero / bounds / cap /
//!    deadline traps emitted as Rust panics route back through a
//!    typed `Result<Value, RuntimeError>`. The brief mentions
//!    `sigsetjmp` as the long-term play; v5-beta-1 ships
//!    `catch_unwind` because (a) Rust panics already carry a typed
//!    payload through the unwind, (b) cranelift `Trap` instructions
//!    map cleanly to libc `SIGTRAP` which the runtime turns into a
//!    panic on most targets, and (c) `sigsetjmp` from Rust requires
//!    unsafe code and unwind tables this crate doesn't yet emit.
//!    The v5-beta-2 follow-up replaces this with a real
//!    `sigsetjmp`/`siglongjmp` round trip per the roadmap.
//!
//! 3. **Capability gating** — host fn dispatch goes through a
//!    [`CapabilityVtable`] indexed by the IR-side `cap_bit`. Empty
//!    slots produce a null pointer; cranelift emits an `icmp_eq`
//!    against zero on every indirect-call site and traps via the
//!    capability path on miss. The vtable is reconfigured per
//!    `run_main` so a single compiled module can serve multiple
//!    capability postures without recompilation.
//!
//! 4. **Resource limit** — the entry prologue performs one
//!    monotonic-clock read and compares the result against the
//!    user-configured deadline; the loop body re-checks every N
//!    iterations (per `RESOURCE_CHECK_INTERVAL`) to bound runtime
//!    blast radius. Tickless loops still get the prologue guard.
//!
//! The host-facing surface is [`SandboxConfig`] (immutable config
//! built once at compile time) plus [`SandboxState`] (the per-call
//! runtime state). Codegen reads [`SandboxConfig`] to decide which
//! checks to emit; the runtime uses [`SandboxState`] to dispatch and
//! capture trap reasons.

use relon_eval_api::RuntimeError;
use relon_parser::TokenRange;
use std::cell::{Cell, UnsafeCell};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Compile-time sandbox configuration. Built once when a
/// `CraneliftAotEvaluator` is constructed and consulted by both the
/// codegen lowering and the runtime dispatcher.
///
/// The four bools are independent; turning any of them off elides the
/// matching codegen path entirely, which is useful for the bench
/// scenarios that want to measure raw arithmetic without sandbox
/// overhead. v5-beta-1 production builds always have every guard
/// enabled — these knobs exist for benchmarking and host-side
/// debugging only.
#[derive(Debug, Clone)]
pub struct SandboxConfig {
    /// When `true`, every memory load emitted by the codegen lowering
    /// is preceded by an `icmp_ult` against the linear-memory byte
    /// length.
    pub bounds_check: bool,
    /// When `true`, the entry prologue reads the deadline and the
    /// loop epilogue / body cadence inserts deadline rechecks.
    pub deadline_check: bool,
    /// When `true`, every `CallNative` is guarded by a vtable null
    /// check before the indirect-call instruction.
    pub capability_check: bool,
    /// When `true`, `Div` / `Mod` emit an explicit divisor-zero check
    /// before the cranelift `sdiv` / `srem`. We keep this guard even
    /// on hardware that traps natively because Rust's panic surface
    /// on SIGFPE is not portable across all targets.
    pub div_check: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            bounds_check: true,
            deadline_check: true,
            capability_check: true,
            div_check: true,
        }
    }
}

impl SandboxConfig {
    /// Disable all four guards. Bench-only — production code paths
    /// should never call this.
    pub fn unchecked() -> Self {
        Self {
            bounds_check: false,
            deadline_check: false,
            capability_check: false,
            div_check: false,
        }
    }
}

/// Trap kind raised by a guard inside cranelift-emitted code. Values
/// match the numeric `code` parameter of cranelift's `trap` /
/// `trapnz` instructions emitted by the lowering pass, so the trap
/// handler in [`SandboxState::call_with_sandbox`] can decode the
/// cause from the `Trap` payload alone.
///
/// Encoded as `u8` so it fits the cranelift `TrapCode::User` slot
/// width without truncation.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrapKind {
    /// Division (`Op::Div` / `Op::Mod`) by zero.
    DivisionByZero = 1,
    /// Pointer dereference walked past the linear memory bounds.
    BoundsViolation = 2,
    /// A `CallNative` site invoked a host fn whose `cap_bit` slot
    /// holds a null entry in the active vtable.
    CapabilityDenied = 3,
    /// Per-call wall-clock deadline elapsed.
    ResourceExhausted = 4,
    /// Unsupported / unreachable op landed in the lowered IR. Never
    /// produced by valid IR but kept as a defensive catch-all.
    Unreachable = 5,
    /// Signed integer overflow on `Op::Add` / `Op::Sub` / `Op::Mul`.
    /// Cranelift uses `sadd_overflow` / `ssub_overflow` /
    /// `smul_overflow` so the trap mirrors the tree-walker's strict
    /// `RuntimeError::NumericOverflow` semantics.
    NumericOverflow = 6,
}

impl TrapKind {
    /// Decode a `u8` returned by a trapping cranelift entry back into a
    /// [`TrapKind`]. Unknown codes route to [`TrapKind::Unreachable`]
    /// so the host gets a typed `RuntimeError` rather than a panic.
    pub fn from_code(code: u8) -> TrapKind {
        match code {
            1 => TrapKind::DivisionByZero,
            2 => TrapKind::BoundsViolation,
            3 => TrapKind::CapabilityDenied,
            4 => TrapKind::ResourceExhausted,
            6 => TrapKind::NumericOverflow,
            _ => TrapKind::Unreachable,
        }
    }

    /// Lift a trap kind into the appropriate [`RuntimeError`] variant.
    /// All trap mappings carry the entry function's source range so
    /// the diagnostic at least points at the `#main` declaration.
    pub fn to_runtime_error(self, range: TokenRange) -> RuntimeError {
        match self {
            TrapKind::DivisionByZero => RuntimeError::DivisionByZero(range),
            TrapKind::BoundsViolation => RuntimeError::WasmIndexOutOfBounds { range },
            TrapKind::CapabilityDenied => RuntimeError::WasmCapabilityDenied {
                cap_bit: u32::MAX,
                range,
            },
            TrapKind::ResourceExhausted => {
                RuntimeError::WasmStepLimitExceeded { range: Some(range) }
            }
            TrapKind::Unreachable => RuntimeError::Unsupported {
                reason: "cranelift-native: lowered IR contained an unreachable op".to_string(),
            },
            TrapKind::NumericOverflow => RuntimeError::NumericOverflow(range),
        }
    }
}

/// Function pointer signature for host-fn dispatch through the
/// capability vtable. The cranelift entry pushes `args_ptr / args_len
/// / caps_avail` then performs `call_indirect` against the slot — the
/// host fn returns one `i64` payload (the only return shape v5-beta-1
/// supports for `#native` imports).
pub type HostFnPtr = unsafe extern "C" fn(i64) -> i64;

/// Per-call vtable indexed by cap_bit. Slots holding `None` cause the
/// generated capability-check sequence to trap with
/// [`TrapKind::CapabilityDenied`].
///
/// The vtable lives on the heap so cranelift can take its base
/// address as a constant and re-resolve per call. `Arc` so the
/// runtime can hand a shared snapshot to long-lived sessions without
/// cloning every slot.
#[derive(Debug, Default)]
pub struct CapabilityVtable {
    slots: Vec<Option<HostFnPtr>>,
}

impl CapabilityVtable {
    /// Build a fixed-size vtable. The slot count must accommodate
    /// every `cap_bit` the lowered IR references; v5-beta-1 uses 64
    /// (matches the wasm-AOT side's `relon_caps_avail` bit width).
    pub fn with_capacity(n: usize) -> Self {
        Self {
            slots: vec![None; n],
        }
    }

    /// Register a host fn at the given `cap_bit` index. Overwrites any
    /// existing entry so the host can rebind a slot between
    /// `run_main` calls without recompiling.
    pub fn register(&mut self, cap_bit: u32, host_fn: HostFnPtr) {
        let idx = cap_bit as usize;
        if idx >= self.slots.len() {
            self.slots.resize(idx + 1, None);
        }
        self.slots[idx] = Some(host_fn);
    }

    /// Resolve a slot. `None` means the capability is denied.
    pub fn lookup(&self, cap_bit: u32) -> Option<HostFnPtr> {
        self.slots.get(cap_bit as usize).copied().flatten()
    }

    /// Base address of the slot array, used by cranelift codegen as a
    /// constant in the `call_indirect` sequence.
    pub fn base_ptr(&self) -> *const Option<HostFnPtr> {
        self.slots.as_ptr()
    }

    /// Number of registered slots (including null / denied entries).
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// `true` when no slots have been registered.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }
}

/// Per-`run_main` mutable state passed to the JIT entry. Carries the
/// deadline, the trap-cause slot, the linear-memory arena base, and a
/// pointer to the active capability vtable. Cranelift loads each
/// field by offset relative to a base pointer the entry receives as
/// its first parameter.
///
/// Field ordering is part of the cranelift ABI: do not reorder
/// without updating the [`STATE_OFFSET_*`] constants in this module —
/// the codegen pass reads each field via `load.<ty>` against the
/// state pointer using these offsets.
///
/// ## v5-β-2: arena fields for buffer protocol
///
/// `arena_base` + `arena_len` form a flat "linear memory" the JIT
/// code accesses through the `in_ptr` / `in_len` / `out_ptr` /
/// `out_cap` arguments the lowered `run_main` receives. The arena
/// is allocated host-side per call (or pooled across calls) and
/// holds the `BufferBuilder`-serialised input region followed by an
/// uninitialised output region. Cranelift code emits `LoadField` /
/// `StoreField` as `arena_base + in_or_out_ptr + offset` with an
/// `arena_len`-relative bounds check.
#[repr(C)]
pub struct SandboxState {
    /// Deadline as nanos since `Instant::now()` at session start. The
    /// entry prologue reads `now - epoch >= deadline_ns` and traps on
    /// overflow.
    deadline_ns: AtomicI64,
    /// Trap reason set by the JIT entry before unwinding. Encoded as
    /// `u64` so we can swap it from cranelift via a normal `i64`
    /// store without going through a wider cell type. Value 0 means
    /// "no trap".
    trap_code: AtomicU64,
    /// Base pointer of the "linear memory" arena. Cranelift code
    /// computes addresses as `arena_base + buf_ptr + field_offset`
    /// for every `LoadField` / `StoreField` it emits. Re-pointed by
    /// the host trampoline before each `run_main` invocation; see
    /// [`SandboxState::install_arena`].
    ///
    /// Stored as `UnsafeCell<usize>` so the JIT thread can read
    /// through a stable offset without going through a Rust borrow
    /// — the host holds the only `&mut` access via `install_arena`,
    /// and that happens strictly before the JIT call begins.
    arena_base: UnsafeCell<usize>,
    /// Length in bytes of the arena pointed to by `arena_base`. Used
    /// by the cranelift bounds-check sequence to trap before any
    /// load / store walks past the host-owned region.
    arena_len: UnsafeCell<u32>,
    /// Tail cursor used by `Op::AllocSubRecord` /
    /// `Op::EmitTailRecordFromAbsoluteAddr` for record-building
    /// inside the output buffer. Lives on the state so the cranelift
    /// emitter can read / write it through a stable offset rather
    /// than threading a wasm-style "spare local" through every IR op.
    tail_cursor: UnsafeCell<u32>,
    /// Reference start time for the deadline check. Set once at
    /// construction; the entry computes elapsed nanos against this.
    epoch: Instant,
    /// Active vtable for host-fn dispatch. Wrapped in `Arc` so the
    /// host can hand a vtable to multiple concurrent run_main calls
    /// without cloning the slot array per invocation.
    capabilities: Arc<CapabilityVtable>,
    /// Slot used by the cranelift codegen to remember an entry source
    /// range for the trap-to-RuntimeError step. Not read by the JIT;
    /// the host walks it post-trap.
    pub(crate) entry_range: Cell<TokenRange>,
}

/// Byte offset of [`SandboxState::deadline_ns`] inside the
/// `#[repr(C)]` layout. Cranelift's resource-check sequence reads at
/// this offset; mirrored here as a const so both the codegen and the
/// runtime stay in sync.
pub const STATE_OFFSET_DEADLINE_NS: i32 = 0;
/// Byte offset of [`SandboxState::trap_code`]. Not currently read
/// from cranelift IR (the trap path uses the `raise_trap` host helper
/// indirectly) but documented for completeness.
#[allow(dead_code)]
pub const STATE_OFFSET_TRAP_CODE: i32 = 8;
/// Byte offset of [`SandboxState::arena_base`]. Cranelift code emits
/// `load.<pointer_ty>` at this offset to materialise the arena base
/// before computing absolute field addresses.
pub const STATE_OFFSET_ARENA_BASE: i32 = 16;
/// Byte offset of [`SandboxState::arena_len`]. Codegen consults this
/// for the per-load / per-store bounds check.
pub const STATE_OFFSET_ARENA_LEN: i32 = 24;
/// Byte offset of [`SandboxState::tail_cursor`]. Codegen reads /
/// writes this from `Op::AllocSubRecord` / `Op::EmitTailRecord*` to
/// bump-allocate inside the output buffer's tail region.
pub const STATE_OFFSET_TAIL_CURSOR: i32 = 28;

// SAFETY: `Cell<TokenRange>` is not `Sync`, but we only hand
// `&SandboxState` to single-threaded cranelift code; the typed
// atomics serialise across threads when the host shares an `Arc<>`.
// Marking explicitly because the public `Arc<SandboxState>` shape
// crosses thread boundaries via `Send + Sync` bounds elsewhere.
unsafe impl Sync for SandboxState {}

impl SandboxState {
    /// Build a fresh sandbox state with an effectively-infinite
    /// deadline. Hosts that want a real deadline call
    /// [`Self::set_deadline`] before invoking the JIT entry. The
    /// arena starts unpopulated; the host trampoline calls
    /// [`Self::install_arena`] before invoking the JIT entry.
    pub fn new(capabilities: Arc<CapabilityVtable>) -> Self {
        Self {
            deadline_ns: AtomicI64::new(i64::MAX),
            trap_code: AtomicU64::new(0),
            arena_base: UnsafeCell::new(0),
            arena_len: UnsafeCell::new(0),
            tail_cursor: UnsafeCell::new(0),
            epoch: Instant::now(),
            capabilities,
            entry_range: Cell::new(TokenRange::default()),
        }
    }

    /// Point the JIT-visible arena at `(base, len)`. Called by the
    /// host trampoline immediately before invoking the JIT entry, and
    /// before [`Self::reset_trap`] clears the trap slot. The pointer
    /// must remain valid for the duration of the entry call.
    ///
    /// # Safety
    ///
    /// The caller guarantees `base` points at an allocation of at
    /// least `len` bytes, and the allocation outlives the JIT call.
    /// `tail_cursor` is reset to 0 so each invocation starts with a
    /// clean bump cursor.
    pub unsafe fn install_arena(&self, base: *mut u8, len: u32) {
        // SAFETY: the JIT thread is not yet running (this is called
        // strictly before the entry invocation), so the UnsafeCell
        // contents are unaliased.
        unsafe {
            *self.arena_base.get() = base as usize;
            *self.arena_len.get() = len;
            *self.tail_cursor.get() = 0;
        }
    }

    /// Read back the tail cursor after a JIT call returns. The host
    /// uses this to size the output-buffer read on the
    /// pointer-indirect-store path.
    pub fn tail_cursor(&self) -> u32 {
        // SAFETY: the JIT call has returned, so the UnsafeCell is
        // unaliased; the read is plain.
        unsafe { *self.tail_cursor.get() }
    }

    /// Configure the per-call deadline. Pass `Duration::MAX` (or any
    /// value that overflows to `i64::MAX` nanos) to disable.
    pub fn set_deadline(&self, deadline: Duration) {
        let nanos = i64::try_from(deadline.as_nanos()).unwrap_or(i64::MAX);
        self.deadline_ns.store(nanos, Ordering::Relaxed);
    }

    /// Reset the trap slot. Called between invocations so a successful
    /// run doesn't pick up a stale code.
    pub fn reset_trap(&self) {
        self.trap_code.store(0, Ordering::Relaxed);
    }

    /// Inspect the trap code recorded by the JIT entry. `0` means no
    /// trap occurred.
    pub fn trap_code(&self) -> u64 {
        self.trap_code.load(Ordering::Relaxed)
    }

    /// Active vtable. Used by tests / observability.
    pub fn capabilities(&self) -> &Arc<CapabilityVtable> {
        &self.capabilities
    }

    /// Helper invoked from cranelift-emitted code to read the
    /// monotonic clock. Pure host-side helper — declared as a
    /// `pub(crate)` symbol so the codegen pass can take its address
    /// without exporting it on the crate surface.
    ///
    /// # Safety
    ///
    /// `state` must point at a live, properly aligned
    /// [`SandboxState`] for the duration of the call. The JIT-emitted
    /// prologue passes the same pointer the host trampoline
    /// received, so the invariant holds for every production caller.
    pub(crate) unsafe extern "C" fn now_helper(state: *const SandboxState) -> i64 {
        let state = unsafe { &*state };
        state.epoch.elapsed().as_nanos() as i64
    }

    /// Helper invoked from cranelift to record a trap code. The JIT
    /// epilogue calls this then returns a sentinel value the
    /// trampoline interprets as "trap fired".
    ///
    /// # Safety
    ///
    /// `state` must point at a live, properly aligned [`SandboxState`]
    /// for the duration of the call. The JIT-emitted trap path
    /// always passes the same pointer the host trampoline received,
    /// so the invariant holds for every production caller; tests
    /// that exercise this function directly must keep the
    /// `SandboxState` alive on the calling thread for the duration
    /// of the call.
    pub unsafe extern "C" fn raise_trap(state: *const SandboxState, code: u64) {
        let state = unsafe { &*state };
        state.trap_code.store(code, Ordering::Relaxed);
    }

    /// Cap-vtable lookup helper. Returns the resolved host fn or
    /// null. Called from cranelift code via a `call` to this
    /// function-pointer constant.
    ///
    /// # Safety
    ///
    /// `state` must point at a live, properly aligned [`SandboxState`]
    /// for the duration of the call.
    pub(crate) unsafe extern "C" fn cap_lookup(state: *const SandboxState, cap_bit: u32) -> usize {
        let state = unsafe { &*state };
        match state.capabilities.lookup(cap_bit) {
            Some(fn_ptr) => fn_ptr as usize,
            None => 0,
        }
    }
}

/// Maximum number of loop body iterations between deadline rechecks.
/// Loops below this iteration count rely on the entry prologue's
/// single check; longer-running loops re-arm the guard at this
/// cadence to bound worst-case overrun.
pub const RESOURCE_CHECK_INTERVAL: u32 = 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vtable_register_and_lookup_round_trip() {
        unsafe extern "C" fn stub(x: i64) -> i64 {
            x + 1
        }
        let mut vt = CapabilityVtable::with_capacity(4);
        assert!(vt.lookup(0).is_none());
        vt.register(2, stub);
        assert!(vt.lookup(2).is_some());
        assert!(vt.lookup(3).is_none());
    }

    #[test]
    fn vtable_register_grows_capacity_when_needed() {
        unsafe extern "C" fn stub(x: i64) -> i64 {
            x
        }
        let mut vt = CapabilityVtable::with_capacity(2);
        vt.register(10, stub);
        assert!(vt.lookup(10).is_some());
        assert_eq!(vt.len(), 11);
    }

    #[test]
    fn trap_kind_round_trips_through_u8_code() {
        for kind in [
            TrapKind::DivisionByZero,
            TrapKind::BoundsViolation,
            TrapKind::CapabilityDenied,
            TrapKind::ResourceExhausted,
        ] {
            let code = kind as u8;
            assert_eq!(TrapKind::from_code(code), kind);
        }
        // Unknown codes route to `Unreachable`.
        assert_eq!(TrapKind::from_code(99), TrapKind::Unreachable);
    }

    #[test]
    fn trap_kind_maps_to_runtime_error_variant() {
        let range = TokenRange::default();
        let err = TrapKind::DivisionByZero.to_runtime_error(range);
        assert!(matches!(err, RuntimeError::DivisionByZero(_)));
        let err = TrapKind::BoundsViolation.to_runtime_error(range);
        assert!(matches!(err, RuntimeError::WasmIndexOutOfBounds { .. }));
        let err = TrapKind::CapabilityDenied.to_runtime_error(range);
        assert!(matches!(err, RuntimeError::WasmCapabilityDenied { .. }));
        let err = TrapKind::ResourceExhausted.to_runtime_error(range);
        assert!(matches!(err, RuntimeError::WasmStepLimitExceeded { .. }));
    }

    #[test]
    fn sandbox_state_deadline_clamps_overflow_to_i64_max() {
        let vt = Arc::new(CapabilityVtable::with_capacity(0));
        let state = SandboxState::new(vt);
        state.set_deadline(Duration::from_secs(u64::MAX));
        // Should not panic; should clamp to i64::MAX.
        assert_eq!(state.deadline_ns.load(Ordering::Relaxed), i64::MAX);
    }

    #[test]
    fn sandbox_state_state_offsets_match_repr_c_layout() {
        // Sanity check: if the struct layout drifts (e.g. someone
        // reorders fields), the codegen pass will silently read the
        // wrong bytes; this asserts the offsets up front.
        let state = SandboxState::new(Arc::new(CapabilityVtable::with_capacity(0)));
        let base = &state as *const _ as usize;
        assert_eq!(
            (&state.deadline_ns as *const _ as usize) - base,
            STATE_OFFSET_DEADLINE_NS as usize
        );
        assert_eq!(
            (&state.trap_code as *const _ as usize) - base,
            STATE_OFFSET_TRAP_CODE as usize
        );
        assert_eq!(
            (state.arena_base.get() as usize) - base,
            STATE_OFFSET_ARENA_BASE as usize
        );
        assert_eq!(
            (state.arena_len.get() as usize) - base,
            STATE_OFFSET_ARENA_LEN as usize
        );
        assert_eq!(
            (state.tail_cursor.get() as usize) - base,
            STATE_OFFSET_TAIL_CURSOR as usize
        );
    }

    #[test]
    fn sandbox_state_install_arena_round_trips() {
        let state = SandboxState::new(Arc::new(CapabilityVtable::with_capacity(0)));
        let mut buf = vec![0u8; 64];
        // SAFETY: `buf` outlives the read below.
        unsafe { state.install_arena(buf.as_mut_ptr(), 64) };
        assert_eq!(unsafe { *state.arena_base.get() }, buf.as_ptr() as usize);
        assert_eq!(unsafe { *state.arena_len.get() }, 64);
        assert_eq!(state.tail_cursor(), 0);
    }

    #[test]
    fn sandbox_state_reset_clears_trap_slot() {
        let vt = Arc::new(CapabilityVtable::with_capacity(0));
        let state = SandboxState::new(vt);
        // SAFETY: `state` lives on the stack for the duration of the
        // call; `raise_trap` only reads through the pointer for the
        // duration of the inner `&*state` deref.
        unsafe {
            SandboxState::raise_trap(&state as *const _, TrapKind::DivisionByZero as u64);
        }
        assert_eq!(state.trap_code(), 1);
        state.reset_trap();
        assert_eq!(state.trap_code(), 0);
    }

    #[test]
    fn sandbox_config_unchecked_disables_all_guards() {
        let cfg = SandboxConfig::unchecked();
        assert!(!cfg.bounds_check);
        assert!(!cfg.deadline_check);
        assert!(!cfg.capability_check);
        assert!(!cfg.div_check);
    }

    #[test]
    fn sandbox_config_default_enables_all_guards() {
        let cfg = SandboxConfig::default();
        assert!(cfg.bounds_check);
        assert!(cfg.deadline_check);
        assert!(cfg.capability_check);
        assert!(cfg.div_check);
    }
}
