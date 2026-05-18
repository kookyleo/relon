# v5-β-2 stage report (2026-05-18)

> **Status**: partial — 3 of 14 spec items landed this tranche. The
> remaining 11 are explicitly deferred to the next agent / session
> with rationale.
>
> **Base**: `34f5aaa merge(codegen-native): v5-beta-1 cranelift HelloWorld + 4-prong sandbox`.

## Landed this session

| Item | Scope | Outcome |
|---|---|---|
| **10 (partial)** | Differential test harness + corpus | New `crates/relon-test-harness` with `diff_test(source, args)` and 52 corpus cases across 8 tiers (ArithControl × 28, StdlibSimple × 9, StdlibMemory × 4, StdlibCaseFold × 5, StdlibList × 2, StdlibNormalize × 2, DictReturn × 2). Tree-walk reference path is fully wired; cranelift cases that fall outside the v5-β-1 envelope surface `CraneliftUnsupported` (informational) rather than failing — the harness is forward-compatible with widening. |
| **6 (partial)** | Codegen widen: `Op::Select`, `Op::Block` / `Op::Loop` with forward `Op::Br` / `Op::BrIf`, `Op::Add` / `Op::Sub` / `Op::Mul` / `Op::BitAnd` / `Op::Lt..Ge` on `IrType::I32` | These are the prerequisites for the simple-stdlib inline lowering: `abs` / `min` / `max` bodies terminate in `Select`, and stdlib helpers do pointer / length math on the I32 slot. `Block` / `Loop` with `result_ty != None` is rejected (needs block-param threading; deferred to stdlib inline tranche). |
| **v6-γ §7.2 hook** | `EffectClass` enum + `Op::effect_class()` | Doc-comment guidance from v6-γ design §7.1 was missing; β-2 promotes to a real enum. Every Op variant returns one of `Pure` / `ReadOnly` / `RecoverableWrite` / `UnrecoverableEffect`. Trace recorder integration in v6-γ M2+ will consume this directly. |

## Gates (final state)

```
cargo build --workspace                                                # green
cargo test --workspace --features 'relon/cranelift-aot'                # 1553 passed / 0 failed (was 1542 — 11 new)
cargo clippy --workspace --all-targets --features 'relon/cranelift-aot' -- -D warnings  # green
cargo fmt --all -- --check                                             # green
cargo build --target wasm32-unknown-unknown -p relon-wasm              # green
```

Baseline before this tranche: 1542 passing tests. The harness adds
2 (1 corpus runner + 1 ignored strict-mode gate). The codegen
widen adds 5 (select × 3, block + br × 1, i32 arith × 1). The
EffectClass enum adds 4. Total +11.

## Deferred (with rationale + recommendation)

