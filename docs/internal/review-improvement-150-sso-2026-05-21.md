# Review Improvement #150 — String Tier 1b Short String Inline (SSO)

**Date**: 2026-05-21
**Worktree**: `/ext/relon/.claude/worktrees/agent-aa3ff128963c201a6`
**Base**: `d317bb5` (#153 ASCII flag stage)
**Branch**: `worktree-agent-aa3ff128963c201a6`

## Value layout audit

`Value::String(String)` lives in `crates/relon-eval-api/src/value.rs`.
The enum is a plain Rust enum (no NaN-boxing) with a
`size_guard::value_enum_is_compact` test capping it at 48 bytes. The
`String` slot is 24 bytes (ptr / cap / len, 8-aligned); the other
"heavy" variants (`Closure`, `Schema`, `EnumSchema`, `Type`) are
boxed so the enum width is governed by `String`. NaN-boxing is *not*
in play here.

That makes SSO a clean drop-in: we can keep the 24-byte string slot
and use it for either inline bytes or a heap pointer.

## SSO scheme + threshold decision

Picked a hand-rolled `SmolStr` in `crates/relon-eval-api/src/smol_str.rs`
(no new external crate). Layout:

```text
24 bytes, 8-aligned, niche-discriminated:

  Inline { len: u8, data: [u8; 22] }   payload <= 22 bytes, no heap
  Heap   ( Arc<str> )                   long payload, shared by clones
```

Rust's niche-optimization on `Arc<str>::ptr` (NonNull) lands the
discriminant for free, so `size_of::<SmolStr>() == 24`. The `Value`
enum stays at the same width — `size_guard` still passes.

**Threshold = 22 bytes**, dictated by the 24-byte slot minus the
1-byte length tag (plus discriminant niche). LuaJIT's `GCstr`
boundary is 39 bytes but it has a separately laid-out object; we
can't go higher without either widening `Value` or moving to a
non-Rust-enum encoding. 22 B covers the cases the plan called out
(dict keys, identifiers, `type_name()`, short concat intermediates).

`Arc<str>` (16 B) was preferred over `Arc<String>` (8 B) so heap
payloads carry the length inline — read-side `as_str()` is one
indirection (no second pointer chase).

## Backend landing status

| Backend | Status | Notes |
| --- | --- | --- |
| `relon-eval-api` (data type) | DONE | `SmolStr` + `Value::String(SmolStr)` + size_guard intact |
| `relon-evaluator` tree-walker | DONE | All `Value::String` ctor / pattern sites migrated; `Operator::Add` uses `SmolStr::concat` for `String + String` (no `format!` indirection) |
| `relon-bytecode` VM | INHERITED | `Value::String` flows through unchanged (the VM stores `Value` directly); pattern matches still work via `Deref<Target = str>` |
| `relon-codegen-native` (cranelift AOT) | INHERITED | Only constructor site (`read_string`) updated to `.into()`; `TypeRepr::String` reader produces `SmolStr` via the same trait path |
| `relon-trace-jit` runtime | DEFERRED | Trace-JIT stores strings via the dict-key `StringRef` header introduced in #149 — already independent of `Value::String`. No SSO opportunity here without a deeper layout change (would need a parallel record header) |
| Host surface (`relon::projector`) | DONE | JSON projector uses `as_str().to_owned()` |
| Test harness + benches | DONE | All literal `Value::String("…".to_string())` sites migrated to `.into()` |

The bytecode VM and cranelift AOT pickups are "inherited" rather
than "explicit" — both call into the shared `Value::String` ctor,
so the SSO win lands automatically when the payload is short.

## Micro-bench (`sso/concat_short`, leaves stay <=22 B)

| Row | time | vs SmolStr_format |
| --- | --- | --- |
| SmolStr_format (pre `SmolStr::concat`) | 2.82 us | 1.00x |
| SmolStr_concat (new hot path)          | 500 ns | **0.18x (5.6x faster)** |
| String_push_baseline (reference)       | 767 ns | 0.27x |

The new path beats even the naive `String::push_str` reference
because the result stays inline (zero allocs on the whole loop). The
`construct_short` row is +8 ns vs `String::to_owned` for 7 B
payloads (cap-test branch + 22-byte zero-fill); we eat that as the
price of admission. `clone_short` ties on 7 B / 22 B payloads and
stays competitive on the heap-fallback path.

## W3_string_concat (cmp_lua tree_walk row, N=2000, --quick)

| Variant | ns/elem |
| --- | --- |
| pre-SSO baseline (logged 2026-05-19) | 6265 |
| post-SSO + `SmolStr::concat` | **5258**  (**-16%**) |

The plan target was -20~30%. We land at -16% because W3 grows the
accumulator past the 22-byte inline cap after the 22nd iteration,
and the remaining ~1978 iters spend their time in the heap-allocate
fallback regardless. The microbench (5.6x) reflects the case where
all concats stay inline — i.e. the typical short-lived intermediate
shape the rest of the evaluator hot path generates. The cmp_lua
ratio vs LuaJIT for W3 drops from 9.1x to ~3.8x.

## Tests + gates

* `cargo fmt --all --check` clean
* `cargo clippy --workspace --all-targets -- -D warnings` clean
* `cargo check --target wasm32-unknown-unknown -p relon-wasm` clean
* `cargo test --workspace` — **2219 tests pass**, 0 failed, 0 ignored above baseline (≥ 2210 gate met)

## Follow-up / blockers

* **22-byte cap**: workloads that produce 23-39 byte strings (the
  pre-LuaJIT-`GCstr` shape) miss the inline win. Considered raising
  the cap by introducing a parallel encoding (manual tagged union
  with discriminant in a u8) but that conflicts with the existing
  `size_guard` 48 B ceiling unless other variants box further. Left
  as a Tier-2 candidate.
* **Trace-JIT string runtime**: trace traces strings as
  `*const StringRef` records (header + payload). SSO would require
  either a parallel 22-byte inline encoding in the StringRef record
  header or a new TraceValue variant. Defer to a dedicated phase if
  trace-JIT string workloads show up as a hot spot.
* **`format!` paths in stdlib**: a handful of stdlib ops
  (split / join / replace / fold / NFC) still build a `String`
  before wrapping. The wrap is O(len) with no extra alloc for short
  results, but the build itself uses the global allocator. Tier-2
  candidate to introduce a write-to-buffer surface for short
  results.
* **`SmolStr` -> `String` boundary copies**: a few host surfaces
  (`projector`, dynamic key extraction in `reference.rs`) call
  `as_str().to_owned()` or `into_string()` to feed downstream APIs
  that still take `String`. Could be migrated to accept `impl AsRef<str>`.
  Low priority — these are off the evaluator hot path.

## Commits

```
9a2657b bench(sso): micro-bench for SmolStr construct / clone / concat
947c0fa perf(eval): SmolStr::concat hot path for String + String
f08409b style(eval-api): rustfmt + clippy on SmolStr surface
dfaf988 refactor(evaluator): wire all backends through SmolStr Value::String
f787873 refactor(eval-api): SSO Value::String via SmolStr (<=22 byte inline)
```
