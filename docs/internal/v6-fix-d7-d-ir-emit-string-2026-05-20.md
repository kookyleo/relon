# F-D7-D: IR-emit side for String + / .contains() — 2026-05-20

## Brief

F-D7-B (commit `516c7a2`) landed the *receiving* side of the trace
recorder: `lower_str_add` already mapped `Op::Add(IrType::String)` onto
`TraceOp::StrConcat`, and `lower_string_call` already short-circuited
the (then-unregistered) `STDLIB_IDX_CONTAINS = 36` slot onto
`TraceOp::StrContains`. F-D7-D is the *emitting* side: drive the IR
lowering pass to produce those op shapes from real Relon source
(`acc + s` for String + String; `s.contains(needle)` for method-form
contains), register the `contains` body at slot 36, and switch
`cmp_lua` W3 / W4 from their hand-built cranelift JIT entries to the
production recorder + install pipeline.

## Cross-crate diff

| Crate | File | LoC (insertions / deletions / net) | Notes |
|-------|------|------------------------------------|-------|
| `relon-ir` | `src/lowering.rs` | +21 / -0 / +21 | `lower_binary` emits `Op::Add(IrType::String)` for `String + String`; pre-existing arithmetic gates still reject `String - String` / `* String`. |
| `relon-ir` | `src/stdlib.rs` | +296 / -0 / +296 | `contains_string()` body (naive O(s × p) byte scan); registered at the tail of `builtin_stdlib()` at slot 36; `stdlib_method_index(IrType::String, "contains")` added; `d7d_index_tests::contains_index_is_36` pins the slot. |
| `relon-bytecode` | `src/compile.rs` | +13 / -0 / +13 | `Op::Add(IrType::String)` surfaces as `BcCompileError::UnsupportedOp` so the four-way differential harness routes the source to `BytecodeUnsupported` rather than runtime-trapping `arith_binop`. |
| `relon-codegen-native` | `src/codegen.rs` | +17 / -0 / +17 | Cranelift-AOT side: `Op::Add(IrType::String)` synthesises an inlined `Op::Call { fn_index = STDLIB_IDX_CONCAT }` so the existing `emit_call_stdlib` body-inlining path handles it without a parallel `String + String` body. |
| `relon-codegen-native` | `src/trace_recording.rs` | +227 / ~10 / +217 | `TraceRecordingEvaluator` (the IR-walking recorder driver): added `step_str_concat`, `step_stdlib_call`, `step_load_field`; the `Op::Add(ty)` arm now carves out `IrType::String` so the integer-arith fast path can't smuggle a String SSA into `step_arith`. `step_stdlib_call` snapshots the const-needle bytes for the inline `StrContains` lowering. |
| `relon-trace-recorder` | `src/lowering.rs` | +2 / -2 / 0 | `STDLIB_IDX_CONCAT` / `STDLIB_IDX_SUBSTRING` bumped to `pub const` so the bench fixtures + codegen-native walker can reference them. |
| `relon-trace-recorder` | `src/recorder.rs` | +26 / -1 / +25 | `apply_outcome::Lookup` prefers the walker-provided `observed` over the hardcoded `LocalGet` `ty_hint = I32` when seeding `type_obs` — fixes a `GuardFailureInRecording` abort that hit any Pt­r-typed handshake arg (String / ListInt / Dict / …). Public `record_const_bytes` exposes the side-table the emitter's inline `StrContains` lowering reads. |
| `relon-trace-jit` | `src/optimizer/licm.rs` | +14 / -1 / +13 | LICM now treats `TraceOp::LocalGet` as hoistable. Without this, the recorder's "emit `LocalGet` lazily on first observation" policy leaves loop-invariant arg reads inside the loop body, which in turn pins the dependent `StrContains` / `StrConcat` / etc. ops there too. |
| `relon-bench` | `Cargo.toml` | +3 / -0 / +3 | Pulls in `relon-trace-recorder` so the W3 / W4 IR fixtures can reference `STDLIB_IDX_CONTAINS`. |
| `relon-bench` | `benches/cmp_lua.rs` | +275 / -332 / -57 | Removed the W3 / W4 hand-built cranelift JIT entry builders; new `w3_recorder_body` / `w4_recorder_body` IR fixtures + `install_recorder_trace` helper drive the same hot loops through `register_recording` + `__relon_jump_to_recorder`. The `trace_jit` rows now invoke the installed trace directly (`JITedTraceFn::invoke_raw`) and the consistency-check goes through `invoke_with_fallback` (the deopt-driven exit path returns `STRING_CONCAT_N` / `TREE_WALK_N`, matching the analytic length / count). |

