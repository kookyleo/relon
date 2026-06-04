//! Minimal runtime state for the LLVM AOT backend's buffer-protocol
//! entries. **Phase B.**
//!
//! The buffer-protocol entry signature mirrors the cranelift-native
//! backend's `EntryShape::BufferProtocol`:
//!
//! ```text
//! fn run_main(state: *const SandboxState,
//!             in_ptr: i32, in_len: i32,
//!             out_ptr: i32, out_cap: i32,
//!             caps: i64) -> i32;
//! ```
//!
//! `LoadField` / `StoreField` ops resolve to absolute host addresses
//! through the formula `arena_base + buf_ptr + offset`, where
//! `arena_base` lives at a stable offset on the state. The LLVM
//! emitter loads it through a `ptrtoint`/`inttoptr` round-trip.
//!
//! We **do not** reuse `relon_codegen_cranelift::SandboxState` here on
//! purpose:
//!
//! - It would require pulling cranelift-native as a hard dependency of
//!   the LLVM crate just to share an opaque struct layout. The LLVM
//!   backend is meant to stand on its own.
//! - The Phase B envelope does not need sandbox traps, deadline
//!   checks, capability bits, or the closure / scratch / trap_code
//!   subsystems. A minimal C-layout `ArenaState` is enough to drive
//!   the buffer protocol.
//! - Keeping the layout local to this crate makes the offsets we
//!   embed in emitted LLVM IR self-contained — if the cranelift
//!   crate ever rearranges `SandboxState` it cannot accidentally
//!   miscompile our IR.
//!
//! Phase C (when sandbox traps + closures land) is the right time to
//! revisit the dep direction; for Phase B this stays self-contained.

use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::sync::Arc;

use relon_eval_api::{NativeArgs, NativeFnCaps, RelonFunction, RuntimeError, Value};
use relon_parser::TokenRange;

/// Per-call arena state handed to the LLVM JIT-compiled entry. The
/// emitter reads `arena_base` (at offset 0 on a 64-bit host) and
/// `arena_len` (offset 8) to resolve every buffer-protocol load /
/// store; everything past those two fields is reserved for Phase C
/// (sandbox traps, deadline, closure table).
///
/// `#[repr(C)]` because the LLVM emitter hard-codes the field
/// offsets through `inttoptr(arena_base_ptr + N)` style address
/// arithmetic.
///
/// `UnsafeCell` on the live fields because the JIT thread mutates
/// them through a raw pointer; Rust's borrow checker cannot see the
/// emitted machine code. The per-call ownership model (one
/// `ArenaState` per `run_main` dispatch) means no aliasing race
/// can occur — the LLVM evaluator allocates a fresh state on the
/// stack before each call.
///
/// ## Phase 0b: native-call dispatch
///
/// `host_fns` + `trap_code` mirror the cranelift backend's
/// `SandboxState` so the LLVM JIT path can dispatch a source-lowered
/// `Op::CallNative` through the host-fn registry the same way (see
/// [`relon_llvm_call_native`]). `host_fns` is a raw pointer (not an
/// `Arc` slot) because the registry is owned by the evaluator and
/// outlives every per-call state; the emitter loads it by offset and
/// hands it back to the helper verbatim. `0` (null) means "no
/// registry installed" — a `CallNative` then records
/// [`NativeTrap::HostFnMissing`] in `trap_code`.
#[repr(C)]
pub struct ArenaState {
    /// Base pointer of the arena bytes the host owns. The emitted
    /// LLVM IR reads this through `load i64, ptr %state` (offset 0),
    /// then `inttoptr` to a byte pointer + i64-extended `buf_ptr` +
    /// `field_offset`. The pointer is `usize`-wide so the cast
    /// matches the host's pointer width.
    pub arena_base: UnsafeCell<usize>,
    /// Length of the arena in bytes. Phase B does not emit bounds
    /// checks (the task spec explicitly notes div0 / overflow /
    /// bounds are exposed as `llvm.trap` / panic at most), so this
    /// is recorded for future use rather than read by the JIT today.
    pub arena_len: UnsafeCell<u32>,
    /// Phase E.1: tail cursor used by pointer-indirect StoreField
    /// (`String` / `ListInt` / `ListFloat` / `ListBool`) to bump-
    /// allocate records inside the output buffer's tail region.
    /// Counts buffer-relative bytes from `out_ptr`. Reset to 0 at the
    /// start of every dispatch.
    pub tail_cursor: UnsafeCell<u32>,
    /// Phase E.1: scratch bump cursor used by stdlib bodies (`concat`,
    /// `substring`, ...) and `Op::StrConcatN` to allocate temporary
    /// records inside the arena's scratch region. Counts bytes from
    /// `scratch_base`. Reset to 0 per dispatch.
    pub scratch_cursor: UnsafeCell<u32>,
    /// Phase E.1: arena-relative byte offset at which the scratch
    /// region starts (= `out_ptr + out_cap`). The bump path reads
    /// `scratch_base + scratch_cursor` as the i32 pointer returned to
    /// the stdlib body.
    pub scratch_base: UnsafeCell<u32>,
    /// Phase 0b: trap code recorded by [`relon_llvm_call_native`] on a
    /// failed dispatch (host-fn missing / host-fn error / unsupported
    /// arg shape). `0` = no trap. The `Op::CallNative` lowering loads
    /// this right after the helper returns and routes a non-zero value
    /// to an `llvm.trap`. Mirrors `SandboxState::trap_code`.
    pub trap_code: UnsafeCell<u64>,
    /// Phase 0b: raw pointer to the host-fn registry installed by the
    /// evaluator before dispatch. Null when no registry was supplied.
    /// The emitter loads this word and hands it to the helper; the
    /// helper re-derives `&HostFnRegistry`. Lives outside the
    /// `#[repr(C)]` codegen-visible prefix only through its offset —
    /// it is a plain pointer-width field the JIT never dereferences
    /// directly (only the helper does, on the Rust side).
    pub host_fns: UnsafeCell<usize>,
}

