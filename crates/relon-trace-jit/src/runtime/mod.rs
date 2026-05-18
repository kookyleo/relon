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
//! ## Why redeclare `TraceContext` here?
//!
//! The emitter pins a concrete `#[repr(C)] TraceContext` layout in
//! `relon_trace_emitter::abi::TraceContext`. This crate has **no
//! dependency** on `relon-trace-emitter` (and must not — the dep
//! direction is host -> jit -> emitter, never the reverse). To keep
//! that invariant we redeclare a **layout-compatible** view of the
//! parts we touch via the FFI boundary.
//!
//! ### Known ABI divergence (TODO v6-gamma phase)
//!
//! The emitter's `TraceContext::deopt_state` is typed as
//! `Option<EmitterSnapshot>` where its snapshot only carries
//! `guard_trace_pc` + `external_pc`. The runtime needs more state —
//! a copy of `ssa_slots` at the deopt point and a drained list of
//! recoverable writes. We therefore declare a **richer**
//! [`deopt::DeoptStateSnapshot`] here and a `TraceContext` view
//! whose `deopt_state` field is of the **same shape** but is the
//! runtime's version, not the emitter's.
//!
//! As long as the host wires its `*mut TraceContext` to the
//! runtime's redeclared type when invoking the JIT trace, the layouts
//! match. The integration phase will reconcile both definitions into
//! a single source of truth (most likely living in a shared
//! `relon-trace-abi` crate that both `relon-trace-emitter` and
//! `relon-trace-jit::runtime` depend on).
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

pub use call_table::{
    register_external_call, resolve_external_call, with_call_table, ExternalCallTable,
    __relon_trace_resolve_call,
};
pub use deopt::{
    DeoptStateSnapshot, GenericState, RecoverableWriteRecord, TraceContext,
    __relon_trace_save_deopt,
};
