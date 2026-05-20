# review-improvement P0-A: OpVisitor trait for backend dispatch

Stage: 2026-05-21
Worktree: `agent-a3ed7712fe5519fba`
Base HEAD: `726ff7ed74bd456a5ce78407724a2b89adad47e2`

## Problem audit

`relon-ir::Op` exposes **72 variants** (const literals, locals, arith /
cmp, control flow, record construction, calls, raw memory, closures,
and ~12 Unicode-table address ops). Three backends consume the stream
through independent `match op { ... }` blocks:

| backend                    | file                                | Op:: arms |
|----------------------------|-------------------------------------|-----------|
| relon-bytecode             | `src/compile.rs`                    | 66        |
| relon-codegen-native       | `src/codegen.rs` (collect + emit)   | 99        |
| relon-evaluator (AST tree) | `src/eval.rs`                       | n/a — walks AST `Node`, not `Op` |

The tree-walker dispatches on `Expr::*` (38 arms over the parser
surface), not on IR ops — historically the original review's claim of
"100+ Op arms in eval.rs" was stale. The actual `Op` dispatch
duplication is concentrated in **two** crates (bytecode + codegen-
native), each carrying separate code-paths for the same variant set.
Adding a new Op variant required at minimum two synchronised arm
updates; missing one surfaces only via the differential corpus.

## Trait design

`crates/relon-ir/src/op_visitor.rs` introduces `pub trait OpVisitor`
with one method per variant (no default bodies). The driver
`walk_op(&Op, &mut V)` performs the single canonical split; any new
`Op` variant breaks `walk_op` compilation until both the driver and
every concrete impl add the matching method. `walk_body` is a
convenience that walks a `&[TaggedOp]` slice and returns the per-op
outputs.

Design constraints honoured:

- `Result<Self::Output, Self::Error>` associated types — backends pick
  their own emit-value + error types without paying a `Box<dyn Error>`
  bill.
- `Copy` payloads (`u32`, `IrType`, `OrderedFloat<f64>`) by value;
  owned data (`Vec`, `String`, `[ClosureCapture]`) by slice / `&str`.
- Nested op-stream payloads (`If` / `Block` / `Loop`) passed verbatim;
  visitors decide whether to recurse via `walk_body`.
- No `dyn` indirection — monomorphic dispatch keeps hot-path codegen
  identical to the previous match arms.
- Module-level unit test (`CountingVisitor`) exercises three
  representative variants and asserts dispatch routing.

## Backend migration

**relon-bytecode** is fully migrated. `CompileState` now `impl
OpVisitor for CompileState<'_>` (72 methods); `compile_one` shrank to
a two-line shim that bumps the IR PC and dispatches via `walk_op`.
The historical "unsupported Op" catch-all is replaced by 35 explicit
`unsupported("VariantName")` methods — adding a new variant forces the
bytecode crate to either land a real lowering or explicitly mark it
unsupported with a deterministic diagnostic. The inline-call path
(`compile_inline_one`) is intentionally left on its dedicated match
because it carries a different dispatch context (inlining state
locals/lets) and intersecting it with the visitor surface would
re-introduce the duplication problem inside a single crate.

**relon-codegen-native** + the tree-walker stay unchanged this phase.
Migrating cranelift requires threading the `&mut FunctionBuilder` +
operand-stack helpers through the trait surface (high churn, deferred
to the follow-up).

## LoC delta

- `crates/relon-ir/src/op_visitor.rs` — 833 LoC (new file: 269 LoC
  trait + dispatch driver, 561 LoC test visitor + module tests + docs).
- `crates/relon-ir/src/lib.rs` — +2 LoC (`pub mod` + re-export).
- `crates/relon-bytecode/src/compile.rs` — 833 → 1441 LoC
  (`+555 ins / -200 del`). The growth is the per-method visitor surface
  trading a dense match (with 35 silent "other" fallbacks) for 72
  explicit methods, including a `unsupported(label)` helper.

The visitor file is one-time growth; the next backend migration adds
zero LoC to `op_visitor.rs` and removes proportional match
duplication from its host crate.

## Gates

- `cargo fmt --all --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo test --workspace` — **2010 tests** pass (3 new in
  `relon_ir::op_visitor::tests`).
- `cargo check -p relon-wasm --target wasm32-unknown-unknown` — clean.

## Follow-up path

1. Migrate `relon-codegen-native::Codegen::emit_op` (≈1.5 kLoC of arms)
   onto the visitor — biggest payoff. The cranelift backend's
   `collect_op` (const-pool scan) is a separate, smaller visitor pass.
2. Decide whether `compile_inline_one` should share the same surface
   via a small wrapper that overlays `local_base` / `let_base` —
   probably yes once codegen-native is on it.
3. Once both codegen backends are visitor-driven, the static guard
   "adding a new Op breaks every backend" becomes structurally true
   instead of process-driven.
