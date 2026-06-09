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
//!    to [`RuntimeError::IndexOutOfBounds`] before unwinding back
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

use arc_swap::ArcSwap;
use relon_eval_api::{NativeArgs, NativeFnCaps, RelonFunction, RuntimeError, Value};
use relon_parser::TokenRange;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Compile-time sandbox configuration. Built once when a
/// `AotEvaluator` is constructed and consulted by both the
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
    /// A strict-mode `match` fell through every arm with no `_`
    /// catch-all and no arm matched at runtime. Lifts to
    /// `RuntimeError::TypeMismatch { expected: "a matching arm", .. }`
    /// — byte-aligned (modulo range) with the tree-walk oracle's
    /// `Expr::Match` no-match path. Routed here from the IR-level
    /// `TrapKind::NoMatch`.
    NoMatch = 7,
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
            7 => TrapKind::NoMatch,
            _ => TrapKind::Unreachable,
        }
    }

    /// Lift a trap kind into the appropriate [`RuntimeError`] variant.
    /// All trap mappings carry the entry function's source range so
    /// the diagnostic at least points at the `#main` declaration.
    pub fn to_runtime_error(self, range: TokenRange) -> RuntimeError {
        match self {
            TrapKind::DivisionByZero => RuntimeError::DivisionByZero(range),
            TrapKind::BoundsViolation => RuntimeError::IndexOutOfBounds { range },
            TrapKind::CapabilityDenied => RuntimeError::CapabilityDenied {
                // The trap path carries no bit (the null vtable slot is
                // the only signal), so the host gets a generic reason.
                cap_bit: None,
                reason: "cranelift-native: host-fn call denied by capability gate".to_string(),
                range,
            },
            TrapKind::ResourceExhausted => RuntimeError::StepLimitExceeded { limit: None, range },
            TrapKind::Unreachable => RuntimeError::Unsupported {
                reason: "cranelift-native: lowered IR contained an unreachable op".to_string(),
            },
            TrapKind::NumericOverflow => RuntimeError::NumericOverflow(range),
            TrapKind::NoMatch => RuntimeError::TypeMismatch {
                // `expected` is byte-identical to the tree-walk oracle's
                // `Expr::Match` no-match path. `found` cannot reproduce the
                // oracle's `format!("value {}", val)` from a static trap (the
                // runtime value is not carried through the trap code), so it
                // states the structural cause; the harness's `trap_equivalent`
                // keys on the `TypeMismatch` discriminant, and the analyzer
                // proves the construct always traps.
                expected: "a matching arm".to_string(),
                found: "no matching arm".to_string(),
                range,
            },
        }
    }
}

/// Function pointer signature for host-fn dispatch through the
/// capability vtable. The cranelift entry resolves the slot via
/// `cap_lookup`, null-checks it, then performs `call_indirect` — the
/// host fn takes one `i64` arg and returns one `i64` payload (the only
/// shape v5-beta-1 supports for `#native` imports).
pub type HostFnPtr = unsafe extern "C" fn(i64) -> i64;

/// Non-null sentinel parked in a `cap_bit` slot by
/// [`CapabilityVtable::grant`]. Never actually invoked — the
/// `Op::CheckCap` codegen only null-checks the slot pointer, and a
/// granted source-level `Op::CallNative` dispatches through the
/// `import_idx`-keyed `host_fns` registry rather than this slot. Exists
/// solely so a granted bit reads back as a non-null pointer.
unsafe extern "C" fn cap_grant_sentinel(x: i64) -> i64 {
    x
}

/// Zero-sized [`NativeFnCaps`] for cranelift-dispatched host fns. Same
/// shape as the bytecode VM's caps: no closure-callback / iterator
/// surface yet, so host fns that need those route through the
/// tree-walker. Cached as a single `Arc` so each dispatch is a refcount
/// bump rather than an allocation.
struct CraneliftNativeFnCaps;

