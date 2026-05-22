# Review-improvement 161 — stdlib write-to-buffer (2026-05-22)

## Goal

Replace the historical `fold_string(...).into()` shape inside stdlib
string helpers with a direct write into the `SmolStr` 22-byte inline
slot. Pre-#161 every helper paid one `String::with_capacity(n)` heap
alloc plus an `Arc::from(String)` wrap even when the result fit inside
the inline cap; #150 (SSO landing) eliminated the second copy for
inline-cap outputs but the first alloc was still in the path. This
ticket closes that loop for the hot-path cases.

## Audit (what was on the String→wrap pattern)

| helper                        | pre-#161 shape                                    | hot? |
| ----------------------------- | ------------------------------------------------- | ---- |
| `StringUpper` / `StringLower` | `fold_string` -> `String` -> `.into()`            | Y    |
| `StringTitle`                 | `fold_string` -> `String` -> `.into()`            | Y    |
| `StringUpperLocale` / etc.    | `fold_string` -> `String` -> `.into()`            | Y    |
| `StringConcat`                | `String::with_capacity` + 2x push + `.into()`     | Y    |
| `StringReplace`               | `input.replace()` -> `String` -> `.into()`        | warm |
| `StringSplit`                 | `part.into()` (already inline-aware)              | —    |
| `StringJoin`                  | `parts.join()` -> `String` -> `.into()`           | warm |
| `StringSubstring`             | `&str[..].into()` (already inline-aware)          | —    |
| `StringNfc/Nfd/Nfkc/Nfkd`     | `to_nfc(s)` -> `String` -> `.into()`              | cold |
| `Value::String + Value::String` (operator) | already `SmolStr::concat` (#150)     | —    |

The four NFx normalization helpers pass through
`relon_eval_api::SmolStr::from(String)` which already inline-copies the
buffer for ≤ 22-byte outputs. Same for `StringReplace` / `StringJoin`
heap output paths — the inline-copy happens implicitly in
`SmolStr::from_string`. The remaining alloc on those paths is the
`String::replace` / `join` working buffer itself; eliminating it would
require a custom byte writer for each algorithm and is deferred as a
follow-up.

## Changes

1. **`SmolStr::try_build_inline(out_len, writer)`** —
   `crates/relon-eval-api/src/smol_str.rs` (+45 LoC). Returns
   `Some(SmolStr::Inline)` when `out_len <= 22`, otherwise `None` so the
   caller falls through to its heap implementation without paying for
   the writer invocation. Four unit tests pin the fill / cap-boundary /
   overflow-skip / empty-input cases.
2. **`fold_string_to_smol` + `fold_string_to_smol_ascii_fast`** —
   `crates/relon-evaluator/src/stdlib.rs` (+60 LoC). Inline ASCII fast
   path for `Upper` / `Lower` / `Title`; writes mask + xor bytes
   directly into the 22-byte slot. Non-ASCII / long / Turkish-locale
   inputs fall through to the existing `fold_string` body so UAX #21
   semantics stay byte-identical.
3. **Surface helpers wired** — `StringUpper` / `StringLower` /
   `StringTitle` / `StringUpperLocale` / `StringLowerLocale` /
   `StringTitleLocale` / `StringConcat` (latter routes through
   `SmolStr::concat` which already had the inline-fast path post-#150).

## Micro-bench (`string_stdlib`, host x86_64-v3)

`stdlib/to_lower_inline`:

| payload | inline_write (new) | string_with_capacity (old) | delta |
| ------- | ------------------ | -------------------------- | ----- |
| 5 B     | 43.8 ns            | 84.5 ns                    | -48%  |
| 12 B    | 44.3 ns            | 106.0 ns                   | -58%  |
| 22 B    | 49.4 ns            | ~108 ns                    | -54%  |

`stdlib/concat_inline`:

| payload | smol_concat (new) | string_with_capacity (old) | delta            |
| ------- | ----------------- | -------------------------- | ---------------- |
| 5 B     | 41.9 ns           | 76.9 ns                    | -45%             |
| 12 B    | 39.7 ns           | 76.9 ns                    | -48%             |
| 22 B    | 40.2 ns           | 75.8 ns                    | -47%             |
| 32 B    | 135 ns            | 112 ns                     | +21% (heap ceil) |

The 32-byte concat row goes to heap on both paths (past inline cap).
The smol path's extra ~20 ns is the `SmolStr::from_borrowed` inline
check that runs before the heap fallback. Negligible in absolute terms
and outside the hot regime the ticket targets.

## cmp_lua W3 / W4 impact

**Unchanged**, by source-level analysis (no re-run needed):

- **W3 (string concat loop)** uses `acc + s` which dispatches through
  `arithmetic.rs::apply_op` `(Operator::Add, String, String)`. That arm
  already calls `SmolStr::concat(a.as_str(), b.as_str())` post-#150 —
  the stdlib `StringConcat` we updated is only reached via the
  `s.concat(t)` method-call surface, not the `+` operator W3 uses.
- **W4 (string contains scan)** uses `s.contains("x")` which dispatches
  to `StringContains`, returning `Value::Bool`. Never allocates a
  `SmolStr`; no string-building path on the hot loop.

The cmp_lua harness has no row that exercises `to_lower` / `to_upper`
on the hot path, so the only place these wins land at the workload
level today is the corpus three-way `stdlib_*` tier. The corpus run is
unchanged on the all-agree axis (2231 tests pass, vs prior 2227 floor).

## Gate

- `cargo fmt --all --check` — clean
- `cargo clippy --workspace --all-targets -- -D warnings` — clean
- `cargo test --workspace` — 2231 passed / 0 failed
- `cargo check -p relon-wasm --target wasm32-unknown-unknown` — clean

## Follow-up (out of scope)

- `StringReplace` / `StringJoin` heap working-buffer reuse — needs a
  byte-writer interface over the algorithm core, larger refactor.
- NFC/NFD inline fast path — output length differs from input length
  in the general case, would need either a 22-byte try-buffer with
  overflow-rollback or a per-codepoint pre-scan. Cold today.
- Wire the StringRef record's ASCII flag bit into
  `fold_string_to_smol_ascii_fast` so the inline path skips the
  `s.is_ascii()` re-scan when the producer already paid it
  (Tier 2c carryover from #153).
