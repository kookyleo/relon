# M2-B phase 4d — bytecode 4-way differential bench activation

Date: 2026-05-21
Owner: review-improvement-144

## Scope

Phase 4d wires the bytecode VM into the `cmp_lua` LuaJIT-paired bench
as a fourth row alongside `relon_tree_walk` / `relon_trace_jit` /
`luajit`, and confirms that the corpus four-way harness already in
`crates/relon-test-harness/tests/bytecode_diff.rs` keeps zero
mismatches under the current bytecode envelope. The bytecode row is
gated on `relon::new_evaluator(src, Backend::Bytecode)`: a successful
construction produces real numbers; a `BackendError::Bytecode(reason)`
records the row as `n/a (UnsupportedOp: <reason>)` and skips the timed
loop without failing.

No source rewrites, no force-enables. The envelope today is the M2-A
scalar shape — `#main(scalar...) -> scalar` with arith / cmp / control
flow only.

## Coverage matrix

| Workload | tree_walk | cranelift_aot | trace_jit | bytecode    | Skip reason (bytecode)                                                  |
| -------- | --------- | ------------- | --------- | ----------- | ----------------------------------------------------------------------- |
| W1 int sum         | y | y | y | n/a | IR lift: unresolved `list` (stdlib + closure outside M2-A envelope)   |
| W2 f64 dot         | y | (y) | — | n/a | analyzer rejects 2 errors (closure / list ctor)                       |
| W3 string concat   | y | — | y | n/a | analyzer rejects 3 errors (closure + reduce + list ctor)              |
| W4 string contains | y | — | y | n/a | IR lift: unknown stdlib method `range/1` (closure + stdlib chain)     |
| W4 long haystack   | y | — | y | n/a | same as W4 (shared Relon source)                                      |
| W5 dict str key    | y | — | y | n/a | analyzer rejects 3 errors (dict literal + closure + list)             |
| W6 dict num key    | y | (y) | y | n/a | analyzer rejects 2 errors (closure + list.sum)                        |
| W7 fib recursion   | y | — | — | n/a | analyzer rejects 4 errors (first-class closure in dict body)          |
| W8 polymorphic     | y | — | — | n/a | analyzer rejects 5 errors (closure dispatch + list.sum)               |
| W9 nested matrix   | y | (y) | — | n/a | analyzer rejects 13 errors (nested range/map/reduce)                  |
| W10 config eval    | y | — | — | n/a | analyzer rejects 3 errors (closure + list.sum)                        |
| W11 cold start     | (ext) | — | — | — | spawn workload, not a hot-loop row                                   |
| W12 p99 tail       | y | (y) | — | **y** | `#main(Int x) -> Int\nx + 1` — inside the M2-A scalar envelope    |

Notes:
- "(y)" marks the cranelift-AOT envelope reach quoted from the
  cmp_lua header — most workloads still fall back to tree-walker on
  the Relon side; only W1/W7/W9/W12 reduce to the cranelift slice.
- "(ext)" for W11 = external process spawn, not a backend-internal
  measurement.

## cmp_lua bench numbers (quick-run, 2026-05-21)

Single workload that produced real bytecode numbers — W12. Bench run
under `--sample-size 10 --measurement-time 1`, machine quiescent
(`governors=16 perf, no_turbo=1, load1=9, 37 - 39 C`).

| W12 / row         | mean per invocation | throughput      |
| ----------------- | ------------------- | --------------- |
| relon_tree_walk   | 1.559 µs            | 641 K elem/s    |
| relon_bytecode    | **447 ns**          | 2.235 M elem/s  |
| luajit            | 107.86 ns           | 9.272 M elem/s  |

Honest reading: the bytecode VM clears the tree-walker by ≈ 3.5 x on
a trivial scalar invocation, but is still ≈ 4 x slower than LuaJIT's
trace tier. The gap is dominated by the per-call setup the
`BytecodeEvaluator::run_main` path still pays (HashMap clone for the
arg map + VM init + bytecode dispatch loop with the wasm-shaped
match-tree). M2-C inline-cache work is the lever for closing it; M2-A
explicitly parks dispatch perf below correctness.

The corpus four-way differential (`bytecode_diff.rs`) carries 52 cases
through `diff_test_4way`. Zero mismatches today; every ArithControl
case (28 / 28) lands on `AllAgree` or `AllTrap`; non-arith tiers
(stdlib / list / closure / dict) bounce as `BytecodeUnsupported`,
counted as soft passes. Strict bytecode-vs-tree-walk parity test
passes on the full ArithControl set with no divergences.

## Phase 4c-cont / M3 blueprint — what unlocks each row

| Row    | Unlocking phase                                                     |
| ------ | ------------------------------------------------------------------- |
| W1, W6 | phase 4c-cont — stdlib `range/sum` surface + list-element loop      |
| W2     | phase 4c-cont — same as W1 plus arith-fold inside `map` closure     |
| W3     | M3 — string ctor + concat + reduce-closure                          |
| W4     | M3 — string + `contains` stdlib + filter / len list ops             |
| W5     | M3 — dict literal + string indexing + closure + list.sum            |
| W7     | M3 — recursive closure (self-reference inside dict body)            |
| W8     | M3 — polymorphic closure dispatch (W7 prerequisite + cmp-chain)     |
| W9     | M3 — nested list / reduce / closure (W7 + W2 prerequisites)         |
| W10    | M3 — boolean-chain closure + list.sum                               |

W12 already passes; it is the canonical "bytecode-from-source"
measurement and the only row contributing a real per-iter cost to the
bytecode column today.

## Gate

```
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace            # 2130 passed, 0 failed
cargo check --target wasm32-unknown-unknown -p relon-wasm
```

All clean.

## Touchpoints

- `crates/relon-bench/Cargo.toml` — added `relon` workspace dep
  (default-features = false: bench host pulls only the
  `Backend::Bytecode` surface, no cranelift baggage).
- `crates/relon-bench/benches/cmp_lua.rs` — `try_build_bytecode`
  helper + one bytecode row per workload. n/a rows log a
  `[cmp_lua Wx] bytecode row n/a (UnsupportedOp: ...)` line at bench
  startup so the failure mode is visible without parsing criterion
  JSON.
- `crates/relon-test-harness/tests/bytecode_diff.rs` — already exists
  (landed in earlier phase 4b-cont); no edits this round. The
  four-way differential continues to pass.
