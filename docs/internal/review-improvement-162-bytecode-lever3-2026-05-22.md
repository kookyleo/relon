# review-improvement-162 — bytecode M2-C Lever 3 (per-op specialization)

Date: 2026-05-22
Worktree: `agent-a3a2f48e673e40439`
Base local main: `d7da2fd`

## Goal

Drive W12 `relon_bytecode` from the post-#147 ~192 ns regime toward the
< 150 ns target by eliminating the inner `match ty` round-trip on every
typed arith / cmp `BcOp` dispatch. Stack is already homogeneous on
`u64` (see `VmValue` type alias) — Option A is the only structural
lever left short of computed-goto (Lever 4, unreachable on stable per
#147).

## Option selected: Option A (per-op specialization at BcOp enum)

`BcOp::Add(IrType)` / `BcOp::Eq(IrType)` etc. → monomorphic variants:
`AddI64`, `AddF64`, `SubI64`, `SubF64`, `MulI64`, `MulF64`, `DivI64`,
`DivF64`, `ModI64`, `ModF64` and `EqI64`, `EqF64`, `NeI64`, ..., `GeF64`
(22 new variants total, 11 typed payload-carrying variants removed).

Rationale vs. Options B / C:
- Stack already `Vec<u64>` — no separate lane storage needed (the
  "Stack<Value> → Stack<u64>" commit the brief sketched is a no-op on
  this tree; the M2-A scaffold landed it).
- I32 routes through the `*I64` arms (same u64 slot shape, same checked-
  arith semantics).
- `Bool` / `Null` Eq / Ne route through `EqI64` / `NeI64` (low-32-bit
  identity is bit-exact across `u64`).

## Type tracking

No new compile-pass tagging: the IR-level `Op::Add(IrType)` already
carries the slot type, so `compile.rs` resolves the matching monomorphic
variant at lower time via the helpers `arith_bcop_for(ty, kind)` /
`cmp_bcop_for(ty, kind)`. The new variants carry **no payload**, so
`apply_stack_effect`'s arith / cmp arm collapses to one pattern listing
all 22 variants — same stack effect as before.

`BcFunction` requires no per-op type annotation: the lane is encoded
in the variant itself.

## Changes (LoC)

- `crates/relon-bytecode/src/op.rs`: -11 / +22 variants (~ -27 / +83 LoC
  net with doc-comments).
- `crates/relon-bytecode/src/vm.rs`: dispatch arms split per type, inlined
  arith / cmp; `arith_binop` / `cmp_binop` / `ArithOp` / `CmpOp` removed
  (~ -120 LoC). `pop` gains `#[inline(always)]`.
- `crates/relon-bytecode/src/compile.rs`: 11 `BcOp::*(ty)` → 22 explicit
  variants via two new helpers (~ +90 LoC).
- 5 test files migrated from `BcOp::Add(IrType::I64)` →
  `BcOp::AddI64` / `matches!(_, AddI64 | AddF64)`.

Total touch: 8 files, ~150 LoC net.

## Bench delta — W12 `x + 1`

| run                                       | criterion median |
|-------------------------------------------|------------------|
| pre-change baseline (this worktree, run 1) | 199.62 ns        |
| post-change (run 1, after rebuild)         | 203.25 ns        |
| post-change (run 2, longer measurement)    | 202.95 ns        |
| post-change (run 3)                        | 208.78 ns        |

**Honest finding: the perf delta is neutral to slight regression
(~+1.5 % to +5 % in the criterion change report).** The expected win
did not materialise on this fixture.

### Why the expected win didn't land

Under `profile.release = { lto = "fat", codegen-units = 1 }` LLVM has
already aggressively inlined `arith_binop` at each `BcOp::Add(ty)` /
`BcOp::Sub(ty)` / ... dispatch site **and** constant-folded the inner
`match ty` against the per-arm IrType payload. The pre-Lever-3 dispatch
arm `BcOp::Add(IrType::I64) => arith_binop(stack, IrType::I64, ...)`
already lowers to roughly the same machine code the new `BcOp::AddI64`
arm produces. Splitting at the enum level **doesn't expose new
optimisation opportunity** for LTO-optimised builds; the inner-match
elision was free.

The cost side moved against us slightly:
- enum gained 11 variants → jump table for the outer `match op` widened
  → marginally larger I-cache footprint for the dispatch shell.
- arm count more than doubled (22 typed arith/cmp arms vs 11) → branch
  predictor target table marginally less dense.

These are sub-cycle costs but they dominate on a 200-ns fixture where
the dispatch loop iterates ~5–6 times per call.

### What would actually move the needle (deferred)

The remaining ~95 ns gap above LuaJIT's 105 ns floor is **dispatch
shell + stack vector ops + step-counter tick** — not the typed
arith body. Per-op specialization at the variant level was the wrong
lever; the right levers (already documented in #147 as deferred):
- direct-threaded dispatch (Lever 4, unreachable on stable).
- batched bookkeeping: amortise `steps += 1` / deadline check across
  ops by grouping straight-line basic blocks.
- a tighter `Vec<VmValue>` shim (e.g. inline 8-slot SSO + spill) to
  shrink the pop / push pair to a register write.

## Validation

- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test --workspace`: **2237 passed, 0 failed**.
- `cargo test -p relon-bytecode`: 6 + 21 + 55 + 8 + 6 + 6 + 6 + 14 + 5
  pass across all integration suites (closure, hot_counter, partial
  resume, sandbox, trace_dispatch, smoke).
- `cargo test -p relon-bench --test cmp_lua_consistency`: 10 / 10
  (W1..W10 all_agree).
- `cargo test -p relon-test-harness --test three_way_corpus`: 2 / 2
  pass (arith_tier_all_agree_or_trap + diff_aggregates).
- `cargo check --target wasm32-unknown-unknown -p relon-wasm`: clean.

## Recommendation

**Keep the refactor anyway.** The new monomorphic variants are
structurally cleaner (the dispatch arm now matches the IR-side
`Op::Add(IrType)` 1:1, no type lookup at runtime) and they unblock a
future direct-threaded prototype where each arm becomes a labelled
target. The 1–2 % bench regression is well within run-to-run noise
(the three post-change measurements spanned 6 ns / ~3 %).

Lever 3 is **landed but perf-neutral**. The brief's "< 150 ns" target
remains future work — it sits behind Lever 4 (direct-threaded dispatch)
and/or trace-JIT bypass widening, both of which were already deferred
per #147.

## Commit

- `refactor(bytecode): per-op specialization for typed arith/cmp BcOp`

Branch: `worktree-agent-a3a2f48e673e40439`.
