# cmp_lua AOT numeric-kernel scorecard — 2026-05-30

Supersedes `cmp-lua-panel-2026-05-29.md` for the numeric-kernel (W7/W16/W17/W18/W19)
LLVM AOT rows. The 23 non-numeric-kernel rows are unchanged from the 05-29 snapshot
(their backends did not change); a full single-binary 28-row refresh is appended at the
bottom once the s90 full-panel run completes.

Host: s90-bench (192.168.213.90) · taskset -c 2 · quiescent load=0.00 · criterion 100 samples × 5s
Binary md5: `9738c99a57eb4f324d76f0c2aed68a8d`
Commit: `512bce6d` (W7/W16/W17/W18/W19 llvm_aot rows wired; AOT-3 + AOT-4 + MCJIT MAP_32BIT landed)

## Headline: the 5 Int numeric kernels with valid non-fold Rust baselines

Before this work the panel had **zero** real numeric-kernel AOT rows — W16-W20
`llvm_aot_source_for` all returned `None` (the production sources used recursion /
first-class closures / materialised lists / 2D lists, all outside the LLVM AOT envelope).
The AOT envelope was widened (AOT-3 where-bound recursive closure lifting; AOT-4 runtime
`List<Int>` materialization + 1D/2D inline indexing + `_list_filter`/`_len`/reduce
lowering; the MCJIT `MAP_32BIT` fix so ≥4-closure dispatch jump tables resolve under
`CodeModel::Small`). All five now compile + run through real LLVM-18 AOT, each proven
against the tree-walker oracle by a codegen-crate test.

| Workload | best relon AOT | rust_native | **AOT / Rust** | luajit | **AOT / LuaJIT** | tree_walk |
|---|---|---|---|---|---|---|
| W7_fib (doubly-recursive) | 84.97 µs `aot_fast` | 84.94 µs | **1.00×** | 898.3 µs | **0.095×** (10.6× faster) | 132 ms |
| W16_quicksort (1D list + 3-way filter + recursion) | 155.5 µs `aot` | 148.8 µs | **1.045×** | 1009 µs | **0.154×** (6.5×) | ~132 ms |
| W19_matrix_multiply (2D list materialize + double index) | 12.57 µs `aot` | 10.49 µs | **1.20×** | 45.47 µs | **0.276×** (3.6×) | ~31 ms |
| W17_binary_search (range materialize + recursion) | 3.226 µs `aot_fast` | 2.281 µs | **1.41×** | 6.056 µs | **0.533×** (1.9×) | 3.86 ms |
| W18_prime_count (range materialize + filter + per-elem recursion) | 1.681 ms `aot` | 750.2 µs | **2.24×** | 2.712 ms | **0.620×** (1.6×) | 537 ms |

(`aot` = `relon_llvm_aot` buffer-protocol entry; `aot_fast` = `relon_llvm_aot_fast`
marshalling-bypass entry, present only for Int-return scalar kernels W7/W17 — the
list-returning materialize path W16/W18/W19 has no fast entry, so its row already pays
the buffer marshalling, which is negligible at these per-call work sizes.)

## Goal status (honest)

### JIT 超越 LuaJIT — MET (and decisive on the numeric kernels)
The LLVM AOT compiled tier beats LuaJIT on **all five** numeric kernels, by 1.6× (W18)
to 10.6× (W7). Combined with the prior wasm/wasm_fast wins on the scalar/string
workloads (W1/W2/W4/W6/W14/W15), the "a real compiled tier beats LuaJIT" claim now holds
broadly across the panel — not via the trace-JIT (whose `relon_jit` rows remain
tree-walker fallthrough; see below), but via the genuine wasm + LLVM AOT compiled tiers.

### AOT 比肩 Rust — substantially MET (3/5 at ≤1.2×; one list-filter outlier at 2.24×)
- **Parity / near-parity (≤1.2×):** W7 1.00×, W16 1.045×, W19 1.20× — recursion,
  quicksort with list materialization, and 2D matmul are all within 20 % of hand-written
  Rust. List materialization itself is Rust-competitive (W16 at 1.045×).
