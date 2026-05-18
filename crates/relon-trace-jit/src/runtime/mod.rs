//! Host-side runtime helpers the v6-gamma trace emitter calls into.
//!
//! `crates/relon-trace-emitter` emits cranelift IR that issues C-ABI
//! calls to a fixed set of host symbols at well-known points
//! (guard-failure deopt block, `TraceOp::Call`, type-check guard
//! fast-path). The emitter is responsible only for the *call site*;
//! the *implementation* of each helper lives here.
//!
//! The three helpers are:
//!
//! ```text
//! __relon_trace_save_deopt(ctx_ptr, guard_pc, external_pc)
//!     -> ()    // snapshot ssa_slots + pending recoverable writes
//!              // into ctx_ptr->deopt_state
//!
//! __relon_trace_resolve_call(ctx_ptr, external_addr)
//!     -> *const u8   // resolve a recorded ExternalAddr to the
//!                    // installed function pointer
//!
//! __relon_trace_inline_cache_lookup(ic_ptr, observed_type)
//!     -> i32   // 0 = Hit, 1 = Miss (matches CacheResult)
//! ```
//!
//! ## Shared ABI (v6-γ M1)
//!
//! The trace ABI types — `TraceContext`, `DeoptStateSnapshot`,
//! `RecoverableWriteRecord` — live in `relon-trace-abi`. Both the
//! emitter and this runtime crate import the canonical definitions
//! from there, so the byte layout is a single source of truth and
//! the host can hand a `TraceContext` allocated through any of the
//! three sibling crates into a JIT-emitted trace without manual
//! marshalling.
//!
//! The dep direction is `host -> trace-jit -> trace-abi` and
//! `host -> trace-emitter -> trace-abi`; the two consumer crates
//! never see each other.
//!
//! ## Concurrency
//!
//! Per design doc §1.4, each thread owns its own JIT-compiled trace
//! buffer and its own `TraceContext`. We honour that here:
//!
//! - The call resolution table lives in a `thread_local!` so
//!   multi-thread hosts never share state.
//! - The deopt write path mutates only the caller's
//!   `*mut TraceContext` — no global state.
//! - The inline-cache helper dispatches into an `InlineCache<N>`
//!   that the caller owns; `InlineCache` itself uses `Cell<...>` for
//!   non-atomic single-threaded interior mutability.

pub mod call_table;
pub mod deopt;
pub mod ic_lookup;

pub use call_table::{
    register_external_call, resolve_external_call, with_call_table, ExternalCallTable,
    __relon_trace_resolve_call,
};
pub use deopt::{
    DeoptStateSnapshot, GenericState, RecoverableWriteRecord, TraceContext,
    __relon_trace_save_deopt,
};
pub use ic_lookup::{__relon_trace_inline_cache_lookup, ic_storage_size, write_ic_header};
