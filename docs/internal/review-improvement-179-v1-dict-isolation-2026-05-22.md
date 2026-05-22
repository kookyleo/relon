# review #179 P3 — v1 dict helper / dict_inline isolation

Date: 2026-05-22
Base: aa53349 (local main)
Branch: worktree-agent-ab31a4c0392a2042c

## Choice — Option A (cfg(test) isolation)

Active install path already routes through v2 (`__relon_trace_dict_lookup_v2`
/ `_prechecked_v2`, wired via `HostHookId::DictLookup` in
`relon-trace-emitter::abi`). Workspace audit confirmed:

- No production caller invokes the v1 helpers — only the in-module tests
  in `dict_list.rs` reach `__relon_trace_dict_lookup{,_prechecked}` and
  the v1 builder `build_dict_record`.
- No production caller invokes `dict_inline::*` — only that module's own
  cranelift-verifier smoke tests do. `relon-bench` (W5 / W6 / `cmp_lua*`
  fixtures) already uses `build_dict_record_v2` exclusively.

Option A is therefore the cleanest path: gate every v1 symbol behind
`#[cfg(test)]` so the production rlib carries zero v1 surface, while the
legacy regression tests still pin the historical layout contract. No
bench-fixture migration needed.

## Coverage

`relon-trace-jit/src/runtime/dict_list.rs`
- `__relon_trace_dict_lookup` → `#[cfg(test)]`
- `__relon_trace_dict_lookup_prechecked` → `#[cfg(test)]`
- `build_dict_record` (v1 builder) → `#[cfg(test)]`
- Module-level doc updated: v1 helpers are now bench-fixture-incompatible
  test-only references; active paths route through v2.

`relon-trace-jit/src/runtime/mod.rs`, `relon-trace-jit/src/lib.rs`
- Dropped v1 re-exports (`__relon_trace_dict_lookup`,
  `__relon_trace_dict_lookup_prechecked`, `build_dict_record`).

`relon-trace-emitter/src/lib.rs`
- `pub mod dict_inline` → `#[cfg(test)] pub mod dict_inline`.
- Dropped all `dict_inline::*` re-exports
  (`emit_dict_lookup_inline{,_unrolled,_with_*}`, `DictInlineHoists`,
  `MAX_INLINE_*`).

`relon-trace-emitter/src/str_inline.rs`
- Two doc-comment intra-doc links to `crate::dict_inline` rewritten as
  inline code spans noting the post-#179 cfg(test) status.

## Bench fixture migration

None required. `relon-bench/tests/w5_w6_recorder_trace.rs`,
`relon-bench/benches/cmp_lua.rs`, and
`relon-bench/benches/cmp_lua_dict_list_trace.rs` already pull
`build_dict_record_v2` / `build_string_record` / `build_flat_list_record`
and stamp the v2 helper symbol (`__relon_trace_dict_lookup_v2`) into the
host hook table.

## Production-build verification

- `cargo check --release -p relon-trace-jit -p relon-trace-emitter`
  succeeds with both crates.
- `cargo rustc --release -p relon-trace-jit -- --emit asm` produces zero
  occurrences of `__relon_trace_dict_lookup\b` /
  `__relon_trace_dict_lookup_prechecked\b` and ten of the v2 symbols —
  v1 ABI surface is now fully gone from production builds.

## Gate

- `cargo fmt --all --check` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo test --workspace` — 2332 passed / 0 failed
- `cargo check -p relon-wasm --target wasm32-unknown-unknown` — clean
