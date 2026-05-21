# Review-Improvement #134: codegen-native phase 3 — HotCounter prologue carved

Date: 2026-05-21
Branch: `worktree-agent-aa6438a80d58b7f8e`
Base: `c55bfb3` (#133 Wave A completion)

## Scope

Phase 3 of the codegen-native sub-file split. Per the #130 stage
report the trunk cluster left after round 2 was:

- `compile_module_with` build pipeline (~700 LoC, risky)
- HotCounter prologue `emit_hot_counter_inject` (~115 LoC, easy win)
- `Codegen` + `emit_op` dispatch (~450 LoC, natural trunk; OpVisitor
  swap deferred)

This phase lands part (a) only (HotCounter extract). Part (b) — the
`emit_op` -> `walk_op(op, &mut codegen)` switch — is honestly
deferred to phase 4 after the audit below.

## (a) HotCounter prologue extract

`codegen/hot_counter.rs` — new sub-file, 177 LoC including module
doc + the existing inline rationale comments. The carved body
matches the original 1-to-1; only the imports were narrowed:
`IntCC`, `BlockArg`, `StackSlotData`, `StackSlotKind` and the
`CallConv` / `AbiParam` / `Signature` set move with the helper.

Interface:

```rust
pub(super) fn emit_hot_counter_inject(
    builder: &mut FunctionBuilder<'_>,
    pointer_ty: cranelift_codegen::ir::Type,
    entry_shape: super::EntryShape,
    fn_id: u32,
    arg_values: &[cranelift_codegen::ir::Value],
);
```

mod.rs LoC: 1906 -> 1764 (-142). Three imports drop from the trunk
header (`BlockArg`, `StackSlotData`, `StackSlotKind`); the rest stay
because they're still referenced by the entry / lambda signature
construction.

## (b) emit_op arm audit

`emit_op` body spans 395 lines covering 78 `Op::*` arms:

| Arm shape                       | Count |
| ------------------------------- | ----- |
| `=> self.emit_xxx(..)?` 单行    | ~40   |
| `=> { ... }` 多行块             | 41    |

The multi-line block arms cluster into three groups:

1. Const-pool address fetches (`CaseFoldTableAddr`,
   `CombiningMarkRangesAddr`, `WhitespaceRangesAddr`,
   `DecompTableAddr`, `CccTableAddr`, `CompositionTableAddr`,
   `FullCaseFoldTableAddr`, `CasedRangesAddr`,
   `CaseIgnorableRangesAddr`, `TurkishCaseFoldTableAddr`) — 10
   near-identical 4-7 line bodies that all do
   `pool.<field>.ok_or_else(...) -> iconst(I32, off) -> push`. Prime
   candidate for a `emit_const_pool_addr(&mut self, slot, label)`
   helper.
2. ConstString / ConstList* arms — 4 near-identical 12-line
   `pool.<map>.get(idx).copied().ok_or_else -> iconst -> push`
   blocks. Same shape, different map.
3. Inline-bodied arms (`Select`, `Trap`, `Add(String)`) that are
   genuinely bespoke and don't reduce to a single delegate call.

Conclusion: switching to `OpVisitor` today would mean either
(i) reproducing the multi-line bodies inside the `visit_xxx`
methods (which buys nothing — visitor's value is in compile-time
exhaustiveness, not arm slimming), or (ii) first hoisting the
const-pool clusters into shared helpers, then doing the switch.
Path (ii) is the right move but it's another full phase's worth of
careful work. Phase 3 stops at (a).

## Remaining trunk blueprint

For phase 4 the trunk leftovers in mod.rs (1764 LoC) are:

- `compile_module_with` build pipeline (~700 LoC) — entry + lambda
  function construction, signature wiring, vtable import, trap
  block setup. Risky because the entry block layout is load-bearing
  and the path is hot during cold-start.
- `Codegen` struct + `emit_op` (~450 LoC) — phase 4 (b): factor the
  const-pool address fetch arms into a shared helper, slim every
  arm to a one-line delegate, then swap `emit_op` for
  `walk_op(op, &mut *self)` via `impl OpVisitor for Codegen`.

## Gate

- `cargo fmt --all --check` — clean.
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo check -p relon-wasm --target wasm32-unknown-unknown` — clean.
- `cargo test --workspace` — 2038 passed / 0 failed (matches the
  pre-change baseline exactly).
