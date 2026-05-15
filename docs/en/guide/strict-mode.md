# Strict mode (`#strict`)

Relon's analyzer runs in one of two modes:

- **non-strict** (default) — every value the analyzer fails to type
  falls back to a runtime-checked dynamic value. The analyzer still
  reports any error it can *prove* statically (broken paths,
  undeclared schemas, non-spreadable spread sources, etc.); it only
  stays silent on sites where inference simply can't reach a static
  type and there's no proof of a real bug.
- **strict** — opted in by writing `#strict` at the top of a module.
  Every value must have a statically inferable type. Sites where the
  analyzer can't determine the type at all (untyped closure params,
  unsignatured native fns, opaque expressions) now produce
  *error*-severity diagnostics describing what couldn't be inferred.

```relon
#strict
{ ... }
```

> The two modes share the same parser and the same runtime. They
> also share every "this is statically wrong" error — strict mode
> only adds the "we don't have enough information" errors on top.

## Contagion across `#import`

Strict mode is decided at the **entry**. A single `#strict` on the
entry file flips `strict_mode = true` on every reachable `#import`
target — even libraries that themselves didn't write `#strict`.
This prevents a strict entry from inheriting silent fallbacks from a
non-strict library.

The reverse is **not** true: a strict library imported from a
non-strict entry does *not* drag strict mode onto the whole
workspace. Strict is a top-down policy.

## Cross-mode errors (fire in both modes)

These are facts the analyzer can derive from source + schemas alone.
The runtime would also fail on them, so v2 promotes them to
cross-mode error severity — non-strict mode no longer gives them a
free pass.

