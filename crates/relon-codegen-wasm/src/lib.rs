#![forbid(unsafe_code)]

//! Lower `relon-ir` to WebAssembly bytecode + runtime adapter (Phase 1+).
//!
//! Implements the four locked design decisions:
//!   1. Binary memory handshake for `#main` params + return
//!      (see `wasm-binary-layout-v1-2026-05-16.md`)
//!   2. Stdlib self-contained (bundled bytecode + check_cap opcode)
//!   3. Source map + ABI metadata in custom sections
//!      (see `wasm-srcmap-section-v1-2026-05-16.md`)
//!   4. Static topological eager evaluation for dict fields
//!
//! Skeleton only at this point — Phase 1 (smoke test) lands the first
//! real lowering for `#main(Int) -> Int : x * 2`.

// Phase 1 will introduce:
//   pub mod codegen;            // IR -> wasm bytecode emit
//   pub mod runtime;            // trait WasmRuntime + WasmtimeRuntime impl
//   pub mod srcmap;             // emit / parse relon.srcmap section
//   pub mod abi;                // emit / parse relon.abi section
//   pub mod host_fn_table;      // emit / parse relon.host_fns section
//   pub mod evaluator;          // impl Evaluator for WasmAotEvaluator

// Suppress unused-crate-dependencies lint while the skeleton is empty.
// Removed once Phase 1 starts pulling these in.
#[allow(unused_imports)]
use relon_eval_api as _;
#[allow(unused_imports)]
use relon_ir as _;
#[allow(unused_imports)]
use relon_parser as _;
#[allow(unused_imports)]
use thiserror as _;
