# Review Improvement #165 — concat_many Backend Wire-up

**Date**: 2026-05-22
**Worktree**: `/ext/relon/.claude/worktrees/agent-aa7cd7bc10e30faa6`
**Base**: `4a99e5b` (local `main` HEAD — no `fetch origin`)
**Branch**: `worktree-agent-aa7cd7bc10e30faa6`

## Context

#152 landed the SmolStr::concat_many fold in the tree-walker; the
bytecode VM, cranelift AOT, and trace-JIT still emitted N-1 pair-wise
`Op::Add(IrType::String)` allocations for the same source chain. The
#152 stage report flagged this as deferred follow-up.

## Option A vs B

Picked **Option A — IR-level `Op::StrConcatN`**. The chain fold lives
once in `relon_ir::lowering::lower_binary` (AST-spine walk of
`Expr::Binary(Add, ...)` collecting String-typed leaves) and the four
IR-consuming backends each implement a single new `visit_str_concat_n`
arm. Option B (per-backend chain recognition) would have repeated the
same spine walk three times against the lowered op stream and lacked a
single source of truth for the operand-count invariant.

## Backend Landing Status

* **tree-walker (relon-evaluator)** — unchanged. The tree-walker
  doesn't go through IR (it evaluates the parser AST directly); its
  existing `try_eval_string_concat_chain` (#152) keeps owning the
  fold. The `OpVisitor` trait does not bind it.
* **bytecode VM (relon-bytecode)** — new `BcOp::StrConcatN { argc }`
  opcode + VM dispatch. The dispatch arm clones the `argc` operand
  handles, sums lengths once, allocates one fresh `StringArena` slot,
  and `push_str`s each operand into the joined buffer. Two-operand
  `Op::Add(String)` still bails on `visit_add` (existing envelope) —
  the AST fold only emits `StrConcatN` for chains of length 3+.
* **cranelift AOT (relon-codegen-native)** — new
  `emit_str_concat_n` helper. Pops `N` `i32` arena offsets, loads
  each operand's `[len:u32]` header once into a parallel `lens`
  vector, sums to `total_len`, calls `emit_alloc_scratch` for one
  scratch record sized `total_len + 4`, stamps the header, and
  emits N `memcpy`s at the running cursor. Replaces the
  `N - 1` intermediate scratch records the unfolded `concat(...)`
  inlining used to emit.
* **trace-JIT (relon-trace-recorder)** — explicit
  `LowerOutcome::Abort(AbortReason::UnsupportedOp("StrConcatN"))`.
  Proper inline emit needs a new `TraceOp::StrConcatN` variant + a
  parallel of the `str_inline.rs` short-rhs lowering in
  relon-trace-emitter. Deferred as the user-flagged optional phase;
  the abort cleanly falls back to cranelift AOT (which has the
  single-alloc path), so no regression vs the pre-#165 baseline.

## Tests

* `relon-ir::lowering::str_concat_chain_tests` — 3 new lowering
  tests: 4-leaf and 3-leaf chains fold to one `Op::StrConcatN`,
  2-leaf concat stays on `Op::Add(String)`.
* `relon-bytecode::tests::bytecode_sandbox::str_concat_n_joins_four_handles_with_single_alloc`
  — direct VM exec of `BcOp::StrConcatN { argc: 4 }`.
* `relon-test-harness::corpus` — two new four-way differential
  cases (`str_concat_chain_four_way`, `str_concat_chain_three_way`)
  validate tree-walker / bytecode / cranelift / trace-JIT agree on
  byte-identical output.

Total workspace tests: **2256** (up from baseline 2252), 0 failures.
`cargo fmt --all --check`, `cargo clippy --workspace --all-targets
-- -D warnings`, and `cargo check --target wasm32-unknown-unknown -p
relon-wasm` all clean.

## Bench Impact

* `sso/concat_tree/heap`: pure helper benchmark — unchanged vs
  #152 baseline. `concat_many` 101 ns vs `nested_concat` 247 ns
  (≈ 2.4x), validating the single-alloc shape that #165 now wires
  through each backend.
* W3 (`string_concat`): no measurable delta — the W3 hot loop is
  `acc = acc + lit_a` per iteration (a 2-operand concat per pass,
  not a chain), so the fold gate (chain length 3+) never fires. W3
  was always going to need a different optimisation (loop-invariant
  buffer reuse / output-length forecasting) regardless of #165.
* `sso/concat_tree` backend rows are not yet wired; adding them
  would be a follow-up bench scaffold (Relon source ←→ raw helper
  bench).

## LoC delta

`+483` insertions across 8 commits, 8 files touched outside docs:

```
crates/relon-bytecode/src/compile.rs            +27
crates/relon-bytecode/src/op.rs                 +16
crates/relon-bytecode/src/vm.rs                 +49
crates/relon-bytecode/tests/bytecode_sandbox.rs +36
crates/relon-codegen-native/src/codegen/const_pool.rs   +3
crates/relon-codegen-native/src/codegen/memory.rs       +110
crates/relon-codegen-native/src/codegen/op_visitor.rs   +12
crates/relon-ir/src/ir.rs                       +24
crates/relon-ir/src/lowering.rs                 +224 (incl tests)
crates/relon-ir/src/op_visitor.rs               +25
crates/relon-test-harness/src/corpus.rs         +19
crates/relon-trace-recorder/src/lowering.rs     +10
```

## Honesty Notes

* The trace-recorder integration is the deferred bit. Real W3-style
  trace work would benefit, but the recorder lowering for
  `StrConcatN` requires both a new `TraceOp::StrConcatN(dst, &[ssa])`
  variant plus the parallel inline emitter
  (`__relon_str_concat_n_alloc` + per-rhs payload write). That's a
  separate phase — kept this commit honest by abort-ing cleanly
  rather than pretending to support it.
* No source-level pair-wise `s + t` improvement — bytecode + the
  AST fold both still leave 2-operand concat on the existing
  `Op::Add(String)` path. The fold's chain-length-≥-3 gate is the
  whole reason `BcOp::StrConcatN` doesn't displace `BcOp::StrConcat`.
