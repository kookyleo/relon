# Review Improvement #138 — Architecture cleanup wave (2026-05-21)

Five-sub-task cleanup pass on the analyzer / ir / evaluator / lsp /
examples surface.

## Status

| Sub-task | Verdict |
|----------|---------|
| (a) `complete.rs` split | Done — 5 sub-files |
| (a) `resolve.rs` split | Done — 3 sub-files |
| (b) `normalization_data.rs` UCD header | Done |
| (c) `relon-evaluator → relon-fmt` audit | Keep (dev-only); comment expanded |
| (d) `relon-lsp` crate doc | Done |
| (e) Rust examples crate | Done (2 examples under `crates/relon/examples/`) |

## (a) `complete.rs` (1306 → 6 files)

`complete.rs` is free-function based (no `Walker` struct), so the
`impl<'a> super::Walker<'a>` extension pattern from
`typecheck/infer` doesn't transfer verbatim. Instead the split is
by topic, mirroring how the entry function `resolve` dispatches:

- `complete/mod.rs` — entry points (`resolve`, `resolve_recovering`,
  `keywords_for_cursor`), types (`CompletionItem`, `CompletionKind`,
  `CursorContext`), shared helpers (`is_ident_byte`, `children_of`,
  `contains_offset`).
- `complete/cursor.rs` — `classify_cursor` + byte-level helpers
  (`preceded_by_type_head`, `inside_generic_args`, `at_field_start`,
  `after_arrow`).
- `complete/scope.rs` — scope walking (`walk_scope`,
  `push_scope_candidates*`, `is_inside_list`,
  `collect_callable_pairs_in_scope`, `find_named_in_scope`).
- `complete/member.rs` — `ident.X` member access
  (`push_member_candidates*`).
- `complete/keywords.rs` — fixed-list / kind-driven candidate
  sources (directives, refs, decorators, stdlib, schemas,
  primitives, imports, generic vars) plus snippet builders.
- `complete/tests.rs` — full integrated suite (22 tests, all pass).

## (a) `resolve.rs` (758 → 4 files)

Has a `Walker` struct, so the extension-impl pattern fits naturally:

- `resolve/mod.rs` — entry (`resolve_references`), structs
  (`Walker`, `ScopeFrame`, `NodeIndexer`, `ResolvedRef`,
  `CrossModuleRef*`, `PendingCrossModuleRef*`), the
  `visit` dispatch loop, and helpers (`path_head`, `build_frame`,
  `merge_dict_into_frame`, `main_param_frame`).
- `resolve/scope.rs` — in-document scope-walk lookups (`resolve` for
  `&sibling/&root/&uncle`, `resolve_variable` for closure-param +
  sibling chain).
- `resolve/cross_module.rs` — `queue_cross_module` (pending
  cross-module references for the workspace post-pass).
- `resolve/tests.rs` — 6 tests, all pass.

## (b) `normalization_data.rs`

Converted the existing `//` AUTO-GENERATED comment into a `//!`
module-level doc block. Records UCD version (14.0.0), the
regenerate command
(`python3 crates/relon-ir/tools/gen_normalization_tables.py`),
the source `.txt` filenames the script consumes,
last-regeneration date (2026-05-18), and a numbered UCD-bump
procedure. Generator script `gen_normalization_tables.py` exists
and is referenced explicitly.

## (c) `relon-evaluator → relon-fmt` audit

**Verdict: keep — already correct.** The `relon-fmt` dependency
on `relon-evaluator` is a `dev-dependency` (not a runtime dep),
used only by `tests/property_tests.rs` for the format-and-re-eval
round-trip property. No `relon_fmt::` use in `src/`. The published
`[dependencies]` graph carries no `relon-fmt` edge, so downstream
hosts linking `relon-evaluator` don't pull the formatter into their
build.

Cargo.toml comment expanded to make the audit verdict explicit so
future readers don't have to re-derive it.

## (d) `relon-lsp` crate doc

`lib.rs` already had a brief module-level doc but didn't enumerate
the LSP capabilities. Expanded to list every implemented request
(hover / completion / goto-definition / find-references / rename /
code-actions / document-symbols / inlay-hints / signature-help /
publish-diagnostics), the two hosting paths (relon-cli stdio
binary, relon-wasm browser playground), and the full sub-module
inventory (`features`, `position`, `workspace`) that was missing
before. `cargo doc -p relon-lsp` shows only the pre-existing
`crate::position` private-link warning.

## (e) Workspace examples

Picked option A — added two top-level Rust examples under
`crates/relon/examples/`:

- `use_evaluator.rs` — `relon::from_str` + serde projection,
  with a fallthrough to `relon::json_from_str` showing
  `serde_json::Value` projection.
- `use_builder.rs` — `EvaluatorBuilder` selecting
  `Backend::TreeWalk`, flipping `TrustLevel`, and registering a
  host native fn (`host_double`) via
  `register_pure_native_fn`. The script calls it through
  `#main(Int x)`.

Both verified with `cargo run -p relon --example use_evaluator` and
`cargo run -p relon --example use_builder`. Output:

```
$ cargo run -p relon --example use_evaluator
project   = Relon Modern
base      = 1500
total     = 1800
tags_cnt  = 3
summary   = Active project: Relon Modern
...

$ cargo run -p relon --example use_builder
host_double(20) + 1 = Int(41)
```

The workspace `examples/` root keeps its `.relon` source samples;
the Rust examples complement them by demonstrating the Rust
embedding side.

## Gate

- `cargo fmt --all --check`: clean.
- `cargo clippy --workspace --all-targets -- -D warnings`: clean.
- `cargo test --workspace`: 2038 passed, 0 failed.
- `cargo check -p relon-wasm --target wasm32-unknown-unknown`:
  clean.

## Commits

- `e0d0862` refactor(analyzer): split complete.rs by trigger type
- `a50a836` refactor(analyzer): split resolve.rs by scope kind
- `a964e5b` docs(ir): document normalization_data.rs UCD source +
  regen path
- `f3c3499` refactor(evaluator): audit relon-fmt dep — confirmed
  dev-only
- `a9123ca` docs(lsp): expand crate-level doc with LSP capability
  list
- `92be77a` feat(examples): add Rust examples demonstrating facade
  + builder API

## Follow-up

None. All five sub-tasks completed cleanly.
