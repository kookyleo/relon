# review-improvement-147 — bytecode M2-C close LuaJIT gap

Date: 2026-05-21
Worktree: `agent-ab0e56bd43ba7c3e5`
Base local main: `e11c333`

## Goal

Close the LuaJIT gap on W12 (`#main(Int x) -> Int : x + 1`). Pre-M2-C
the bytecode row ran ~447 ns vs LuaJIT 107.86 ns (~4× slower); target
W12 ≤ 250 ns (~2× LuaJIT).

## Result

W12 `relon_bytecode` dropped from **448.59 ns → 181.39 ns**
(−60 %, −267 ns delta, **target ≤ 250 ns surpassed**). Ratio vs
LuaJIT now ~1.68× (was 4.15×). p = 0.00 < 0.05 on the criterion
change report; performance gain is robust to bench noise.

### W12 before / after table

| backend                | before (ns) | after (ns) | delta    |
|------------------------|-------------|------------|----------|
| `relon_bytecode`       | 448.59      | 181.39     | −267 ns  |
| `luajit` (reference)   | 107.86      | 107.86     | 0 ns     |
| `relon_tree_walk`      | unchanged   | unchanged  | —        |
| `relon_trace_jit`      | unchanged   | unchanged  | —        |

Ratio vs LuaJIT: 4.15× → 1.68×.

## Levers landed

### Lever 1 (typed-i64 fast-path API) — landed, ~−260 ns

`BytecodeEvaluator::run_main_i64(&[i64]) -> Result<i64, RuntimeError>`
mirrors the cranelift `run_main_legacy_i64` shape. The bench now drives
the concrete `BytecodeEvaluator` through this method, bypassing:

- `HashMap<String, Value>` host-arg packing (was the dominant cost —
  same shape #136 found on the cranelift side).
- `Value::Int` wrapping on every arg + return slot.
- The `Option<&Schema>` + `schema.fields.len()` epilogue walk
  (combined with lever 5 below).

Args travel as `as u64` reinterprets; the inline 4-arg buffer keeps
the wide-arity Vec allocation out of the hot path.

### Lever 2 (inline cache for stdlib dispatch) — landed, **W12 row: 0 ns** delta

`BytecodeVm` gains a single-entry monomorphic IC for
`BcOp::CallNative`. Hot loops dispatching the same `import_idx`
repeatedly skip the per-call `HashMap<u32, Arc<dyn RelonFunction>>`
probe; the cache invalidates on a polymorphic site and resets at the
top of every `invoke_*` so a `register_host_fn` swap between calls is
honoured. Three new tests pin the dispatch shape (monomorphic loop,
polymorphic alternation, reset-between-invokes).

W12 doesn't exercise `CallNative` so the cmp_lua W12 row reads zero
delta — the cache motivates the wider stdlib-driven fixtures (host-fn
loops) where the HashMap probe dominated. Reserved for the next
phase's fixture cohort.

### Lever 5 (cache main_schema) — landed, folded into lever 1 + lever 5 commit

The evaluator caches `return_shape: ReturnShape` (Copy enum),
`cached_return_field_count: u32`, and `cached_param_count: usize` at
construction time. The hot dispatch epilogue branches on a cheap
enum rather than re-walking `schema.fields` on every invoke; five
callers (inner / resume / metrics / typed-i64) switched to the cached
helper.

### Lever 3 (per-op specialization) — NOT landed, blueprint

Rationale: with W12 already at 181 ns vs target 250 ns the ROI on
type-tag elimination drops below the engineering cost. Per-op
specialization requires:

- separate stack lanes per type (Stack<i64> + Stack<f64> + ...) OR
  untagged value slots backed by careful compile-pass tagging;
- the compile pass must emit lane-tagged ops (`BcOp::AddI64Raw` etc);
- the deopt-resume `stack_recipe` table needs a per-slot lane
  annotation so partial-resume rebuilds the right lane.

Blueprint reserved for an M2-D phase where the trace-JIT bytecode
bridge motivates the lane work.

### Lever 4 (threaded dispatch / computed goto) — NOT landed, doc conclusion

Pure Rust on stable 1.95 cannot express direct-threaded dispatch:

- `goto` / labels-as-values are not Rust syntax;
- `become` (LLVM tail-call) lands on nightly 1.83+, not stable;
- `naked_functions` + inline asm is unstable and pulls in the
  `unsafe_code` budget the bytecode crate currently forbids
  (`#![forbid(unsafe_code)]`).

`match`-based dispatch compiles to a jump table under LLVM; the
remaining overhead vs threaded dispatch is the per-iteration jump
back to the dispatcher head. On x86-64 with LTO = fat and codegen-
units = 1 (the bench profile) LLVM does NOT thread the dispatches.
**Conclusion**: lever 4 is unreachable on stable Rust; defer until
the compiler ships stable computed-goto, OR until a separate "raw"
dispatcher crate is OK'd to carry the `unsafe` budget.

## Validation

- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test --workspace`: pending (running at report time);
  per-crate runs:
  - `cargo test -p relon-bytecode`: 19 + 53 + 6 + 6 + 14 + 5 tests
    pass, including the 3 new inline-cache tests.
  - `cargo test -p relon-test-harness --test three_way_corpus`:
    2 / 2 pass (`arith_tier_all_agree_or_trap`, `diff_aggregates`).
  - `cargo test -p relon-bench --test cmp_lua_consistency`:
    W1..W10 all 10 pass.
- `cargo check --target wasm32-unknown-unknown -p relon-bytecode`:
  clean.

## Commits

- `5d9127d` — `perf(bytecode): typed-i64 fast path API`
- `9630248` — `perf(bytecode): inline cache for stdlib dispatch`
- `983cf7d` — `style(bytecode): apply rustfmt to M2-C levers 1+2`
- `9244b1a` — `perf(bytecode): cache main_schema in BcFunction`

Branch: `worktree-agent-ab0e56bd43ba7c3e5`.
