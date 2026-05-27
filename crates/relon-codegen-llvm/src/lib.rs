//! LLVM-backed AOT evaluator for Relon. **Phase B production envelope.**
//!
//! This crate is the second slice of the dual-backend strategy
//! decided in Phase A: cranelift keeps the trace-JIT throne
//! (`relon-codegen-native`) and the LLVM AOT pipeline here chases
//! Rust-native peak performance for the `#main` entry path.
//!
//! ## Scope (Phase B)
//!
//! - Two entry shapes accepted:
//!   - **Legacy-i64** (`(I64...) -> I64`) for `from_ir_direct`
//!     callers (tests, bench fixtures) — the Phase A bootstrap
//!     envelope, retained for cross-backend comparison.
//!   - **Buffer-protocol** (`(*state, i32, i32, i32, i32, i64) -> i32`)
//!     for `from_source` callers. Matches the cranelift backend's
//!     `EntryShape::BufferProtocol` so the runtime envelopes line up.
//! - Source-driven pipeline (`from_source`): parse + analyze +
//!   lower (`relon_ir::lower_workspace_single`) + LLVM emit + JIT
//!   compile + per-call arena dispatch. The cmp_lua W1 / W2
//!   workloads (list.sum(range(n)) / list.sum(range(n).map(...))) go
//!   end-to-end through this path.
//! - Op set covers what `lower_workspace_single` synthesises for
//!   the W1 / W2 shape after the IR's `range_pipeline` peephole has
//!   collapsed `range.map.sum` into a single accumulator loop:
//!   `LocalGet`, `ConstI64` / `ConstI32` / `ConstBool`, `LetGet` /
//!   `LetSet`, `LoadField` / `StoreField` (scalar slots),
//!   `Add` / `Sub` / `Mul` / `Div` / `Mod` / `BitAnd` (I32 + I64),
//!   `Eq` / `Ne` / `Lt` / `Le` / `Gt` / `Ge`, structured control flow
//!   (`Block` / `Loop` / `Br` / `BrIf` / `If`), and `Return`. The IR's
//!   peephole turns `list.sum` / `list.map` / `iter.len` into the
//!   above op set directly — no stdlib call indirection needed.
//!
//! Everything past the Phase B envelope (sandbox traps, pointer-
//! indirect StoreField, MakeClosure / CallClosure, schema-method
//! dispatch, stdlib call surfaces beyond peephole-inlined shapes)
//! stays parked on the cranelift backend. Phase C widens the emitter
//! when the cmp_lua W3..W12 work calls for it.
//!
//! ## What this crate deliberately does **not** do today
//!
//! - **Sandbox traps / capability vtable** — Phase B does not emit
//!   `__relon_raise_trap` / capability-bit checks. `Div(I64)` /
//!   `Mod(I64)` lower to LLVM's `sdiv` / `srem`, which are UB on
//!   div-by-zero and produce host-level signals. Bounds checks on
//!   `LoadField` / `StoreField` are also omitted (the host owns the
//!   arena and the IR's static offsets fit). Phase C wires the
//!   helper-call surface for sandbox parity.
//! - **`.o` / `.so` emit + dlopen** — Phase B still uses the
//!   in-process MCJIT engine. The single-knob `OptimizationLevel`
//!   API hides the engine choice so Phase C / ORC migration is a
//!   localised diff.
//! - **Pointer-indirect StoreField** — Phase B accepts only scalar
//!   `LoadField` / `StoreField` (I32 / I64 / F64 / Bool / Null). The
//!   IR's tail-cursor protocol for String / ListInt returns stays on
//!   the cranelift backend. W1 / W2 only emit scalar Int returns,
//!   so this is sufficient for the Phase B target workloads.
//! - **MakeClosure / CallClosure** — the IR's `range.map(...).sum`
//!   peephole inlines the closure body directly into the loop, so
//!   no first-class closure surface is needed. Closures past the
//!   peephole (W3 / W4 / W9) move with Phase C.
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
mod state;
mod str_helpers;

pub use error::LlvmError;
pub use evaluator::LlvmAotEvaluator;
