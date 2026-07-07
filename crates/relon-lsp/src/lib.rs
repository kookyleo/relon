#![forbid(unsafe_code)]
//! Relon Language Server.
//!
//! Implements the editor-facing LSP capabilities exposed by relon-cli's
//! `relon lsp` subcommand and re-used by `relon-wasm-bindings` for the browser
//! playground:
//!
//! - hover (`textDocument/hover`)
//! - completion (`textDocument/completion`)
//! - go-to-definition (`textDocument/definition`)
//! - find-references (`textDocument/references`)
//! - rename (`textDocument/rename`)
//! - code-actions (`textDocument/codeAction`)
//! - document-symbols (`textDocument/documentSymbol`)
//! - inlay-hints (`textDocument/inlayHint`)
//! - signature-help (`textDocument/signatureHelp`)
//! - formatting (`textDocument/formatting`, via `relon-fmt`)
//! - publish-diagnostics (analyzer-derived warnings / errors)
//!
//! ## Crate layout
//!
//! - [`diagnostics`] — pure conversion from `relon-analyzer`
//!   diagnostics to `lsp_types::Diagnostic`. Easy to unit-test.
//! - [`features`] — per-capability adapters that pull the static
//!   analysis out of `relon-analyzer` and shape it into the wire
//!   types `lsp_types` expects.
//! - `position` (private) — UTF-16 line/character ↔ byte-offset
//!   conversions shared by every feature module.
//! - [`server`] — the `lsp-server` glue (initialize, document store,
//!   request/notification dispatch). Uses synchronous I/O so the crate
//!   keeps a small dependency footprint.
//! - [`workspace`] — multi-file project tracking (the analyzer's
//!   workspace tree, kept in sync with the editor's open document
//!   state).
//!
//! ## Hosting
//!
//! The `relon-lsp` binary wires [`server::run_stdio`] into a stdio
//! transport — the default integration path for VS Code, Neovim and
//! other editors. Hosts that want a custom transport (e.g. embedding
//! the server in another process, or driving it from the WASM
//! playground's browser-side adapter) can call into [`server`]
//! directly without going through stdio.

pub mod diagnostics;
pub mod features;
pub(crate) mod position;
pub mod server;
pub mod workspace;
