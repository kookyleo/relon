//! Internal ABI between Relon's compiled execution surfaces.
//!
//! This crate is **not** a host embedding API — hosts configure and
//! drive evaluation through `relon-eval-api` (`Context`, `Value`,
//! `Evaluator`, capabilities). What lives here is the machinery the
//! compiled backends (`relon-ir` lowering, cranelift AOT, LLVM AOT,
//! `relon-rs-shims`) share so they agree byte-for-byte on how values
//! cross the machine-code boundary:
//!
//! * [`schema_canonical`] — the canonical wire [`Schema`](schema_canonical::Schema)
//!   / [`TypeRepr`](schema_canonical::TypeRepr) plus the deterministic
//!   32-byte schema hash carried in the `relon.abi` custom section.
//! * [`schema_lower`] — analyzer `SchemaDef` → canonical `Schema`
//!   lowering, so every backend derives the wire schema from one
//!   implementation.
//! * [`layout`] — fixed-record offset tables
//!   ([`OffsetTable`](layout::OffsetTable)) that size the binary
//!   handshake buffer; codegen and host marshalling must compute
//!   identical offsets from the same schema.
//! * [`buffer`] — [`BufferBuilder`](buffer::BufferBuilder) /
//!   [`BufferReader`](buffer::BufferReader), the typed writer/reader
//!   pair for the host <-> compiled-code binary handshake.
//! * [`verifier`] — bounds/shape verification that certifies a buffer
//!   or arena region before any reader dereferences it.
//! * [`inplace_return`] — the backend-shared host side of the in-place
//!   region-walk return ABI (negative-sentinel decode → region select →
//!   verify → in-place read).
//!
//! Everything here is an implementation contract between in-tree
//! backends. It is versioned with the workspace and offers no
//! stability promise to external consumers; layout and buffer
//! semantics in particular are load-bearing for compiled-code memory
//! safety (see `verifier` and the ADRs under `docs/internal/adr/`),
//! so changes must keep every backend and the mirrored `rs-shims`
//! structs in lockstep.

#![forbid(unsafe_code)]
// rustc ≥ 1.93 false-positive: `unused_assignments` fires on fields of every
// `#[derive(thiserror::Error)]` enum (the derive expands to internal
// let-bindings that the lint mis-reads). Mirror the eval-api crate's allow
// and drop it once the rustc fix lands.
#![allow(unused_assignments)]

pub mod buffer;
pub mod inplace_return;
pub mod layout;
pub mod schema_canonical;
pub mod schema_lower;
pub mod verifier;
