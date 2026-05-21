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
//!    to the host through the JIT entry's `catch_unwind` boundary.
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
//!    capability postures without recompilation. Slot population is
//!    funnelled through the [`relon_eval_api::CapabilityGate`] trait
//!    (see [`CapabilityVtable::register_via_gate`]) so the cranelift
//!    backend's policy decision and the tree-walker's dispatch-time
//!    check share a single source of truth — the enforcement timing
//!    differs (build-time for cranelift, dispatch-time for the
//!    tree-walker) but the policy bit consulted is the same.
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
    /// v6-γ M2: when `Some(fn_id)`, the codegen emits a HotCounter
    /// prologue at the entry block. Every invocation atomically (per
    /// design § 3, non-atomic) bumps slot `fn_id` of the global
    /// `__relon_hot_counters` table; when the slot crosses
    /// [`crate::trace_install::RELON_HOT_THRESHOLD`], the prologue
    /// calls [`crate::trace_install::__relon_jump_to_recorder`] and
    /// returns a sentinel so the host can install a JIT-compiled
    /// trace fn.
    ///
    /// `None` (default) elides the prologue entirely — backward-
    /// compatible with the v5-γ stage 2 code path.
    pub trace_jit_fn_id: Option<u32>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            bounds_check: true,
            deadline_check: true,
            capability_check: true,
            div_check: true,
            trace_jit_fn_id: None,
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
            trace_jit_fn_id: None,
        }
    }
}