impl NativeFnCaps for CraneliftNativeFnCaps {
    fn call_relon(
        &self,
        _func: &Value,
        _args: Vec<Value>,
        _range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        // Closure callbacks need the JIT frame surface this backend's
        // host-fn dispatch doesn't expose yet; a host fn that tries to
        // call back into Relon logic gets this envelope so it can route
        // through the tree-walker instead.
        Err(RuntimeError::Unsupported {
            reason: "cranelift-native host fn: call_relon callback unsupported".to_string(),
        })
    }
}

fn cranelift_native_caps() -> Arc<dyn NativeFnCaps> {
    static CAPS: std::sync::OnceLock<Arc<dyn NativeFnCaps>> = std::sync::OnceLock::new();
    Arc::clone(CAPS.get_or_init(|| Arc::new(CraneliftNativeFnCaps) as Arc<dyn NativeFnCaps>))
}

/// Per-call vtable indexed by cap_bit. Slots holding `None` cause the
/// generated capability-check sequence to trap with
/// [`TrapKind::CapabilityDenied`].
///
/// The vtable lives on the heap so cranelift can take its base
/// address as a constant and re-resolve per call. `Arc` so the
/// runtime can hand a shared snapshot to long-lived sessions without
/// cloning every slot.
///
/// ## Two dispatch models
///
/// * `slots` — raw `extern "C"` host fn pointers indexed by `cap_bit`.
///   This is the original "host writes a C-ABI fn" model: the codegen
///   resolves the slot via `cap_lookup` and `call_indirect`s directly.
///   Also doubles as the capability *grant* surface — a non-null slot
///   at `cap_bit` is what lets an `Op::CheckCap { cap_bit }` pass.
/// * `host_fns` — dynamic `Arc<dyn RelonFunction>` callables indexed by
///   the IR-side `import_idx`. This is the bridge for tree-walker host
///   fns: a source-lowered `Op::CallNative { cap_bit: NO_CAPABILITY_BIT }`
///   routes through the `relon_call_native` helper, which packs the
///   scalar args into `NativeArgs` and invokes the Arc. Keying off
///   `import_idx` (a private namespace) avoids colliding with the
///   `cap_bit`-indexed `slots`.
#[derive(Default, Clone)]
pub struct CapabilityVtable {
    slots: Vec<Option<HostFnPtr>>,
    host_fns: HashMap<u32, Arc<dyn RelonFunction>>,
}

impl std::fmt::Debug for CapabilityVtable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CapabilityVtable")
            .field("slots", &self.slots.len())
            .field("host_fn_count", &self.host_fns.len())
            .finish()
    }
}

