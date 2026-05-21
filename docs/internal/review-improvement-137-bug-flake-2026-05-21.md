# review-improvement-137 stage report — pre-existing bug sweep

Date: 2026-05-21
Base: `c55bfb3501d6bc174306f69ef35cc5f0ee6f4361` (worktree HEAD)
Branch: `worktree-agent-ace68383a5d36d4ad`

## (a) `jump_helper_aborts_recording_for_unsupported_op` flake

Root cause: three `jump_helper_*` tests in
`crates/relon-test-harness/tests/trace_jit_smoke.rs` shared
`global_trace_jit_state()` (an `RwLock<HashMap<u32, Arc<JITedTraceFn>>>`)
and asserted invariants against the **global** `installed_count()` —
a delta check that any concurrent install / `invalidate_trace` by a
sibling test in the same binary would break. Each test owns a unique
`fn_id` (700 / 701 / 703); the recorder correctness invariant is
"this fn_id must (or must not) be installed", not "the registry size
is unchanged". The count assertion was over-broad.

Fix: scope every assertion to the test's own `fn_id`. Replaced
`installed_count` delta checks with `lookup_trace(fn_id).is_some()` /
`is_none()`, and added an explicit `invalidate_trace(fn_id)` setup
step so a sticky install from a previous in-process run no longer
silently skips the test. The "must not double-install" check now
captures the pre/post `Arc<JITedTraceFn>` and verifies
`Arc::ptr_eq` — strictly stronger than the count check, and immune
to concurrent registry churn.

Verified: 10 consecutive `cargo test --workspace` runs passed
`jump_helper_aborts_recording_for_unsupported_op` (and its two
siblings) cleanly. Single-thread run also passes.

## (b) `cargo doc --no-deps` link warnings

Before: 94 warnings across 11 crates (cumulative count from rustdoc
summary lines).
After: 0 warnings.

Categories repaired:

- Stale links into items that had moved / been renamed (e.g.
  `BytecodeEvaluator::from_ir` → `from_ir_legacy`, the missing
  `TraceBuffer::const_bytes_for` retargeted to `OptimizedTrace`).
- Cross-crate links into items not in the local doc graph
  (`relon_codegen_native::CraneliftAotEvaluator::from_source` from
  `relon-bytecode`, `relon_evaluator::eval::Capabilities` from
  `relon-analyzer`, `relon_ir::Op::*` from `relon-trace-jit`) —
  converted to plain `code` spans where the dep is absent, or to
  the correct intra-crate path where the dep exists.
- Doc-links to private items (`pub(crate)` modules, private fns
  like `prepare_tree_walk_context`, `evaluate_source`,
  `is_hoistable`) → converted to inline code.
- `List<Int>` written as raw text was parsed by rustdoc as an
  unclosed HTML tag (8 occurrences across `relon-ir/src/ir.rs` and
  `relon-eval-api/src/buffer.rs`); wrapped in backticks.
- `iters[i]` (one occurrence in `relon-bench/src/bench_stats.rs`)
  parsed as a reference-style link target; wrapped in backticks.

## Gate

- `cargo fmt --all --check`: clean
- `cargo clippy --workspace --all-targets -- -D warnings`: clean
- `cargo test --workspace`: 136 result blocks, all pass
- `cargo doc --workspace --no-deps`: 0 warnings
- `cargo check -p relon-wasm --target wasm32-unknown-unknown`: clean

## Follow-up

None. Both fixes are surface-level repairs; no design-layer changes
required.
