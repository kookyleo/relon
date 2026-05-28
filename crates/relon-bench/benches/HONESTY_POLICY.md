# cmp_lua Bench Honesty Policy

Audit history: the `v6_lambda_cmp_lua` panel has been cleaned up
reactively through eight rounds (audits W7 / `_fixture` rename / W1
fallback / W5 / W8 / W9 / W10 collapse / W1 / W2 / W6 closed-form fold).
The Tier 1 panel expansion (2026-05-28) added W13 / W14 / W15 with the
gates pre-applied at row-add time rather than after-the-fact audit.
This document codifies the patterns so future agents catch paper wins
at PR review time, not in audit #N+1.

## Red lines (delete the row — do not "disclose and keep")

Per `/perf` Honesty Rules, a row must answer all three questions
identically to the LuaJIT row it pairs with:

1. **Same algorithm?** Same complexity class. Loops vs closed form
   is the most common violation.
2. **Same code path?** The bench label names a path
   (`relon_bytecode`, `relon_llvm_aot`, `relon_trace_jit`); the
   timed inner loop must actually take that path. A trace JIT label
   that silently falls back to the bytecode tier is the
   `_fixture` family violation (W2 / W4 / W5 / W6 / W8 / W9 / W12
   history).
3. **Same I/O shape?** Same input args, same return type. A row that
   returns the byte length while the production source builds a
   `String` is the W3 violation (audit #298).

### Five known paper-win patterns

1. **Algorithm substitution** — Replacing a doubly-recursive `fib`
   with an iterative `(a, b) := (b, a + b)` rewrite, or with Binet's
   closed form. History: W7 trace_jit_iterative (audit #260, user
   red-flag); the iterative variant is still flagged in the W7 wasm
   doc-comment as "the canonical W7 algorithm-substitution trap".

2. **Algebraic collapse / skipping load-bearing work** — When the
   production source uses constructs the lowering envelope rejects
   (first-class closures, dict literals, list materialisation, bare
   `Dict` return), it is tempting to write an inlined variant that
   produces the same final value via skipping the per-iter
   indirection. The LuaJIT row pays the indirection; the relon row
   doesn't; comparison is unfair. History: W5 / W8 / W9 / W10
   (audit #318 — deleted bytecode + wasm + LLVM AOT + rust_native
   rows for these four).
   Gated by `paper_win_collapsed_variant_label`.

3. **Compiler-side closed-form fold** — The source loop is
   byte-identical to production, but LLVM at `-O3` (or any
   sufficiently strong optimizer) reduces the arithmetic-progression
   sum to a closed-form polynomial. Post-O3 IR shows zero loop
   instructions; per-iter time is `(measured_ns / N)` and lands
   sub-1 ns (much less than a cycle). History: W1 / W2 / W6
   `relon_llvm_aot` + `relon_llvm_aot_fast` + `rust_native`
   (audit #332).
   Gated by `paper_win_closed_form_fold_label`.

4. **Fixture-disguised production label** — A row labelled
   `relon_jit` / `relon_trace_jit` runs through a hand-built
   recorder body plus a closed-form fallback closure
   (`try_build_jit_with_fixture` pattern, removed in cleanup #309).
   The column name promises the production tier; the timing
   measures a synthetic kernel. If a row's trace body is
   hand-built, the row name MUST carry a `_fixture` suffix and the
   final-report claim section must drop it from the
   "JIT exceeds LuaJIT" count.

5. **Schema mismatch** — Fallback closure returns a scalar count
   while the production source builds a String / Dict. The bench
   tracks `byte_length` instead of `String` reconstruction. History:
   W3 trace_jit returning analytic byte length (audit #298).

## Yellow lines (allow with disclosed comment + suffix)

* `relon_trace_jit_fixture` rows that run a hand-built recorder
  body whose per-iter op count matches production (W4 / W4_long /
  W10). Must have:
  - `_fixture` suffix in the BenchmarkId pair string.
  - Doc comment naming the production op chain the IR fixture mirrors.
  - The final-report "JIT exceeds LuaJIT" claim count drops the
    row (or notes it as a lower-bound floor).

* Compiler optimizations that preserve algorithm complexity class
  — LICM hoisting an invariant load out of the loop, vectorising an
  inner sum across 4 lanes, dead-code-eliminating a debug branch.
  These reduce the constant factor but keep `O(n)` work. Document
  why algorithm is preserved.

## Pre-commit checklist for a new row

Before adding a `group.bench_function(BenchmarkId::new(label, name), ...)`:

1. **Source path**: is the source byte-identical to the production
   source (`wN_relon_src()`)? If the source goes through a
   `_bytecode` / `_LLVM_SRC` / `_inline` variant, does the variant
   preserve EVERY per-iter operation the production source executes?
   (Dict probe stays. Closure call stays. List materialisation stays.)

2. **Algorithm preserved**: same complexity class? Same per-iter op
   count? If the lowering inlines / specialises / unrolls, that's
   OK; if it folds to a closed form, that's a red line.

3. **Time math sanity**: compute per-iter cost = (median ns) / N.
   - Loop-shape sources < 1 ns/iter → almost certainly closed-form fold.
   - String ops < 5 ns/iter → suspect (memchr SIMD can hit 0.5 ns
     for short haystacks, but anything sub-1 ns on a meaningful
     workload is suspect).
   - Recursive fib < expected per-call cost → suspect Binet's fold.

4. **Same I/O shape**: same `#main(...)` signature? Same return
   type? If the source returns `String` and the fallback returns
   the byte length, the row is misleading.

5. **Tier verification**: if the row label promises a tier
   (`relon_trace_jit`, `relon_jit`), assert `active_tier() ==`
   expected before the timed inner loop. Pattern in
   `trace_jit_production_label_eligible` + the warmup loop that
   asserts `active_tier == JitTier::Trace`.

## LLVM auto-fold detection

For a `relon_llvm_aot` row that uses `list.sum(range(n).map(f))` or
`range(n).reduce(0, ...)` shapes, the fold MUST be verified:

```bash
export LLVM_SYS_181_PREFIX=/usr/lib/llvm-18
mkdir -p /tmp/audit_<label>_artifacts
RELON_LLVM_DUMP_DIR=/tmp/audit_<label>_artifacts \
    cargo run -p relon-codegen-llvm --example dump_audit_w1_w2_w6 -- <label>
less /tmp/audit_<label>_artifacts/module.post_o3.ll
```

The lambda body (`@relon_llvm_entry_fast` for the fast-path row)
should contain a real loop:

```
loop_head:
  cmp i, n
  br_if loop_exit
  acc += <work>
  i += 1
  br loop_head
loop_exit:
  ret acc
```

If instead the IR shows a sequence of `add` / `mul` / `lshr`
without a `loop` / `br_if` back-edge, LLVM folded the sum and the
row is a paper win.

Magic constants to look for:
- `lshr i65 %x, 1` — division by 2, typical for `n*(n-1)/2`.
- `6148914691236517206 = (2^64 - 4) / 3` — division by 3, used in
  Faulhaber's cubic sum identity.
- `mul i65` (65-bit wide multiply) — LLVM's signed-overflow-safe
  closed-form lowering for `i64 * i64`.

## "If doubt → delete > disclose > keep"

When a row's honesty is borderline, the user-explicit policy is to
delete rather than disclose. Disclosed paper wins still get pasted
into release notes and shape "AOT exceeds LuaJIT" / "JIT exceeds
LuaJIT" headline counts; deleted rows can't. The single exception
is the engineer-facing `_fixture` rows, retained for tracking the
lower-bound floor under a name that downstream tooling treats as
non-headline.

## Revised "exceeds LuaJIT" claim methodology

After this audit (#332) and the prior cleanups, the
"`relon_llvm_aot` exceeds LuaJIT" and "`relon_jit` exceeds LuaJIT"
headline counts must be re-derived from the surviving rows only.
The deleted rows MUST be excluded from those counts; the final
report should:

1. Enumerate the surviving rows per workload.
2. For each surviving row, run the time-math check (per-iter ns
   reasonable for the workload's per-iter cost).
3. Tally exceedances ONLY from rows that pass the check.

The previous reports' counts (e.g. "Relon JIT exceeds LuaJIT on
8/12 workloads") are stale relative to the current row set and
should not be repeated without re-derivation.

## Tier 1 panel expansion (2026-05-28)

The panel grows Relon-flavour workloads to balance the original
12-row matrix's micro-codegen bias. Each row is added with the
HONESTY checklist applied at row-add time:

* **W13_deep_dict_access** — 5-level `cfg.db.pool.connections.max`
  chain inside `range(n).reduce(0, ...)`. Models the canonical
  Relon config-tree access pattern.
  - tree_walk + luajit only.
  - Bytecode / LLVM AOT / wasm reject the production source
    (dict-literal as `#internal cfg` binding + bare `Dict` return).
  - `rust_native` gated by `paper_win_closed_form_fold_label`
    (constant-fold collapses the dict-chain reads to `n * 5100`).
* **W14_schema_validate** — two boolean range checks per iter,
  modelling Relon's `#expect ...` schema-gate surface.
  - tree_walk + luajit + bytecode (if envelope accepts).
  - LLVM AOT / rust_native gated (both predicates constant-true
    over the input domain → folds to `n * 2`).
* **W15_conditional_field** — `?:` ternary per iter, modelling the
  declarative-DSL "pick one of two expressions" pattern.
  - tree_walk + luajit + bytecode (if envelope accepts).
  - LLVM AOT / rust_native gated (arithmetic progression folds to a
    closed-form polynomial).

Re-introducing the gated rows requires a `black_box`-on-acc shape
that defeats LLVM's induction-variable reduction (or an LLVM emitter
flag that disables `IndVarSimplify` / `LoopIdiom` / `LoopReduce` on
the lambda body).

## Tier 3 panel expansion (2026-05-28)

The panel grows numeric-kernel workloads to test the codegen quality
on triple-nested loops and Float arithmetic. Each row carries the
HONESTY checklist at row-add time; both rows ship architectural
limitation disclosures.

* **W19_matrix_multiply** — 16x16 matrix multiply (triple-nested
  O(n^3) loop), built from two `range(size).map.map` matrices and
  reduced cell-by-cell. The mod-100 entries on `a` / `b` defeat
  algebraic collapse, so neither rustc nor LLVM can closed-form
  fold the inner reduce.
  - tree_walk + luajit only (production source level).
  - canonical-panel relon_jit row runs the production source through
    `JitEvaluator`, which falls through to the tree-walker (nested
    2D-list materialisation + 2D index sit outside the bytecode VM's
    M2-A scalar envelope; cranelift / LLVM AOT inherit the same
    rejection — `unknown stdlib method range` at lowering time).
  - rust_native row IS valid (mod-100 discontinuity blocks closed-
    form fold). Throughput uses `size^3` (4096 ops for 16x16) so the
    per-element ns is comparable across sizes.
  - **Architectural limitation discovered**: the 2D `range(size).
    map((i) => range(size).map(...))` lowering reaches the bytecode /
    cranelift / LLVM AOT codegen pipelines as `unknown stdlib method
    range (arity=1)`. This is the same envelope gap that rejects
    W16/W17/W18's `_list_filter` closures — the lowering pipeline
    does not handle first-class closure values returned from outer
    `map` callbacks. Tracked alongside the W7 closure-recursion
    follow-up (Phase F.W7-style envelope widening).

* **W20_n_body_softened** — 4-body 1D Verlet integration over 1000
  time steps. Asymmetric masses + initial conditions defeat the
  momentum-conservation `Σ x_i = const` collapse the symmetric
  4-body 1D system would otherwise exhibit.
  - tree_walk + luajit only (Float type + first-class closures both
    outside today's bytecode / cranelift / LLVM / wasm envelopes).
  - canonical-panel relon_jit row falls through to tree-walker
    (same envelope reason — `closure value cannot cross the wasm
    module boundary`).
  - rust_native row IS valid (Verlet feedback shape blocks closed-
    form fold). Throughput uses `n_steps * n_bodies^2` (16k pair-
    force ops for n=1000) so per-pair ns is the unit.
  - **Architectural limitation disclosed (algorithm note, NOT a
    paper-win)**: the canonical Newtonian gravity kernel uses
    `F = m * dx / r^3` which requires `sqrt(dx^2 + dy^2)` (1D
    reduces to `sqrt(dx^2) = |dx|`, but the algebra still needs
    `1/r^3`). Relon's `std/math` stdlib exposes only `abs` / `max` /
    `min` / `clamp` — there is NO `sqrt` / `pow` / `exp` today. The
    source substitutes a softened `1/(r^2 + eps)^2` kernel, which
    is shape-equivalent (per-step cost is 4 mul + 1 add + 1 div per
    pair, same as `dx / r^3`) but mathematically NOT Newtonian
    gravity. The row label `W20_n_body_softened` (NOT `W20_n_body`)
    reflects the substitution at row-add time. The Lua row runs the
    SAME softened kernel — both sides pay the same per-pair cost.
    Future `Z.4.x` Float arm widening + `std/math` extension
    (`sqrt` + `pow`) can promote the row to the canonical kernel;
    until then the substitution stays explicit at the label level.
  - **Honesty checks**: the Float consistency assertion uses
    absolute tolerance `W20_FLOAT_TOL = 1e-6` rather than exact
    equality — Verlet over 1000 steps × 64 fp ops/step accumulates
    ~1e-10 relative drift, well within the tolerance; using a
    tighter check would surface FMA-lane-fusion order differences
    between the tree-walker and `rustc`'s lowering.

### Row-add-time gates applied for Tier 3 W20

The per-loop canonical-panel additions for W20 include three small
but load-bearing gates:

1. **`llvm_aot_source_for` returns None** for W20 — no inlined
   paper-win variant. The source-level rejection mirrors the W7 /
   W16 / W17 / W18 pattern of "the production source's load-bearing
   surface (closures, Float, 2D lists) is outside the envelope; an
   inlined rewrite would be the algorithm-substitution trap".
2. **`rust_native_dispatch` panics on W20** because W20 returns
   `f64` and the dispatcher's return type is `i64`. The canonical-
   panel loop instead routes W20 to a dedicated f64 `rust_native_w20`
   call site that preserves the Float result for the criterion
   `black_box` consumer.
3. **The wasm row block skips W20** entirely (Float return + Z.1
   classifier scope-cut). Skipping early avoids the panic on
   `relon_int_result` when the expected-value driver runs.
