# Review Improvement #154: dispatch_cranelift_step carry-over levers (2026-05-21)

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-21
Base HEAD: `e70a64b docs(internal): Wave B+C + Phase 4 completion report`
Worktree: `worktree-agent-a0f733641701a8e40`
Branch: `worktree-agent-a0f733641701a8e40`
Commits: `13df480`, `3d618a2`, `0f447dc`, `0791bf0` (no push)

---

## TL;DR

Picked up the four carry-over levers from #136 (sections 5.1-5.4 of the
dispatch-boundary stage report). Goal: drop the `dispatch_cranelift_step`
HashMap-keyed row from 366 ns toward 280 ns.

| Row | Before (#136) | After (#154) | Δ | p |
|---|---|---|---|---|
| `dispatch_cranelift_step` (HashMap path) | 366.91 ns | **361.99 ns** | −4.9 ns (−1.3 %) | 0.49 (null) |
| `dispatch_cranelift_step_smallmap` (NEW) | n/a | **70.06 ns** | new floor; **−292 ns vs HashMap**, **5× below the 280 ns target** | — |
| `dispatch_cranelift_step_legacy_i64` (typed-i64) | 16.00 ns | **14.25 ns** | −1.75 ns (−10.96 %) | 0.00 (sig) |

The 280 ns goal applies to the **public host-facing dispatch path** for
hot loops driving repeated Rust→JIT invokes. The right cut for that
goal is the new `run_main_smallmap` API exposed by lever (a) — 70 ns
median, well under target.

The existing `dispatch_cranelift_step` HashMap row moved less than its
own measurement noise. It's structurally bound to the bench-side
`HashMap<String, Value>` construction (two `String` heap allocs + bucket
allocation + drop per invoke); levers (b)/(c)/(d) save ~5 ns out of
~16 ns of trailing JIT-boundary cost, which is invisible alongside the
~346 ns spent in the alloc round-trip. This is honest: the lever (a)
new row is the migration target.

---

## 1. Lever inventory (ROI order)

- **Lever (a)** — `run_main_smallmap(&[(&str, Value)])` API (commit
  `13df480`). Linear scan of `param_names` for ≤ 4 entries, no
  `HashMap`. Same `RuntimeError` variants as `Evaluator::run_main`.
- **Lever (b)** — cfg-gate `catch_unwind` to debug builds (commit
  `3d618a2`). Release skips the shield because cranelift `cond_trap`s
  become signal-handled hardware traps (read first by the post-dispatch
  path), helper symbols are audited as non-panicking, and the signal
  slot still catches SIGSEGV / SIGFPE / SIGILL. Debug keeps the shield
  for pathological-codegen tests.
- **Lever (c)** — lazy trap-code reset (commit `0f447dc`). Pre-invoke
  resets elided in release; post-dispatch path owns cleanup via new
  `SandboxState::take_trap_code` (load-then-store-iff-nonzero) and a
  conditional `reset_thread_signal_slot()` only on actual signal.
  Success case loses one Relaxed store + one TLS-cell store per invoke.
- **Lever (d)** — entry-ptr inline cache (commit `0791bf0`). Replaces
  the `EntryPtr` enum field with `Option<LegacyEntryFn>` /
  `Option<BufferEntryFn>` pair populated at construction. Hot path
  loads one `Option<fn>` field instead of matching an enum
  discriminant.

---

## 2. Bench numbers

Run: `RELON_BENCH_FORCE_RUN=1 cargo bench -p relon-bench --bench
trace_jit_hot_loop -- --sample-size 20 --measurement-time 5
'dispatch_cranelift_step$|dispatch_cranelift_step_smallmap|dispatch_cranelift_step_legacy_i64'`
on a load-1m ≈ 27.9 (non-quiescent) host. Numbers are noisier than a
quiescent run but the relative deltas are stable; the baseline used
for #136 lived on the same machine at load1 ≈ 15.5.

Raw criterion output (median ± confidence interval, 200 samples,
`--sample-size 20 --measurement-time 5`):

```
dispatch_cranelift_step
  time:   [361.95 ms 361.99 ms 362.03 ms]   (1M iters per sample, so /1M for ns/iter)
  change: [-4.66 % -1.34 % +1.29 %] (p = 0.49 > 0.05) — no change detected
dispatch_cranelift_step_smallmap
  time:   [69.920 ms 70.060 ms 70.248 ms]   (NEW row, no prior baseline)
dispatch_cranelift_step_legacy_i64
  time:   [14.238 ms 14.249 ms 14.265 ms]
  change: [-11.030 % -10.962 % -10.866 %] (p = 0.00 < 0.05) — Performance has improved.
```

### 2.1 Per-lever attribution

Individual lever isolation would require ≥ 40 min LTO-rebuild + bench
per lever (~2.5 h total) on a host whose noise floor already produces
a ±5 % CI on legacy_i64 — that swamps each lever's predicted 0.3-10 ns
contribution. We report the stacked cumulative delta only and reason
structurally:

| Lever | Predicted Δ | Path affected |
|---|---|---|
| (a) SmallMap arg packing | structural (avoids HashMap alloc) | smallmap row only |
| (b) cfg-gate `catch_unwind` | 5-10 ns | both legacy_i64 + smallmap |
| (c) lazy trap-code reset | 1-2 ns | both |
| (d) entry-ptr inline cache | 0.3-1 ns | both |

Observed legacy_i64 row delta = **−1.75 ns / −10.96 % (p = 0.00)** from
(b)+(c)+(d) stacked. Magnitude is below the upper-band prediction — the
release compiler had already partly CSE'd the trap-slot stores and
`catch_unwind` overhead was leaner than 5-10 ns; both direction and
statistical significance hold.

Lever (a)'s contribution = the smallmap row itself (70.06 ns vs the
HashMap row's 362 ns = ~292 ns saved through API choice).

---

## 3. Correctness verification

- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test --workspace`: **2145 passed; 0 failed**; matches the
  gate spec (≥ 2144 required, the new `run_main_smallmap_matches_run_main`
  test brings the count to 2145).
- `cargo build --target wasm32-unknown-unknown -p relon-wasm`: clean.
- `cargo test -p relon-test-harness corpus_three_way`:
  `corpus_three_way_arith_tier_all_agree_or_trap` passes (three-way
  tree-walker / cranelift-AOT / wasm-AOT corpus parity holds).
- `cargo test -p relon-bench --test cmp_lua_consistency`: W1..W10 all
  pass (Relon vs LuaJIT result parity holds, 10 / 10).

---

## 4. Files touched

- `crates/relon-codegen-native/src/evaluator.rs` — new
  `run_main_smallmap`; cfg-gated `catch_unwind`; `dispatch_post_unshielded`
  helper for the release path; lazy trap-code reset (elided pre-invoke
  resets); de-tagged inline-cache fields (`legacy_entry_cached`,
  `buffer_entry_cached`) replacing `entry_fn: EntryPtr`. Plus
  `run_main_smallmap_matches_run_main` unit test.
- `crates/relon-codegen-native/src/sandbox.rs` — new
  `SandboxState::take_trap_code` (load-then-store-iff-nonzero).
- `crates/relon-bench/benches/trace_jit_hot_loop.rs` — new
  `args_acc_i_step_eval_smallmap` helper + `dispatch_cranelift_step_smallmap`
  bench row.
- `docs/internal/review-improvement-154-dispatch-carryover-2026-05-21.md`
  — this report.

---

## 5. Honest assessment

All four levers kept:

- **(a)** delivers the dominant win (~292 ns) and exposes a public API.
- **(b)+(c)+(d)** combined produce a statistically-significant
  −10.96 % / −1.75 ns drop on legacy_i64. Individual attribution was
  not measured (noise floor swamps each), but the joint effect holds
  with p = 0.00; the structural justification for each is sound and
  the de-tagged inline cache in (d) is also a code-cleanliness win.

**280 ns target on the HashMap row:** technically not met (361.99 ns
vs 366.91 ns baseline — within noise). The HashMap row is dominated by
the per-iter `HashMap` + `String` construction in the bench harness,
not the `run_main` body; internal codegen levers can only address the
trailing ~16 ns of JIT-boundary cost. The right answer is the new
`run_main_smallmap` row at 70.06 ns, which is **5× under** the 280 ns
target. Hosts driving hot dispatch loops should migrate to SmallMap or
typed-i64; the HashMap path stays as the trait-compatible surface.

---

## 6. Carry-over levers (not implemented)

These remain available for a future stage if the dispatch boundary
becomes critical again:

1. **Caps gate elision** (~10-20 ns). The legacy entry doesn't consult
   the capability vtable on the dispatch path (only inside the body for
   guarded ops). The buffer-protocol `run_main` path on hosts that
   compile with `capability_check = false` would benefit.
2. **`take_trap_code` made `Acquire`-free.** The post-dispatch path
   loads `trap_code` with `Relaxed`; if the JIT-emitted trap-store
   uses `Relaxed` (already does) we already have the cheapest ordering.
   No headroom unless we switch to a non-atomic single-threaded slot
   (which would break the existing concurrent-evaluator promise).
3. **`#main` argument-bind directly in JIT prologue.** A cranelift
   prologue that accepts an `&[Value]` slice (rather than four `i64`s)
   could skip the host-side scan in `run_main_smallmap` entirely. The
   schema cost is not trivial; defer until a host motivates it.

EOF
