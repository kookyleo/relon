# Review-improvement P2-A — inline/standalone emit_op sync lint

Date: 2026-05-21
Scope: `crates/relon-trace-emitter`

## Audit

Manual diff of `emitter.rs::TraceEmitterState::emit_op` (24 match arms)
vs `inline_emit.rs::InlineEmitterState::emit_op` (24 match arms). Both
paths cover the full `TraceOp` enum today (Add, Sub, Mul, Div, Mod,
Cmp, Load, Store, ConstI32, ConstI64, LocalGet, Guard, Call, Return,
MarkLoopHead, MarkLoopBack, Str{Concat,Contains,Find,Substring},
ListGet, DictLookup, DictShapeGuard, DictLookupPrechecked). Inline
routes Call / Str* / List / Dict* arms to `CallNotSupportedInInline`
deliberately. **No drift at HEAD `726ff7e`.**

## Plan choice

Plan C (test-time lexical lint). Rationale:

- Plan A (`OpEmit` trait + macro) would refactor both paths and
  doesn't add value over Rust's existing exhaustive-match check.
- Plan B (build.rs source-hash) couples build time to source layout;
  failure mode is "build fails with hash mismatch" — non-actionable.
- Plan C runs under `cargo test`, prints a precise diff
  (`only in emitter.rs: [...]`, `only in inline_emit.rs: [...]`), and
  adds zero compile-time cost.

## Implementation

`crates/relon-trace-emitter/tests/inline_emit_sync_lint.rs` (~250 LoC).
Pure-byte-stream parser (UTF-8 safe; doc comments contain em-dashes):

1. Locate `fn emit_op(` in each source, brace-balance to extract body.
2. Scan for `TraceOp::<Ident>` occurrences → `BTreeSet<String>`.
3. Walk `pub enum TraceOp { ... }` to extract declared variants.
4. Three tests:
   - `emit_op_traceop_variants_match_between_paths` — set equality.
   - `emit_op_covers_every_traceop_enum_variant` — every declared
     variant reachable in both paths.
   - `collect_traceop_variants_strips_doc_lookalikes` — extractor
     sanity check on inline fixture.

`src/lib.rs` doc gained an `inline_emit / emitter sync` section
pointing at both lint + smoke test.

## Drift validation

Removed inline's `TraceOp::Mod` arm + added wildcard catch-all to
satisfy Rust's exhaustive match. Re-ran lint:

```
emit_op paths drifted:
- only in emitter.rs (standalone): ["Mod"]
- only in inline_emit.rs (embedded): []

TraceOp variants declared in relon-trace-jit but missing from
inline_emit.rs::emit_op: ["Mod"]
```

Both tests fail with actionable messages. Reverted; clean again.

## Gate

`cargo fmt --all --check` clean. `cargo clippy --workspace
--all-targets -- -D warnings` clean. `cargo test --workspace` all
pass. `wasm32-unknown-unknown` check fails on a pre-existing
`StringRef::len` const-assert in `relon-trace-jit` (verified by
`git stash` baseline) — unrelated to this change.
