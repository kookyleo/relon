# Improvement #155 — Tier-2 `glob_match` bytecode + trace-JIT wire-up

Stage report — 2026-05-21.

## Scope

`glob_match(s, pattern) -> Bool` (stdlib slot `GLOB_MATCH_INDEX = 37`) was
landed in #140 with tree-walker + cranelift backends only. This pass wires
the bytecode VM and trace-JIT paths so all four backends route through the
same `relon_ir::glob::glob_match` matcher.

## Bytecode design

Reused the dedicated-op pattern (`BcOp::StrConcat` / `StrEq`) rather than
piggy-backing on `BcOp::CallNative` (which is gated behind the host-fn
capability table — wrong semantics for a pure stdlib body) or
`CallStdlibScalar` (i64-shaped, no string-handle plumbing).

- New `BcOp::StrGlobMatch` — pops `[s_handle, pattern_handle]`, resolves
  both in `StringArena`, defers to `relon_ir::glob::glob_match`, pushes
  0/1.
- `compile_inline_call` short-circuits on
  `fn_index == relon_ir::GLOB_MATCH_INDEX` so the bundled sentinel `Trap`
  body never reaches the inliner.
- Promoted `GLOB_MATCH_INDEX` to the `relon-ir` top-level re-export.

## Trace-JIT helper-call path

Matcher is ~150 LoC with backtracking; IR-inlining would bloat trace
bodies past the per-iter cost budget. Mirrored `StrContains`:

- `TraceOp::StrGlobMatch(dst, s, pattern)` — Pure, two inputs, one i32
  output.
- `STDLIB_IDX_GLOB_MATCH = 37` constant + drift guard in the recorder,
  recorder lowering rule emits one `NotNull(haystack)` guard before the
  call (pattern null-handling stays in the helper).
- `HostHookId::StrGlobMatch` + `HostHookFuncIds::str_glob_match: Option<u32>`
  (opt-in like `str_concat_alloc` / dict helpers).
- `emit_str_glob_match` lowers to a direct `call __relon_str_glob_match`;
  inline-emit returns `CallNotSupportedInInline` (same fallback as the
  rest of the str ops).
- Helper body lives in `relon-codegen-native::trace_glob_helper` (NOT in
  `relon-trace-jit::runtime`, which intentionally has zero in-tree deps
  beyond `relon-trace-abi`). `register_trace_runtime_symbols` registers
  the symbol; `trace_install` declares the FuncId and forwards it via
  `HostHookFuncIds`.

## Test verify

- Bytecode: `str_glob_match_matches_and_misses` (Unicode + `?` glob),
  `compile_routes_glob_match_call_to_str_glob_match_op` (drift guard
  against falling back to the sentinel `Trap`).
- trace-recorder: `glob_match_stdlib_index_emits_str_glob_match`,
  `glob_match_underflow_aborts`, drift guard extended.
- trace-emitter: `str_glob_match_surfaces_missing_helper_error_under_default_hooks`
  + `str_glob_match_emits_call_when_helper_declared`.
- trace-JIT helper: 5 unit tests in `trace_glob_helper` + end-to-end
  cranelift JIT exec test (`str_glob_match_trace_exec.rs`) running 6
  cases through the real `JITModule` + helper symbol resolution.
- Workspace: `cargo fmt --all --check` clean, `cargo clippy --workspace
  --all-targets -- -D warnings` clean, `cargo test --workspace` 2189
  passed / 0 failed (≥ 2171 baseline). wasm32 `cargo check` clean.

## Follow-up

- Inline-needle specialisation for short literal patterns (parallel to
  F-D7-C `StrContains` inline scan). Would require teaching the recorder
  to populate a const-bytes side table for the pattern operand and
  emitting a backtracking automaton in cranelift IR for ≤ N-codepoint
  patterns. Deferred until a bench surfaces `glob_match` in the hot
  loop with a constant pattern — current production callers feed
  user-supplied patterns through the helper-call path which is already
  optimal for that surface.
- LICM hoist of loop-invariant `glob_match(s, const_pat)` is already
  available because the op is classified `Pure`; no extra work needed
  here.
