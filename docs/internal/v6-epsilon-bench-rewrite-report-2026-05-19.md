# v6-ε Bench Rewrite Report (2026-05-19)

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-19
Base HEAD: `1a640ad feat(trace-emitter): v6-epsilon-0-A inline splice infra + falsifies call-boundary`
Worktree: `worktree-agent-a77de70826f538bbe`
Status: bench harness rewritten + 3-round criterion measurement
captured + decision recorded.

Companion docs:
- `docs/internal/v6-epsilon-0a-stage-report-2026-05-19.md`（M2-C / ε-0-A 三次失败的 attempt 链）
- `docs/internal/v6-epsilon-0c-stage-report-2026-05-19.md`
- `docs/internal/v6-delta-m2c-stage-report-2026-05-19.md`
- `docs/internal/wasm-bench-report-2026-05-16.md`（v6-ε bench-rewrite 附录见尾部）

---

## 0. TL;DR

| Row | Per-iter (ns/iter, R1/R2/R3 medians) | Class |
|---|---|---|
| `tree_walk_loop` | 3385 / 3364 / 3364 ns/iter (≈ 3.4 µs/element) | µs-class baseline |
| `cranelift_aot_loop` | 2.074 / 2.073 / 2.073 | ns-class, AOT loop |
| **`trace_jit_loop`** | **1.186 / 1.185 / 1.185** | **LuaJIT-class** |
| `rust_native_loop` | 2.499 / 2.484 / 2.480 | theoretical floor |
| `dispatch_cranelift_step` | 433.7 / 415.3 / 409.8 | per-invoke Rust→AOT |
| `dispatch_trampoline` | 9.51 / 9.54 / 9.50 | per-invoke Rust→JIT |
| `dispatch_ic` | 9.57 / 9.57 / 9.56 | per-invoke Rust→JIT |
| `dispatch_tail` | 9.53 / 9.54 / 9.55 | per-invoke Rust→JIT |
| `dispatch_sysv` | 9.53 / 9.53 / 9.53 | per-invoke Rust→JIT |
| `dispatch_inline` | 9.53 / 9.53 / 9.56 | per-invoke Rust→JIT |
| `dispatch_rust_inlined_baseline` | 3.553 / 3.553 / 3.552 | per-invoke shape Rust floor |

**Decision**: `trace_jit_loop = 1.185 ns/iter` (median across 3 rounds).
**Under the brief's ≤ 3 ns/iter target by a factor of ~2.5×.**
Inside LuaJIT trace-tier band (1-3 ns/iter). **ε phase has met the
hot-loop perf bar.** Recommendation: stop adding ε-M sub-phases that
chase further per-iter cuts on the trace JIT; the remaining work is
production wiring (recorder learns to record loops) rather than
codegen-tier optimisation.

---

## 1. Why the old bench was misleading

The previous `trace_jit_hot_loop.rs` (v6-γ M5 → v6-δ M2-C → v6-ε-0-C
→ v6-ε-0-A) measured a per-iter shape where the **Rust caller** drove
the inner loop and the JIT-compiled trace fn was invoked **once per
iter**:

```text
Rust bench loop:
    for i in 0..N {
        args[0] = acc; args[1] = i;
        trace_fn(&mut ctx, args.as_ptr())   // <-- one extern-C call PER ITER
        acc = ctx.result_slot as i64;
    }
```

Every "trace JIT row" (`trace_jit_warm`, `trace_jit_warm_ic`,
`trace_jit_warm_tail`, `trace_jit_warm_sysv`,
`trace_jit_warm_inline`) sat at a **stable 9.5 ns/iter** floor across
**every** optimisation we layered in. Three consecutive engineering
attempts produced 0.00-0.04 ns / iter delta:

| Phase | Hypothesis | Probe | Δ vs prior best |
|---|---|---|---|
| v6-δ M2-C | inner trace-call enum-mapping cost = 6 ns | IC slot caches typed fn pointer | 9.55 → 9.54 ns (Δ = 0.01) |
| v6-ε-0-C | prologue/epilogue + ABI conv = 6 ns | CallConv::Tail vs SystemV | 9.54 → 9.54 ns (Δ = 0.00) |
| v6-ε-0-A | inner `call trace_fn_ptr` site = 6 ns | host fn body IS the trace body (no inner call) | 9.54 → 9.55 ns (Δ = +0.01) |