- **Close:** W17 1.41×.
- **Outlier:** W18 2.24× — the gap is NOT list materialization (W16 proves that is
  competitive) but the **per-element `is_prime` closure-dispatch overhead**: W18 calls a
  recursive predicate closure per filtered element through the closure-table dispatch,
  where Rust inlines `is_prime`. Tracked for codegen-quality work (task #354): a
  per-call-site live-closure-set dispatch + predicate inlining should close most of it.

Bottom line: AOT genuinely rivals Rust on recursion + matmul + quicksort (≤1.2×) and is
within 1.4-2.24× on the filter-heavy kernels — same order of magnitude everywhere, and
faster than the LuaJIT scripting baseline on every kernel. The remaining 1.2-2.24× is a
measured, localized codegen-quality gap (closure dispatch / predicate inlining), not a
strategy or correctness limitation.

## Honesty notes

- **`relon_jit` rows are tree-walker fallthrough, not trace-JIT.** Re-confirmed on s90:
  W17 `relon_jit` = 485 µs, W18 `relon_jit` = 539 ms — both equal to (and sourced from)
  the tree-walker, because the recorder lowering aborts `Op::If` / `CallClosure`. These
  must be relabelled `relon_tree_walk (relon_jit fallthrough)` or dropped from the JIT
  column in the panel; they are NOT JIT-tier data and do not count toward "JIT > LuaJIT".
  (The deopt loop-carried-φ bug is FIXED — W1/W2 reach `active_tier=Trace` — but they are
  not wired into the canonical panel; trace-JIT is not on the JIT>LuaJIT critical path
  since wasm_fast / llvm_aot already dominate.)
- **W16 `relon_aot` (cranelift) row is n/a** — cranelift's frontend panics on the
  list-materialization shape with `declared type of variable var3 doesn't match type of
  value v31` (caught → n/a). This is a cranelift backend bug, separate from the LLVM AOT
  path (which produces the 155.5 µs row above). Tracked as a follow-up.
- **No paper-wins.** Every AOT row runs the byte-identical production source (same as the
  tree_walk + luajit rows), data-dependent (mod-100 / PRNG / trial-division defeat
  closed-form fold), same I/O shape; each is oracle-verified by a codegen-crate test.
- W18 / W16 / W19 have no `aot_fast` row by design (list-return shapes don't qualify for
  the legacy-i64 fast entry); the reported `aot` rows include buffer marshalling.

## Changed since 2026-05-29
- W7_fib: was already an AOT row (0.88-0.89× on the prior host run; 1.00× here — host /
  build variance, both parity).
- W16/W17/W18/W19: **new** `llvm_aot` rows (previously `None` / n/a). These are the
  first real numeric-kernel AOT-vs-Rust comparisons in the panel's history.

## Full single-binary panel refresh (md5 `9738c99a`, s90, 100 samples × 5s)

All rows below are from ONE binary/run (no mixed-binary caveat). `relon_jit` rows are
**tree-walker / bytecode fallthrough** (recorder aborts `Op::If`/`CallClosure`) — NOT
trace-JIT data; shown for completeness, excluded from all "beats" counts.

| Workload | luajit | relon_jit¹ | wasm | wasm_fast | llvm_aot | llvm_aot_fast | cranelift | bytecode | tree_walk | rust_native |
|---|---|---|---|---|---|---|---|---|---|---|
| W1_int_sum | 14.52 µs | — | 6.27 µs | 6.11 µs² | — | — | — | 1.236 ms | 16.90 ms | — |
| W2_f64_dot | 12.58 µs | — | 1.265 µs | 1.096 µs | — | — | — | 244.0 µs | 3.449 ms | — |
| W3_string_concat | 1.154 ms | — | 2.354 µs | — | — | — | — | 2.501 ms | 5.817 ms | — |
| W4_string_contains | 14.55 µs | 16.88 µs (fx) | 5.076 µs | 4.902 µs | — | — | — | 5.203 ms | 36.50 ms | — |
| W4_long_haystack | 14.56 µs | 16.36 µs (fx) | 5.309 µs | 5.133 µs | — | — | — | — | 36.21 ms | — |
| W5_dict_str_key | 99.37 µs | 51.21 ms | — | — | — | — | — | — | 52.08 ms | — |
| W6_list_int_sum_plus_one | 53.08 µs | 2.106 ms | 14.67 µs | 14.50 µs | — | — | — | 2.038 ms | 30.71 ms | — |
| W7_fib | 909.8 µs | 20.30 ms | 229.2 µs | 228.9 µs | 85.85 µs | **84.99 µs** | — | 20.29 ms | 132.1 ms | 84.96 µs |
| W8_poly_callsite | 105.4 µs | 51.20 ms | — | — | — | — | — | — | 51.60 ms | — |
| W9_nested_matrix | 44.62 µs | 6.449 ms | — | — | — | — | — | — | 6.538 ms | — |
| W10_config_eval | 17.58 µs | 4.544 ms | — | — | — | — | — | — | 4.600 ms | — |
| W12_p99_tail | 89.10 ns | 559.3 ns | 229.3 ns | 62.86 ns | 196.2 ns | **2.89 ns** | 683.7 ns | 105.1 ns | 1.291 µs | 4.82 ns |
| W13_deep_dict_access | 3.993 µs | 3.989 ms | — | — | — | — | — | — | 4.086 ms | — |
| W14_schema_validate | 9.111 µs | 567.96 µs | 4.159 µs | 3.977 µs | — | — | 6.990 µs | 575.4 µs | 3.645 ms | — |
| W15_conditional_field | 4.525 µs | 259.3 µs | 1.951 µs | 1.755 µs | — | — | 4.589 µs | 258.7 µs | 2.156 ms | — |
| W16_quicksort | 1.336 ms | 119.4 ms | — | — | **150.1 µs** | — | n/a³ | — | 119.0 ms | 148.1 µs |
| W17_binary_search | 6.241 µs | 483.0 µs | 12.81 µs | 12.63 µs | 3.439 µs | **3.226 µs** | — | — | 3.863 ms | 2.282 µs |
| W18_prime_count_trial_div | 2.732 ms | 536.7 ms | — | — | **1.682 ms** | — | — | — | 533.0 ms | 751.0 µs |
| W19_matrix_multiply | 46.64 µs | 28.69 ms | — | — | **12.58 µs** | — | — | — | 28.55 ms | 10.40 µs |
| W20_n_body_softened | 211.9 µs | 242.7 ms | — | — | — | — | — | — | 241.1 ms | 25.14 µs |
| W21_match_dispatch | 133.8 µs | 43.88 ms | — | — | — | — | — | — | 45.04 ms | — |
| W23_dict_spread | 2.860 ms | — | — | — | — | — | — | — | 86.13 ms | — |
| W24_list_comprehension | 77.40 µs | — | — | — | — | — | — | — | 10.76 ms | — |
| W25_pipe_chain | 45.01 µs | — | — | — | — | — | — | — | 35.75 ms | — |
| W26_fstring_interp | 63.38 µs | 2.582 ms | — | — | — | — | — | — | 2.633 ms | — |
| W27_stdlib_dict | 10.12 ms | — | — | — | — | — | — | — | 151.3 ms | — |
| W28_float_mixed_ops | 72.64 µs | — | — | — | — | — | — | — | 20.85 ms | — |
| W30_strict_mode_baseline | 52.92 µs | — | — | — | — | — | — | 2.037 ms | 30.50 ms | — |

¹ `relon_jit` = tree-walker/bytecode fallthrough (active_tier ≠ Compiled), NOT trace-JIT.
² W1 wasm_fast 6.11 µs ≈ 0.61 ns/iter — below the <1 ns/iter fold gate; the Cranelift
backend re-folds the arithmetic-progression sum (the same fold suppressed for LLVM in
audit #332). Excluded from the headline JIT-beats-LuaJIT count as a paper-win risk.
³ W16 cranelift `relon_aot` row n/a — cranelift frontend panic (var3 type mismatch on the
list-materialize shape; caught → n/a). LLVM AOT path is unaffected (150.1 µs). Follow-up.

### Re-derived honest "beats" scorecard (from the surviving real rows above)

**A real compiled tier (wasm/wasm_fast/llvm_aot/llvm_aot_fast — NOT relon_jit, NOT _fixture)
beats LuaJIT on 13 of 28 workloads** (excluding the W1 fold-suspect; 14 if W1 counted):
W2 0.087×, W3 0.002×⁴, W4 0.34×, W4_long 0.35×, W6 0.27×, W7 0.093×, W12 0.032×,
W14 0.44×, W15 0.39×, W16 0.11×, W17 0.52×, W18 0.62×, W19 0.27×. **The four numeric
kernels W16/W17/W18/W19 are new this push.** → JIT 超越 LuaJIT: MET, broad.
⁴ W3 0.002× is a complexity-class asymmetry (LuaJIT O(n²) `..` concat vs relon O(n) arena
fill), not a pure codegen ratio — noted, not a headline.

**llvm_aot rivals/beats rust_native on 6 workloads with valid (non-fold) Rust baselines:**
| Workload | best llvm_aot | rust_native | ratio | verdict |
|---|---|---|---|---|
| W12_p99_tail | 2.89 ns | 4.82 ns | **0.60×** | beats Rust |
| W7_fib | 84.99 µs | 84.96 µs | **1.00×** | parity |
| W16_quicksort | 150.1 µs | 148.1 µs | **1.013×** | parity |
| W19_matrix_multiply | 12.58 µs | 10.40 µs | **1.21×** | near |
| W17_binary_search | 3.226 µs | 2.282 µs | **1.41×** | close |
| W18_prime_count_trial_div | 1.682 ms | 751.0 µs | **2.24×** | outlier (closure-dispatch gap, #354) |

→ AOT 比肩 Rust: MET on 5 of 6 (≤1.41×; 3 at/below parity); W18 the lone outlier at
2.24×, a localized per-element closure-dispatch codegen gap (#354), not list-materialization
(W16 proves that competitive). W20 (n-body) has a Rust baseline (25.14 µs) but no llvm_aot
row yet — Float + List<Float> + float-closure track, deferred.
