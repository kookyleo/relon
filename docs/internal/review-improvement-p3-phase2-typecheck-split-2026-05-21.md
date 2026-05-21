# P3-phase2: typecheck.rs + infer.rs split

Continuation of P3-phase1 (Unicode + stdlib). Tackles the two analyzer
files phase1 deferred: `typecheck.rs` (5765 LoC) and `infer.rs`
(1808 LoC).

## phase1 blueprint vs actual

phase1 stage report listed three blockers for the typecheck split:

1. shared mutable Walker state (`tree.diagnostics` / `scope_stack` /
   `schema_index`) made trait passes negative-ROI;
2. the 324-line `visit_internal` `match &*node.expr` was a natural split
   anchor but only if dispatch-table strategy was settled;
3. infer.rs was small enough to defer until typecheck shape was known.

phase2 resolves all three:

* **dispatch strategy**: `impl<'a> super::Walker<'a> { ... }`
  sibling-file extension blocks, same pattern P1-A codegen-native used.
  No new trait, no `WalkerCtx` abstraction — every domain method
  keeps direct access to the same private fields. The `Walker` struct
  and `visit` / `visit_internal` dispatch stay in `typecheck/mod.rs`
  as the single fan-out point.
* **visit_internal** stays a single match: every arm just calls into a
  method that now lives in a sibling sub-module. The fan-out cost is
  ~320 LoC of pattern matching with zero domain logic.
* **infer.rs** got the same treatment after typecheck shape stabilized:
  the 238-LoC `walk_path` + its 4 private helpers + `PathTailOutcome`
  enum extract cleanly into `infer/walk.rs` (412 LoC), leaving
  `infer_type` + `subsumes` + `join` + `binary` + types in mod.rs.

## typecheck.rs audit (pre-split)

LoC by group, with destination file:

| Group | LoC | Destination |
|------:|-----|-------------|
| module doc + Walker struct + visit/dispatch + entry | 470 | `mod.rs` |
| helpers (free + small walker scaffold) | 369 | `helpers.rs` |
| pre-walk index builders + type aliases | 241 | `index.rs` |
| FnCall + method/index dispatch (8 fns) | 520 | `fn_call.rs` |
| dict v1.3 + spread (6 fns) | 382 | `spread.rs` |
| match / closure return / exhaustiveness (4 fns) | 223 | `pattern.rs` |
| binary mismatch + const fold + strict fn (3 fns) | 129 | `binary.rs` |
| reference / variable / path-tail (4 fns) | 287 | `reference.rs` |
| typed binding + generics + custom schema (4 fns) | 303 | `typed_binding.rs` |
| tests (~120 tests) | 3093 | `tests.rs` |

Every domain file under 600 LoC, integrated suite under 3100,
`mod.rs` under 500 — well under the 1500-LoC budget.

## infer.rs audit (pre-split)

| Group | LoC | Destination |
|------:|-----|-------------|
| types (SchemaIndex / SchemaBaseIndex / InferredType + impl) + helper free fns | 700 | `mod.rs` |
| TypeScope + infer_type (302-LoC dispatch) | 390 | `mod.rs` |
| path-walking machinery (walk_path + 4 helpers + PathTailOutcome) | 433 | `walk.rs` |
| tests (~30 tests) | 319 | `tests.rs` |

## sub-module responsibilities

* `typecheck/helpers.rs` — pure free fns shared by every check group
  (format_type, levenshtein, closest_variant, same_outer_container,
  required_and_max, extract_closure_signature, param_is_polymorphic,
  stdlib_registered_names, stdlib_names) + small Walker extension
  methods (build_type_scope / with_closure, is_known_fn,
  lookup_field_node, dynamic_save). All < 30 LoC each.
* `typecheck/index.rs` — pre-walk side-table builders fired once at
  the top of `typecheck()`. Houses the SchemaIndex / EnumIndex /
  VariantFieldIndex aliases and `substitute_generics_in_typenode`
  (re-exported at the crate root for the evaluator).
* `typecheck/fn_call.rs` — every check that fires on a FnCall arm:
  Stage 2.7 unresolved-callable, Stage 3.5/3.6 signature-aware arity +
  type check (with v1.1 generic unification), Schema-rooted Phase B
  method dispatch + private-method enforcement, §J index dispatch.
* `typecheck/spread.rs` — dict v1.3 walker + its 5 spread-source
  classifiers. Tightly coupled (they share infer::walk_path /
  resolve_call_signature / schema_index lookups).
* `typecheck/pattern.rs` — match arm types, closure return type,
  exhaustiveness + UnknownVariant + duplicate-arm, infer_enum_type.
