# #163 — Evaluator ASCII flag wiring (follow-up to #153 + #161)

Date: 2026-05-22
Branch: `worktree-agent-ace00a8aba6c018bf`
Commits: `98285d0` + `f08472e`

## Goal

Land the last hop of the `#150 (SmolStr SSO)` -> `#153 (StringRef
ASCII flag)` -> `#161 (write-to-buffer fold)` chain on the tree-walk
side: the surface `String{Upper,Lower,Title}{,Locale}` helpers now
pass a pre-classified `AsciiHint` into `fold_string_with_ascii_hint`
instead of letting the fold engine re-scan every payload via SIMD.

## SmolStr ASCII detection (commit `98285d0`)

* `relon-eval-api::SmolStr::is_ascii() -> bool`. Inline path scans
  `data[..len]` (≤ 22 bytes — single SIMD load + cmp on every target);
  heap path delegates to `str::is_ascii()`. No layout change; the 24-
  byte slot + niche-discriminated `Arc<str>` stays intact. A future
  revision can cache the bit beside the heap pointer (mirroring
  `STRING_RECORD_ASCII_FLAG_BIT` on the trace-JIT StringRef header).
* Six parity tests for the inline/heap × ascii/non-ascii cells; non-
  ASCII payloads constructed from raw UTF-8 bytes (`0xC3 0xA9`) so the
  source file stays pure-ASCII.

## Stdlib propagation (commit `f08472e`)

* New `AsciiHint::from_smol(&SmolStr)` adapter; new
  `fold_string_to_smol_with_hint(s, mode, locale_turkish, hint)`
  threads the hint through both the inline `#161` write-to-buffer
  fast path and the heap fallback `fold_string_with_ascii_hint`.
* `String{Upper,Lower,Title}` + `String{Upper,Lower,Title}Locale` all
  reach in via the new `expect_smol_string` helper, classify the
  `SmolStr` once at the call boundary, and surface
  `AllAscii` / `KnownNonAscii` downstream. Inline ASCII path skips the
  redundant `s.is_ascii()` re-check; inline non-ASCII path bails out
  of the inline fast path without touching the bytes; heap ASCII
  routes through `case_fold_ascii_fast_into_string` (no prefix
  scan); heap non-ASCII enters the slow path at codepoint 0.
* Legacy `fold_string` top-level wrapper is now `#[cfg(test)]`-only.
  `AsciiHint::Unknown` stays as a variant for future bytecode /
  trace-JIT callers that may want the legacy SIMD-scan shape.
* `ascii_hint_wiring_tests` module pins byte-for-byte parity across
  all four inline/heap × ascii/non-ascii dispatch cells.

## Gate

* `cargo fmt --all --check`, `cargo clippy --workspace --all-targets
  -- -D warnings`, `cargo test --workspace` all green; 2241 tests
  pass (gate ≥ 2231). `cargo check --target wasm32-unknown-unknown
  -p relon-wasm` clean.

## Bench impact

The shape change is wiring-only; the
`crates/relon-bench/benches/ascii_case_fold.rs` micro-bench rows
(`preclassified_*`) already showed `−86 % .. −87 %` vs `simd_*` in
the #153 report, validating the underlying preclassified path. Real-
workload delta (corpus three-way `stdlib_case_fold` tier) was not
re-measured in this PR — the hot caller now reaches the same
preclassified primitive, so any further win comes from caching the
heap-path ASCII bit (out of scope here, called out in the SmolStr
follow-up note). Honest framing: this PR removes a redundant SIMD
scan per call but does not change the asymptotic cost on heap-sized
payloads until the heap-side cache lands.