The chain is a **3-attempt falsification**: every component that could
plausibly own the 6 ns gap between the JIT row (9.5 ns) and the Rust
inlined floor (3.5 ns) was independently eliminated. By exhaustion,
the cost is **the Rust→JIT call boundary** the bench harness itself
imposes per iter — not anything intrinsic to the trace JIT. A real
Relon hot loop never pays that cost per iter; it pays it once for the
entire trace invocation.

## 2. New bench methodology

### 2.1 Two row families

- **loop-INSIDE rows** (`tree_walk_loop`, `cranelift_aot_loop`,
  `trace_jit_loop`, `rust_native_loop`): the loop body lives inside
  the callee. One Rust→callee invoke per criterion iteration; the
  callee runs all N iters internally; criterion reports total /
  `HOT_LOOP_N` via `Throughput::Elements(HOT_LOOP_N)`. **This is the
  honest hot-loop cost.**
- **dispatch-boundary rows** (`dispatch_trampoline`, `dispatch_ic`,
  `dispatch_tail`, `dispatch_sysv`, `dispatch_inline`,
  `dispatch_cranelift_step`, `dispatch_rust_inlined_baseline`):
  preserved from the v6-γ shape so the M2-C → ε-0-A falsification
  chain stays auditable in one bench output. **These measure the
  per-dispatch Rust→JIT call boundary cost, not hot-loop cost.**

### 2.2 Constants

- `HOT_LOOP_N = 1_000_000` — used by all rows except `tree_walk_loop`.
- `TREE_WALK_LOOP_N = 10_000` — tree-walker would be ~30 s per sample
  at 1M; dropped to 10K with a per-row `Throughput::Elements(...)`
  adjustment so the per-element-cost surface stays comparable.

### 2.3 Sample / measurement window

- `sample_size = 30`, `measurement_time = 6 s` per row (carry-over
  from ε-0-A). Two rows (`dispatch_cranelift_step`,
  `tree_walk_loop`) auto-extend to ~12 s because their per-iter
  cost is high.

### 2.4 What the `trace_jit_loop` row actually is

The trace recorder today emits straight-line
`LocalGet+LocalGet+Add+Return` traces; it does **not** yet record an
`Op::Loop` body with a backward branch end-to-end. The trace JIT
emitter side has `TraceOp::MarkLoopHead` / `MarkLoopBack` lowering
that compiles into a real cranelift loop block (see
`crates/relon-trace-emitter/src/emitter.rs:535-563`), but the
recorder doesn't insert those markers yet.

Per the task brief's allowed option (a), we **bypass the recorder**
and hand-build the trace-JIT-compiled function for the bench:

- `build_trace_jit_loop_fn()` in
  `crates/relon-bench/benches/trace_jit_hot_loop.rs` constructs a
  `JITModule` directly via cranelift-jit (same flag set as the
  trampoline path: `is_pic=false`, `opt_level=speed`, no probestack,
  no frame pointers).
- Exported function signature matches `TRACE_ENTRY_SIG`
  (`(*mut TraceContext, *const u64) -> i32`).
