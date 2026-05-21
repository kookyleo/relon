# review-improvement-148 — bytecode M3 stage report (2026-05-21)

## Scope landed

M3 phase 1 — bytecode VM closure surface. The closure ops + arena +
dispatch + sub-loop are wired; the IR-side lambda-body hoist that
would unlock W1-W8 source-level workloads stays as a follow-up.

Branch: `worktree-agent-aa4c0376034cf5a20`
Base: `d8ef62a` (local main HEAD at start)
HEAD: `52686c9`
Worktree: `/ext/relon/.claude/worktrees/agent-aa4c0376034cf5a20`

Commits (5, split by op):

```
a1fdebc feat(bytecode): closure arena scaffold
923a1f4 feat(bytecode): MakeClosure / CallClosure / CaptureGet bcops
7b44bbf feat(bytecode): VM dispatch for closure ops + closure-body sub-loop
4c54bb7 test(bytecode): closure dispatch end-to-end + BcFunction field plumbing
52686c9 chore(bytecode): clippy + fmt cleanup post closure ops
```

## Closure design

Three new ops in `BcOp`:

- `MakeClosure { body_idx: u32, capture_count: u32 }` — pops
  `capture_count` operands in declaration order, allocates a fresh
  `ClosureSlot { body_idx, captures }` in `VmMemory.closures`, pushes
  the resulting `u32` handle into the operand-stack `u64` lane.
- `CallClosure { argc: u32 }` — pops `argc` args + the closure handle,
  resolves the body via the slot's `body_idx` into the enclosing
  `BcFunction::closure_bodies` slice, recurses through
  `invoke_closure_body` which lays out args into the body's `locals`
  and threads the captures into `dispatch_one` for `CaptureGet`
  consultation.
- `CaptureGet { idx: u32 }` — reads `captures[idx]` of the
  currently-executing closure body. Outside a closure body surfaces
  as `StackUnderflow` (compiler-bug envelope, matches the existing
  arena-OOR + local-OOR convention).

Storage model: `ClosureArena` parallels `ListArena` / `StringArena` /
`DictArena` in `crate::arena`. Monotonic alloc, `Arc<ClosureSlot>`
for cheap refcount-bump handle propagation. Slot count rolls into
`VmMemory::total_slot_count`.

Body addressing: each `BcFunction` gains a `closure_bodies:
Vec<BcFunction>` field carrying the lambdas it instantiates. The
compile pass starts populating this slice once the IR-side hoist
lands; until then hand-built tests fill it directly.

## Iter stdlib choice

Option (a) deferred. The bench audit revealed that even the trivial
W1 path (`list.sum(range(n))`) routes through `Op::Call { fn_index =
list_int_sum }` whose body (`crates/relon-ir/src/stdlib/defs.rs`
lines 1240-1335) is already buffer-protocol-shaped — `LoadI64AtAbsolute`,
`BitAnd`, `LoadField`, alignment arithmetic. Inlining it through the
existing bytecode compile path would require lifting the absolute-load
ops + the BitAnd op into bytecode equivalents; that's a bigger lift
than the M3 closure surface and is its own follow-up phase.

The cleanest target remains option (a): once the bytecode crate gains
`BcOp::IntRangeIterInit / IntRangeIterNext` (or equivalent
arena-free reducer skeleton), the analyzer can desugar
`list.sum(range(n))` into an explicit-loop IR shape that compiles
straight into bytecode. The closure ops landed here are the
foundation that integration relies on — once range/reduce
desugaring lands, the closure body is the per-element step. The
reducer-pattern hand-built test
(`closure_reducer_sum_of_zero_to_n_minus_one`) pins the dispatch
invariants the future pipeline will rely on.

## Workload coverage

| Workload | Source pattern               | Status |
|----------|------------------------------|--------|
| W1       | `list.sum(range(n))`         | still n/a (UnsupportedOp: Call) |
| W2       | `list.sum(range(n).map(..))` | still n/a (closure path blocked on IR-side hoist) |
| W3-W6    | `range(n).reduce(..)` etc    | still n/a |
| W7       | recursive `fib`              | still n/a (Op::Call) |
| W8       | `list.sum([..])`             | still n/a (Op::Call, buffer-protocol body) |
| W11/W12  | trivial scalar `x + 1`       | OK (M2-A) |

