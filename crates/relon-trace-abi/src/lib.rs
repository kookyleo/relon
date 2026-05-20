//! `relon-trace-abi` — single source of truth for the v6-γ trace JIT
//! pipeline's wire-format types.
//!
//! ## Why this crate exists
//!
//! Multiple crates need to agree byte-for-byte on the layout of the
//! types passed across the cranelift / Rust boundary:
//!
//! - [`relon-trace-emitter`] generates cranelift IR that reads /
//!   writes `TraceContext` and `DeoptStateSnapshot` fields by raw byte
//!   offset.
//! - [`relon-trace-jit`] hosts the runtime helper functions
//!   (`__relon_trace_save_deopt`, `__relon_trace_resolve_call`,
//!   `__relon_trace_inline_cache_lookup`) that the emitted IR calls
//!   into.
//! - [`relon-codegen-native`] embeds emitted traces into the generic
//!   backend's dispatch table.
//!
//! Prior to this crate each consumer redeclared a "layout-compatible
//! view". With this crate we centralise the definitions and require
//! all three consumers to **import**, never redefine.
//!
//! ## ABI invariant
//!
//! Every `#[repr(C)]` type exposed here has a load-bearing field order
//! and size. The `tests/layout_smoke.rs` integration test pins both
//! and **will refuse to pass** if any field is added, moved, or has
//! its type-width changed. Reviewers MUST update the smoke test in
//! the same PR as any intentional change.
//!
//! ## Dependency direction
//!
//! This crate sits at the **bottom** of the trace JIT dep graph; it
//! must not depend on any other `relon-*` crate. The cyclic risk if
//! it did would be high: `relon-trace-emitter` already depends on
//! `cranelift-codegen`, so any back-edge from here would mean the
//! cranelift dependency leaks into pure-runtime consumers.
//!
//! ## Feature flags
//!
//! - `serde` (off by default) — opts every ABI type into
//!   `Serialize` + `Deserialize`. Off-path consumers (trace dumpers,
//!   golden-file ABI checks) enable this; the emitter / runtime hot
//!   path never enables it to keep build / link time minimal.
//!
//! [`relon-trace-emitter`]: https://docs.rs/relon-trace-emitter
//! [`relon-trace-jit`]: https://docs.rs/relon-trace-jit
//! [`relon-codegen-native`]: https://docs.rs/relon-codegen-native

#![deny(missing_docs)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod context;
pub mod deopt;
pub mod effect;
pub mod entry;
pub mod external;
pub mod hash;
pub mod observed;

pub use context::{
    HostHookTable, TraceContext, TraceHookFn, TraceIcLookupFn, TraceResolveCallFn, TraceSaveDeoptFn,
};
pub use deopt::{DeoptStateSnapshot, RecoverableWriteRecord};
pub use effect::EffectClass;
pub use entry::{AbiSignature, AbiType, TraceEntryStatus, TRACE_ENTRY_SIG};
pub use external::{ExternalAddr, ExternalPc, ExternalSlot};
pub use hash::{fx_hash_bytes, fx_hash_key_record};
pub use observed::ObservedType;
