# review-improvement #156: Codegen emit_op via OpVisitor

Stage: 2026-05-21
Worktree: `agent-a0c8f164870acdf73`
Base HEAD: `206b663` (local `main`, no `origin` fetch)

## Problem

P0-A landed `relon_ir::OpVisitor` + `walk_op` and wired bytecode +
ConstPool, but `codegen-native::Codegen` kept a hand-rolled 78-arm
`match op` (308 source lines) inside `emit_op`. Last `Op` consumer
not routed through `walk_op` — defeated the compile-time
exhaustiveness guarantee for new variants.

### emit_op arm audit (before)

| category | arms | shape |
|---|---|---|
| One-line `self.emit_*` delegate | 37 | trivial |
| Const-pool table fetch | 10 | 6-9 lines (ok_or_else + iconst + push) |
| `ConstString` / `ConstList*` | 4 | 10 lines (same pattern, different map) |
| Multi-statement bespoke | 23 | 3-15 lines |
| `other =>` catch-all (13 variants) | 1 | one `unsupported` arm |

## Change

Three commits on `worktree-agent-a0c8f164870acdf73`:

- `b2c91ea` — `emit_const_pool_address(offset, label)` in new
  `const_pool_emit.rs`; rewrites the 10 `Op::*TableAddr` arms onto it.
- `4cc2168` — `emit_const_value(idx, kind)` (with `ConstValueKind`
  tag) in `const_pool_emit.rs`; rewrites the 4 `ConstString` /
  `ConstList*` arms.
- `71dddd2` — replaces the entire `emit_op` body with
  `relon_ir::walk_op(op, self)` and adds a full `OpVisitor` impl on
  `Codegen` in new `op_visitor.rs`. The 13 previously-unsupported
  variants get dedicated `visit_*` bodies returning the same
  `CraneliftError::Codegen("unsupported op ...")` so auto-tier
  fallback stays intact.

`emit_op` is now 3 lines (doc + dispatch + brace). Each variant lowers
through one named `visit_*` method, matching the bytecode + ConstPool
shape.

### Helpers extracted

| helper | location | replaces |
|---|---|---|
| `emit_const_pool_address` | `const_pool_emit.rs` | 10 `Op::*TableAddr` |
| `emit_const_value` + `ConstValueKind` | `const_pool_emit.rs` | 4 `Op::Const{String,List*}` |
| `unsupported(name)` free fn | `op_visitor.rs` | old catch-all |

Task plan's Step 4 (extract `Op::Select` / `Op::Trap` /
`Op::Add(String)` to sub-files) skipped — each fits in 4-8 lines
inside its `visit_*` body and lacks a second consumer to justify
the indirection.

## Verification

- `cargo fmt --all --check`: clean
- `cargo clippy --workspace --all-targets -- -D warnings`: clean
- `cargo test --workspace`: **2158 passed; 0 failed**
- `cargo check --target wasm32-unknown-unknown -p relon-wasm`: clean

IR bit-identical: each `visit_*` calls the same `emit_*` helper the
matching `match` arm called, with identical argument order. No emit
reordering. The 13 unsupported variants still surface the same
`unsupported op in v5-beta-2 stage 3: <name>` error string, only
keyed on the variant name rather than `format!("{op:?}")`.

## Walk_op switch status

Done. All four `Op` consumers (bytecode `CompileState`, ConstPool
layout scan, codegen-native `Codegen`, future wasm-AOT) now dispatch
through the shared `walk_op` driver. Adding an `Op` variant fails
this crate's build until a matching `visit_*` method lands.

## Follow-up

Open: widening cranelift coverage to `IrType::F64` arms in
`visit_add` / `visit_eq` / etc. — localised within each `visit_*`
method now that the top-level dispatch is shared. Step 4 (bespoke
sub-files) can be picked up if a second consumer emerges.