/// Byte offset of [`ArenaState::arena_base`] inside the `#[repr(C)]`
/// layout. Used by the LLVM emitter to materialise the load.
pub const ARENA_STATE_OFFSET_BASE: u32 = 0;

/// Byte offset of [`ArenaState::arena_len`]. Reserved for Phase C
/// bounds-check work; the emitter leaves it untouched today.
#[allow(dead_code)]
pub const ARENA_STATE_OFFSET_LEN: u32 = std::mem::size_of::<usize>() as u32;

/// Byte offset of [`ArenaState::tail_cursor`]. The pointer-indirect
/// StoreField path loads and stores this u32 to bump-allocate the
/// output buffer's tail region.
pub const ARENA_STATE_OFFSET_TAIL_CURSOR: u32 = ARENA_STATE_OFFSET_LEN + 4;

/// Byte offset of [`ArenaState::scratch_cursor`]. Loaded / stored by
/// the `Op::AllocScratch` / `Op::AllocScratchDyn` lowering.
pub const ARENA_STATE_OFFSET_SCRATCH_CURSOR: u32 = ARENA_STATE_OFFSET_TAIL_CURSOR + 4;

/// Byte offset of [`ArenaState::scratch_base`]. Loaded by the scratch
/// allocator to compute the arena-relative offset of a freshly-
/// reserved scratch block (`scratch_base + scratch_cursor`).
pub const ARENA_STATE_OFFSET_SCRATCH_BASE: u32 = ARENA_STATE_OFFSET_SCRATCH_CURSOR + 4;

/// Byte offset of [`ArenaState::trap_code`]. The three trailing u32
/// fields (`arena_len`, `tail_cursor`, `scratch_cursor`,
/// `scratch_base`) total 16 bytes past `arena_base`; the `u64`
/// `trap_code` follows on its natural 8-byte boundary. The
/// `Op::CallNative` lowering reads / writes this offset; a runtime
/// assert in [`ArenaState`]'s test module pins the layout.
pub const ARENA_STATE_OFFSET_TRAP_CODE: u32 = 24;

/// Byte offset of [`ArenaState::host_fns`]. The `usize`-wide registry
/// pointer follows `trap_code` on its natural boundary. Only the Rust
/// helper [`relon_llvm_call_native`] dereferences this field (via
/// `state.host_fns.get()`), so the emitter never materialises the
/// offset — it exists for the layout assertion + documentation.
#[allow(dead_code)]
pub const ARENA_STATE_OFFSET_HOST_FNS: u32 = ARENA_STATE_OFFSET_TRAP_CODE + 8;