| Scenario | Diagnostic |
|---|---|
| Spread a statically non-dict value (`{ src: 1 + 2, out: { ...src } }`, where `src: Int`) | `non_spreadable_source` |
| Spread `<UndeclaredSchema>` (schema name doesn't exist in the workspace) | `unresolved_schema` |
| Path descends into an undeclared schema field (`&u.unknown` where `u: User` has no `unknown`) | `unknown_reference_type` |
| Path descends past a leaf type (`&u.id.something` where `u.id: Int`) | `unknown_reference_type` |
| Duplicate field — spread contributes a key the dict already declares | `duplicate_field` |
| Explicit `Any` annotation (`Any x: 1`, `(Any n) => …`, `List<Any>`, nested forms) | `explicit_any_forbidden` |
| Bare generic container — `List` / `Dict` / `Closure` / `Fn` / `Enum` without `<...>` | `bare_generic_container` |

> `unresolved_reference` (warning) still fires for free identifiers
> that don't resolve to any binding. It stays a warning because the
> name *might* be supplied at runtime by an upstream spread the
> analyzer can't see; strict mode upgrades the same site to an
> `unknown_reference_type` error.

## Strict-only errors (fire only under `#strict`)

These are sites where the static information is *genuinely* missing.
The analyzer can't refute or confirm them — it just refuses to keep
going.

| Scenario | Diagnostic |
|---|---|
| Spread source's type is unknown (untyped closure param, untyped binding) and no `<T>` hint | `spread_source_type_unknown` |
| Dynamic dict key without a `<T>` hint (`{ [k]: v }`) | `dynamic_key_type_unknown` |
| Free identifier with no binding (`nowhere`, when no spread / import could supply it) | `unknown_reference_type` (escalated from the cross-mode `unresolved_reference` warning) |
| Closure parameter has no type annotation (`(n) => n + 1`) | `closure_param_type_missing` |
| Closure body's return type can't be derived (no `-> ReturnType`, body lands on `Any`) | `closure_return_type_unknown` |
| Native fn call where the host registered the name but no signature | `native_fn_signature_missing` |
| Generic "expression type unknown" — opaque expression in a typed slot | `expression_type_unknown` |

## Full matrix

The table below is generated from
`crates/relon-analyzer/tests/strict_matrix.rs` — every row is a real
source snippet run through `analyze_with_options` with both flags.
The matrix test asserts the cell contents, so this table stays in
lock-step with the analyzer.

### Spread `{ ...e }`

| Scenario | Non-strict | Strict |
|---|---|---|
| Spread a dict literal: `{ ...{a: 1} }` | — | — |
| Spread a sibling field typed as Schema (`Extra e: {...}` → `{ ...e }`) | — | — |
| Spread a statically non-dict reference (`{ src: 1 + 2, out: { ...src } }`) | `non_spreadable_source` | `non_spreadable_source` |
| Spread a reference whose type is unknown (e.g. untyped closure param) | — | `spread_source_type_unknown` |
| Spread an untyped source with explicit hint (`{ ...<Extra> e }`) | — | — |
| Spread `<UndeclaredSchema>` | `unresolved_schema` | `unresolved_schema` |

### Dynamic dict key `{ [expr]: ... }`

| Scenario | Non-strict | Strict |
|---|---|---|
| `{ [k]: 1 }` without `<T>` hint | — | `dynamic_key_type_unknown` |
| `{ [<String> k]: 1 }` with hint | — | — |

### Native fn calls

| Scenario | Non-strict | Strict |
|---|---|---|
| Host-registered native fn, **with** signature | — | — |
| Host-registered native fn, **without** signature | — | `expression_type_unknown` + `native_fn_signature_missing` |

### Closures

| Scenario | Non-strict | Strict |
|---|---|---|
| `(Int n) => n + 1` (typed param) | — | — |
| `(n) => n + 1` (untyped param) | — | `closure_param_type_missing` + `closure_return_type_unknown` |
| `(n) => ext_call(n)` (param + body both unclassified) | — | `closure_param_type_missing` + `closure_return_type_unknown` + `native_fn_signature_missing` |

### Reference paths

| Scenario | Non-strict | Strict |
|---|---|---|
| `&sibling.u.name` where `u: User` and `User { String name }` | — | — |
| `&sibling.u.unknown` (schema field doesn't exist) | `unresolved_reference` + `unknown_reference_type` | `unresolved_reference` + `unknown_reference_type` |
| `&sibling.u.id.something` (descend past a leaf type) | `unknown_reference_type` | `unknown_reference_type` |
| `nowhere` (free identifier, no binding) | `unresolved_reference` | `unresolved_reference` + `unknown_reference_type` |

### `#main(...)` parameters

| Scenario | Non-strict | Strict |
|---|---|---|
| `#main(Int x) -> Dict<String, Int>` | — | — |
| `#main(x) -> ...` (untyped param) | *(rejected by the parser — `#main` parameters require a type annotation regardless of mode)* | — |

## When to use which

**Default to non-strict** for:

- Quick experiments and playground sessions, where you want to type
  one line and see it run.
- Glue code that holds opaque values mid-pipeline, before you've
  refined the schema.
- Host integrations during development, before you've registered
  signatures for every native fn.

**Switch to strict** for:

- Production rule files (pricing, scoring, validation) where you
  want a build-time guarantee that no value silently lands on `Any`.
- Workspace libraries imported by strict entries — the contagion
  rule means you can't sneak silent fallbacks past the entry's
  `#strict`.
- Code that targets the AOT compilation path: the type-driven
  optimizations described in the
  [architecture overview](./architecture.md) only fire when every
  shape is statically known.

## Recipes — fixing each diagnostic

| Diagnostic | Typical fix |
|---|---|
| `non_spreadable_source` | The spread source's type is wrong, not missing — replace it with a dict literal, a schema-typed binding, or a `Dict<K, V>` expression. |
| `spread_source_type_unknown` | Type the spread source (`Extra e: ...` → `{ ...e }`) or write the hint inline: `{ ...<Extra> e }`. |
| `dynamic_key_type_unknown` | Add the key-type hint: `{ [<String> k]: v }`. |
| `unresolved_schema` | Declare the schema before the reference (`#schema Missing { ... }`) or drop the `<Missing>` annotation. |
| `unknown_reference_type` | Check the path: the named field must exist on the head's schema, and you can't descend past a leaf (`Int.something` etc.). |
| `expression_type_unknown` | Annotate the surrounding binding so inference has a target, or refactor the expression so its type is derivable. |
| `native_fn_signature_missing` | Expose the host fn through `host_fn_signatures` with a declared return type, or stop calling it from a strict module. |
| `closure_param_type_missing` | Annotate the parameter: `(Int n) => ...`. |
| `closure_return_type_unknown` | Declare `-> ReturnType` on the closure, or refactor the body so its type is reachable from inference. |

Every diagnostic points at the source location of the offending
site, and IDE quick-fixes propose the matching annotation where the
analyzer can guess one (currently for `spread_source_type_unknown`
and a subset of path-tail errors).
