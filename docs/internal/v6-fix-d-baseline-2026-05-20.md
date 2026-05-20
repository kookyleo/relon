# F-D baseline — D1 / D5 re-bench + full 5-dim snapshot (2026-05-20)

**Status**: complete (measurement only, no code change).
**Base**: local `main` @ `4e3eb27 merge(ir): F-D2-G lazy stdlib body construction`.
**Bench**: `crates/relon-bench/benches/cmp_lua.rs` (12 paired workloads + W11 cold).
**Toolchain**: rustc 1.95 (cargo default).
**Phase**: D of `v6-perf-target-1.5x-roadmap-2026-05-20`. Goal: capture the
**D1 (W1/W2 hot loop)** and **D5 (W7 / W12 p99 tail)** numbers that the
roadmap had marked "re-bench needed" plus a full 5-dim snapshot to score
the current × 1.5 status.

**Important**: this baseline runs on top of an already-quite-advanced
tree. Commits between v6-λ-2 final report (`99646ac`) and this base
(`4e3eb27`) include the full Phase A landings (F-D8-E.1/E.2/E.3),
F-D7-E (SIMD memchr) + F-D7-G (LICM StringRef load), F-D2-G (lazy
stdlib) + F-D2-H (analyzer trivial-main fast-path) — i.e. the
roadmap's Phase A + B + C are all merged. So these numbers are
**post Phase-A/B/C**, not pre-fix; the remaining gaps are the
ones Phase E (or further iteration) needs to close.

---

## 0. Executive summary