/// Phase 0b native-dispatch trap codes recorded in
/// [`ArenaState::trap_code`] by [`relon_llvm_call_native`]. Mirrors the
/// cranelift backend's `TrapKind` numbering for the subset the LLVM
/// dynamic-dispatch path can raise. `0` is reserved for "no trap".
#[repr(u64)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeTrap {
    /// The `Op::CheckCap` gate denied a gated native call (the granted
    /// `caps` bitmask had the required bit clear). Matches cranelift's
    /// `TrapKind::CapabilityDenied` (= 3); lifts to
    /// `RuntimeError::CapabilityDenied`.
    CapabilityDenied = 3,
    /// No host fn registered at the requested `import_idx`, or no
    /// registry installed at all. Surfaces as
    /// `RuntimeError::Unsupported`. Matches cranelift's
    /// `TrapKind::Unreachable` (= 5) so the host-observable outcome
    /// class is identical across backends.
    HostFnMissing = 5,
    /// The host fn returned an error, or a value outside the phase-0b
    /// scalar return envelope (`Int` / `Bool` / `Null`). Surfaces as
    /// `RuntimeError::Unsupported`. A distinct code from `HostFnMissing`
    /// only for post-mortem readability — both lift to `Unsupported`.
    HostFnError = 7,
}

impl NativeTrap {
    /// Lift a trap code recorded in [`ArenaState::trap_code`] into the
    /// matching [`RuntimeError`]. Unknown / `0` codes are treated as
    /// `Unsupported` (defensive — the JIT only ever stores the codes
    /// above). Mirrors cranelift's `TrapKind::to_runtime_error` for the
    /// subset the LLVM dynamic-dispatch path raises.
    pub fn runtime_error_from_code(code: u64) -> RuntimeError {
        match code {
            3 => RuntimeError::CapabilityDenied {
                cap_bit: None,
                reason: "llvm-aot: host-fn call denied by capability gate".to_string(),
                range: TokenRange::default(),
            },
            _ => RuntimeError::Unsupported {
                reason: "llvm-aot: native-fn dispatch failed (host fn missing / errored / \
                         returned a non-scalar value)"
                    .to_string(),
            },
        }
    }
}

impl ArenaState {
    /// Construct a state that points at `arena[0..]` for a single
    /// dispatch. The caller owns the backing storage; this struct
    /// only borrows it through a raw pointer for the JIT's
    /// lifetime.
    ///
    /// `scratch_base` is the arena-relative offset where temporary
    /// allocations (string concat, ...) live; pass `arena.len()` to
    /// disable the scratch path. The cursors are reset to 0 so the
    /// JIT bump path starts fresh on every dispatch.
    ///
    /// # Safety
    ///
    /// The caller must keep `arena` live and exclusively owned by the
    /// `run_main` invocation that consumes this state. The emitted
    /// JIT code reads and writes through `arena_base` without
    /// touching the Rust borrow checker.
    pub fn new(arena: &mut [u8], scratch_base: u32) -> Self {
        Self {
            arena_base: UnsafeCell::new(arena.as_mut_ptr() as usize),
            arena_len: UnsafeCell::new(arena.len() as u32),
            tail_cursor: UnsafeCell::new(0),
            scratch_cursor: UnsafeCell::new(0),
            scratch_base: UnsafeCell::new(scratch_base),
            trap_code: UnsafeCell::new(0),
            host_fns: UnsafeCell::new(0),
        }
    }

    /// Point the state at a host-fn registry for the duration of one
    /// dispatch. Pass `0` (or skip the call) to leave the registry
    /// unset — a `CallNative` then traps `HostFnMissing`.
    ///
    /// # Safety
    ///
    /// `registry` must outlive the JIT dispatch that consumes this
    /// state, and must be a valid `*const HostFnRegistry` (or null).
    /// The per-call ownership model keeps the `UnsafeCell` unaliased.
    pub unsafe fn install_host_fns(&self, registry: *const HostFnRegistry) {
        unsafe {
            *self.host_fns.get() = registry as usize;
        }
    }

    /// Read the trap code recorded by the JIT-side `Op::CallNative`
    /// dispatch. `0` means no native-dispatch trap fired.
    pub fn trap_code(&self) -> u64 {
        // SAFETY: the dispatch has returned, so the cell is unaliased.
        unsafe { *self.trap_code.get() }
    }

    /// Read the current tail-cursor value. Used by the evaluator
    /// after a dispatch returns to know how much was written into the
    /// tail region (for `String` return-value decoding).
    #[allow(dead_code)]
    pub fn tail_cursor(&self) -> u32 {
        // SAFETY: caller owns the state exclusively for a single
        // dispatch — no aliasing read can happen.
        unsafe { *self.tail_cursor.get() }
    }
}

