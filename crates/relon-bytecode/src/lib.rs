//! v6-δ M2-A: stack-based bytecode VM with IR-PC bookkeeping.
//!
//! ## Rationale
//!
//! v6-γ trace JIT landed; v6-δ M1 cleared 4 of 5 residuals but R3
//! (partial-resume from a deopt'd trace at the exact next IR op)
//! cannot complete inside the tree-walker because the tree-walker
//! walks the parser AST, not a flat op stream — it has nowhere to
//! anchor an "external_pc → (block, ip)" table. This crate introduces
//! a bytecode VM that maintains that table, opening the door for
//! pixel-perfect partial-resume in M2-B.
//!
//! ## Architecture
//!
//! 1. [`BcOp`] flat opcode set mirroring the subset of [`relon_ir::Op`]
//!    that the cranelift legacy-i64 entry shape consumes (arith,
//!    comparison, control flow, locals, return). The buffer-protocol
//!    shape is deliberately out of scope for M2-A — that envelope
//!    requires arena-laid-out String/List records and is much wider
//!    than what a "scaffold" milestone can absorb without risking
//!    correctness gaps.
//! 2. [`BcFunction`] holds the compiled op stream plus a parallel
//!    `ir_pc_map: Vec<ExternalPc>` so every bytecode index can be
//!    rewound to the IR-side op it was lowered from.
//! 3. [`compile_function`] turns a [`relon_ir::Func`] into a
//!    [`BcFunction`]. Branch offsets resolved to bytecode indices via
//!    a two-pass walk that mirrors the wasm `block` / `loop` /
//!    `br` / `br_if` / `br_table` discipline.
//! 4. [`vm::BytecodeVm`] runs the stack-based dispatch loop —
//!    match-based (computed-goto would require nightly + the
//!    `naked_functions` road; the perf gap is not the M2-A target,
//!    M2-C inline-cache work is).
//! 5. [`BytecodeEvaluator`] implements [`relon_eval_api::Evaluator`]
//!    so hosts can swap it in via the `Backend::Bytecode` enum
//!    variant exposed by `relon::new_evaluator`.
//!
//! ## Sandbox prongs
//!
//! All four hard sandbox guarantees from the v5 design doc fire in
//! the VM, mirroring the tree-walker `RuntimeError` shapes:
//!
//! - **bounds**: `BoundsViolation` traps from oversized
//!   `BrTable` / out-of-bounds bytecode jumps.
//! - **trap**: `DivisionByZero`, `NumericOverflow` lift through the
//!   same `RuntimeError` variants the tree-walker emits.
//! - **capability**: a [`CapabilityVtable`] indexed by `cap_bit`; an
//!   absent slot trips [`RuntimeError::WasmCapabilityDenied`] without
//!   ever calling the host fn. v6-δ M2-A only carries the surface —
//!   the cranelift-AOT capability vtable is the canonical lookup
//!   path today, so the VM's vtable starts empty.
//!   **M2-B phase 1**: the vtable now accepts an optional
//!   `Arc<dyn relon_eval_api::CapabilityGate>` so the unified P0-B
//!   policy boundary can be installed ahead of native-fn dispatch
//!   wire-up. Phase 1 only parks the hook — callers register a gate
//!   via [`BytecodeEvaluator::with_capability_gate`] or
//!   [`vm::CapabilityVtable::set_gate`]; native-fn dispatch + the
//!   in-VM consult follow in subsequent phases (see
//!   `docs/internal/rfc-m2-b-bytecode-jit-integration-2026-05-21.md`).
//! - **resource**: an instruction counter (`BcVmConfig::max_steps`)
//!   plus a per-call deadline. Ticks once per bytecode op so the
//!   tree-walker's `WasmStepLimitExceeded` shape is reachable.

#![forbid(unsafe_code)]
#![deny(missing_docs)]
#![allow(unused_assignments)] // mirrors the rustc-1.93 false positive
                              // pattern used in the evaluator + eval-api crates.

pub mod compile;
pub mod evaluator;
pub mod op;
pub mod vm;

pub use compile::{compile_function, BcCompileError};
pub use evaluator::{BytecodeError, BytecodeEvaluator, ResumeMetrics};
pub use op::{BcFunction, BcOp, ExternalPc, StackOrigin};
pub use vm::{BcVmConfig, BcVmError, BytecodeVm, VmValue};
