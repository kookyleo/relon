# Review-Improvement #130: codegen-native continued split + ConstPool OpVisitor PoC

Date: 2026-05-21
Branch: `worktree-agent-a6dcdf4b6f04d9caf`
Base: `b19f303`

## Scope

Phase-2 follow-on to P0-A (`OpVisitor` trait) + P1-A (codegen
sub-file split). Two threads:

1. `impl OpVisitor for ConstPool` PoC — replace the legacy
   `match op { Op::ConstString { .. } => ..., _ => {} }` with full
   dispatch so a new const-bearing variant fails compile instead of
   slipping past the catch-all.
2. codegen/mod.rs sub-file extraction round 2 — five new sub-files
   on top of the four P1-A landed.

## mod.rs LoC audit (pre-round-2 = 3458 LoC)

| Category       | Lines moved | Sub-file              |
| -------------- | ----------- | --------------------- |
| control-flow   | ~513        | `control_flow.rs`     |
| record         | ~218        | `record.rs`           |
| closure        | ~167        | `closure.rs`          |
| call dispatch  | ~250        | `call.rs`             |
| field / buffer | ~333        | `field.rs`            |

Trunk leftovers: build pipeline (`compile_module_with` /
`compile_module_to_object_bytes` / `lower_module_into`), the
`Codegen` struct + dispatch `emit_op` + locals plumbing
(`get_local` / `get_let` / `set_let` / `push` / `pop` / `emit_body`),
and `emit_hot_counter_inject`.

## ConstPool OpVisitor landing notes

`type Output = ()`, `type Error = CraneliftError`. Const-bearing
variants get real bodies; every other variant gets a one-line
`Ok(())` so the compiler refuses any new variant that lacks a method.
`If` / `Block` / `Loop` / `Call` recurse via `walk_body`, preserving
arm-order traversal. Three new unit tests pin declaration-order
layout, duplicate-idx no-op, and If-arm recursion.

**Extending the pattern to `Codegen`?** Not yet. `ConstPool` had ~10
interesting variants vs ~60 no-ops. `Codegen::emit_op` has ~30
interesting variants plus inter-method dependencies through
`inline_frames` + let-locals window. Cleaner path: keep splitting
the sub-files so `emit_op` becomes one-line-per-arm, *then* swap to
`OpVisitor`. Deferred to phase 4.

## Verification

* `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`, and `cargo test --workspace` (2032 passing) all
  clean.
* `cargo test --workspace --test cmp_lua_consistency` →
  W1..W10 all_agree.
* `wasm32-unknown-unknown` builds clean for every wasm-targeted
  crate (codegen-native is host-only).

ConstPool bit-identity proof: the three new
`const_pool::tests::*` cases assert raw byte layout (`pool.bytes[..]`
prefix + payload). The dispatch re-route changes the call shape but
not the byte order — every variant re-emits the same prefix + value
bytes through the same `align_to(4|8)` gates the legacy `match` used.
cmp_lua_consistency W3 (string_concat) + W5 (dict_str_key) — which
exercise ConstString + ConstListInt + multi-record layout — pass
unchanged, confirming end-to-end byte identity.

## LoC delta

* mod.rs: 3458 → 1906 LoC (-45 %, -1552 LoC).
* nine sibling sub-files total 3096 LoC.
* Max per sub-file: 626 LoC (`control_flow.rs`); next is
  `const_pool.rs` 742 LoC (~150 of which is the OpVisitor no-op
  boilerplate).

## Commits

```
9c279f8 refactor(codegen-native): extract field / buffer sub-module
1879607 refactor(codegen-native): extract record / closure / call sub-modules
7d81020 refactor(codegen-native): extract control-flow into sub-module
012d444 refactor(codegen-native): impl OpVisitor for ConstPool
```

## Phase-3 blueprint

Three remaining clusters:

1. **Build pipeline** (~700 LoC) — `compile_module_with` /
   `_to_object_bytes` / `lower_module_into` + the `EntryShape` /
   `CompiledModule` types. Candidate `pipeline.rs`. Risk: ISA-flag
   / JIT-vs-object / lambda-loop cross-cutting makes the cut harder
   than a per-op category.
2. **HotCounter prologue** (~115 LoC) — `emit_hot_counter_inject`
   is self-contained; quick extraction to `trace_prologue.rs`.
3. **Codegen + dispatch** (~450 LoC) — `Codegen` struct +
   `emit_op` + locals helpers. Natural to leave as the trunk.

Recommended order: extract HotCounter prologue first (low risk),
then if `emit_op` survives further compression after that, do the
`OpVisitor` swap. Build-pipeline extraction is the highest-LoC
saver but also the riskiest; leave for last.
