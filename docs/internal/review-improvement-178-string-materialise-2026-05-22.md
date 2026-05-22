# Review #178 P2 — JITedTraceFn high-level materialise API

Worktree: `worktree-agent-a1ba3691bc83aafd4`
Base: `0af9e34` (local main HEAD)
Commits:
- `ef7f39d` feat(codegen-native): JITedTraceFn::invoke_materialised high-level API
- `370fecc` refactor(test-harness): migrate StrConcatN trace tests to invoke_materialised

## API design

`JITedTraceFn` grew two coupled invoke surfaces on top of the existing
`invoke` / `invoke_raw` low-level pair:

1. `invoke_with_string_reclaim<R, F>(ctx, args, f) -> R` — scoped,
   closure-based power-user API. Reads `ctx.result_slot` and the raw
   status, hands a `RawInvokeResult { status, result: u64 }` to the
   caller's closure, then unconditionally runs
   `reclaim_trace_strings` after the closure returns. The closure may
   peek at any arena pointer derived from `result.result` so long as
   it materialises the payload before returning.

2. `invoke_materialised(ctx, args, return_kind) -> Result<MaterialisedValue,
   MaterialisedInvokeError>` — high-level API the bulk of callers
   should use. Returns an owned `MaterialisedValue` (`Int(i64)` /
   `Float(f64)` / `Bool(bool)` / `String(SmolStr)`); the caller never
   touches an arena pointer. Built on top of (1).

Both share a private `materialise_success_slot(result, return_kind)`
free function (no `unsafe`) so the scalar dispatch is unit-testable on
its own.

## ReturnKind dispatch

`ReturnKind` is a caller-supplied ABI hint (the trace itself does not
self-describe its return type at the boundary):

- `I64` → `result as i64`.
- `F64` → `f64::from_bits(result)`.
- `Bool` → `result != 0`.
- `String` → treat `result` as `*const StringRef`; copy payload via
  `StringRef::as_str` + `SmolStr::from_borrowed`. Null pointers and
  unmappable payloads degrade to an empty `SmolStr` (matches the
  str_ops shim null-input convention). The 22-byte inline cap on
  `SmolStr` means short string returns (W3-shaped) never allocate
  twice — the original `__relon_str_concat_n_alloc` arena block is
  copied into the inline slot and the arena reclaim drops the source.

`GuardFailed` / `Aborted` statuses surface as
`MaterialisedInvokeError::GuardFailed` /
`MaterialisedInvokeError::Aborted`. The reclaim pass still runs on
both so partial allocations made before the guard fired are released.

## Caller migration

- `crates/relon-codegen-native/tests/str_concat_n_trace_exec.rs`:
  both the assertion helper (`assert_concat_n_matches_shim`) and the
  hot-loop case migrate to `invoke_materialised(..., ReturnKind::String)`.
  The oracle payload is captured **before** the trace invoke (the
  reclaim drains the oracle's intermediates too). The hot-loop case
  re-registers operands per-iter (production hosts intern outside the
  trace arena; this mirrors that shape) and now asserts
  `trace_string_arena_len() == 0` after each iter — an invariant the
  pre-migration test could not enforce without re-implementing the
  reclaim path.
- Power-user sites (bench `cmp_lua.rs` rows, `trace_jit_hot_loop.rs`)
  stay on `invoke_raw` deliberately: the bench measures the raw
  indirect-call cost and does not want the arena reclaim folded into
  the inner loop's timing.
- `invoke_with_resume` / `invoke_with_fallback*` (production deopt
  paths) keep their current shape; the materialise API is for trace
  returns the host already plans to consume as a value, not for the
  cranelift IC stub.

## Test verification

Five new unit tests in `relon_codegen_native::trace_install::tests`:

- `invoke_materialised_i64_unwraps_const_return` — `ConstI64 +
  Return` trace through `ReturnKind::I64`, checks the unwrapped
  `MaterialisedValue::Int(123)`.
- `invoke_materialised_string_outlives_reclaim` — `LocalGet(0) +
  Return` trace fed an arena `StringRef::from_static`, checks the
  returned `SmolStr` survives the reclaim and that
  `trace_string_arena_len() == 0` post-call.
- `invoke_with_string_reclaim_passes_raw_then_reclaims` — seeds the
  arena, invokes through the closure, asserts both the raw
  `(status, result_slot)` pass-through and the post-closure drain.
- `materialise_success_slot_handles_scalar_kinds` — I64 / F64 / Bool
  bit-cast paths.
- `materialise_string_null_pointer_returns_empty` — null pointer
  degrades to an empty `SmolStr` rather than UB.

## Gate

```
cargo fmt --all --check                              clean
cargo clippy --workspace --all-targets -- -D warnings  clean
cargo test --workspace                               2321 passed (baseline 2316 + 5 new)
cargo check -p relon-trace-jit --target wasm32-unknown-unknown  clean
```

The workspace-wide `--target wasm32-unknown-unknown` still fails on
the pre-existing `memmap2`/`region` dep (same baseline as #175);
codegen-native cannot target wasm32 (cranelift host backend); the new
materialise types live entirely inside that crate, so wasm32 coverage
is unaffected.

## Known follow-up (out of scope)

- `ReturnKind::List` / `ReturnKind::Dict` — the v6-γ trace ABI does
  not return aggregates today (`Return` lowers a single SSA slot);
  when the recorder grows aggregate returns the materialise enum can
  pick up two more variants behind feature gates without an ABI
  break.
- Bench rows that currently use `invoke_raw` could opt into
  `invoke_with_string_reclaim` to bound RSS during long bench runs;
  out of scope here because the current bench fixtures use a tight
  outer iter cap and the criterion sampler already drops between
  rows.