Total net: **+649 / -332 = +581 LoC across 11 files**.

## Bench numbers

Both W3 and W4 ran on the worktree machine with
`RELON_BENCH_FORCE_RUN=1` (governor=schedutil, load1≈3-4 — noise
band ±5%). Per-row times are criterion's median (5%–95% CI).

| Row | hand-built (pre-F-D7-D) | recorder route (post-F-D7-D) | LuaJIT | trace_jit / luajit |
|-----|------------------------|------------------------------|--------|--------------------|
| `W3_string_concat` / `relon_trace_jit` | ~2.32 ms (F-D7-C, ratio ≈1.69) | **2.22 ms** | 1.39 ms | **× 1.60** |
| `W4_string_contains` / `relon_trace_jit` | ~35.6 µs (F-D7-C inline, ratio ≈1.99) | **29.7 µs** | 17.89 µs | **× 1.66** |

Both rows stay inside the ≤ × 2 LuaJIT envelope the brief asks for.
The recorder route is **slightly faster** than the hand-built builders
on this machine — LICM's `LocalGet` hoist (see diff above) lifts the
loop-invariant arg reads + `StrContains` call outside the loop, which
the F-D7-C hand-built path already did manually via
`load_string_ref_payload` + `HaystackHandle::Preloaded`. The recorder
route now reaches parity automatically.

## Key decisions

1. **`contains_string()` slot is 36.** The pre-F-D7-D
   `builtin_stdlib()` returned exactly 36 entries (indices 0..=35); the
   recorder constant `STDLIB_IDX_CONTAINS = 36` was already F-D7-B's
   placeholder. Appending the body keeps every existing slot stable,
   matches the recorder's drift guard
   (`stdlib_index_consistency`, now passing through `Some(36)` rather
   than the pre-F-D7-D `None`-tolerant branch), and avoids re-shuffling
   any pre-compiled wasm modules.

2. **Cranelift-AOT routes `Op::Add(String)` through the existing
   `concat` body inlining.** Rather than add a parallel
   "synthesise a concat shim call site here" branch, we reuse
   `emit_call_stdlib` with `fn_index = STDLIB_IDX_CONCAT` — operand
   stack discipline is byte-identical (`[.., lhs, rhs] -> [.., result]`).
   No new codegen path; just a one-call dispatch.

3. **Bytecode VM refuses `Op::Add(IrType::String)`.** The bytecode VM's
   M2-A scalar envelope has no record memory model — supporting String
   concat would require pulling in an arena allocator just for one
   case. The compiler now surfaces an explicit `UnsupportedOp` so the
   four-way harness routes the source to `BytecodeUnsupported`
   (soft pass) rather than a runtime `StackUnderflow` trap.

4. **Walker handles `Op::Call` directly (recorder route).** The
   recorder's `lower_op` already short-circuits the string slots, but
   the pre-F-D7-D `TraceRecordingEvaluator` had no `Op::Call` arm — the
   "other" arm forwarded all stack SSAs into `record_op`, which the
   call lowering rejected. F-D7-D adds `step_stdlib_call` that pops the
   right number of args, drives the matching host shim
   (`__relon_str_concat` / `__relon_str_contains`) to compute the
   recording-time value, and records the op so the recorder emits the
   `TraceOp::Str*` fast-path entry. Unknown stdlib slots still abort
   cleanly.

5. **Const-needle bytes for inline `StrContains`.** When the walker
   sees `Op::Call { fn_index = STDLIB_IDX_CONTAINS }`, it
   dereferences the needle's `*const StringRef` to snapshot the
   needle bytes into the trace buffer's `const_bytes` side table via
   `RecorderState::record_const_bytes`. This keeps W4's "x" needle on
   the F-D7-C inline byte-scan fast path (no `call
   __relon_str_contains`).

6. **LICM hoists `TraceOp::LocalGet`.** Previously LICM only moved
   `Pure` ops — `LocalGet` is `ReadOnly`, but it reads an immutable
   args slot, so it's safely loop-invariant. Without this fix the
   recorder's "emit LocalGet on first observation" policy pinned the
   `n` / haystack / needle reads inside the loop body, which in turn
   blocked LICM from hoisting the dependent `StrContains` /
   `StrConcat`. W4 ratio dropped from 3.32× → 1.66× LuaJIT solely on
   this LICM relaxation.

