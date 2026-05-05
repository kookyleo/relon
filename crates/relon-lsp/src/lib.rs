//! LSP plumbing for Relon.
//!
//! The crate is split into:
//!
//! * [`diagnostics`] — pure conversion from `relon-analyzer` diagnostics
//!   to `lsp_types::Diagnostic`. Easy to unit-test.
//! * [`server`] — the `lsp-server` glue (initialize, document store,
//!   request/notification dispatch). Uses synchronous I/O so the crate
//!   keeps a small dependency footprint.
//!
//! The `relon-lsp` binary wires `server::run_stdio()` into a stdio
//! transport. Hosts that want a custom transport (e.g. embedding the
//! server in another process) can call into `server` directly.

pub mod diagnostics;
pub mod features;
pub mod server;