* `typecheck/reference.rs` — Reference/Variable/strict-path/path-tail.
  All four touch references / node_index / schema_index in the same
  shape and share the strict-mode escalation branch.
* `typecheck/binary.rs` — three small but discrete checks
  (check_binary_mismatch / check_const_fold / check_strict_fn_call)
  triggered from the Binary/Unary/FnCall arms.
* `typecheck/typed_binding.rs` — the central `Type field: value`
  subsumption gate + generics walker + custom-schema validator +
  build_generic_subst. The four recurse into each other.
* `typecheck/tests.rs` — 280+ integrated assertions exercising every
  group through the public `typecheck` entry. No domain-specific
  inner tests; the integrated suite is the contract guard.
* `infer/walk.rs` — path_segments + WalkSeg + walk_segments +
  schema_generic_params + qualify_type_node_for_alias +
  PathTailOutcome + walk_path (238 LoC) + infer_path_inferred.
* `infer/tests.rs` — 30 assertions across join / subsumes / binary
  validity / walk_path outcomes.

## infer.rs handling

Done in same phase. The walk-path machinery was the clear natural
split anchor (412 LoC of cohesive cross-module / generic-subst
logic) — leaving it in mod.rs would have meant ending phase2 with
`infer/mod.rs` still at 1808 LoC, just one file. After extraction
mod.rs is 1088 LoC (types + InferredType impl + TypeScope +
infer_type + binary), within budget without further surgery. The
remaining `infer_type` (302-LoC dispatch) mirrors typecheck's
`visit_internal` and stays as the central fan-out point.

## LoC delta

phase entry: typecheck.rs 5765 + infer.rs 1808 = 7573.
phase exit:

```
crates/relon-analyzer/src/typecheck/
   binary.rs       129
   fn_call.rs      520
   helpers.rs      369
   index.rs        241
   mod.rs          491
   pattern.rs      223
   reference.rs    287
   spread.rs       382
   tests.rs       3093
   typed_binding.rs 303
                  6038

crates/relon-analyzer/src/infer/
   mod.rs         1088
   tests.rs        319
   walk.rs         433
                  1840

total              7878
```

Net delta: **+305** (4 %). Comes from sub-module doc headers, `use
super::Walker` / cross-module imports, and `pub(super) fn …` visibility
annotations. No code logic changed — every diff is a move or
visibility widening.

## Validation: contract-guarding tests

* **typecheck/tests.rs** (280 tests) — covers every domain through the
  public `typecheck` entry. Names like `flags_unresolved_sibling_reference`,
  `flags_static_type_mismatch_on_typed_field`,
  `flags_non_exhaustive_match_on_sum_enum`,
  `flags_match_arm_type_mismatch`, `stage3_stdlib_signatures_cover_all_register_fn_names`
  exercise reference / typed_binding / pattern / fn_call respectively.
* **infer/tests.rs** (30 tests) — `v1_4_walk_path_*` family (8 cases)
  exhaustively covers the walk.rs extraction: unknown_head /
  single_seg_via_frame / schema_missing_field / dict_value /
  optional_strip / descend_into_leaf / any_short_circuits.
* **relon-trace-recorder** stdlib_index_consistency — still green
  (untouched; not in this phase's scope).
* **strict_mode integration test** (`crates/relon-analyzer/tests/strict_mode.rs`)
  — green, covers the reference.rs / typed_binding.rs strict-mode
  escalation paths end-to-end.

## Gate state

* `cargo fmt --all --check`: clean
* `cargo clippy --workspace --all-targets -- -D warnings`: clean
* `cargo test --workspace`: 2029 passed / 0 failed / 5 ignored (parity
  with phase entry: 2029 passed)
* `cargo check --target wasm32-unknown-unknown -p relon-ir`: clean

## Branch + commits

* branch: `worktree-agent-a547caa7c2107fe5e`
* worktree: `/ext/relon/.claude/worktrees/agent-a547caa7c2107fe5e`
* commits (oldest first):
  * `17c21c7` refactor(analyzer): extract typecheck helpers into sub-module
  * `ebc4cfd` refactor(analyzer): extract typecheck index + fn_call sub-modules
  * `ba45f50` refactor(analyzer): extract typecheck spread + pattern sub-modules
  * `446dfb5` refactor(analyzer): extract typecheck binary + reference + typed_binding + tests
  * `bd160fd` refactor(analyzer): split infer.rs by domain (walk-path machinery + tests)
* HEAD: `bd160fd2e2dd4ea4e0db15922a6fed3a26ad165b`

Not pushed.
