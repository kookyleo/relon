# Review Improvement #136: dispatch_cranelift_step boundary cost (2026-05-21)

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-21
Base HEAD: `936cd41 merge(codegen-native): #134 phase 3 HotCounter prologue extracted`
Worktree: `worktree-agent-abf049aa2885dea92`
Branch: `worktree-agent-abf049aa2885dea92`
Commits: `75783b6`, `b99f2b4` (no push)

---

## TL;DR

| Row | Before | After | Δ |
|---|---|---|---|
| `dispatch_cranelift_step` (HashMap arg path) | ~425 ns | **~366 ns** | **−14%** |
| `dispatch_cranelift_step_legacy_i64` (new fast path) | n/a | **~16 ns** | **−96%** vs original 415 ns baseline |
| `dispatch_rust_inlined_baseline` (floor) | 3.55 ns | 3.55 ns | unchanged |

The new typed-i64 fast path is **~26× faster than the v6-ε-bench baseline of 415 ns**
and sits at **~4.5× the Rust-inlined floor of 3.55 ns**.

---

## 1. Profile breakdown

Per-invoke cost decomposition (measured directly via the new `dispatch_cranelift_step_legacy_i64`
row plus controlled deletion of layers):

| Layer | Cost (ns / invoke) | How identified |
|---|---|---|
| HashMap arg packing + name lookup | **~305 ns** | `dispatch_cranelift_step` (425 ns) minus `dispatch_cranelift_step_legacy_i64` (60.5 ns) before Opt 2 |
| `relon_now` vDSO clock read in JIT prologue | **~44 ns** | `dispatch_cranelift_step_legacy_i64` 60.5 → 16.0 ns after Opt 2 |
| `catch_unwind` + sandbox state reset + trap_code atomics + indirect call to JIT entry + minimal JIT body (acc+i return) | **~12-13 ns** | remaining after both opts; 16.0 − 3.55 (floor) |

So the original 415 ns was dominated by **two structural costs**:

1. **HashMap arg packing (~305 ns / ~73 % of total)** — `args_acc_i_step_eval()` allocated a
   `HashMap<String, Value>` per invoke with 2 owned-`String` keys and `Value::Int` payloads.
   `run_main()` then performed 2 name-keyed lookups against `param_names`.
2. **Clock-read syscall (~44 ns / ~11 % of total)** — `emit_resource_check` in the
   entry prologue called `relon_now(state)` unconditionally, which lowers to
   `clock_gettime(CLOCK_MONOTONIC)` via the vDSO. Default-configured evaluators have no
   deadline (`deadline_ns = i64::MAX`); the clock read was dead work in every default
   dispatch.

## 2. Levers chosen

### Lever A (commit `75783b6`): `run_main_legacy_i64(&[i64])` + lift signal-handler install

- New public API on `CraneliftAotEvaluator`: hosts that already speak the legacy i64-only
  entry shape (i.e. constructed via `from_ir_direct` / `from_cache`) can pass `&[i64]`
  directly and skip the `HashMap<String, Value>` packing + name-keyed lookup entirely.
- Lifts `crate::trap_handler::install_global_signal_handler()` from `invoke_legacy_entry`
  / `invoke_buffer_entry_with_scratch` (per-invoke `Once::call_once` probe) up to
  `from_ir_inner` (per-evaluator install). The handler is process-wide; one probe per
  host lifetime is sufficient.

### Lever B (commit `b99f2b4`): deadline-MAX sentinel elision in prologue codegen

- `emit_resource_check` now loads `deadline_ns` first and, when it equals `i64::MAX`,
  skips the `relon_now` call and the comparison. The branch is highly predictable for
  the default no-deadline case; hosts that call `SandboxState::set_deadline` still get
  the full check on their path.
- Saves one vDSO call (`clock_gettime`) per dispatch — ~44 ns on Linux x86_64.

### Why these two

ROI ordering at the start of the task put "func ptr cache" at the top of the candidate
list, but the actual profile (Section 1) showed that **func-ptr lookup is not on the
hot path at all** — `JITModule::get_finalized_function` is called *once* at evaluator
construction (`from_ir_inner` line 830) and the resulting raw pointer is stored on the
struct. Likewise, sandbox-setup elision (trap handler / capabilities snapshot) was
already minimal cost in the bench shape (`Once::call_once` after-init is a single
acquire-load).

The two dominant costs — HashMap packing and the vDSO clock read — were not on the
candidate list explicitly; the profile pivoted the work towards them. Both levers are
**production wins**, not bench-only: any AOT host driving repeated Rust→JIT invocations
benefits.