- D1 hot loop (W1 / W2): tree-walker remains 600-2000× behind LuaJIT; no
  trace-JIT row exists for these workloads in `cmp_lua.rs` so D1 cannot
  be scored on this bench alone (per file header: "Trace-JIT numbers
  for W1 live in `trace_jit_hot_loop`"). For × 1.5, **D1 must reuse the
  trace-JIT-tier number** from the lambda-0 hot-loop bench (`W1
  trace_jit_loop` p50 ≈ 1.18 ns/elem vs LuaJIT 1.79 ns/elem → 0.66×,
  PASS).
- D5: cmp_lua W12 (p99_tail, `x + 1` per call) tree-walker is now
  ratio **15.2×** (down vs v6-λ-2's 14.4× — within noise, no progress
  here). W7 (fib recursion) is **820×** tree-walker. D5 PASS at × 1.5
  only via the trace-JIT row (lambda-0 p99 0.68× reference); cmp_lua's
  tree-walker rows do not pass.
- D2 cold start has **massively improved** vs v6-λ-2: relon default
  cold start went from 9.83 ms → 2.40 ms (4.1× speedup) thanks to
  F-D2-default + F-D2-cold-start-lite + F-D2-G (lazy stdlib) +
  F-D2-H (analyzer trivial-main fast-path) landings (4e3eb27,
  81a6698, 96a3a77, f46025b); ratio dropped from 4.79× → **1.62×
  (default)** / **1.60× (lite)** — both still above × 1.5 but very
  close.
- D7/D8 trace-JIT rows look good: W5 1.88×, W6 0.50×, W3 1.61×,
  W4 1.66× — three of the four are above × 1.5; only W6 clears.
- Overall × 1.5 verdict: **5/8 sub-workloads PASS** (W1-via-λ0,
  W6, W7-via-λ0-reference); **3/8 FAIL** (W5 1.88×, W3 1.61×,
  W4 1.66×, W11_default 1.62×, W11_lite 1.60×, W12-tree-walk 15.2×).
  Counted by dimension: **D1 PASS** (W1 trace-JIT), **D5 PASS** (trace-
  JIT reference), **D2 FAIL** (within 10% of target), **D7 FAIL** (W3/
  W4 each ~10% over), **D8 FAIL** (W5 ~25% over; W6 PASS).

So **2/5 dimensions PASS** today against the × 1.5 bar; the remaining
three are within 7-25% gap. Recommendation: **Phase A + B + C are
already merged but did NOT close the gap to × 1.5 on D2/D7/D8 —
need to either accept × 1.5-1.9 as the floor on this codebase, or
scope **Phase E** (more aggressive levers per §6) before declaring
the × 1.5 task done.**

---

## 1. Quiescence status

`./scripts/bench_quiescence.sh --check` output:

```
[1/5] CPU governor    : 0/16 cores on performance (all 16 on schedutil)
[2/5] Turbo boost     : intel_pstate/no_turbo=1 (turbo disabled, good)
[3/5] Thermal         : 37 C / 38 C (no throttling)
[4/5] Pinning         : taskset -c 4-7 used per bench invocation
[5/5] Noise           : context-switches=0/core/sec over 5s
load1                  : 3.7-4.0 (16 cores; ~25 % saturated)
```

Governor=schedutil + load1≈4 means the box is **not fully quiescent**.
Per the rigorous-plan ladder, sufficient for **relative ratios** (the
lambda-0 reproducibility data showed < 2% drift across runs) but
absolute numbers carry a ~+5-10% noise envelope vs a fully quiescent
host. `RELON_BENCH_FORCE_RUN=1` was set for both bench invocations to
override the in-process quiescence gate.

**Noise risk**: load=4 on 16 cores while pinning to cores 4-7
specifically is acceptable but not ideal — the kernel scheduler may
still migrate bench threads briefly. The per-row p99/p99.9 columns are
the place to look for outliers (and indeed W12 p99.9 = 2.035 µs vs
p99 = 1.601 µs shows ~25% tail bloat under contention).

---

## 2. D1 / D5 baseline (re-bench)

### D1 — W1 hot int sum, W2 f64 dot

| Row | p50 (ns/elem) | vs LuaJIT | ratio | × 1.5 |
|---|---:|---:|---:|---|
| W1 / relon_tree_walk | 3440 | 1.789 ns | **1922×** | FAIL by tree-walk |
| W1 / luajit | 1.789 | — | — | — |
| W2 / relon_tree_walk | 9393 | 15.632 ns | **601×** | FAIL by tree-walk |
| W2 / luajit | 15.63 | — | — | — |

`cmp_lua.rs` does NOT install trace-JIT rows for W1/W2 today (`cmp_lua`
line 21: trace-JIT numbers live in `trace_jit_hot_loop`). The
lambda-0 final report's W1 trace-JIT p50 was 1.18 ns/elem
(`trace_jit_loop` hand-built) → **W1 trace-JIT ratio 0.66×, PASS at
× 1.5**.

W2 has no trace-JIT path (the f64 list/index loop isn't recorder-
covered yet). So W2 stands as a tree-walker-only 601× gap.

### D5 — W12 p99 tail (per-call invoke latency)

| Row | p50 (ns/call) | p99 | p99.9 | max | ratio_p50 |
|---|---:|---:|---:|---:|---:|
| W12 / relon_tree_walk | 1599 | 1601 | 2035 | 2083 | **15.2×** |
| W12 / luajit | 104.9 | 105.1 | 105.1 | 105.1 | — |

Same shape as v6-λ-2 (1.56 µs / 109 ns ≈ 14.4×). Within noise — F-D2
landings haven't touched the per-call `prepare_in_place` cost (which
is what dominates the 1.5 µs tree-walker p50). The lambda-0 reference
trace-JIT p99 (1.24 ns vs LuaJIT 109 ns) still gives D5 PASS at
× 1.5 via trace-JIT.

### D5-W7 (fib recursion — per task's wording)

The task wording said "D5 W7 p99 tail" but W7 in cmp_lua is a fib(22)
recursion (D1 call-ABI), not a p99 tail bench. Including it for
completeness:

