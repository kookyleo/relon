# review-improvement-160 — bytecode M3 phase 2 (2026-05-22)

Branch: `worktree-agent-ad2ebaf43430f87c1` (worktree-agent SHA below in commit log).

## TL;DR

Picked **Option (a) IR-side desugar** for the `list.sum(range(...))`
hot-loop shape. Lifts cmp_lua **W1** off the bytecode "unsupported"
shelf and onto the live four-way differential corpus. **W2 / W6 / W8
stay blocked** at the analyzer level (untyped closure params), not at
the IR / bytecode level — out of scope for this stage.

## What the phase 4b-cont report got wrong

The carry-over note claimed the iter stdlib bodies bounced because the
bytecode VM lacked `LoadI64AtAbsolute` / `BitAnd` / alignment math.
That framing implicitly assumed the bytecode pipeline ever reaches the
`list_int_sum` body. It does not:

* `list` is a **Relon-source module alias** (`#import list from
  "std/list"`) the tree-walker resolves dynamically through the host
  module loader. The IR lowering pass has no notion of "user-imported
  namespace"; the receiver `list` falls through as
  `UnresolvedVariable("list")` in `lower_method_receiver`.
* `range` is a **tree-walker-only host fn** (`Range` in
  `crates/relon-evaluator/src/stdlib.rs`), with **no IR stdlib slot**
  in `crates/relon-ir/src/stdlib/registry.rs`.

So neither the bundled `list_int_sum` body nor the buffer-protocol ops
were ever on the hot path for the W1 source. The probe (a one-shot
test that ran the analyzer + lowering on each W{1..8} src) confirmed
the actual failure modes:

* W1 — lowering: `UnresolvedVariable("list")`.
* W2 / W6 / W8 — **analyzer** rejects (`ClosureParamTypeMissing`).
* W4 — lowering: `UnknownStdlibMethod("range")`.

## Option (a) — IR-side peephole

`crates/relon-ir/src/lowering.rs`: added `try_lower_list_sum_range`,
called from the top of `lower_fn_call`. Recognises the two surface
shapes the cmp_lua bench uses:

* `list.sum(range(end))` — `start = 0`.
* `list.sum(range(start, end))`.

Emits the same arithmetic shape the bundled `list_int_sum` body
computes (i64 accumulator over `start..end`), but **without
materialising the intermediate `List<Int>`** — three i64 let-slots
(`start`, `end`, `acc`) under one `Op::Block { Op::Loop { ... } }`.
The loop exits via `BrIf` when `start >= end` and ticks `start += 1`
each iteration, matching the host's `(start..end).map(Value::Int)`
behaviour exactly (empty range returns 0; inverted range returns 0).

Why peephole vs Option (b) bytecode op family:

* Option (b) (add `BcOp::LoadI64AtAbsolute` / `BitAnd` etc.) would
  not have helped — the IR call to `list.sum` never resolves to a
  body the bytecode pipeline can walk, so the body's ops are
  irrelevant. (b) would only matter for a future host-fn lowering
  that produces a real `List<Int>`.
* Option (a) collapses the allocation **and** unblocks cranelift for
  the same source (cranelift goes through the same IR pipeline). The
  desugar lives once in IR; bytecode + cranelift both inherit it.

## Workloads unlocked

| Workload | Before | After |
|----------|--------|-------|
| W1 (int sum) | bytecode `n/a (UnresolvedVariable "list")` | **bytecode live** |
| W2 | analyzer-rejected | analyzer-rejected (untyped `(i)`) |
| W6 | analyzer-rejected | analyzer-rejected |
| W8 | analyzer-rejected | analyzer-rejected |
| W12 | bytecode live | bytecode live (unchanged) |

So: **bytecode cmp_lua coverage = 2/12** (W1 + W12), up from 1/12.
W2 / W6 / W8 need analyzer-side closure-param type inference to lift;
the lowering envelope is fine.

## Bench numbers (release, N = 10 000, quiescence overridden)

| Row | time (median) | thrpt |
|-----|---------------|-------|
| `W1_int_sum/relon_tree_walk` | 34.42 ms | 290 K elem/s |
| `W1_int_sum/relon_bytecode` | **1.59 ms** | **6.29 M elem/s** |
| `W1_int_sum/luajit` | 17.94 µs | 558 M elem/s |

Bytecode is ~22 × faster than the tree-walker (no `Value::Int`
allocation per iteration, no dict-of-lambda dispatch) and ~89 × slower
than LuaJIT (expected — bytecode is the fallback tier; the trace-JIT
row would be the cmp_lua target). W12 stays at ~210 ns per invoke for
sanity.

## Corpus differential coverage

Added three cases to `crates/relon-test-harness/src/corpus.rs`
(`StdlibList` tier):

* `stdlib_list_sum_range_n` — `list.sum(range(n))`, n = 100.
* `stdlib_list_sum_range_start_end` — `list.sum(range(5, n))`.
* `stdlib_list_sum_range_empty` — n = 0 empty-range boundary.

Four-way diff harness (`tests/bytecode_diff.rs`):
`stdlib_list: bytecode_match=3 / 5` (was `0 / 2`). Tree-walker +
cranelift + trace-JIT + bytecode all agree on the desugared form;
the remaining 2 `StdlibList` cases (`[1,2,3].sum()` /
`[1..].max()`) still need bytecode list-arg support and stay on the
phase 3 shelf.

## Gate

* `cargo fmt --all -- --check`: clean.
* `cargo clippy --workspace --all-targets -- -D warnings`: clean.
* `cargo test --workspace`: **2233 passed / 0 failed** (≥ 2227 base).
* `cargo check --target wasm32-unknown-unknown -p relon-wasm`: clean.

## Remaining workloads — blueprint

* **W2 / W6** (`list.sum(range(n).map((i) => ...))`) — needs analyzer-
  side closure-param type inference from the receiver `List<Int>`
  (today the recorder-driven trace path does this; the strict
  analyzer route doesn't). Once that lands, extending the
  `try_lower_list_sum_range` peephole to also match
  `list.sum(range(n).map(<closure>))` is mechanical — emit the
  closure body inline as the per-iter expression instead of `i` raw.
* **W4** (`range(n).map(...).filter(...).len()`) — analyzer accepts
  via dynamic typing but lowering rejects `range` standalone. Either
  add `range` as an IR stdlib that emits a list-arena allocator, or
  extend the peephole to recognise `range(...).len()` / `len(range(...))`
  pairs.
* **W8** — closure capture of outer `dispatch` lambda needs the
  closure ABI through the analyzer's strict gate plus the per-call
  IR closure surface; orthogonal to the peephole work.

## Files touched

* `crates/relon-ir/src/lowering.rs` — `try_lower_list_sum_range`
  helper + call site in `lower_fn_call`.
* `crates/relon-bytecode/tests/list_sum_range_desugar.rs` — 6 new
  tests pinning the desugar (1-arg, 2-arg, empty, large, inverted,
  Gauss-formula identity at multiple N).
* `crates/relon-test-harness/src/corpus.rs` — 3 new `StdlibList`
  cases driving the four-way differential.

No bench file change — the W1 bytecode row was already gated on
`try_build_bytecode`; it now activates automatically.