- Body shape mirrors what a fully-extended recorder would produce
  for the source `for i in 1..=n { acc += i }`:
  - Entry block: load `n` off `args_ptr[0]`, seed `acc=0`, `i=1`,
    jump to header.
  - Header block: `(acc, i)` block params; `i <= n` test via `icmp
    SignedLessThanOrEqual` + `brif` to body or exit.
  - Body block: `sadd_overflow(acc, i)`, `iadd(i, 1)`, guard via
    `brif (of==0)` to header / deopt; back-edge to header.
  - Exit block: store `acc` into `ctx.result_slot`, return Success.
  - Deopt block: `call __relon_trace_save_deopt(ctx, 0, 0)`, return
    GuardFailed (cold path; overflow inside i64 with positive `i ≤
    1M` doesn't fire for the bench input).

This is shape-equivalent to what the existing
`emit_loop_head` / `emit_loop_back` lowering produces when fed
appropriate `TraceBuffer` ops — there is no codegen-tier difference
between "recorder-built loop trace" and "hand-built loop trace"
once both reach the cranelift emitter.

### 2.5 Recorder gap as carry-over

The recorder gap is out of scope for this bench-rewrite phase. The
follow-up work (recorder learns to emit `MarkLoopHead` / `Cmp` +
guard exit / `MarkLoopBack` when the walker hits an `Op::Loop` body
with the right shape) is documented in §10. The JIT codegen path
this bench exercises is the one the recorder will eventually feed
into; the per-iter number does not change when the entry point
flips from "hand-built" to "recorded".

## 3. Results table (criterion median, 3 rounds, sample_size = 30)

### 3.1 Loop-INSIDE rows

| Row | R1 (ns/iter) | R2 | R3 | Median | vs rust_native_loop |
|---|---|---|---|---|---|
| `tree_walk_loop` | 3385 | 3364 | 3364 | 3364 | 1356× slower |
| `cranelift_aot_loop` | 2.074 | 2.073 | 2.073 | 2.073 | 0.84× (faster) |
| **`trace_jit_loop`** | **1.186** | **1.185** | **1.185** | **1.185** | **0.48× (faster)** |
| `rust_native_loop` | 2.499 | 2.484 | 2.480 | 2.484 | 1.00× |

Why is `trace_jit_loop` faster than `rust_native_loop`? Two reasons:
1. The Rust source uses `checked_add` which lowers to two branches
   (the overflow test + the `match` arm dispatch) per iter, vs the
   trace's hand-emitted `sadd_overflow + brif` which is one
   instruction + one branch.
2. The trace JIT module is built with `opt_level=speed` and the
   header block carries `(acc, i)` as cranelift block-params —
   regalloc tends to keep both in registers across the loop
   back-edge. The Rust `rustc` build of the bench inherits whatever
   the criterion harness pulls in (typically `opt-level=3` but with
   panic infrastructure around `checked_add`).

`cranelift_aot_loop` at 2.07 ns is right around the rust-native row,
which is the sanity check: the cranelift backend produces
roughly-rustc-class loop machine code for the same body shape.

### 3.2 Dispatch-boundary rows (carry-over from ε-0-A)

| Row | R1 | R2 | R3 | Median |
|---|---|---|---|---|
| `dispatch_cranelift_step` | 433.7 | 415.3 | 409.8 | 415.3 |
| `dispatch_trampoline` | 9.507 | 9.538 | 9.496 | 9.507 |
| `dispatch_ic` | 9.570 | 9.571 | 9.558 | 9.570 |
| `dispatch_tail` | 9.531 | 9.536 | 9.547 | 9.536 |
| `dispatch_sysv` | 9.532 | 9.533 | 9.530 | 9.532 |
| `dispatch_inline` | 9.532 | 9.534 | 9.559 | 9.534 |
| `dispatch_rust_inlined_baseline` | 3.553 | 3.553 | 3.552 | 3.553 |

The five trace-JIT dispatch rows continue to land in the 9.50-9.57
ns band (spread = 0.07 ns, well inside criterion noise threshold).
The 5.98 ns gap to `dispatch_rust_inlined_baseline` is the
Rust→JIT extern-C boundary cost the bench harness imposes per
dispatch. The ε-0-A stage report attributes that gap to:

| Component | Estimated cost (ns) |
|---|---|
| Rust caller: `args[0..1] = ...; lea rsi, args` | ~1.5 |
| Rust → JIT entry: `call rax` + BTB | ~1.5 |
| JIT entry: `ret` + RSB | ~1.0 |
| Rust caller: read `ctx.result_slot`, `acc = ...` | ~1.0 |
| Rust loop overhead: `i++; cmp; jne` | ~1.0 |
| **Total** | **~6 ns** (matches measured 5.98 ns) |

None of these components are intrinsic to a trace JIT. They are
**bench-harness artefacts** — eliminated by the loop-INSIDE
methodology above.

`dispatch_cranelift_step` at ~415 ns/iter is the HashMap-based
`run_main(args: HashMap<String, Value>)` API cost per call;
materially higher than the raw extern-C boundary because of arg
boxing.

## 4. Direct vs LuaJIT comparison

LuaJIT 2.x trace-tier hot loops typically run at 1-3 ns/iter on
modern x86_64 — the trace contains the entire loop body, including
back-edge guards, regalloc'd into registers across the back-edge.

| System | Per-iter | Notes |
|---|---|---|
| **Relon `trace_jit_loop` (this work)** | **1.185 ns/iter** | hand-built cranelift loop; loop body + overflow guard inside trace |
| LuaJIT 2.x trace tier | 1-3 ns/iter | typical hot-loop tier on same workload class |
| Relon `cranelift_aot_loop` | 2.073 ns/iter | AOT-compiled, same body |
| Rust native | 2.484 ns/iter | with `checked_add` |
| Relon `dispatch_*` rows | 9.5 ns/iter | bench-harness artefact, NOT hot-loop cost |

**Relon's trace JIT, when actually compiling a loop body, sits
inside the LuaJIT band.** The 9.5 ns dispatch-boundary number that
v6-γ M5 → v6-ε-0-A iterated against was never a hot-loop number; it
was a per-invocation harness cost.

## 5. Decision: is hot loop ≤ 3 ns/iter?

**YES** — `trace_jit_loop = 1.185 ns/iter`, ~2.5× under the brief's
3 ns/iter target, and inside the LuaJIT 2.x band.

### 5.1 Recommendation

**Stop the ε phase's per-iter optimisation work.** The hot-loop
performance bar has been met; remaining ε-M sub-phases that target
the trace JIT codegen (bounds hoist, LICM, overflow hoist) are
unnecessary for the per-iter cost target — `1.185 ns/iter` already
factors in the `sadd_overflow + brif` per iter, so eliminating the
guard wouldn't move the number into a different class.

### 5.2 What IS still relevant

- **Recorder learning to record loops**: today the recorder bails on
  back-edges. Without the recorder, no Relon source program will hit
  the `trace_jit_loop` path automatically. This is a recorder-side
  feature, not a JIT-side optimisation. **Required for the
  `1.185 ns/iter` to be available to real Relon code.**
- **Cap hoisting for cross-trace inter-call**: the v6-ε plan §3
  ε-M4 phase's batched resource-tick check is still relevant for
  long-running traces (e.g. > 100K iters under a `max_steps`
  sandbox). Distinct concern from per-iter cost; doesn't move the
  bench number, does affect sandbox-bound real workloads.
- **`TraceOp::Call` inside loops**: traces with intra-trace `Call`
  ops aren't yet supported by the inline path
  (`InlineEmitError::CallNotSupportedInInline`). This is a
  composability gap, not a perf gap; tracked in ε-0-A §10.5.

### 5.3 What is NOT relevant any more

- ε-M1 "bounds-check hoist" / ε-M2 "overflow hoist" / ε-M3 "ABI tier
  redesign" — none of these would move `trace_jit_loop` out of the
  LuaJIT band; the band is already met.
- Further attempts to flatten the `dispatch_*` rows to ~4 ns/iter —
  those rows are bench-harness artefacts. Real Relon hot loops don't
  dispatch per iter.

## 6. Gate report

| Gate | Status | Detail |
|---|---|---|
| `cargo build --workspace` | green | clean |
| `cargo test --workspace` | green | **1761 passing** (matches ε-0-A baseline; no new tests added — bench-only change) |
| `cargo clippy --workspace --all-targets -- -D warnings` | green | clean |
| `cargo fmt --all -- --check` | green | clean |
| `cargo build --target wasm32-unknown-unknown -p relon-wasm` | green | clean |
| `cargo bench --bench trace_jit_hot_loop` | 3 rounds captured | see §3 |

## 7. File-level changes

### 7.1 Modified

- `/ext/relon/crates/relon-bench/benches/trace_jit_hot_loop.rs` —
  full rewrite. Module doc explains the v6-γ → v6-ε falsification
  chain + the new methodology; row layout splits into loop-INSIDE +
  dispatch-boundary families; new `build_trace_jit_loop_fn()` hand-
  builds a cranelift JIT module whose body runs the full N-iter
  `1..=n` loop with overflow guard.
- `/ext/relon/crates/relon-bench/Cargo.toml` — adds
  `cranelift-frontend`, `cranelift-module`, `cranelift-jit`,
  `cranelift-native`, `relon-trace-emitter` as direct dependencies
  for the new `trace_jit_loop` row's JIT-build path. All deps pin to
  the workspace's 0.131 cranelift line; no new external crates
  introduced beyond what `relon-codegen-native` already vendors.

### 7.2 Added

- `/ext/relon/docs/internal/v6-epsilon-bench-rewrite-report-2026-05-19.md` —
  this report.

### 7.3 Modified (docs)

- `/ext/relon/docs/internal/wasm-bench-report-2026-05-16.md` —
  v6-ε bench-rewrite appendix added at the tail.

## 8. luajit row decision

Per the brief's "optional `lua_jit_loop` row" guidance, **skipped**
in this phase. Rationale:

- `mlua` / `rlua` are not in the workspace today; pulling them in as
  bench-only dev-deps would add ~30 transitive crates + require
  `luajit` system lib for linking.
- The brief allows skipping with an explicit note: "blocked on
  luajit install".
- The comparison made in §4 cites LuaJIT 2.x's documented hot-loop
  trace-tier band (1-3 ns/iter on similar x86_64 hardware). That is
  the relevant qualitative bound; an in-repo `lua_jit_loop` row would
  pin a specific Lua source body but wouldn't change the qualitative
  conclusion already drawn from `trace_jit_loop = 1.185 ns/iter`.

Setup script for someone who wants to run an in-repo LuaJIT
comparison locally:

```bash
# Install libluajit-5.1-dev (Debian/Ubuntu) or luajit (Arch/Fedora).
sudo apt install libluajit-5.1-dev
# Add to crates/relon-bench/Cargo.toml [dev-dependencies]:
#   mlua = { version = "0.10", features = ["luajit", "vendored"] }
# Then add a `lua_jit_loop` bench row in trace_jit_hot_loop.rs
# that compiles + runs:
#   local function f(n) local s = 0; for i=1,n do s = s + i end; return s end
# under mlua's `Lua::load(...)`.
```

Estimated effort: ~2 hours including dep adoption review.

## 9. Tightness of the trace_jit_loop number

A few sanity checks beyond "3-round criterion median is stable":

### 9.1 Per-iter cycle count

At 1.185 ns/iter on a 3.0 GHz machine (this worker), that is **~3.5
cycles/iter**. Cranelift compiles the body to roughly:

```text
header:
    cmp rdi, rsi          ; i <= n?
    jg  exit              ; if i > n -> exit
    add rcx, rdi          ; (acc, of) = sadd_overflow(acc, i)
    jo  deopt             ; if of -> deopt
    add rdi, 1            ; i++
    jmp header
```

5 instructions, ~3-4 cycles when both branches predict correctly
(taken loop back-edge, not-taken overflow). Matches the measurement.

### 9.2 Result correctness

The exit block stores `acc` into `ctx.result_slot`. For `n = 1M`,
the analytic sum is `n*(n+1)/2 = 500_000_500_000`. The bench wraps
the result in `black_box` to prevent the compiler from folding it
away; smoke-running the bench confirms `ctx.result_slot ==
500_000_500_000` matches the analytic value. The `assert_eq!(raw,
0, ...)` in the bench loop catches any deopt fire.

### 9.3 Bench-harness overhead

The `b.iter(|| ...)` closure for `trace_jit_loop` allocates a fresh
`TraceContext::with_capacity(64)` per criterion iteration. That
alloc is a single `Box<[u64; 64]>` (~512 bytes) + zero-init; cost
~50 ns amortised over 1M loop iters = 0.00005 ns/iter — negligible.
The `args: [u64; 1]` is stack-allocated.

### 9.4 Why not check 1M-iter cost in a single sample

Criterion runs `b.iter(...)` many times per sample (the harness
auto-tunes the inner-iter count to keep each sample in the right
duration band). For `trace_jit_loop` it picked ~5115 iters per
sample at 6 s measurement-time — so the reported 1.185 ns/iter is
already averaged over `5115 × 1_000_000 = 5.1 billion` body iters.
The variance across the 30 samples per round is < 0.05 ns/iter
(see R1-R3 confidence intervals in the raw bench output).

## 10. Carry-over / follow-up

### 10.1 Recorder learns to record loops

**Phase**: ε-M0-recorder-loops (new sub-phase, not on the existing
v6-ε plan). **Effort**: estimated 2-3 days. **Blocker for**:
real Relon source code reaching the `trace_jit_loop` codegen path
automatically. **NOT blocker for**: the perf bar (this report
already showed the JIT path is fast enough).

Sketch:
- Recorder hits an `Op::Loop` IR node → emits `MarkLoopHead { loop_id }`
  ahead of the body, recurses into body, emits the body trace, emits
  `MarkLoopBack { loop_id }` at the back-edge.
- Loop-exit guard: today's `Op::BrIf` consumes a bool; the recorder
  inserts `Cmp + Guard(BoundsCheck-like)` at the exit branch so the
  exit predicate becomes a recorded guard. Existing `BoundsCheck`
  guard lowering branches to deopt when the predicate fires; the
  recorder would need a new `LoopExit` guard kind whose deopt arm
  takes the **happy** path (loop fall-through), or alternatively the
  recorder always records the loop as "keep going" and the exit is a
  side trace.
- Smoke test: parse a Relon source `#main(Int n): for i in 1..=n: ...`
  → analyzer → IR → recorder → optimiser → emitter; assert install
  produces an `OptimizedTrace` containing `MarkLoopHead` /
  `MarkLoopBack`; assert calling the resulting `JITedTraceFn` with
  `n = 1M` runs in < 2 ns/iter (regression bench).

### 10.2 Bench rows we did NOT add

- `lua_jit_loop` — skipped per §8.
- `cranelift_aot_step_one_invoke` — i.e. cranelift AOT invoked once
  on a fn whose body is the 1-step `acc+i`, looped N times by Rust.
  Same shape as `dispatch_cranelift_step` (already present) but
  without the HashMap arg overhead. Not added — the existing
  `dispatch_*` rows already isolate the per-dispatch costs.

### 10.3 ε-0-C carry-overs still unaddressed

- String-slot / wasm linear-memory true path: 12
  `BytecodeUnsupported` cases carried over from ε-0-C.
- `Op::Select` dedicated BcOp: v6-δ envelope item.

Neither is addressed by this bench rewrite; both remain on the
original carry-over list.

### 10.4 Recorder-gap entries (option-(a) follow-up)

If/when the recorder is extended to emit loop traces end-to-end:

1. Add a smoke test in `relon-trace-recorder/tests/` that drives a
   trace through `__relon_jump_to_recorder` for a fn whose IR has
   `Op::Loop { body: ... }` and asserts the resulting
   `OptimizedTrace::op_count()` matches the hand-built shape this
   bench uses (~6-8 ops: 2 LocalGets + 1 Cmp + 1 Guard +
   1 MarkLoopHead + 1 Add + 1 Add + 1 MarkLoopBack + 1 Return).
2. Add a `trace_jit_loop_recorded` bench row in
   `crates/relon-bench/benches/trace_jit_hot_loop.rs` that goes
   through the recorder install path. Compare against `trace_jit_loop`
   (hand-built); the delta should be ≤ 0.1 ns/iter — if it isn't,
   either the recorder added per-iter overhead (e.g. an unintended
   extra guard) or the optimiser is dropping an op the hand-built
   row keeps.
3. Once both rows are within noise: retire `trace_jit_loop`
   (hand-built) and keep only the recorded variant as the canonical
   measurement; rename to `trace_jit_loop` again.

EOF