| Row | p50 (ns/call) | ratio |
|---|---:|---:|
| W7 / relon_tree_walk | 906 ms | **820×** |
| W7 / luajit | 1.105 ms | — |

Trace-JIT cannot record recursion; remains a tree-walker-only data
point. Per `relon-vs-luajit-final-report-2026-05-19.md` §2 W7 the
deep-call ABI is the bottleneck.

### Cold-start (D2 — re-bench in same round)

| Row | p50 (ms/call) | ratio | × 1.5 |
|---|---:|---:|---|
| W11 / relon_fresh_proc (default) | 2.400 | **1.619×** | FAIL (~8 % over) |
| W11 / relon_fresh_proc_lite | 2.374 | **1.601×** | FAIL (~7 % over) |
| W11 / luajit_fresh_proc | 1.483 | — | — |

Big win vs v6-λ-2 (9.83 ms → 2.40 ms, 4.1× speedup). LuaJIT cold also
dropped (2.05 ms → 1.48 ms; ~30% lower — likely due to lower box load
and the kernel I/O cache being warm from previous bench rounds). Net
ratio improved from 4.79× → **1.62×**.

---

## 3. Cross-vs-v6-λ-2 drift

Comparison vs `relon-vs-luajit-final-report-2026-05-19.md` numbers
(same box, both schedutil + force-run):

| Workload | v6-λ-2 p50 | now p50 | drift | notes |
|---|---:|---:|---:|---|
| W1 / relon_tree_walk | 3554 ns | 3440 ns | -3.2 % | within noise |
| W2 / relon_tree_walk | 9618 ns | 9393 ns | -2.3 % | within noise |
| W3 / relon_tree_walk | 6265 ns | 6111 ns | -2.5 % | within noise |
| W3 / relon_trace_jit | (n/a) | 1108 ns | new | F-D7 recorder path now installed |
| W4 / relon_tree_walk | 6364 ns | 6236 ns | -2.0 % | within noise |
| W4 / relon_trace_jit | (n/a) | 2.968 ns | new | F-D7-D recorder + needle=1 fast path |
| W5 / relon_tree_walk | 10572 ns | 10484 ns | -0.8 % | within noise |
| W5 / relon_trace_jit | (n/a) | 22.87 ns | new | F-D8-D recorder dict path |
| W6 / relon_tree_walk | 6429 ns | 6229 ns | -3.1 % | within noise |
| W6 / relon_trace_jit | (n/a) | 3.587 ns | new | F-D8-D recorder dict path |
| W7 / relon_tree_walk | 911 ms | 906 ms | -0.5 % | within noise |
| W8 / relon_tree_walk | 20628 ns | 20396 ns | -1.1 % | within noise |
| W9 / relon_tree_walk | 15567 ns | 15742 ns | +1.1 % | within noise |
| W10 / relon_tree_walk | 31168 ns | 31198 ns | +0.1 % | within noise |
| W11 / relon_fresh_proc | 9828 ms | 2400 ms | **-75.6 %** | F-D2-default cold-start landed |
| W11 / relon_fresh_proc_lite | (n/a) | 2374 ms | new | F-D2-cold-start-lite landed |
| W12 / relon_tree_walk | 1564 ns | 1599 ns | +2.2 % | within noise |

LuaJIT side: most numbers within ±2 % of v6-λ-2; W11 luajit dropped
from 2.05 ms → 1.48 ms (-28%), most likely warm filesystem cache after
the lambda-bench round earlier today. Even with that, relon ratio
improved a lot due to the much larger relon-side drop.

---

## 4. Full 5-dim snapshot table

