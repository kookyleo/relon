//! Cranelift-backed native AOT evaluator for Relon.
//!
//! v5-β-2 stage 4 made this the only AOT backend in the workspace
//! — the wasm-AOT path (`relon-codegen-wasm`) retired here because
//! the cranelift path covers the full IR corpus the tree-walker
//! exercises (51/52 corpus parity, the missing case being analyzer-
//! rejected). The cranelift JIT produces native machine code in
//! the host process and dispatches `run_main` through a panic-
//! shielded trampoline; single-call latency targets LuaJIT trace
//! tier (sub-microsecond warm).
//!
//! ## Architecture
//!
//! 1. [`AotEvaluator::from_source`] runs the full pipeline:
//!    parse + analyze + lower (via the shared `relon-ir` crate) +
//!    cranelift codegen + JIT finalize.
//! 2. The lowering pass emits cranelift IR with explicit
//!    [`sandbox::TrapKind`]-tagged trap instructions for every guard
//!    the spec mandates: bounds checks, divide-by-zero, capability
//!    misses, and resource-limit overruns.
//! 3. `run_main` materialises the host's i64 args, resets the
//!    per-call trap slot, invokes the JIT entry inside a
//!    `catch_unwind` shield, and routes the captured trap code back
//!    into the standard `RuntimeError` variants.
//!
//! ## Sandbox guarantees
//!
//! All four hard guarantees from the v5 design doc are present in the
//! emitted code:
//!
//! * **Bounds**: every memory load goes through an `icmp_ult` against
//!   the active linear-memory length. v5-beta-1 stitches this in for
//!   the constant String case via [`sandbox::SandboxConfig::bounds_check`];
//!   the wider Op surface (LoadField / etc.) is gated behind v5-beta-2
//!   when more of the IR's pointer ops are lowered.
//! * **Trap handler**: divide-by-zero, bounds, unreachable, and
//!   resource-deadline traps are produced via cranelift `trap` /
//!   `trapnz` instructions with [`sandbox::TrapKind`]-encoded codes.
//!   The runtime captures the code in a per-state `AtomicU64` and
//!   re-surfaces it through the host trampoline.
//! * **Capability gating**: a per-call [`sandbox::CapabilityVtable`]
//!   indexes host fn pointers by cap_bit; the codegen pass emits
//!   `cap_lookup` + null-check sequences before every guarded
//!   indirect call.
//! * **Resource limit**: a deadline read at function entry traps when
//!   the wall-clock has run past the host-configured budget;
//!   `RESOURCE_CHECK_INTERVAL` documents the cadence for inner
//!   loop rechecks once the IR's loop ops are lowered (v5-beta-2).
//!
//! ## v5-γ follow-up
//!
//! Items deliberately *not* in this crate's scope today (deferred
//! from v5-β-2 stage 4):
//!
//! * Real `sigsetjmp` / `siglongjmp` trap handler (today we lean on
//!   `catch_unwind`; the typed `RuntimeError` payload makes the
//!   difference invisible to callers but the JIT pays the cost of a
//!   panic unwind on hot paths).
//! * `Op::CallNative` full indirect dispatch via the capability
//!   vtable + per-sig marshalling.
//! * `Op::CallClosure` + closure-bearing higher-order list ops.
//! * `Op::Loop` with `result_ty != None` (block-param threading) +
//!   `Op::BrTable` jump tables.
//! * `RESOURCE_CHECK_INTERVAL` cadence on inner-loop back-edges.
//! * Module cache backed by `cranelift-object` so the cold-start path
//!   skips the JIT entirely.
//!
//! See `tests/` for end-to-end smoke runs against the corpus
//! scenarios.

#![allow(unused_assignments)]

pub mod bytecode_bridge;
pub mod cache;
pub(crate) mod codegen;
pub mod error;
pub mod evaluator;
pub mod glob_helper;
pub mod object_cache_integration;
pub mod sandbox;
pub mod schema_cache;
pub mod trace_glob_helper;
pub mod trace_ic;
pub mod trace_inline;
pub mod trace_install;
pub mod trace_recording;
pub mod trap_handler;
pub mod vtable;

pub use bytecode_bridge::{CraneliftHotTrigger, CraneliftTraceLookup};
pub use cache::{deserialize as deserialize_cache, serialize as serialize_cache, CacheEntry};
pub use error::CraneliftError;
pub use evaluator::AotEvaluator;

/// Deprecated alias kept so downstream hosts that pinned the old
/// `CraneliftAotEvaluator` name during the Dart-style rename still
/// compile. New code should use `AotEvaluator`. Slated for removal
/// after 1-2 seasons (tracked in the naming-alignment design note).
#[deprecated(
    since = "0.3.0",
    note = "renamed to `AotEvaluator`; the `Cranelift` implementation-detail prefix was dropped as part of the Dart-style JIT/AOT split"
)]
pub use evaluator::AotEvaluator as CraneliftAotEvaluator;
pub use object_cache_integration::{
    compute_source_hash, default_cache_dir, emit_module_object_bytes, host_target_triple,
    ir_cache_path_for, LoadedCache, GENERATOR_VERSION,
};
pub use sandbox::{
    CapabilityVtable, HostFnPtr, SandboxConfig, SandboxState, TrapKind, STATE_OFFSET_ARENA_BASE,
    STATE_OFFSET_ARENA_LEN, STATE_OFFSET_DEADLINE_NS, STATE_OFFSET_TAIL_CURSOR,
};
pub use trace_ic::{TraceIcSlot, IC_WAYS};
pub use trace_inline::{compile_inline_host_fn, InlineHostFn, InlineHostFnError};
pub use trace_install::{
    clear_recording, default_host_hooks, global_trace_jit_state, hot_counter_peek,
    hot_counter_reset, hot_counter_reset_all, hot_counters_base, install_recorder_trace_warmup,
    jump_helper_call_count, recording_registration_count, register_recording,
    register_trace_runtime_symbols, reset_jump_helper_call_count, JITedTraceFn,
    RecordingRegistration, TraceEntryFn, TraceJitError, TraceJitState, HOT_COUNTERS_SYMBOL,
    MAX_FN_ID, RELON_HOT_THRESHOLD, TINY_TRACE_OP_THRESHOLD,
};
pub use relon_trace_abi::TraceContext;
pub use trace_recording::{RecordingOutcome, StackCell, TraceRecordingEvaluator};