| Item | Scope | Why deferred | Recommendation |
|---|---|---|---|
| **1** | Lower `Op::LoadField` / `LoadStringPtr` / `LoadFieldAtAbsolute` with `cond_trap` bounds check | The IR's load ops assume **absolute wasm linear-memory addresses** (an i32 offset into module-owned memory). The cranelift backend has no linear memory; to lower these meaningfully the SandboxState must grow `in_ptr` / `in_len` / `out_ptr` / `out_cap` fields and the entry signature must change from `(*state, i64, i64, i64, i64)` to accept buffer pointers. That redesign is the dominant cost of item #9 (`from_source` end-to-end) and shouldn't be done in two passes. | Pair with item #9. |
| **2** | Lower `Op::StoreField` / `StoreFieldAtRecord` / `AllocSubRecord` / `EmitTailRecord` | Same root cause as #1: tied to the buffer protocol redesign. | Pair with #9. |
| **3** | Simple stdlib inline (`length` / `list_*_length` / `is_empty` / `abs` / `min` / `max`) | `abs` / `min` / `max` are reachable today (now that `Op::Select` is lowered) but the IR's `Op::Call { fn_index = N }` references the bundled stdlib *by wasm function-table index*. To inline them, the cranelift codegen must look up the stdlib `Func` by index from `relon_ir::builtin_stdlib()` and recursively lower its body. That's mechanical work but the touch surface is large; the test harness is now in place to drive it once landed. | Standalone agent task. Use `relon_ir::stdlib::builtin_stdlib()` + a stable index → inline-lowering map. Add `Op::Call` arm to `emit_op` that recurses through the body's `TaggedOp` stream, treating the callee's `params` as fresh `LocalGet` slots. |
| **4** | Full `CallNative` indirect dispatch via capability vtable | β-1 wired the vtable presence check (`CheckCap`); the actual `call_indirect` requires (a) materialising the `fn_ptr` signature (cranelift `SigRef`) for each `(param_tys, ret_ty)` shape, (b) deciding the calling-convention boundary (`SystemV` vs `WasmtimeSystemV`), (c) handling argument marshalling for String / List pointer types. (a) + (b) are ~150 lines; (c) blocks on items #1/#2/#9. | Land (a) + (b) standalone for I64-only host fns; (c) waits on buffer protocol. |
| **5** | Real `sigsetjmp` / `siglongjmp` trap handler via `signal-hook` + libc | β-1 routes traps through host helper `raise_trap` + early return (the cranelift `trap` instruction raises SIGILL which `catch_unwind` cannot intercept on stable Rust without `signal-hook`). The current approach works for all sandbox guards; the only motivation to switch is reclaiming the trap-instruction cost — ~2 ns saved per guard. Low priority vs widening the surface. | Defer to v5-γ unless guard density becomes hot. |
| **6 (remainder)** | Real `Op::Loop` back-edge + `Op::BrTable` + `RESOURCE_CHECK_INTERVAL` cadence recheck | The current `emit_block(is_loop=true)` covers the forward-only fast path (loop body falls through End to exit). True back-edge loops with iteration carriers need block-param threading on the loop header. `BrTable` is a one-liner once block-param-passing exists. Resource-check cadence is independent: emit one `emit_resource_check()` call every N IR ops inside loop bodies. | Land in same tranche as item #3 stdlib inlining — those bodies have real back-edge loops. |
| **8 / 11 / 12 / 13 / 14** | Delete `relon-codegen-wasm` crate, switch `Backend::Auto` to cranelift-only, drop `--backend wasm-aot` CLI flag, drop `wasm-aot` feature, scrub CI | **Strongly counter-recommended in this tranche**. The wasm-AOT backend is currently the only AOT path that handles real `from_source` programs (full schema + buffer protocol). Deleting it before cranelift can serve those programs (items #1, #2, #9) would regress `run_main` for every production source larger than `#main(Int...) -> Int`. The current `AutoEvaluator` (`crates/relon/src/auto_evaluator.rs` `build_aot`) already tries cranelift first and falls back to wasm-AOT, so the production fast path will naturally migrate once cranelift widens. | Land **after** items #1/#2/#9 prove cranelift handles the full corpus. |
| **9** | `from_source` end-to-end through cranelift, including buffer-protocol ops | Largest deferred item. Requires: (a) redesign of `SandboxState` to expose `in_ptr` / `in_len` / `out_ptr` / `out_cap`; (b) redesign of the JIT entry signature to accept these (`(*state, *const u8, u32, *mut u8, u32) -> i32` style — note no longer `*state, i64, i64, i64, i64`); (c) host-side trampoline that allocates an arena buffer, runs `BufferBuilder::write_args` style serialisation of the input Dict, invokes the entry, deserialises the output. Each of (a) / (b) / (c) is single-commit-sized; together they're a coherent unit. | Standalone agent task. Coordinate with #1 + #2 in the same agent's session; the unit tests in `crates/relon-codegen-wasm/tests` provide a reference behavioral spec. |

## Why this scope, not the full 14

The original spec acknowledged "3-4 weeks of dedicated work" and
explicitly authorised partial landing: *"如果你 context 撑不住整批，
先 commit 一组（如 1-3 + simple stdlib），写阶段报告，由 host 决定续命
还是 SendMessage 继续。"*

The two main blockers that drove the deferred set are:

1. **Buffer protocol assumption baked into the IR**. Items #1 / #2 /
   #3 (stdlib inlining) / #4 (CallNative full) / #9 (`from_source`)
   all share a single root cause: the IR was designed against a
   wasm linear-memory model where every pointer is an absolute
   i32 address into a wasm `Memory`. The cranelift backend has no
   linear memory by design. Mapping cleanly requires a coordinated
   redesign across `SandboxState` layout, entry signature, host
   trampoline, and the host-side `BufferBuilder` glue.
2. **wasm-AOT crate is still load-bearing for production**. The
   spec lists items #8 / #11-#14 as deletion-style work, but
   today `WasmAotEvaluator::from_source` is the only AOT path
   that handles full Relon source. Deleting wasm-AOT before
   cranelift can substitute would regress every production caller
   that uses `Backend::Auto`. The right sequencing is: cranelift
   widens → bench confirms parity → wasm-AOT retires.

## Crate count after this tranche

13 → 13 (+ `relon-test-harness`, *not* counting deletion of
`relon-codegen-wasm` which is deferred). The spec's "11 final
crates" milestone lands at the same time as items #8 + #11-#14.

## What the next agent should know

1. **Run `cargo test -p relon-test-harness -- --nocapture`** to see
   per-tier pass / unsupported counts. The numbers are the
   measurement instrument: every tier transition from
   `unsupported` to `match_ok` is concrete progress on a v5-β-2 spec
   item.
2. **Unignore `corpus_arith_tier_must_match`** in
   `crates/relon-test-harness/tests/corpus_differential.rs` once
   item #9 (`from_source` end-to-end) covers ArithControl. That
   test then serves as the regression gate going forward.
3. **The `Op::effect_class()` classification is conservative**.
   When you add new Ops (e.g. for buffer protocol), set the class
   to `UnrecoverableEffect` until you've verified the trace
   recorder can replay it from `RecoverableWrite` deopt state.
   `crates/relon-ir/src/ir.rs::effect_tests` are the regression
   guard.
4. **The cranelift codegen's `emit_block` carries a
   `result_ty != None` rejection**. When stdlib body inlining
   needs blocks that yield values (some loop forms), extend
   `LabelFrame` with a `Vec<ir::Block>` of param-passing arms and
   thread the popped result through the matching `jump(..., &[v])`
   call.

## Recommended next-tranche commit shape

```
feat(codegen-native): widen SandboxState with buffer pointers + entry signature
feat(codegen-native): from_source end-to-end (item #9)
feat(codegen-native): inline simple stdlib (abs / min / max) via Op::Call
feat(codegen-native): lower LoadField / StoreField with cond_trap bounds (items #1+#2)
feat(codegen-native): widen Loop block-param threading + BrTable + resource cadence
test(harness): un-ignore arith tier strict gate
```

That's ~6 commits, ~1500 lines of code, mostly mechanical once the
buffer-protocol redesign in commit 1 is settled.

---

**Author**: Relon perf 直路 v5-β-2 implementer agent (stage 1)
**Date**: 2026-05-18
**License**: Apache-2