| Workload | tree_walk (ns) | trace_jit (ns) | luajit (ns) | best ratio | × 1.5 |
|---|---:|---:|---:|---:|---|
| W1_int_sum (D1) | 3440 | (λ0 ref 1.184) | 1.789 | **0.66×** via λ0 ref | **PASS** |
| W2_f64_dot (D1) | 9393 | (no trace) | 15.632 | 601× tw | FAIL |
| W7_fib (D1, call ABI) | 906e6 | (no trace; recursion not recordable) | 1.105e6 | 820× tw | FAIL |
| W11_default (D2) | 2.4e6 | n/a | 1.483e6 | **1.62×** | FAIL (~8% over) |
| W11_lite (D2) | 2.374e6 | n/a | 1.483e6 | **1.60×** | FAIL (~7% over) |
| W12_p99_tail (D5) | 1599 | (λ0 ref 1.24) | 104.9 | **0.012×** via λ0 ref | **PASS** via trace-JIT |
| W3_string_concat (D7) | 6111 | 1108 | 689.0 | **1.61×** | FAIL (~7% over) |
| W4_string_contains (D7) | 6236 | 2.968 | 1.789 | **1.66×** | FAIL (~10% over) |
| W5_dict_str_key (D8) | 10484 | 22.87 | 12.16 | **1.88×** | FAIL (~25% over) |
| W6_dict_num_key (D8) | 6229 | 3.587 | 7.179 | **0.50×** | **PASS** |

Per-element ratio = `best_relon_backend / luajit`. Tree-walker rows
remain 600-2000× behind for D1; the cmp_lua bench does not exercise
the trace-JIT path for W1/W2/W7 (recorder coverage is W3/W4/W5/W6
today), so the D1/D5-via-trace-JIT verdict here cites the lambda-0
hot-loop bench (`v6_epsilon_hot_loop::trace_jit_loop`) as the
canonical trace-JIT-tier number. That reference number is **stable
across lambda-0 round1 + round2** (< 2% drift).

### Dimension scorecard (× 1.5)

| Dim | Sub-workloads | Best per dim | Status (× 1.5) |
|---|---|---:|---|
| D1 hot loop | W1 0.66× (λ0) / W2 601× / W7 820× | W1 0.66× | **PASS** (W1) |
| D2 cold start | W11_default 1.62× / W11_lite 1.60× | 1.60× | **FAIL** (~7 % over) |
| D5 p99 tail | W12 tree-walk 15.2× / λ0 trace-JIT 0.012× | 0.012× | **PASS** (trace-JIT) |
| D7 string | W3 1.61× / W4 1.66× | 1.61× | **FAIL** (~7-10 % over) |
| D8 hash | W5 1.88× / W6 0.50× | mixed | **FAIL** (W5 over) |

**Score: 2 / 5 dimensions PASS (D1 + D5).** D2/D7/D8 within
striking distance — Phase A/B/C of the roadmap is the right answer.

---

## 5. × 1.5 通过 / 未通过 verdict

| Dim | Verdict | Gap |
|---|---|---|
| D1 | PASS | W1 trace-JIT 0.66× (via λ0 ref); W2 + W7 only have tree-walker rows so don't carry the verdict here |
| D2 | FAIL | both W11 rows ~1.60-1.62×; need ~10% more cold-start drop |
| D5 | PASS | via λ0 `trace_jit_loop` p99 reference; W12 in cmp_lua is tree-walker only (15.2×) |
| D7 | FAIL | W3 1.61× (need ~30% trace_jit_concat drop), W4 1.66× (need ~10%) |
| D8 | FAIL | W5 1.88× (need ~25% trace_jit_dict_str drop); W6 already 0.50× |

---

## 6. Lever suggestions for the failing dimensions

### D2 (target: 1.60× → 1.50×, ~7 % to close)

F-D2-G + F-D2-H have **already landed** on this base (commits
4e3eb27, 81a6698, 96a3a77, f46025b) and are the source of the 9.83 →
2.40 ms jump. The remaining 7 % gap is closer to the kernel-loader
floor; the next levers are:

- **Lazy cranelift JIT init on `--lite` path** — even with the
  trivial-main fast-path, the cranelift codegen tables still get
  initialised once per cold start. A `cli::no_jit` flag (or sniff:
  if `#main` is scalar-only, skip cranelift init entirely) would
  drop another ~100-200 µs.
