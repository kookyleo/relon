//! Cranelift-backed native AOT evaluator for Relon — v5-beta-1
//! HelloWorld tier.
//!
//! The crate sits alongside `relon-codegen-wasm` as a second AOT
//! backend behind the [`relon_eval_api::Evaluator`] trait. The
//! cranelift path produces native machine code via the in-process JIT
//! and runs it directly, without going through wasmtime. The
//! single-call latency is roughly an order of magnitude lower than
//! the wasm-AOT path; in exchange the codegen surface is narrower
//! (v5-beta-1 covers integer arithmetic + a small slice of stdlib
//! calls only).
//!
//! ## Architecture
//!
//! 1. [`CraneliftAotEvaluator::from_source`] runs the full pipeline:
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
//! ## v5-beta-2 follow-up
//!
//! Items deliberately *not* in this crate's scope today, tracked in
//! `docs/internal/wasm-aot-v4-roadmap-sandbox-safe.md`:
//!
//! * Real `sigsetjmp` / `siglongjmp` trap handler (today we lean on
//!   `catch_unwind`; the typed `RuntimeError` payload makes the
//!   difference invisible to callers but the JIT pays the cost of a
//!   panic unwind on hot paths).
//! * Full IR-op coverage (`Op::LoadStringPtr`, the dict / record
//!   builders, the stdlib bodies that walk strings byte-by-byte).
//! * Module cache backed by `cranelift-object` so the cold-start path
//!   skips the JIT entirely.
//!
//! See `tests/` for end-to-end smoke runs against each of the six
//! HelloWorld scenarios.

#![allow(unused_assignments)]

pub mod cache;
mod codegen;
pub mod error;
pub mod evaluator;
pub mod sandbox;

pub use cache::{deserialize as deserialize_cache, serialize as serialize_cache, CacheEntry};
pub use error::CraneliftError;
pub use evaluator::CraneliftAotEvaluator;
pub use sandbox::{
    CapabilityVtable, HostFnPtr, SandboxConfig, SandboxState, TrapKind, STATE_OFFSET_ARENA_BASE,
    STATE_OFFSET_ARENA_LEN, STATE_OFFSET_DEADLINE_NS, STATE_OFFSET_TAIL_CURSOR,
};