/// Trap kind raised by a guard inside cranelift-emitted code. Values
/// match the numeric `code` parameter of cranelift's `trap` /
/// `trapnz` instructions emitted by the lowering pass, so the trap
/// handler at the JIT entry's `catch_unwind` boundary can decode the
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

    /// Capability-gated registration. Consults `gate` for `cap_bit`
    /// via the shared [`relon_eval_api::CapabilityGate`] trait; if the
    /// gate denies the bit, the slot stays `None` so the IR-level
    /// `cap_lookup` returns null and the call traps with
    /// `TrapKind::CapabilityDenied`. This is the cranelift backend's
    /// half of the unified-enforcement design: the same policy that
    /// the tree-walker consults at dispatch time is consulted here at
    /// vtable-build time, so denying a bit on the host side produces
    /// the same outcome class (`RuntimeError::*CapabilityDenied`) on
    /// both backends.
    ///
    /// Returns `true` if the slot was populated; `false` if the gate
    /// denied the bit (slot left `None`).
    pub fn register_via_gate<G: relon_eval_api::CapabilityGate>(
        &mut self,
        gate: &G,
        cap_bit: relon_eval_api::CapabilityBit,
        host_fn: HostFnPtr,
    ) -> bool {
        match gate.check(cap_bit) {
            Ok(()) => {
                self.register(cap_bit.bit_index(), host_fn);
                true
            }
            Err(_) => false,
        }
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
/// without updating the `STATE_OFFSET_*` constants in this module —
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
    /// Scratch bump cursor used by stdlib bodies that allocate
    /// temporary records (memory stdlib `concat` / `substring` etc.)
    /// inside the arena's scratch region. Counts bytes from
    /// `scratch_base`. Reset by `install_arena` to 0 so each call
    /// starts with a clean scratch heap.
    scratch_cursor: UnsafeCell<u32>,
    /// Arena-relative byte offset at which the scratch region starts.
    /// Always equals `out_ptr + out_cap` for the current call. The
    /// cranelift `Op::AllocScratch` / `Op::AllocScratchDyn` sequence
    /// reads this to produce `scratch_base + scratch_cursor` as the
    /// arena-relative i32 pointer returned to the stdlib body.
    scratch_base: UnsafeCell<u32>,
    /// Pointer to the start of the closure-table: an array of host-
    /// address-sized fn pointers, indexed by the `fn_table_idx` field
    /// of the per-closure handle. The cranelift `Op::CallClosure`
    /// sequence reads `closure_table_base + sizeof(usize) *
    /// fn_table_idx` to materialise the lambda's host fn pointer and
    /// `call_indirect`s through it.
    ///
    /// `0` when the module has no lambda funcs; the cranelift code
    /// never reads through it in that shape because no `Op::CallClosure`
    /// is emitted.
    ///
    /// Stored as `UnsafeCell<usize>` because the host populates the
    /// table after JIT finalize but before any JIT call; once the
    /// table is installed it remains valid for the SandboxState's
    /// lifetime.
    closure_table_base: UnsafeCell<usize>,
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

/// Byte offset of `SandboxState::deadline_ns` inside the
/// `#[repr(C)]` layout. Cranelift's resource-check sequence reads at
/// this offset; mirrored here as a const so both the codegen and the
/// runtime stay in sync.
pub const STATE_OFFSET_DEADLINE_NS: i32 = 0;
/// Byte offset of `SandboxState::trap_code`. Not currently read
/// from cranelift IR (the trap path uses the `raise_trap` host helper
/// indirectly) but documented for completeness.
#[allow(dead_code)]
pub const STATE_OFFSET_TRAP_CODE: i32 = 8;
/// Byte offset of `SandboxState::arena_base`. Cranelift code emits
/// `load.<pointer_ty>` at this offset to materialise the arena base
/// before computing absolute field addresses.
pub const STATE_OFFSET_ARENA_BASE: i32 = 16;
/// Byte offset of `SandboxState::arena_len`. Codegen consults this
/// for the per-load / per-store bounds check.
pub const STATE_OFFSET_ARENA_LEN: i32 = 24;
/// Byte offset of `SandboxState::tail_cursor`. Codegen reads /
/// writes this from `Op::AllocSubRecord` / `Op::EmitTailRecord*` to
/// bump-allocate inside the output buffer's tail region.
pub const STATE_OFFSET_TAIL_CURSOR: i32 = 28;
/// Byte offset of `SandboxState::scratch_cursor`. Codegen uses this
/// for the `Op::AllocScratch` / `Op::AllocScratchDyn` bump path the
/// memory stdlib (`concat` / `substring` / …) relies on.
pub const STATE_OFFSET_SCRATCH_CURSOR: i32 = 32;
/// Byte offset of `SandboxState::scratch_base`. The arena-relative
/// start of the scratch region; the cranelift bump-allocator returns
/// `scratch_base + scratch_cursor` as the i32 pointer the stdlib body
/// then dereferences via `LoadI32AtAbsolute` / `MemcpyAtAbsolute` etc.
pub const STATE_OFFSET_SCRATCH_BASE: i32 = 36;
/// Byte offset of `SandboxState::closure_table_base`. The host
/// installs the per-module closure fn pointer table here after JIT
/// finalize; cranelift `Op::CallClosure` reads through this address
/// `+ sizeof(usize) * fn_table_idx` to materialise the lambda's host
/// fn pointer.
pub const STATE_OFFSET_CLOSURE_TABLE_BASE: i32 = 40;

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
            scratch_cursor: UnsafeCell::new(0),
            scratch_base: UnsafeCell::new(0),
            closure_table_base: UnsafeCell::new(0),
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
            *self.scratch_cursor.get() = 0;
            *self.scratch_base.get() = 0;
        }
    }

    /// Set the arena-relative start of the scratch region. The
    /// trampoline calls this immediately after `install_arena` (and
    /// before invoking the JIT entry) so the scratch bump allocator
    /// reads a meaningful base. Setting it separately keeps
    /// `install_arena`'s `(base, len)` envelope source-of-truth for
    /// "what the JIT sees as linear memory" while the scratch base is
    /// a derived address chosen by the trampoline based on the input
    /// / output buffer layout.
    ///
    /// # Safety
    ///
    /// Must be called strictly before the JIT entry begins running.
    pub unsafe fn install_scratch_base(&self, scratch_base: u32) {
        unsafe {
            *self.scratch_base.get() = scratch_base;
        }
    }

    /// Point the JIT-visible closure table at `table_base` — the
    /// address of an array of host-fn pointers, indexed by the
    /// `fn_table_idx` field of a closure handle. The cranelift
    /// `Op::CallClosure` lowering reads through this address.
    ///
    /// `0` means "no closure table" (the module has no lambdas).
    /// Hosts install the address once per evaluator construction,
    /// after JIT finalize resolves the lambda func pointers.
    ///
    /// # Safety
    ///
    /// `table_base` must point at a properly aligned `usize` array of
    /// length at least `closure_table.len()` from the IR module the
    /// JIT entry was compiled against. The pointer must remain valid
    /// for the SandboxState's lifetime; once installed the host must
    /// not reallocate / mutate the table.
    pub unsafe fn install_closure_table(&self, table_base: usize) {
        unsafe {
            *self.closure_table_base.get() = table_base;
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

    /// Read the current arena base pointer as a raw `usize`.
    ///
    /// Used by host helpers that need to resolve an arena-relative
    /// i32 offset to an absolute address (e.g. the `glob_match`
    /// helper that takes two `String` operands as offsets and reads
    /// the wasm-style `[len][bytes]` records out of the arena).
    ///
    /// Returns `0` when no arena is installed.
    pub fn arena_base(&self) -> usize {
        // SAFETY: helpers are only invoked while the JIT thread is
        // executing host code on behalf of a still-live JIT entry —
        // `install_arena` has already published a value, and no other
        // writer is racing.
        unsafe { *self.arena_base.get() }
    }

    /// Read the current arena length (`install_arena`'s `len`
    /// argument). Companion to [`Self::arena_base`].
    pub fn arena_len(&self) -> u32 {
        // SAFETY: see `arena_base`.
        unsafe { *self.arena_len.get() }
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
        assert_eq!(
            (state.scratch_cursor.get() as usize) - base,
            STATE_OFFSET_SCRATCH_CURSOR as usize
        );
        assert_eq!(
            (state.scratch_base.get() as usize) - base,
            STATE_OFFSET_SCRATCH_BASE as usize
        );
        assert_eq!(
            (state.closure_table_base.get() as usize) - base,
            STATE_OFFSET_CLOSURE_TABLE_BASE as usize
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
    fn register_via_gate_denies_when_capability_not_granted() {
        unsafe extern "C" fn stub(x: i64) -> i64 {
            x
        }
        let caps = relon_eval_api::Capabilities::default();
        let mut vt = CapabilityVtable::with_capacity(8);
        // `reads_fs` not granted in the default snapshot — slot stays null.
        let populated = vt.register_via_gate(&caps, relon_eval_api::CapabilityBit::ReadsFs, stub);
        assert!(!populated, "denied gate must leave slot unpopulated");
        assert!(vt.lookup(0).is_none());
    }

    #[test]
    fn register_via_gate_populates_when_capability_granted() {
        unsafe extern "C" fn stub(x: i64) -> i64 {
            x
        }
        let caps = relon_eval_api::Capabilities::all_granted();
        let mut vt = CapabilityVtable::with_capacity(8);
        let populated = vt.register_via_gate(&caps, relon_eval_api::CapabilityBit::Network, stub);
        assert!(populated, "granted gate must populate the slot");
        assert!(vt
            .lookup(relon_eval_api::CapabilityBit::Network.bit_index())
            .is_some());
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