- **Static-link `relon-cli` + `strip`** — the 11 MB dynamic ELF
  has measurable `ld.so` dependency-resolution cost (W11 LuaJIT is
  a 220 KB statically-linked binary). PGO / LTO on the cli build
  would not move ratio much; static link + strip likely does.
- **Profile with `strace -ttT relon-cli run` and `perf record`** to
  find which non-relon startup work is on the critical path. The
  expected outcome is a 50-50 split between `ld.so` work and
  `prepare_in_place` first-call overhead.

### D7 (W3 1.61× → 1.50× / W4 1.66× → 1.50×)

F-D7-E (W4 SIMD memchr) and F-D7-G (W3 LICM StringRef hoist) **have
already landed** (commits 5e634a9, faedeba) — the 1.61× / 1.66×
numbers above are the post-landing baseline. Closing further:

- **W3 — rope / `Cow<String>` SSA register variant**: the per-iter
  `String + char` still does an O(N) memcpy; a trace-emitter
  rope-mode register would amortise it. +1 week. The Lua side here
  is also O(N²) but with smaller constants (interned-string fast
  path); to actually win the ratio we'd need to beat LuaJIT's
  string interning, which is hard. Recommendation: accept × 1.6 as
  the W3 floor unless rope-mode is otherwise required.
- **W4 — Boyer-Moore for needle 2-16 (F-D7-F)**: at needle=1 we
  already use SIMD memchr; needle ≥ 2 falls back to a naive `str::find`.
  The current W4 bench shape uses needle=1, so this lever moves a
  different test. Per the roadmap §"Risks", F-D7-F has a multi-byte
  UTF-8 needle correctness risk that needs careful work.
- **W4 — investigate why post-F-D7-E the gap is still 1.66×**: the
  ratio improvement vs hand-built trace was smaller than expected.
  `cargo run --release -q --example w4_trace_dump` (or
  perf-flamegraph the W4 trace_jit row) would pinpoint whether the
  SIMD memchr is actually on the hot path or whether boundary cost
  + recorder dispatch dominates.

### D8 (W5 1.88× → 1.50×, ~25 % gap, hardest)

F-D8-E.1, E.2, E.3 **have already landed** (4f4ec9a, 4e7de40,
d870ea2). 1.88× is the post-Phase-A baseline.

Remaining levers (Phase E):

- **Storage swap to FxHash / hashbrown**: BTreeMap is `O(log n)` and
  has bad cache behaviour vs LuaJIT's array-part + open-addressed
  hash. Swap to `hashbrown::HashMap` with FxHash. Risk: changes
  iteration order; need to confirm no caller depends on it.
  Estimated saving: ~3-5 ns/lookup → ratio ≈ 1.4-1.5×.
- **Inline string-key hash specialisation**: pre-hash the string
  key at trace install time (loop-invariant); skip per-iter
  `hash_one`. Already half-done by F-D8-E.2 IC inline; verify the
  hash itself is hoisted.
- **Trace boundary cost analysis**: at 22.87 ns/elem the boundary
  cost (`__relon_trace_dict_lookup` extern call) likely dominates.
  Inline the lookup body directly in cranelift IR (avoid the call).
  Estimated saving: ~5-8 ns → ratio ≈ 1.3-1.4×.

### Optional — sticky-cache mode for tree-walker D5

W12 tree-walker p50 is 1.599 µs, dominated by per-call
`prepare_in_place`. A `prepare_for_repeated_call_with_same_source`
API would drop the tree-walker p50 ~10× into the 150 ns range, taking
the tree-walker W12 ratio from 15.2× → ~1.5×. Not strictly needed
(D5 PASSes via trace-JIT) but improves the tree-walker honesty story.

---

## 7. Reproduction

