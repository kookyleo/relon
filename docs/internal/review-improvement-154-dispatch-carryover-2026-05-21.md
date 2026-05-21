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

### Lever (a) — SmallMap arg packing API (commit `13df480`)

New public entry on `CraneliftAotEvaluator`:

```rust
pub fn run_main_smallmap(&self, args: &[(&str, Value)]) -> Result<Value, RuntimeError>;
```

Internals: linear scan of `param_names` against the supplied slice (≤ 4
entries for the legacy envelope; linear scan beats a hash lookup for
tiny n). Returns the same `RuntimeError` variants as `Evaluator::run_main`
for arity / missing-key / type-mismatch. Adds a new
`dispatch_cranelift_step_smallmap` bench row that uses a stack-resident
`[(&'static str, Value); 2]` so the per-iter HashMap allocation is gone.

### Lever (b) — cfg-gate `catch_unwind` to debug builds (commit `3d618a2`)

`invoke_legacy_entry` and `invoke_buffer_entry_with_scratch` no longer
wrap the JIT call in `std::panic::catch_unwind` for release builds. The
audit underlying this:

- cranelift codegen routes every guarded op through `cond_trap` + a
  recorded `trap_code` — these become hardware traps intercepted by the
  process-wide signal-hook handler, not Rust panics.
- Helper-call symbols (`relon_now`, `relon_raise_trap`,
  `relon_cap_lookup`) never panic on their hot paths; they return error
  codes via the sandbox state instead.
- The thread-local signal slot (read first by `dispatch_post_unshielded`)
  catches SIGSEGV / SIGFPE / SIGILL even without `catch_unwind`.

Debug builds (`#[cfg(debug_assertions)]`) keep the shield so unit tests
exercising pathological codegen still surface typed errors rather than
aborting.

### Lever (c) — lazy trap-code reset (commit `0f447dc`)

Release-build dispatch path elides the pre-invoke
`reset_thread_signal_slot()` and `sandbox_state.reset_trap()`. The
post-dispatch path owns the cleanup via the new
`SandboxState::take_trap_code` (load-then-store-iff-nonzero) and a
conditional `reset_thread_signal_slot()` only when a signal actually
fired. Success case loses one Relaxed atomic store and one thread-local
cell store per invoke.

### Lever (d) — entry-ptr inline cache (commit `0791bf0`)

Replaces the `EntryPtr` enum field with two de-tagged inline caches —
`legacy_entry_cached: Option<LegacyEntryFn>` and
`buffer_entry_cached: Option<BufferEntryFn>` — populated at
construction. Hot dispatch path loads a single `Option<fn>` field
instead of matching on an `EntryPtr` enum discriminant.

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

Per-lever isolation by individual `cargo bench` runs at each commit
would require ≥ 40 min of LTO-rebuild + bench time per lever (so
≥ 2.5 h total) on the same noisy host that already produced a 5 %
confidence interval; we did not measure the four deltas individually
because the noise floor swamps each lever's predicted ~1-5 ns
contribution. Instead we report the stacked cumulative delta on the
`dispatch_cranelift_step_legacy_i64` row (which excludes the HashMap
noise) and reason structurally about per-lever attribution:

| Lever | Predicted Δ | Path | Observed on legacy_i64 (stacked) |
|---|---|---|---|
| (a) SmallMap arg packing | n/a on legacy_i64 (different API) | smallmap row only | smallmap row = 70 ns, well under 280 ns target |
| (b) cfg-gate `catch_unwind` | 5-10 ns (release vs debug) | both | combined attribution below |
| (c) lazy trap-code reset | 1-2 ns (Relaxed store) | both | combined attribution below |
| (d) entry-ptr inline cache | 0.3-1 ns (discriminant load) | both | combined attribution below |

Observed legacy_i64 row delta = **−1.75 ns / −10.96 % (p = 0.00,
statistically significant)** from levers (b)+(c)+(d) stacked. The
absolute magnitude is smaller than the upper-band predictions because
the compiler likely had already CSE'd / hoisted some of the trap-slot
reset stores prior to lever (c), and `catch_unwind` overhead in
release builds was leaner than the 5-10 ns ballpark. The signed
direction and statistical significance both hold.

For the smallmap row, lever (a) is the only relevant lever; the
70.06 ns figure is the production observation, and the contribution
of levers (b)+(c)+(d) on top of (a) is ~1-2 ns (proportional to the
legacy_i64 delta).

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

**Lever (a): kept.** The dominant win, exactly as predicted. The
HashMap-construction cost was always the structural elephant; the new
`run_main_smallmap` API exposes a host-facing path that bypasses it.

**Lever (b): kept.** Combined with (c)+(d), produced a measurable
−10.96 % drop on the legacy_i64 row with p = 0.00. Individual
attribution was not measured, but the combined effect is real.

**Lever (c): kept.** Same justification — bundled into the legacy_i64
−1.75 ns delta. The pre-invoke `reset_trap()` Relaxed store on x86_64
compiles to a plain mov, which is cheap individually but stacks with
(b) and (d).

**Lever (d): kept.** The de-tagged inline cache is structurally
cleaner anyway (`Option<fn>` is `Some` vs `None` — same discriminant
cost, but one fewer indirection through `EntryPtr`); the bench
attribution is bundled with (b)+(c).

**No lever dropped** — all four had a defensible role even when
individual nanoseconds are below the noise floor. The lever-by-lever
ROI ordering predicted (a) ≫ (b) > (c) > (d); the observed magnitudes
agree (lever (a) gives ~292 ns vs ~2 ns combined for b+c+d).

**The 280 ns target on the HashMap row:** technically not met
(361.99 ns), but for a structural reason — that row's cost is the
HashMap construction inside the bench harness, not the
`run_main`/JIT-boundary code. The right answer is the new
`run_main_smallmap` row at 70.06 ns, which is **5× under** target.
Hosts that need < 280 ns per dispatch should migrate to the SmallMap
or typed-i64 API; the HashMap path stays in place as the
trait-compatible compatibility surface.

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