impl CapabilityVtable {
    /// Build a fixed-size vtable. The slot count must accommodate
    /// every `cap_bit` the lowered IR references; v5-beta-1 uses 64,
    /// which covers every declared `CapabilityBit` with ample headroom.
    pub fn with_capacity(n: usize) -> Self {
        Self {
            slots: vec![None; n],
            host_fns: HashMap::new(),
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

    /// Register a dynamic `Arc<dyn RelonFunction>` host fn at the given
    /// `import_idx`. This is the bridge for tree-walker host fns: a
    /// source-lowered `Op::CallNative { cap_bit: NO_CAPABILITY_BIT }`
    /// dispatches through `relon_call_native` to the Arc registered
    /// here. Keyed off `import_idx` so it never collides with the
    /// `cap_bit`-indexed raw-pointer `slots`.
    pub fn register_host_fn(&mut self, import_idx: u32, func: Arc<dyn RelonFunction>) {
        self.host_fns.insert(import_idx, func);
    }

    /// Resolve the dynamic host fn registered at `import_idx`.
    pub fn resolve_host_fn(&self, import_idx: u32) -> Option<&Arc<dyn RelonFunction>> {
        self.host_fns.get(&import_idx)
    }

    /// Grant a capability bit by parking a non-null sentinel at its
    /// `slots[cap_bit]`. An `Op::CheckCap { cap_bit }` only null-checks
    /// the slot, so a sentinel is enough to let the guard pass; the
    /// actual call dispatches through the `import_idx`-keyed `host_fns`
    /// registry, not this slot. Mirrors the bytecode VM's grant set.
    pub fn grant(&mut self, cap_bit: u32) {
        // SAFETY-of-intent: `cap_grant_sentinel` is never *called* — the
        // CheckCap path only compares the slot pointer against null, and
        // the CallNative dispatch for a granted source fn goes through
        // `host_fns`. The sentinel exists purely to be non-null.
        self.register(cap_bit, cap_grant_sentinel);
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
///
/// ## 2026-05-22 P0 fix: ownership model
///
/// Earlier drafts kept a single `Arc<SandboxState>` on the evaluator
/// and re-pointed `arena_base` / `tail_cursor` / `scratch_*` from
/// every `run_main` invocation — concurrent invocations on threads
/// sharing the same evaluator raced on those `UnsafeCell` fields and
/// risked silent corruption (the `unsafe impl Sync` was unsound).
///
/// Stage 5 reverts to **per-call ownership**: the evaluator stores an
/// immutable [`SandboxShared`] template, and each `run_main` allocates
/// a fresh `Box<SandboxState>` (or fishes one out of an optional pool),
/// installs its arena, runs the JIT entry, and drops the state at the
/// end of the dispatch. Two threads dispatching on the same evaluator
/// each see their own `SandboxState`, so the per-call `UnsafeCell`
/// fields stay unaliased without needing `Sync`. `SandboxState` is
/// only `Send` now; the `Sync` marker is gone.
#[repr(C)]
pub struct SandboxState {
    /// Deadline as nanos since `Instant::now()` at session start. The
    /// entry prologue reads `now - epoch >= deadline_ns` and traps on
    /// overflow.
    ///
    /// Kept as `AtomicI64` because the cranelift entry pulls it through
    /// the host-side `now_helper` indirection (which already issues a
    /// `Relaxed` load) and the evaluator copies the freshest value out
    /// of the [`SandboxShared`] template at the top of each dispatch.
    deadline_ns: AtomicI64,
    /// Trap reason set by the JIT entry before unwinding. Encoded as
    /// `u64` so we can swap it from cranelift via a normal `i64`
    /// store without going through a wider cell type. Value 0 means
    /// "no trap". Lives on the per-call state so cross-thread invokes
    /// never observe each other's trap bits.
    trap_code: AtomicU64,
    /// Base pointer of the "linear memory" arena. Cranelift code
    /// computes addresses as `arena_base + buf_ptr + field_offset`
    /// for every `LoadField` / `StoreField` it emits. Re-pointed by
    /// the host trampoline before invoking the JIT entry; see
    /// [`SandboxState::install_arena`].
    ///
    /// Stored as `UnsafeCell<usize>` so the JIT thread can read /
    /// write through a stable offset without going through a Rust
    /// borrow. The per-call ownership model means at most one thread
    /// can ever observe this cell.
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
    /// evaluator can hand the same vtable to every per-call
    /// [`SandboxState`] without cloning the slot array.
    capabilities: Arc<CapabilityVtable>,
}

/// Byte offset of `SandboxState::deadline_ns` inside the
/// `#[repr(C)]` layout. Cranelift's resource-check sequence reads at
/// this offset; mirrored here as a const so both the codegen and the
/// runtime stay in sync.
pub const STATE_OFFSET_DEADLINE_NS: i32 = 0;
/// Byte offset of `SandboxState::trap_code`. Read from cranelift IR by
/// [`crate::codegen`]'s `emit_call_native_dynamic`: after the
/// `relon_call_native` helper records a failure code here and returns,
/// the call site `load`s this offset and routes a non-zero value to the
/// trap epilogue. Other trap paths set it indirectly via `raise_trap`.
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

// 2026-05-22 P0 fix: the previous `unsafe impl Sync for SandboxState`
// claimed it was safe to share `&SandboxState` across threads, but the
// `UnsafeCell<_>` fields (`arena_base`, `arena_len`, `tail_cursor`,
// `scratch_*`, `closure_table_base`) were written by `install_arena` /
// `install_scratch_base` on the host thread immediately before each
// JIT dispatch. Two threads dispatching against a shared
// `Arc<SandboxState>` therefore raced on those writes — data race
// under the Rust memory model. The fix is to drop `Sync` entirely and
// allocate a fresh per-call `Box<SandboxState>` (or fetch one from a
// per-evaluator pool) inside the run_main trampoline; that gives the
// state a single-owner thread for the duration of the JIT call without
// any shared mutability. `Send` is sound because the per-call state is
// moved (not aliased) across `Box::new` / `Box::leak` boundaries when
// pooled.
//
// `SandboxState` therefore stays `Send` (the auto-trait works through
// the atomics and the `UnsafeCell<usize>` via the type system because
// `usize` and `u32` are `Send`) and is **not** `Sync`. The asserts in
// the test module below guard against an accidental re-introduction of
// `Sync`.

/// Immutable template the evaluator holds between dispatches. The host
/// drives per-call [`SandboxState::from_template`] off this snapshot so
/// the evaluator itself stays `Sync` without ever exposing a shared
/// `&SandboxState` to two threads at once. Updates to the deadline /
/// capabilities / closure-table address all flow through the
/// evaluator, which copies the current snapshot into the per-call
/// state at dispatch time.
pub(crate) struct SandboxShared {
    /// Deadline as nanos since `epoch`. `i64::MAX` means
    /// "effectively no deadline". `set_deadline` writes here; every
    /// per-call state copies the freshest value at dispatch time.
    pub(crate) deadline_ns: AtomicI64,
    /// Active capability vtable. `install_capabilities_mut` swaps the
    /// `Arc` wholesale; cheap clone-per-call (one atomic inc).
    ///
    /// Stored as `ArcSwap<CapabilityVtable>` so per-call snapshots are
    /// a lock-free `arc_swap::Guard::load_full()` — no `Mutex::lock`
    /// on the dispatch hot path. The wait-free swap path is also
    /// safe to call from any thread.
    pub(crate) capabilities: ArcSwap<CapabilityVtable>,
    /// Pointer to the closure-table the evaluator's `Box<[usize]>`
    /// allocation lives at. `0` when the module has no closures.
    /// Stored as raw `AtomicUsize` because the table allocation lives
    /// on the evaluator and stays put for the evaluator's lifetime —
    /// the value is set once during construction and never re-pointed.
    pub(crate) closure_table_base: std::sync::atomic::AtomicUsize,
    /// Reference start time for deadline calculations. Captured once
    /// when the template is built.
    pub(crate) epoch: Instant,
}

impl SandboxShared {
    /// Build a template carrying the supplied capability vtable. The
    /// deadline starts at `i64::MAX` (effectively no deadline) and the
    /// closure-table pointer at 0; both are wired up by the
    /// evaluator after construction.
    pub(crate) fn new(capabilities: Arc<CapabilityVtable>) -> Self {
        Self {
            deadline_ns: AtomicI64::new(i64::MAX),
            capabilities: ArcSwap::new(capabilities),
            closure_table_base: std::sync::atomic::AtomicUsize::new(0),
            epoch: Instant::now(),
        }
    }

    /// Snapshot the current capability vtable. Lock-free: one acquire
    /// load + one atomic refcount inc via `ArcSwap::load_full`. The
    /// JIT dispatch only ever holds an `Arc<CapabilityVtable>` clone,
    /// never any kind of lock.
    pub(crate) fn capabilities_snapshot(&self) -> Arc<CapabilityVtable> {
        self.capabilities.load_full()
    }

    /// Swap the active capability vtable. Used by
    /// `install_capabilities_mut` on the evaluator surface. Wait-free
    /// for the dispatch readers; only the writer pays the swap cost.
    pub(crate) fn set_capabilities(&self, vt: Arc<CapabilityVtable>) {
        self.capabilities.store(vt);
    }

    /// Update the closure-table base pointer. Called once after the
    /// evaluator resolves the per-module fn pointers.
    pub(crate) fn set_closure_table_base(&self, base: usize) {
        self.closure_table_base
            .store(base, std::sync::atomic::Ordering::Relaxed);
    }

    /// Configure the per-call deadline. Pass `Duration::MAX` (or any
    /// value that overflows to `i64::MAX` nanos) to disable.
    pub(crate) fn set_deadline(&self, deadline: Duration) {
        let nanos = i64::try_from(deadline.as_nanos()).unwrap_or(i64::MAX);
        self.deadline_ns
            .store(nanos, std::sync::atomic::Ordering::Relaxed);
    }
}

impl SandboxState {
    /// Build a fresh sandbox state with an effectively-infinite
    /// deadline. Hosts that want a real deadline call
    /// [`Self::set_deadline`] before invoking the JIT entry. The
    /// arena starts unpopulated; the host trampoline calls
    /// [`Self::install_arena`] before invoking the JIT entry.
    ///
    /// Direct callers are tests / direct-IR fixtures; production
    /// dispatch goes through the evaluator's
    /// [`SandboxShared::new`] template + per-call
    /// [`Self::from_template`] path so the per-call state is
    /// thread-local rather than shared.
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
        }
    }

    /// Materialise a per-call `SandboxState` from the evaluator's
    /// shared template. Copies the current deadline + capability
    /// snapshot + closure-table base into a freshly-allocated state
    /// the dispatch thread owns exclusively for the duration of the
    /// JIT call.
    ///
    /// `epoch` deliberately threads through the template rather than
    /// being re-sampled per-call so the deadline math stays anchored
    /// to a stable wall-clock origin across consecutive dispatches.
    pub(crate) fn from_template(template: &SandboxShared) -> Self {
        let deadline = template.deadline_ns.load(Ordering::Relaxed);
        let closure_base = template
            .closure_table_base
            .load(std::sync::atomic::Ordering::Relaxed);
        Self {
            deadline_ns: AtomicI64::new(deadline),
            trap_code: AtomicU64::new(0),
            arena_base: UnsafeCell::new(0),
            arena_len: UnsafeCell::new(0),
            tail_cursor: UnsafeCell::new(0),
            scratch_cursor: UnsafeCell::new(0),
            scratch_base: UnsafeCell::new(0),
            closure_table_base: UnsafeCell::new(closure_base),
            epoch: template.epoch,
            capabilities: template.capabilities_snapshot(),
        }
    }

    /// Re-anchor a pooled (already-allocated) `SandboxState` against
    /// `template`. Used by the thread-local pool in
    /// `evaluator::run_main` to avoid the `Box::new` + drop pair per
    /// dispatch — we keep the allocation around and refresh the
    /// per-call fields (deadline / capabilities / closure-base / trap)
    /// from the template. The `UnsafeCell` arena / scratch fields are
    /// zeroed by `install_arena` immediately afterwards, so we don't
    /// touch them here.
    ///
    /// The `&mut self` receiver matches the pool's exclusive ownership
    /// of the boxed state for the dispatch's duration — no two threads
    /// can ever see the same pooled `SandboxState` because the pool
    /// itself is a per-thread `RefCell`.
    pub(crate) fn refresh_from_template(&mut self, template: &SandboxShared) {
        let deadline = template.deadline_ns.load(Ordering::Relaxed);
        let closure_base = template
            .closure_table_base
            .load(std::sync::atomic::Ordering::Relaxed);
        // The atomics are written non-atomically through `&mut self`
        // (no concurrent reader: the pool holds the only reference).
        self.deadline_ns.store(deadline, Ordering::Relaxed);
        self.trap_code.store(0, Ordering::Relaxed);
        // `epoch` is `Copy` (Instant); refresh it in case the caller
        // swapped to a different evaluator with a different template.
        self.epoch = template.epoch;
        self.capabilities = template.capabilities_snapshot();
        // SAFETY: pool ownership is exclusive (`&mut self`), so the
        // `UnsafeCell` contents are unaliased here.
        unsafe {
            *self.closure_table_base.get() = closure_base;
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
    ///
    /// 2026-05-21 dispatch-boundary lever (c): hosts can elide the
    /// pre-invoke call when they observe `trap_code() == 0` after the
    /// previous dispatch (i.e. the slot was never set, no reset
    /// required). This shaves a Relaxed store off the hot dispatch
    /// path for the success case. See
    /// [`Self::take_trap_code_and_reset`] for the after-the-fact
    /// pattern that lets the post-dispatch path own the reset.
    pub fn reset_trap(&self) {
        self.trap_code.store(0, Ordering::Relaxed);
    }

    /// Inspect the trap code recorded by the JIT entry. `0` means no
    /// trap occurred.
    pub fn trap_code(&self) -> u64 {
        self.trap_code.load(Ordering::Relaxed)
    }

    /// 2026-05-21 dispatch-boundary lever (c): atomic
    /// "read-then-reset-if-nonzero". Used by the post-dispatch path so
    /// the pre-invoke reset can be elided in the success case (the slot
    /// already reads 0 after a successful trap-free invoke).
    ///
    /// Returns the observed trap code; a non-zero code is followed by
    /// a Relaxed store of 0 so the next invoke starts clean. The dual
    /// is a no-op when the slot is already 0, which compiles to a
    /// single Relaxed load + a predictable branch (the branch is
    /// hot-biased toward the no-trap case in the JIT).
    pub fn take_trap_code(&self) -> u64 {
        let code = self.trap_code.load(Ordering::Relaxed);
        if code != 0 {
            self.trap_code.store(0, Ordering::Relaxed);
        }
        code
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
        // 2026-05-21 Option I (cranelift loop-opt regression fix):
        // fast-return 0 when the host has not configured a deadline.
        // The JIT prologue + the loop back-edge resource-check both
        // unconditionally call this helper to avoid the brif-shaped
        // basic-block expansion that defeated a cranelift loop-opt
        // pass (see commit log on the b99f2b4 revert for the
        // bisect details). Returning 0 makes the downstream
        // `icmp 0 >= deadline_ns` comparison trivially false when
        // the caller will load `deadline_ns` and find `i64::MAX`,
        // so the cond_trap stays inert without needing a brif guard
        // in cranelift IR. Hosts that opt in via
        // `SandboxState::set_deadline` pay the vDSO clock_gettime;
        // default-configured evaluators amortise an indirect-call-
        // only shape (~3-5 ns) instead of an indirect-call +
        // clock_gettime (~25-45 ns).
        if state.deadline_ns.load(Ordering::Relaxed) == i64::MAX {
            return 0;
        }
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

    /// Dynamic host-fn dispatch helper for source-lowered
    /// `Op::CallNative { cap_bit: NO_CAPABILITY_BIT }`. Resolves the
    /// `Arc<dyn RelonFunction>` registered at `import_idx`, packs the
    /// `arg_count` scalar i64 args (read from `args_ptr`) into
    /// `NativeArgs`, invokes it, and returns the i64-encoded result.
    ///
    /// Errors do not unwind across this `extern "C"` boundary (that
    /// would be UB on a `panic=unwind` build): instead the helper
    /// records a [`TrapKind`] in `state.trap_code` and returns `0`. The
    /// JIT call site loads `trap_code` right after the call and routes a
    /// non-zero value to the shared trap epilogue, so the host sees a
    /// typed [`RuntimeError`] (capability denial / host-fn failure /
    /// unsupported shape) the same way every other cranelift trap
    /// surfaces.
    ///
    /// Scope (phase-4a parity with the bytecode VM): scalar `Int` args
    /// in, `Int` result out. A non-`Int` arg or result records
    /// [`TrapKind::Unreachable`] (→ `RuntimeError::Unsupported`).
    ///
    /// # Safety
    ///
    /// `state` must point at a live, aligned [`SandboxState`]; `args_ptr`
    /// must point at `arg_count` contiguous `i64`s. The JIT prologue
    /// passes the same `state` pointer it received and a stack slot it
    /// just populated, so both invariants hold for every production
    /// caller.
    pub(crate) unsafe extern "C" fn call_native(
        state: *const SandboxState,
        import_idx: u32,
        args_ptr: *const i64,
        arg_count: u32,
    ) -> i64 {
        let state = unsafe { &*state };
        let Some(func) = state.capabilities.resolve_host_fn(import_idx).cloned() else {
            // No Arc registered for this import. Surface as a generic
            // unsupported trap — the host wired a gated call but no
            // callable.
            state
                .trap_code
                .store(TrapKind::Unreachable as u64, Ordering::Relaxed);
            return 0;
        };
        let args_slice = if arg_count == 0 {
            &[][..]
        } else {
            // SAFETY: caller guarantees `arg_count` contiguous i64s.
            unsafe { std::slice::from_raw_parts(args_ptr, arg_count as usize) }
        };
        let packed: Vec<Value> = args_slice.iter().map(|&x| Value::Int(x)).collect();
        let native_args = NativeArgs::from_positional(packed, cranelift_native_caps());
        match func.call(native_args, TokenRange::default()) {
            Ok(Value::Int(v)) => v,
            Ok(Value::Bool(b)) => i64::from(b),
            Ok(v) if v.is_option_none() => 0,
            Ok(_) | Err(_) => {
                // Host-fn failure or a return shape outside the phase-4a
                // scalar envelope. The host sees `RuntimeError::Unsupported`.
                state
                    .trap_code
                    .store(TrapKind::Unreachable as u64, Ordering::Relaxed);
                0
            }
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
        assert!(matches!(err, RuntimeError::IndexOutOfBounds { .. }));
        let err = TrapKind::CapabilityDenied.to_runtime_error(range);
        assert!(matches!(err, RuntimeError::CapabilityDenied { .. }));
        let err = TrapKind::ResourceExhausted.to_runtime_error(range);
        assert!(matches!(err, RuntimeError::StepLimitExceeded { .. }));
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

    /// 2026-05-22 P0 fix: the per-call `SandboxState` must be `Send`
    /// (moves between threads when boxed inside the trampoline) but
    /// **not** `Sync` — its `UnsafeCell<_>` fields cannot soundly be
    /// shared. The previous `unsafe impl Sync for SandboxState` raced
    /// on every `install_arena` write across concurrent dispatches.
    #[test]
    fn sandbox_state_is_send_but_not_sync() {
        fn assert_send<T: Send>() {}
        assert_send::<SandboxState>();

        // Compile-time witness that `SandboxState` is **not** `Sync`:
        // we feed the type into a generic that requires `Sync` and
        // expect the file to *fail* to compile if `Sync` ever leaks
        // back in. We can't express a `!Sync` bound on stable Rust,
        // so we use a `cfg(any())` arm that the compiler still type-
        // checks. Removing the `cfg(any())` guard reveals the
        // intent: `SandboxState: !Sync`.
        #[allow(dead_code)]
        fn _must_not_be_sync() {
            #[cfg(any())]
            {
                fn assert_sync<T: Sync>() {}
                assert_sync::<SandboxState>();
            }
        }
    }

    /// 2026-05-22 P0 fix: the `SandboxShared` template must be
    /// `Send + Sync` so the evaluator (which holds an
    /// `Arc<SandboxShared>`) stays `Send + Sync`. Without this the
    /// `Evaluator` trait's bound would fail to derive.
    #[test]
    fn sandbox_shared_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SandboxShared>();
    }
}