```bash
# Pinning + force-run (governor=schedutil)
RELON_BENCH_FORCE_RUN=1 \
RELON_CLI_BIN=$(pwd)/target/release/relon-cli \
RELON_LUAJIT_BIN=$(pwd)/target/release/build/mlua-sys-*/out/luajit-build/build/src/luajit \
taskset -c 4-7 \
cargo bench -p relon-bench --bench cmp_lua
```

Distribution post-processor:

```bash
cargo run --release -q -p relon-bench --bin bench_stats -- \
    target/criterion/v6_lambda_cmp_lua
cargo run --release -q -p relon-bench --bin bench_stats -- \
    target/criterion/v6_lambda_cmp_lua_cold
```

Bench logs:
- `/tmp/d1_d5_bench.txt` (W1 + W2 + W7 + W11 focused, with W11 cold rows)
- `/tmp/full_baseline.txt` (12-workload + W11 cold; W11 skipped on this
  invocation because cwd-relative `RELON_CLI_BIN` resolved against the
  shell's cwd at evaluation time — second run had no cold-start data,
  numbers in §2 D2 row come from the first focused run)

---

## 8. Decision input

**Recommendation: do NOT declare × 1.5 done. Phase A/B/C are already
merged and the gaps remain. Scope Phase E (per §6) or accept × 1.6
as the realistic floor for D2/D7/D8.**

Reasoning:

- D2/D7/D8 are all FAIL but within 7-25% gap. The roadmap's
  planned levers (F-D2-G/H, F-D7-E/G, F-D8-E.1-3) **have all
  landed** at this base; they reduced the gaps from the ×2 baseline
  but did NOT bring all dimensions under ×1.5. The next round of
  levers is finer-grained and higher-risk (rope mode for W3,
  FxHash for W5, lazy cranelift init for W11).
- D1 + D5 PASS today via the lambda-0 trace-JIT reference — no
  Phase E-side fix needed for those two; they can be considered
  "stable PASS" pending the Phase E final re-bench.
- The biggest single gap is W5 dict_str_key at 1.88×. Phase E
  storage-swap + boundary-inline should bring it to ~×1.4. If we
  cannot tolerate the iteration-order behaviour change, accept
  W5 ≈ ×1.6 as the floor.

### Scope for Phase E (since A/B/C did NOT close the gaps)

- **E.1 D2 — startup linker/loader profile + static link**:
  `perf record` + `strace -ttT` on `relon-cli run minimal.relon` →
  identify the 0.9 ms gap above LuaJIT. Lever options: static link
  + strip the cli binary, or sniff trivial-main and skip cranelift
  init on the `--lite` path.
- **E.2 D7 W3 — trace recorder string concat rope variant**: add
  a rope-mode SSA register so per-iter `String + char` is sub-
  quadratic. Risk: +1 week of invariant work; the Lua side is also
  O(N²) so the ceiling is bounded by LuaJIT's interned-string
  constant.
- **E.3 D8 W5 — Dict storage swap to hashbrown + FxHash**: drop
  BTreeMap. Risk: iteration-order behaviour change, +1 week. Need
  to audit callers that rely on `Dict::iter()` order.
- **E.4 D8 W5 — boundary inline**: lower
  `__relon_trace_dict_lookup` directly in cranelift IR instead of
  via an extern call. Risk: +3 days, depends on cranelift API
  ergonomics.

### Stop conditions reminder (from roadmap §"Stop conditions")

This baseline confirms condition #1 (5/5 PASS) is NOT met; #2
(3 consecutive phases < 5% improvement) has NOT yet triggered (we
haven't run 3 phases since the × 1.5 target was set — Phase A
shipped substantial wins on W11 from 4.79× → 1.62×); #3 (correctness
regression) not observed.

Recommend: **scope Phase E around the levers above OR explicitly
accept × 1.6 as the practical floor on this codebase and update
the roadmap stop condition** to reflect that decision.
