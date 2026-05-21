# Review-Improvement-132: Small Cleanups Stage Report (2026-05-21)

Base: `b19f3032f8b6f42a7e91a0e5d35c3e7940ef4d80` (worktree
`agent-a884b5c61a01daf3b`). Four independent ROI sweeps, each
landed as its own revert-safe commit (sub-task (b) was already in
the target state — no commit needed). Gate (`cargo fmt --check
&& cargo clippy --workspace --all-targets -D warnings && cargo
test --workspace`) passes; `cargo check --target
wasm32-unknown-unknown -p relon-trace-jit` now also passes.

## Sub-task status

| ID  | Topic                                          | Status                |
| --- | ---------------------------------------------- | --------------------- |
| (a) | `relon-parser::lower` visibility               | done, commit `e672936` |
| (b) | `linker_lld` cfg-gate                          | already done pre-base |
| (c) | `relon-fmt` unit tests                         | done, commit `3fe9aae` |
| (d) | `relon-trace-jit` wasm32 const-assert          | done, commit `bec4c84` |

## (a) Audit — `lower::` reach

Workspace grep for `relon_parser::lower::` returned zero
out-of-crate hits. The only in-tree consumers are
`crates/relon-parser/src/lib.rs` and
`crates/relon-parser/src/fast_path.rs`, both inside the same crate.
Tightened `pub mod lower` to `pub(crate) mod lower` with an
explanatory module-level comment; no `#[doc(hidden)]` or
deprecation alias was needed because nothing public ever pointed at
it. All three `pub fn` entry points (`lower_document`,
`lower_document_node_v2`, `range_from_offsets`) keep their `pub`
keyword but now collapse to crate-private through the gated module.

## (b) `linker_lld` audit

The cfg-gate the task description asks for is already in place at
base (`crates/relon-object-link/src/lib.rs:54-55,61-62`): both the
module declaration and the `pub use linker_lld::LldLinker`
re-export sit behind `#[cfg(feature = "lld-inproc")]`. The
`Cargo.toml` feature block (lines 34-40) already calls out the
experimental rationale ("`lld-sys` / `lld` crates are not on a
stable release channel..."). No edit required; leaving the doc
comment at `lib.rs:36` (which refers to the module by name) untouched
because it documents intent for readers regardless of the gate
state.

## (d) wasm32 const-assert — root cause + fix

Reproduced: `cargo check --target wasm32-unknown-unknown -p
relon-trace-jit` panicked at const-eval on
`crates/relon-trace-jit/src/runtime/str_ops.rs:97` with
`StringRef::len offset drift`. Root cause:
`STRING_REF_PTR_OFFSET = 0` and `STRING_REF_LEN_OFFSET = 8` are
hard-coded for a 64-bit ABI, but `StringRef { ptr: *const u8, len:
usize }` puts `len` at byte 4 on `wasm32` because `usize` is 4
bytes there.

Picked option A (gate the assert): wrapped the `const _: () = {
... }` block in `#[cfg(target_pointer_width = "64")]` with an
inline comment explaining why wasm32 can skip it (the trace JIT
runtime never executes on wasm32 — cranelift cannot target the host
from wasm — so the offsets are only contractual on 64-bit JIT
hosts). Native build (`cargo check -p relon-trace-jit`) still
trips the assert on any future layout drift; wasm32 check now
finishes cleanly.

## Gate evidence

- `cargo fmt --all --check` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo test --workspace` passed on the canonical run; one
  pre-existing flake (`jump_helper_aborts_recording_for_unsupported_op`)
  surfaced once under full-workspace parallel runs and passed
  cleanly on a follow-up dedicated run (`cargo test -p
  relon-test-harness --test trace_jit_smoke`). Logged as
  pre-existing flakiness, unrelated to the four changes here.
- `cargo check --target wasm32-unknown-unknown -p relon-trace-jit`
  clean (was failing at base).

## Follow-ups

None blocking. The trace_jit_smoke flake is worth investigating
under a dedicated phase but is unrelated to this sweep.