Honest: **0 workloads unlocked this session.** The closure VM ops are
foundational but the source-level pipeline needs (a) the IR-side
closure body hoist to populate `closure_bodies` from
`Op::MakeClosure` and (b) the iter stdlib desugaring. Both are
multi-session follow-up work.

## 4-way bench coverage matrix update

Unchanged from M2-C — W12 remains the only row green; all others
record `n/a (UnsupportedOp)` against the bytecode column. The matrix
will move when the IR-side closure hoist + the iter desugaring land.

## Gate status

- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean (one
  `#[allow(clippy::too_many_arguments)]` added on `dispatch_one`; the
  alternative — bundle dispatch state into a struct — buys nothing
  since the fields are pulled apart for borrow-discipline reasons).
- `cargo test --workspace`: 2171 tests pass, zero failures (above
  the 2161 floor).
- `cargo check -p relon-bytecode --target wasm32-unknown-unknown`:
  clean (bytecode crate stays cranelift-free as required).
- `corpus_four_way_diff_aggregates`: 32 AllAgree + 4 AllTrap + 1
  BytecodeMatchesBaseline + 18 BytecodeUnsupported + 0 mismatches
  (same as M2-C baseline).

## LoC delta

```
crates/relon-bytecode/src/arena.rs                 | 102 +++-
crates/relon-bytecode/src/compile.rs               |  45 ++
crates/relon-bytecode/src/lib.rs                   |   4 +-
crates/relon-bytecode/src/op.rs                    |  75 +++
crates/relon-bytecode/src/vm.rs                    | 160 +++++-
crates/relon-bytecode/tests/bytecode_sandbox.rs    |  47 ++
crates/relon-bytecode/tests/closure_dispatch.rs    | 415 ++++++
crates/relon-bytecode/tests/hot_counter_dispatch.rs|   3 +
crates/relon-bytecode/tests/partial_resume_sandbox.rs |  1 +
9 files changed, 847 insertions(+), 5 deletions(-)
```

Net delta: +842 LoC, concentrated in the new
`closure_dispatch.rs` test (415) + the vm.rs dispatch arms (~160) +
the arena scaffold (~100). All other files are mechanical
plumbing (BcFunction's new `closure_bodies` field threaded through
existing hand-built tests).

## Follow-up blueprint

Ordered by ROI for the cmp_lua coverage matrix:

1. **IR-side closure body hoist** — teach
   `relon_ir::lower_workspace_single` to emit per-lambda `Func`
   entries the bytecode compile pass can pull into
   `BcFunction::closure_bodies`. Once landed, `visit_make_closure`
   stops returning `UnsupportedOp` and emits `BcOp::MakeClosure { body_idx, .. }`
   against the hoisted body. Unlocks: nothing alone (need step 2).

2. **Iter stdlib desugaring** — pre-pass in the analyzer or IR
   lowering that rewrites `list.sum(range(n))` /
   `list.sum(range(n).map(f))` / `range(n).reduce(init, f)` into an
   explicit `Op::Loop` shape over an i64 counter, so the bytecode
   compile pass doesn't need to lift the buffer-protocol-shaped
   `list_int_sum` body. Unlocks: W1, W2, W8.

3. **Recursive closure call** — fib(W7) needs the bytecode VM to
   support a closure body that calls itself via the same handle.
   With step 1 + step 2 landed this is a small extra: the closure
   handle just needs to be readable from inside the body as a
   capture (currently it's not). Unlocks: W7.

4. **`map` + `filter` desugaring** — same approach as step 2 but
   the loop body invokes a user-visible closure per element via
   `CallClosure` then collects via `ListPush`. Unlocks: W3-W6,
   the string-shaped workloads (W3/W4 still need string concat
   intern bridging through the closure return).

## Risks left honest

- Closure-body resource accounting reuses the outer VM's
  `max_steps` counter but **resets** the step counter to 0 on
  sub-loop entry. That's a deliberate choice (each closure body
  re-evaluates against its own loop) but combined with a large
  outer loop calling a closure body could let a malicious source
  burn more cycles than the outer cap nominally allows. Tracked
  as M3-phase-2 hardening once the source-level path can reach
  this. Acceptable for the hand-built test path landed here.
- `dispatch_one` now takes 8 parameters (with the new
  `current_captures`); clippy threshold raised via
  `#[allow(clippy::too_many_arguments)]` rather than refactor. The
  alternative — bundle into a `DispatchFrame` struct — was rejected
  because the operand stack / locals / arena need pulled-apart
  mutable borrows that the struct shape would force back together.
