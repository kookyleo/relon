#![forbid(unsafe_code)]

//! Linear-typed IR between `relon-analyzer`'s `AnalyzedTree` and codegen
//! backends (WASM, future native / JS).
//!
//! See `docs/internal/wasm-crate-structure-2026-05-16.md` for the
//! IR-first crate split rationale, and
//! `docs/internal/wasm-binary-layout-v1-2026-05-16.md` for the binary
//! handshake protocol the IR exposes to downstream codegen.
//!
//! Skeleton only at this point — Phase 1 (smoke test) is where the
//! lowering surface lands. Held intentionally empty so the workspace
//! build / test / clippy pipeline picks up the crate without false
//! warnings about unused deps. The dev-deps below stand for the deps
//! the IR will pull in once lowering exists.

// Phase 1 will replace these with real lowering modules:
//   pub mod ir;          // IR ops + value types
//   pub mod lowering;    // AnalyzedTree -> IR
//   pub mod verifier;    // IR well-formedness checks
//   pub mod passes;      // cycle detection, dead-code elim, ...

// Suppress unused-crate-dependencies lint while the skeleton is empty.
// Removed once Phase 1 starts pulling these in.
#[allow(unused_imports)]
use miette as _;
#[allow(unused_imports)]
use relon_analyzer as _;
#[allow(unused_imports)]
use relon_eval_api as _;
#[allow(unused_imports)]
use relon_parser as _;
#[allow(unused_imports)]
use thiserror as _;
