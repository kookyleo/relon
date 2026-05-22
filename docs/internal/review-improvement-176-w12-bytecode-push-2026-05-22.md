# review-improvement-176 — W12 bytecode × 1.5 push

Date: 2026-05-22
Branch: `worktree-agent-a4bdc12fd66764464`
Worktree: `/ext/relon/.claude/worktrees/agent-a4bdc12fd66764464`

## Goal

W12 bytecode row sat at × 1.88 vs LuaJIT (204.74 ns vs 106.68 ns).
Target: ≤ × 1.5 (≤ 160 ns).

## Audit

W12 source: `#main(Int x) -> Int\nx + 1` — 4-op stream
(`LocalGet + ConstI64 + AddI64 + Return`). The bench drives
`BytecodeEvaluator::run_main_i64`, the typed-i64 fast path.

Per-call hot path through `run_main_i64_inner`:

1. `BcVmConfig::clone()` — 3 `Arc::clone`, empty `Vec` clone, empty
   `HashMap` clone in `CapabilityVtable` (~10-15 ns)
2. `BytecodeVm::new()` — struct + `RefCell::new` (~1 ns)
3. `vec![0u64; needed]` for `locals` (~10-15 ns)
4. `Vec::with_capacity(16)` for `stack` (~10-15 ns)
5. `VmMemory::default()` — 4 empty Vec stamps (~1 ns)
6. dispatch loop 4× (step tick + bounds + match dispatch_one + StepOutcome)
7. `BcRunOutcome { final_locals: Vec<u64>, ... }` move on return (~5 ns)

Option A (wire trace-bridge to bench) — **architecturally blocked**:
`run_main_i64`'s docstring explicitly opts out of the `trace_lookup`
dispatcher switch because "recorder/installed-trace overhead would
defeat the purpose." Forcing the bypass through `run_main_i64` would
contradict that contract, and the alternate `run_main` trait surface
adds back the `HashMap<String, Value>` arg-packing cost the typed
path was built to avoid.

## Lever shipped

**Lever 7 — alloc-free typed-i64 fast path.** New
`BytecodeVm::invoke_pooled_typed_i64`:

- Thread-local `Vec<u64>` scratch (`POOLED_LOCALS`, `POOLED_STACK`).
  After warm-up: zero per-call heap alloc for locals + stack (just a
  `Vec::clear` + memset).
- Returns `Result<VmValue, BcVmError>` directly — skips the
  `BcRunOutcome::final_locals` Vec move.
- Reuses `dispatch_one` for correctness — every BcOp arm + sandbox
  prong behaves identically to the general path.

`run_main_i64_inner` rewired to drive the pooled entry. The
`BytecodeVm::new()` + `default_config.clone()` cost remains per-call
(VM carries a `RefCell<CallNativeCache>` and is `!Sync`; caching it
on the `Send + Sync` `BytecodeEvaluator` needs a wider rework).

## Numbers

| Row           | Before    | After     | vs LuaJIT |
| ------------- | --------- | --------- | --------- |
| LuaJIT        | 106.68 ns | 104.29 ns | × 1.00    |
| **bytecode**  | 204.74 ns | 120.21 ns | × 1.15    |
| trace_jit     | 149.09 ns | 149.76 ns | × 1.44    |
| tree_walker   | 1570 ns   | 1560 ns   | × 14.95   |

Bytecode dropped 84 ns (-41 %) on a single lever — well past the
× 1.5 (160 ns) target, landing at × 1.15 vs LuaJIT.

## Gates

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo test --workspace` — 2306 passed / 0 failed
- `cargo check -p relon-wasm --target wasm32-unknown-unknown` clean

## Follow-ups

- The remaining ~16 ns over LuaJIT is mostly `BcVmConfig::clone()` +
  `BytecodeVm::new()`. Caching `BytecodeVm` on the evaluator needs
  either an interior-mutable `Sync` wrapper (`Mutex<CallNativeCache>`
  trades perf for sync-safety) or restructuring so the `Send + Sync`
  evaluator can lazily hand out a per-thread VM view.
- The freed budget is large enough that broader workloads (W1, W2,
  W6) on bytecode should re-bench to see how lever 7 lands once the
  scaffold widens past the W12 envelope.