/// Phase 0b host-fn registry: `import_idx -> Arc<dyn RelonFunction>`.
///
/// Mirrors the `host_fns` half of the cranelift backend's
/// `CapabilityVtable`. The LLVM evaluator owns one of these (built via
/// [`Self::with_host_fns`]) and points each per-call [`ArenaState`] at
/// it through [`ArenaState::install_host_fns`]; a source-lowered
/// `Op::CallNative` then resolves the `import_idx`-keyed callable via
/// [`relon_llvm_call_native`].
///
/// Keying off `import_idx` (the IR-side private namespace) keeps it
/// distinct from the capability-bit namespace the `Op::CheckCap`
/// gate consumes — exactly the cranelift split.
#[derive(Default, Clone)]
pub struct HostFnRegistry {
    host_fns: HashMap<u32, Arc<dyn RelonFunction>>,
}

impl std::fmt::Debug for HostFnRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostFnRegistry")
            .field("host_fn_count", &self.host_fns.len())
            .finish()
    }
}

impl HostFnRegistry {
    /// Build an empty registry.
    pub fn new() -> Self {
        Self {
            host_fns: HashMap::new(),
        }
    }

    /// Register a callable at `import_idx`. Overwrites any prior entry.
    pub fn register(&mut self, import_idx: u32, func: Arc<dyn RelonFunction>) {
        self.host_fns.insert(import_idx, func);
    }

    /// Resolve the callable registered at `import_idx`.
    pub fn resolve(&self, import_idx: u32) -> Option<&Arc<dyn RelonFunction>> {
        self.host_fns.get(&import_idx)
    }

    /// Number of registered host fns.
    pub fn len(&self) -> usize {
        self.host_fns.len()
    }

    /// `true` when no host fns are registered.
    pub fn is_empty(&self) -> bool {
        self.host_fns.is_empty()
    }
}

/// Zero-surface [`NativeFnCaps`] for LLVM-dispatched host fns. Same
/// envelope as the cranelift backend's `CraneliftNativeFnCaps`: no
/// closure-callback / iterator surface yet, so a host fn that tries to
/// call back into Relon logic gets a typed `Unsupported` error rather
/// than a segfault. Cached as a single `Arc` so each dispatch is a
/// refcount bump.
struct LlvmNativeFnCaps;

impl NativeFnCaps for LlvmNativeFnCaps {
    fn call_relon(
        &self,
        _func: &Value,
        _args: Vec<Value>,
        _range: TokenRange,
    ) -> Result<Value, RuntimeError> {
        Err(RuntimeError::Unsupported {
            reason: "llvm-aot host fn: call_relon callback unsupported".to_string(),
        })
    }
}

fn llvm_native_caps() -> Arc<dyn NativeFnCaps> {
    static CAPS: std::sync::OnceLock<Arc<dyn NativeFnCaps>> = std::sync::OnceLock::new();
    Arc::clone(CAPS.get_or_init(|| Arc::new(LlvmNativeFnCaps) as Arc<dyn NativeFnCaps>))
}

/// Stable symbol name the LLVM module declares the native-dispatch
/// helper under. The evaluator maps it onto
/// [`relon_llvm_call_native`]'s address via `engine.add_global_mapping`
/// before resolving the entry pointer. Mirrors the cranelift backend's
/// `RelonCallNative` vtable slot — same `(state, import_idx, args_ptr,
/// arg_count) -> i64` shape, resolved by symbol here instead of through
/// a data-vtable slot.
pub const RELON_LLVM_CALL_NATIVE_SYMBOL: &str = "relon_llvm_call_native";

