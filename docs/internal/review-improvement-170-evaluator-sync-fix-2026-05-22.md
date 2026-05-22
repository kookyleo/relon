# Review Improvement #170 — `CraneliftAotEvaluator` `Sync` UB Fix

Date: 2026-05-22 \
Branch: `worktree-agent-ac32151fcb2ced9cc` \
Scope: `crates/relon-codegen-native/src/{sandbox,evaluator}.rs`

## Problem

`CraneliftAotEvaluator` declared `unsafe impl Send + Sync`, but every
`run_main` invocation re-pointed the shared `Arc<SandboxState>`'s
`arena_base` / `arena_len` / `tail_cursor` / `scratch_cursor` /
`scratch_base` / `closure_table_base` `UnsafeCell<_>` fields via
`install_arena` / `install_scratch_base`. Two threads dispatching the
same evaluator concurrently raced on those writes — a data race / UB
under the Rust memory model. `sandbox.rs:421` admitted as much in a
comment that hand-waved away the per-call `UnsafeCell` writes.

## Decision: Option B (per-call ownership)

- **Option A** (drop `Sync`): rejected — the `Evaluator` trait in
  `relon-eval-api` requires `Send + Sync` and host code routes
  `Box<dyn Evaluator>` through multi-thread paths; cascading bound
  change touches every backend + the auto-evaluator surface.
- **Option C** (per-call state-ptr arg via cranelift ABI): rejected —
  changes the JIT entry signature and needs matching emit changes
  across every backend / cached-object generation.
- **Option B** (per-call `SandboxState` from immutable shared
  template): chosen — minimal blast radius, no cranelift ABI change,
  no `Evaluator` trait change.

## Implementation

1. **`SandboxShared`** (new): immutable template the evaluator holds
   in `Arc<_>` — `deadline_ns: AtomicI64`, `capabilities: Mutex<Arc<…>>`,
   `closure_table_base: AtomicUsize`, `epoch: Instant`. Stays
   `Send + Sync`. Mutex is taken only for capability swap / snapshot,
   never across a JIT dispatch.
2. **`SandboxState`**: removed `unsafe impl Sync`; removed
   `entry_range: Cell<TokenRange>` field (was never read — evaluator
   already owns `entry_range` directly). Added
   `from_template(&SandboxShared)` factory that snapshots deadline +
   caps + closure base into a fresh state. Now strictly `Send + !Sync`,
   asserted by a new unit test.
3. **`invoke_legacy_entry` / `invoke_buffer_entry_with_scratch`**:
   each allocates `Box::new(SandboxState::from_template(&shared))`,
   installs arena pointers on the local box, dispatches the JIT entry
   with `&*state` as the state pointer, then reads `trap_code` off the
   same local box in `dispatch_post*`. Two threads each see their own
   `Box<SandboxState>`; no shared `UnsafeCell` writes.
4. **`install_capabilities_mut` / `set_deadline`**: now write the
   template; the next dispatch picks up the new value, invocations
   already in flight keep the snapshot they took at dispatch entry.
5. **`dispatch_post` / `dispatch_post_unshielded`**: take the per-call
   `&SandboxState` as an extra arg instead of reading
   `self.sandbox_state`.

## Tests

- `crates/relon-codegen-native/tests/multi_thread_run_main.rs` (new):
  two regression tests spawning 4 threads × 256 dispatches each on
  the shared evaluator, asserting unique `(a, b)` / `x` inputs map
  to the expected output every iteration. Pre-fix this would race on
  the arena pointer and corrupt the return record; post-fix both
  cases pass.
- `sandbox.rs` unit tests: added
  `sandbox_state_is_send_but_not_sync` (compile-time witness) and
  `sandbox_shared_is_send_and_sync`.
- Full workspace: **2286 / 2286 tests pass** (baseline 2282 + 4 new),
  including the existing trap / cache / call_native / closure dispatch
  suites that touch the renamed fields.

## Perf delta

Bench: `v6_epsilon_hot_loop/backend/dispatch_cranelift_step`
(HOT_LOOP_N = 1280000 dispatches per sample, `RELON_BENCH_FORCE_RUN=1`
on schedutil hardware).

| Variant                | Per-sample time | Per-dispatch | Δ vs baseline |
|------------------------|-----------------|--------------|---------------|
| Baseline (pre-fix)     | 351.27 ms       | ≈ 274.4 ns   | —             |
| Post-fix (per-call Box)| 420.11 ms       | ≈ 328.2 ns   | **+53.8 ns**  |

Δ ≈ **+54 ns/dispatch**, well under the +100 ns budget the task brief
called the cut-line. Cost decomposition (rough estimate): `Box::new`
+ deallocation ≈ 30 ns; `Mutex::lock()` + `Arc::clone()` for caps
snapshot ≈ 15 ns; field init ≈ 5 ns; cache eviction noise ≈ 5 ns.

A follow-up could reclaim ~30 ns by pooling the boxes via
`thread_local!` and ~10 ns by swapping `Mutex<Arc<_>>` for an atomic
pointer (ArcSwap-style); both deferred — soundness ships first.

## Gate

- `cargo fmt --all --check` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` — 2286 / 2286 pass.
- `cargo check --target wasm32-unknown-unknown -p relon-wasm` clean.