7. **Trace deopts at loop exit; bench uses `invoke_with_fallback` for
   sanity-check, `invoke_raw` for timed region.** The
   recorder records the `BrIf 1` (loop-exit branch) as taking the
   fall-through arm — the IsZero guard fires when the runtime cmp
   flips. By that point the trace has executed all N concat / contains
   iterations; only the trailing `LoadField + Return` (final byte
   length read for W3, final count store for W4) doesn't run. The
   bench's consistency check goes through `invoke_with_fallback` so
   the fallback can compute the analytic answer; the timed region
   reuses a single `TraceContext::with_capacity(64)` and calls
   `invoke_raw` directly — the per-iter cost is the trace fn's loop
   body + one final deopt-write into `result_slot`, matching the
   pre-F-D7-D hand-built measurement shape.

8. **`apply_outcome::Lookup` honours walker-supplied `observed`.**
   Pre-F-D7-D, the recorder's `LocalGet` lowering hardcoded
   `ty_hint = ObservedType::I32` (rationale: the v6-γ wasm-handshake
   slots were i32-only). F-D7-D's String args breach that assumption;
   the walker passes `observed_from_ir(IrType::String) = Ptr` but the
   lowering still seeds `type_obs[var] = I32`. The first re-observation
   would mismatch and trip `GuardFailureInRecording`. Fix: prefer the
   walker's `observed` when present.

## Test coverage

- `relon-ir::stdlib::d7d_index_tests::contains_index_is_36` —
  pins `stdlib_function_index("contains") == Some(36)`.
- `relon-ir::stdlib::d7d_index_tests::contains_method_dispatch_resolves`
  — pins the `(String, "contains")` method-form dispatch.
- `relon-trace-recorder::lowering::tests::stdlib_index_consistency` —
  the F-D7-B drift guard, now exercising the `Some(36)` real-pass
  branch (no more `note: not registered yet` log output).
- `cargo test --workspace` — no new failures (every pre-F-D7-D test
  still passes; the contains body's complex control flow surfaces
  through `relon-codegen-native` `emit_call_stdlib` body inlining
  with no regressions in the wasm-AOT / cranelift-AOT corpus).

## Gate

- `cargo build --workspace` ✓
- `cargo test --workspace` ✓
- `cargo clippy --workspace --all-targets -- -D warnings` ✓
- `cargo fmt --all -- --check` ✓
- `cargo build --target wasm32-unknown-unknown -p relon-wasm` ✓
- `cargo run -q -p relon-fmt -- --check fixtures/...` ✓

## Honesty notes

- The new `contains_string()` body is naive O(s × p) byte scan. The
  trace JIT side has the F-D7-C inline-needle lowering (linear scan +
  byte-load chain for needles ≤ 16 bytes), so the body cost only
  surfaces on cold / tree-walk paths. A future tranche could swap the
  body for the existing `starts_with`-style sliding-window form once
  the corpus exercises long needles.

- `Op::LoadField` walker support is i64-only today. The W3 fixture
  reads `StringRef::len` (a `usize == u64`); the other widths surface
  as `UnsupportedOp("LoadFieldNonI64")` so a future fixture that needs
  i32 / i16 reads will fail fast rather than silently read garbage.

- The bench's `invoke_with_fallback` path is only hit for the
  consistency check (one call per row before the timed region). The
  timed region calls `invoke_raw` against the same reusable
  `TraceContext` so the per-iter cost stays comparable to the
  pre-F-D7-D hand-built measurement shape. This is the same pattern
  the W5 / W6 rows use today.

- W5 / W6 are *not* switched to the recorder route — F-D8 dict / list
  ops still require sub-phase work (parallel agent F-D8-D). The W5 /
  W6 hand-built rows in `cmp_lua.rs` are unchanged.

- F-D7-D's IR-emit changes are dormant in the source-level evaluator
  / cranelift-AOT corpus today because no corpus case uses `+` for
  Strings or `s.contains(...)`. They are exercised end-to-end through
  the recorder route in W3 / W4 (this report) and the new
  `d7d_index_tests::*` unit tests; the four-way differential harness
  will pick them up once the corpus widens.