/// Dynamic host-fn dispatch helper for a source-lowered
/// `Op::CallNative`. The JIT-emitted call site passes the per-call
/// `ArenaState` pointer, the IR `import_idx`, a pointer to `arg_count`
/// contiguous i64 args (spilled into an `alloca` by the lowering), and
/// the arg count. The helper:
///
/// 1. loads the `host_fns` registry pointer from the state;
/// 2. resolves the `Arc<dyn RelonFunction>` registered at `import_idx`;
/// 3. packs the i64 args as `Value::Int`s into `NativeArgs`;
/// 4. invokes the callable and returns the i64-encoded scalar result.
///
/// Failures (no registry / no callable / host-fn error / non-scalar
/// return) do **not** unwind across this `extern "C"` boundary (that
/// would be UB on a `panic=unwind` build): the helper records a
/// [`NativeTrap`] code in `state.trap_code` and returns `0`. The JIT
/// call site loads `trap_code` right after the call and routes a
/// non-zero value to an `llvm.trap`, so the host sees a typed
/// `RuntimeError` the same way every other LLVM trap surfaces. Mirrors
/// the cranelift backend's `SandboxState::call_native`.
///
/// Scope (phase-0b parity with the bytecode VM + cranelift dynamic
/// path): scalar `Int` args in, `Int` / `Bool` / `Null` result out.
///
/// # Safety
///
/// `state` must point at a live, aligned [`ArenaState`]; `args_ptr`
/// must point at `arg_count` contiguous `i64`s. The JIT prologue passes
/// the same `state` pointer it received and a stack slot it just
/// populated, so both invariants hold for every production caller.
pub unsafe extern "C" fn relon_llvm_call_native(
    state: *const ArenaState,
    import_idx: u32,
    args_ptr: *const i64,
    arg_count: u32,
) -> i64 {
    // SAFETY: caller guarantees a live, aligned ArenaState.
    let state = unsafe { &*state };
    // SAFETY: per-call ownership — the JIT thread is the only reader.
    let registry_ptr = unsafe { *state.host_fns.get() } as *const HostFnRegistry;
    let record_trap = |code: NativeTrap| {
        // SAFETY: per-call ownership; the JIT call has not returned yet
        // but no other thread can see this state.
        unsafe {
            *state.trap_code.get() = code as u64;
        }
    };
    if registry_ptr.is_null() {
        record_trap(NativeTrap::HostFnMissing);
        return 0;
    }
    // SAFETY: the evaluator installs a registry that outlives the
    // dispatch (it lives on the evaluator, behind an Arc).
    let registry = unsafe { &*registry_ptr };
    let Some(func) = registry.resolve(import_idx).cloned() else {
        record_trap(NativeTrap::HostFnMissing);
        return 0;
    };
    let args_slice = if arg_count == 0 {
        &[][..]
    } else {
        // SAFETY: caller guarantees `arg_count` contiguous i64s.
        unsafe { std::slice::from_raw_parts(args_ptr, arg_count as usize) }
    };
    let packed: Vec<Value> = args_slice.iter().map(|&x| Value::Int(x)).collect();
    let native_args = NativeArgs::from_positional(packed, llvm_native_caps());
    match func.call(native_args, TokenRange::default()) {
        Ok(Value::Int(v)) => v,
        Ok(Value::Bool(b)) => i64::from(b),
        Ok(Value::Null) => 0,
        Ok(_) | Err(_) => {
            record_trap(NativeTrap::HostFnError);
            0
        }
    }
}

/// Address of [`relon_llvm_call_native`] as a `usize`, for
/// `engine.add_global_mapping`. Two-step cast silences the
/// `fn-as-usize` lint (mirrors `relon_llvm_str_contains_arena_addr`).
#[inline]
pub fn relon_llvm_call_native_addr() -> usize {
    relon_llvm_call_native as *const () as usize
}

// ---------------------------------------------------------------------
// Built-in WASI-backed capability primitives — NATIVE target helpers.
//
// On the native (non-wasm) target the built-in `clock()` / `random()`
// primitives (`Op::ReadClock` / `Op::ReadRandom`) lower to a `call` of
// these host-resident `extern "C"` symbols, resolved at MCJIT link time
// via `engine.add_global_mapping` (same mechanism as
// `relon_llvm_call_native`). On wasm32 the same ops instead emit a
// standard WASI import (`clock_time_get` / `random_get`) — see
// `crate::wasi_cap`. The capability gate (`reads_clock` / `uses_rng`)
// rides the preceding `Op::CheckCap`.
// ---------------------------------------------------------------------

/// Symbol the LLVM module declares the native `clock()` helper under.
pub const RELON_LLVM_READ_CLOCK_SYMBOL: &str = "relon_llvm_read_clock_ns";

/// Symbol the LLVM module declares the native `random()` helper under.
pub const RELON_LLVM_READ_RANDOM_SYMBOL: &str = "relon_llvm_read_random_i64";

