# review-improvement-180 — differential harness support-claim ratchet

Date: 2026-05-22
Branch: `worktree-agent-a7896994b297d741d`
Worktree: `/ext/relon/.claude/worktrees/agent-a7896994b297d741d`

## Why

The 2-/3-/4-way corpus drivers treated every `CraneliftUnsupported`,
`TraceJitNotApplicable`, `TreeWalkMissingStdlibSurface` and
`BytecodeUnsupported` surface as a soft pass. Once a backend widened
its envelope onto a case (e.g. bytecode W12 per `#176`), a later
refactor could silently regress that coverage back to its fallback
variant and the harness would stay green. Priority #4 of crate-review
asked for a ratchet that locks each backend's claimed coverage in.

## Support matrix (live as of this commit)

| tier | tw | cr | trace | bc |
| --- | --- | --- | --- | --- |
| ArithControl (28) | 28 | 27 | 22 | 27 |
| StdlibSimple (9) | 9 | 9 | 9 | 9 |
| StdlibMemory (6) | 6 | 6 | 4 | 0 |
| StdlibCaseFold (8) | 8 | 8 | 8 | 0 |
| StdlibList (5) | 5 | 5 | 2 | 3 |
| StdlibNormalize (2) | 2 | 2 | 2 | 0 |
| DictReturn (2) | 2 | 2 | 0 | 1 |

Cranelift gap: `let_chain` (analyzer-only forward-ref). Trace-JIT gaps:
4 trap-boundary arith + the 2 `StrConcatN` chains + the 3
`list.sum(range())` shapes + dict construction. Bytecode gaps: every
`String`-typed return + list-literal entry shapes + `let_chain`.

## Design

- `BackendKind { TreeWalk, CraneliftAot, TraceJit, Bytecode }` —
  distinct from `Backend` so the trace-JIT tier (a runtime overlay,
  not a `Backend::*` variant) gets first-class claim status.
- `CorpusCase.supported_by: &'static [BackendKind]` — populated from
  one live probe walk (commit `41e6a51`).
- `mod ratchet` exposes `check_two_way` / `check_three_way` /
  `check_four_way` returning `RatchetViolation`s. Drivers aggregate
  per-case and fail the test after the existing mismatch-check.
- Five reused constants (`FULL_SUPPORT`, `TW_CR_BC`, `TW_CR_TJ`,
  `TW_CR`, `TW_ONLY`) keep the per-entry annotation single-line.

`BytecodeMatchesBaseline.trace_skip_reason` carries the upstream
soft-pass tier; the four-way ratchet parses the prefix
(`cranelift_unsupported` / `tree_walk_missing_stdlib_surface` /
default trace-JIT) so the violation lands on the right tier.

## Bytecode supported_by (the new tranche)

`FULL_SUPPORT` (bytecode in): every ArithControl AllAgree / AllTrap
case except `let_chain` (boundaries are TW_CR_BC since trace traps on
overflow boundary), every StdlibSimple case (all 9 currently land on
AllAgree four-way).
`TW_CR_BC` (bytecode in, trace out): div_by_zero / mod_by_zero /
boundary_{max,min} ArithControl traps, 3 `list_sum_range_*` shapes,
`dict_simple_return`.
`TW_CR_TJ` / `TW_CR` / `TW_ONLY` (bytecode out): every String-return
tier + chain shapes + dict_with_string + let_chain.

## Negative gate

`tests/ratchet_regression.rs` — 9 unit cases feed hand-crafted soft-
pass outcomes through `ratchet::check_*` and assert violations fire
under matching claim lists, stay silent otherwise. Covers all four
backends + `BytecodeMatchesBaseline`'s reason-prefix routing.

## Gates

- `cargo fmt --all --check` clean
- `cargo clippy --workspace --all-targets -- -D warnings` clean
- `cargo test --workspace` — 2325 passed / 0 failed (+9 vs baseline)
- `cargo check -p relon-wasm --target wasm32-unknown-unknown` clean

## Commits

- `41e6a51` — feat(test-harness): per-case supported_by metadata
- `15398b3` — feat(test-harness): four-way ratchet fallback->failure for claimed-support backend
- `2d26f7a` — test(test-harness): negative ratchet regression