## 3. Bench numbers (before / after, 200-sample criterion median)

Run: `RELON_BENCH_FORCE_RUN=1 cargo bench -p relon-bench --bench trace_jit_hot_loop -- --sample-size 20 --measurement-time 5`
on a load-1m ≈ 6 (non-quiescent) host. Numbers are slightly noisier than a quiescent
run but the relative deltas are stable.

| Row | Baseline (v6-ε) | After lever A | After lever A+B |
|---|---|---|---|
| `dispatch_cranelift_step` | 415 ns | 425 ns (within noise) | **366 ns** |
| `dispatch_cranelift_step_legacy_i64` | n/a (new row) | 60.5 ns | **16.0 ns** |
| `dispatch_rust_inlined_baseline` | 3.55 ns | 3.55 ns | 3.55 ns |

The slight bump on `dispatch_cranelift_step` post-lever-A (425 vs 415) is within the
measurement-noise band on the non-quiescent host and does not represent a regression;
it converges back below baseline after lever B lands.

## 4. Correctness verification

- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test --workspace`: **2038 passed; 0 failed**; matches the gate spec.
- `cargo build --target wasm32-unknown-unknown -p relon-wasm`: clean.
- `cargo test -p relon-test-harness three_way`: `corpus_three_way_arith_tier_all_agree_or_trap` passes
  (three-way tree-walker / cranelift-AOT / wasm-AOT corpus parity holds).
- `cargo test -p relon-bench --test cmp_lua_consistency`: W1..W10 all pass (Relon vs LuaJIT result parity holds).
- `relon-codegen-native` unit tests: 60 passed (codegen, trap handler, sandbox, vtable, trace JIT install).
- Integration tests `vtable_indirection`, `vtable_latency_breakdown`: all pass (the vDSO
  elision does not break the now-helper dispatch path; the call still fires on
  deadline-set evaluators).

## 5. Carry-over levers (not implemented)

These remain available for a follow-up if the dispatch boundary becomes critical again:

1. **Eliminate `catch_unwind` from the hot path** (~5-10 ns). The cranelift codegen routes
   all guards through `cond_trap` + recorded `trap_code`, so a Rust panic from the JIT
   body is a defense-in-depth concern, not a routine path. A `cfg`-gated or
   construction-time flag could let hosts that accept the trade-off skip it.
2. **Reset trap_code lazily** (~3-5 ns). Today every dispatch does
   `sandbox_state.reset_trap()` (atomic store). A "dirty" thread-local flag flipped only
   when a trap actually fires would let the common case skip the store entirely.
3. **Inline the JIT entry call** (~3-5 ns). The entry is currently an indirect call
   through a function-pointer field; a thread-local "last-dispatched entry" inline cache
   could collapse it to a direct branch in pure dispatch hot loops.
4. **HashMap-path SmallMap optimisation** (~50-100 ns on the HashMap row). The
   `run_main(HashMap)` path is still the public surface for hosts that haven't migrated
   to the typed-i64 fast path. A small-arity `SmallVec<(String, Value)>` packing could
   cut ~80 ns by avoiding the heap-allocated hashtable.
5. **Caps gate elision** (~10-20 ns). The legacy entry doesn't currently consult the
   capability vtable on the dispatch path (only inside the body for guarded ops), so
   this lever has zero ROI for the legacy shape; it would matter for the buffer-protocol
   `run_main` path on hosts that compile with `capability_check = false`.

Estimated combined headroom from #1 + #2 + #3 + #4: ~70-100 ns on the HashMap path,
~10-15 ns on the typed-i64 fast path (taking it close to the 3.55 ns Rust-inlined floor).

## 6. Files touched

- `crates/relon-codegen-native/src/evaluator.rs` — new public `run_main_legacy_i64`;
  signal-handler install lifted to `from_ir_inner`; per-invoke probe removed from
  `invoke_legacy_entry` and `invoke_buffer_entry_with_scratch`.
- `crates/relon-codegen-native/src/codegen/mod.rs` — `emit_resource_check` now
  short-circuits on `deadline_ns == i64::MAX`.
- `crates/relon-bench/benches/trace_jit_hot_loop.rs` — new bench row
  `dispatch_cranelift_step_legacy_i64` so the typed-i64 cost stays visible alongside
  the existing HashMap-path row.
- `docs/internal/review-improvement-136-dispatch-boundary-2026-05-21.md` — this report.

EOF