/// Host helper backing the built-in `clock()` primitive on the native
/// target. Returns the wall-clock reading as nanoseconds since the Unix
/// epoch — the native analogue of the wasm `clock_time_get(REALTIME)`
/// import, so both backends read off the same physical clock.
///
/// `extern "C"` + no unwind: a clock read cannot fail in a way that
/// needs to cross the boundary (the `unwrap_or(0)` keeps it total).
pub extern "C" fn relon_llvm_read_clock_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Host helper backing the built-in `random()` primitive on the native
/// target. Returns 8 fresh random bytes packed into an i64 (LE) — the
/// native analogue of the wasm `random_get` import. Backed by OS
/// entropy via `/dev/urandom` (std-only; the supported target is
/// Linux-x86_64). Returns 0 on read failure (degraded but well-defined;
/// production hosts on this target always have `/dev/urandom`).
pub extern "C" fn relon_llvm_read_random_i64() -> i64 {
    use std::io::Read;
    let mut buf = [0u8; 8];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => i64::from_le_bytes(buf),
        Err(_) => 0,
    }
}

/// Address of [`relon_llvm_read_clock_ns`] for `add_global_mapping`.
#[inline]
pub fn relon_llvm_read_clock_addr() -> usize {
    relon_llvm_read_clock_ns as *const () as usize
}

/// Address of [`relon_llvm_read_random_i64`] for `add_global_mapping`.
#[inline]
pub fn relon_llvm_read_random_addr() -> usize {
    relon_llvm_read_random_i64 as *const () as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arena_state_offsets_match_repr_c_layout() {
        let mut buf = [0u8; 16];
        let state = ArenaState::new(&mut buf, 16);
        let base = &state as *const _ as usize;
        assert_eq!(
            (state.arena_base.get() as usize) - base,
            ARENA_STATE_OFFSET_BASE as usize
        );
        assert_eq!(
            (state.arena_len.get() as usize) - base,
            ARENA_STATE_OFFSET_LEN as usize
        );
        assert_eq!(
            (state.tail_cursor.get() as usize) - base,
            ARENA_STATE_OFFSET_TAIL_CURSOR as usize
        );
        assert_eq!(
            (state.scratch_cursor.get() as usize) - base,
            ARENA_STATE_OFFSET_SCRATCH_CURSOR as usize
        );
        assert_eq!(
            (state.scratch_base.get() as usize) - base,
            ARENA_STATE_OFFSET_SCRATCH_BASE as usize
        );
        assert_eq!(
            (state.trap_code.get() as usize) - base,
            ARENA_STATE_OFFSET_TRAP_CODE as usize
        );
        assert_eq!(
            (state.host_fns.get() as usize) - base,
            ARENA_STATE_OFFSET_HOST_FNS as usize
        );
    }

    struct AddOne;
    impl RelonFunction for AddOne {
        fn call(&self, args: NativeArgs, _r: TokenRange) -> Result<Value, RuntimeError> {
            match args.positional.first() {
                Some(Value::Int(x)) => Ok(Value::Int(x + 1)),
                _ => Err(RuntimeError::Unsupported {
                    reason: "AddOne expects Int".into(),
                }),
            }
        }
    }

    #[test]
    fn call_native_helper_dispatches_registered_fn() {
        let mut reg = HostFnRegistry::new();
        reg.register(0, Arc::new(AddOne));
        let mut buf = [0u8; 16];
        let state = ArenaState::new(&mut buf, 16);
        // SAFETY: `reg` outlives the call below.
        unsafe { state.install_host_fns(&reg as *const _) };
        let args = [41i64];
        let r = unsafe { relon_llvm_call_native(&state as *const _, 0, args.as_ptr(), 1) };
        assert_eq!(r, 42);
        assert_eq!(state.trap_code(), 0);
    }

    #[test]
    fn call_native_helper_traps_when_unregistered() {
        let reg = HostFnRegistry::new();
        let mut buf = [0u8; 16];
        let state = ArenaState::new(&mut buf, 16);
        unsafe { state.install_host_fns(&reg as *const _) };
        let r = unsafe { relon_llvm_call_native(&state as *const _, 7, std::ptr::null(), 0) };
        assert_eq!(r, 0);
        assert_eq!(state.trap_code(), NativeTrap::HostFnMissing as u64);
    }

    #[test]
    fn call_native_helper_traps_when_no_registry() {
        let mut buf = [0u8; 16];
        let state = ArenaState::new(&mut buf, 16);
        // No install_host_fns — registry pointer stays null.
        let r = unsafe { relon_llvm_call_native(&state as *const _, 0, std::ptr::null(), 0) };
        assert_eq!(r, 0);
        assert_eq!(state.trap_code(), NativeTrap::HostFnMissing as u64);
    }
}
