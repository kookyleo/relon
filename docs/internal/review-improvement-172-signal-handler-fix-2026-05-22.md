# Review Improvement #172: trap_handler honesty + release shield restore (2026-05-22)

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-22
Base HEAD: `54699de docs(internal): #173 hot counter atomic stage report`
Worktree: `worktree-agent-a32b14b1d299923e6`
Branch: `worktree-agent-a32b14b1d299923e6`

---

## TL;DR

P1 review finding: `trap_handler` module-level docs claimed "process keeps
running because the handler returns" after SIGSEGV / SIGFPE / SIGILL, but the
handler only writes a thread-local slot and returns — the kernel re-executes
the faulting instruction, the chained default handler then takes over (core
dump / abort). Combined with the 2026-05-21 lever (b) cfg-gate that removed
`catch_unwind` from the release dispatch path, a hardware fault could not
reliably surface as a typed `RuntimeError` in release builds either.

Two-part fix (Option A in the brief — "telemetry / fail-fast" framing):

1. **Doc rewrite** of `crates/relon-codegen-native/src/trap_handler.rs`
   stating honestly that the handler is a telemetry / fail-fast hook, NOT a
   recovery mechanism. Spells out why a slot-set-and-return cannot recover
   from a synchronous hardware fault; references `sigsetjmp` / `siglongjmp`
   (Option B) as the long-term follow-up for genuine recovery.
2. **Revert** of 2026-05-21 lever (b) — `catch_unwind` shield is back in
   both debug and release dispatch paths. Helper-panic regression
   protection is now an enforced runtime invariant, not a static audit
   promise.

The `dispatch_post_unshielded` release-only helper is removed; `dispatch_post`
is the single post-call processor for both build modes. It still keeps lever
(c)'s lazy `take_trap_code` (load-then-store-iff-nonzero) on the success
path, so the no-trap fast path retains its zero-atomic-store cost.

---

## 1. Option choice

The brief offered two options:

- **Option A** — collapse the promise to "telemetry / fail-fast", restore
  `catch_unwind` in release. Workload: M. Honest about the limits.
- **Option B** — implement real `sigsetjmp` / `siglongjmp` round-trip.
  Workload: L. Needs a C shim (`libc` exposes `setjmp` but not
  `sigsetjmp` because the latter is a per-platform macro), cross-platform
  variants (linux / macOS / windows differ), per-thread `sigjmp_buf` with
  strict lifetime rules. Defer to the v6-γ trace-recorder deopt work
  which needs the same machinery anyway.

Picked Option A. The module-doc rewrite already records Option B as the
tracked follow-up so the deferred work is not lost.

## 2. Files touched

- `crates/relon-codegen-native/src/trap_handler.rs` — module-level doc
  rewrite. New sections: **Scope** (telemetry / fail-fast), **Why this is
  fail-fast, not recoverable** (kernel re-executes the faulting instruction
  after a slot-set-and-return handler; chained default takes over),
  **How typed traps actually surface** (cond_trap -> `relon_raise_trap` ->
  `state.trap_code` is the real path; signal slot is best-effort
  defense-in-depth only), **Follow-up: Option B** (sigsetjmp tracked).
  Also tightened the per-signal `register_signal_unchecked` comment to
  reference the chain's fail-fast role, and added an honest qualifier to
  `read_thread_signal_slot`'s doc.
- `crates/relon-codegen-native/src/evaluator.rs` — reverted lever (b):
  removed `#[cfg(debug_assertions)]` gates around `catch_unwind`,
  collapsed `dispatch_post` and `dispatch_post_unshielded` into a single
  `dispatch_post`. Pre-invoke `reset_thread_signal_slot()` now runs in
  both build modes. Doc comment on `invoke_legacy_entry` rewritten with
  the new rationale and the measured release-build perf delta.

## 3. Bench numbers (release-build perf delta)

Bench: `dispatch_cranelift_step_legacy_i64` (LTO-release, criterion 200
samples, 5 s measurement, 2 s warmup). Same load-1m band (~4-5) for both
runs on the same host.

| Build | Median (per 1M-invoke iter) | Per-invoke |
|---|---|---|
| Baseline (lever (b) cfg-gated, release skips catch_unwind) | 93.96 ms | 93.96 ns |
| After (catch_unwind also in release) | 96.66 ms | 96.66 ns |
| **Delta** | **+2.7 ns** | **+2.86 %** (p = 0.00) |

The +2.7 ns / +2.86 % cost is well below the brief's 20 ns budget and
the prior 5-10 ns prediction in #154 lever (b). The delta is paid to
restore the typed-error guarantee for any helper-panic regression that
escapes the audit promise. The other dispatch rows (`smallmap`, the
HashMap-keyed wide row) share the same shield restore.

> Note: the 93-96 ns absolute is higher than #154's 14.25 ns figure
> because the 2026-05-22 P0 fix switched to per-call
> `Box::new(SandboxState::from_template(...))` (~30-50 ns of `Box::new` on
> glibc) plus 2026-05-22 #173's `AtomicU32` hot-counter rework. Those are
> not part of this stage; the +2.86 % is the isolated shield-restore cost.

## 4. Correctness verification

- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean (debug).
- `cargo clippy --workspace --all-targets --release -- -D warnings`:
  clean.
- `cargo test --workspace`: **2316 passed; 0 failed**; matches the gate
  spec floor.
- `cargo build --target wasm32-unknown-unknown -p relon-wasm`: clean (the
  `cfg(unix)` gates around the signal-handler bodies remain so the wasm
  path still compiles into the trivial no-op).

## 5. Option B follow-up (sigsetjmp round-trip)

Tracked in the trap_handler module doc under "Follow-up: Option B". The
work plan:

1. C shim crate exposing `sigsetjmp` + `siglongjmp` (per-platform
   tiny wrapper because `libc` lacks them).
2. Per-thread `sigjmp_buf` storage with `RefCell` or unsafe slot.
3. Trampoline path that calls `sigsetjmp` before the JIT entry, stores
   the buf, and the signal handler `siglongjmp`s back on hardware fault.
4. Drop / Box ownership review — `siglongjmp` skips destructors on the
   unwound frames, so the per-call `Box<SandboxState>` needs to be
   either pre-installed in a slot the longjmp landing reads, or
   relinquished via `mem::forget` + manual free.

The v6-γ trace-recorder deopt path needs the same machinery; rolling
the two together avoids duplicate C shims.

EOF
