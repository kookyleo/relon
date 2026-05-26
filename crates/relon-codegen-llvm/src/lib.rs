//! LLVM-backed AOT evaluator for Relon. **Phase A bootstrap.**
//!
//! This crate is the first slice of the dual-backend strategy
//! decided in Phase A: cranelift keeps the trace-JIT throne
//! (`relon-codegen-native`) and a new LLVM AOT pipeline starts here
//! aimed at chasing Rust-native peak performance.
//!
//! ## Scope (Phase A)
//!
//! - Single `#main(Int...) -> Int` entry shape (the legacy-i64
//!   envelope the cranelift crate's `from_ir_direct` path consumes).
//! - Ops: `ConstI64` / `ConstI32` / `ConstBool` literals, `LocalGet`
//!   for parameter access, binary arithmetic
//!   (`Add` / `Sub` / `Mul`) on `I64`, and `Return`.
//! - Drives LLVM's MCJIT engine in-process so the bootstrap test can
//!   round-trip an IR module → emitted LLVM IR → native code →
//!   typed return value without leaving the test binary.
//!
//! Everything past the Phase A envelope (LetSet / Block / Loop /
//! Call / closures / stdlib / sandbox vtable / object cache) is
//! deferred to Phase B/C. The cranelift crate covers those today;
//! the LLVM emitter widens behind feature work tracked in the
//! design notes.
//!
//! ## What this crate deliberately does **not** do today
//!
//! - **Sandbox / capability vtable** — the cranelift crate's
//!   `SandboxState` / `__relon_capability_vtable` integration stays
//!   put. Phase B introduces the equivalent helper-call surface
//!   through inkwell's `add_function` + `ExecutionEngine::add_global_mapping`.
//! - **`.o` / `.so` emit + dlopen** — the Phase A bootstrap uses
//!   the in-process MCJIT engine. Phase A.4 keeps the surface
//!   single-knob so we can swap MCJIT for ORC + write-to-file
//!   without breaking the [`LlvmAotEvaluator`] API.
//! - **Buffer-protocol entry** — `lower_workspace_single` always
//!   emits buffer-protocol IR (`[I32, I32, I32, I32, I64] -> I32`).
//!   The cranelift crate handles that envelope today; the LLVM
//!   crate's `from_source` path is therefore stubbed until Phase B
//!   adds the matching emitter.
//!
//! ## Decision log (Phase A.1)
//!
//! Picked `inkwell` over `llvm-sys` and external `clang`/`llc`:
//!
//! - `inkwell` 0.9.0 with the `llvm18-1` feature pins llvm-sys
//!   181.3.0 against the system LLVM 18.1.3 install at
//!   `/usr/lib/llvm-18`. Safe Rust wrappers eliminate the per-op
//!   `unsafe` block the raw FFI path would impose.
//! - `llvm-sys` would force every IR-builder call through `unsafe`
//!   raw pointer arithmetic — maintainability cost on the AOT
//!   widening Phase B/C is too high for the same target set.
//! - `clang`/`llc` shell-out drops in-memory JIT verification (we
//!   want a smoke test to round-trip without writing a file) and
//!   bloats cold-start with subprocess fork/exec latency. `opt`
//!   piping also forces stringly-typed IR generation that's awkward
//!   to debug.

mod emitter;
mod error;
mod evaluator;

pub use error::LlvmError;
pub use evaluator::LlvmAotEvaluator;
