# Changelog

## [Unreleased] — Schema-rooted dispatch (trait-bound foundation)

### Breaking: `relon-object-cache` — `IntegrityMode::TrustOnWrite` removed

`relon_object_cache::IntegrityMode::TrustOnWrite` skipped both the
SHA-256 recompute and the HMAC enforcement, making it a footgun for
callers who hashed a source-derived key into the filename stem and
forgot to pass an HMAC key — the loader would then silently downgrade
to a no-integrity read (the exact bypass #171 closed at the
integration layer). v0.x permits the breaking removal: any external
caller still on `TrustOnWrite` should migrate to
`IntegrityMode::HmacRequired` with a real per-installation HMAC key
(`relon_object_cache::ensure_key()`), which authenticates the entire
on-disk body via the trailing HMAC tag instead. No production caller
inside the workspace used `TrustOnWrite`; the variant existed only to
back legacy tests, which have been rewritten against `HmacRequired`.

### Language: strict mode by default, opt out via `#relaxed` / `#unstrict`

Relon's analyzer runs in strict static-inference mode by default.
Sites where inference can't reach a static type produce error-severity
diagnostics describing what couldn't be determined. A module opts out
by writing `#relaxed` (or its synonym `#unstrict`) at the top — both
spellings are bare directives and exactly equivalent.

Strict mode is decided at the entry. The entry's mode is stamped onto
every reachable `#import` target so the workspace presents a single
mode end to end: a strict entry analyses every reachable library
under strict rules, and a `#relaxed` entry analyses every reachable
library under relaxed rules.

### Playground: Mod-click go-to-definition

The browser playground now mirrors VS Code / IntelliJ navigation:
hold Cmd (macOS) or Ctrl, hover any identifier that has a known
definition and it picks up an underline + pointer cursor; click jumps
to the definition. Cross-file jumps (`#import lib from "./lib.relon"`
+ `lib.shout(...)`) switch tabs automatically. Driven by a new
`goto_definition` WASM export that wraps the analyzer's resolver.

### Analyzer / LSP: cross-file go-to-definition

`#import` bindings now carry through to definition resolution. The
analyzer's per-document `resolve` pass records `Variable` and `FnCall`
heads that match a `#import alias` / `#import { x }` / `#import *`
binding as pending cross-module refs; a workspace post-pass drains
those into `AnalyzedTree::cross_module_references` once every
imported module has been parsed. The LSP `textDocument/definition`
handler reads the new map (when a `WorkspaceTree` is provided) and
returns a `Location` pointing at the imported module's URI + the
target field's range.

A related fix: `Expr::FnCall` heads are now resolved against the
local scope chain too (the previous resolver only walked `Variable` /
`Reference`), so same-file `multiply(...)` call sites get a
references entry without needing the cross-module path.



Phases A.1 / B / C / D-API of the design recorded in
[`docs/internal/schema-rooted-model-2026-05-11.md`](./docs/internal/schema-rooted-model-2026-05-11.md):

### Language: `with { ... }` blocks and `#extend` (parser)

```relon
#schema Money { Int cents: * } with {
    #derive Equatable
    eq(other: Self) -> Bool: self.cents == other.cents
    #private
    helper() -> Int: self.cents * 2
}

#extend String with {
    is_email() -> Bool: self.contains("@")
}
```

A `#schema` or `#extend` may carry a trailing `with { ... }` block
declaring methods. Methods support `#derive Constraint` /
`#no_auto_derive Constraint` / `#native` / `#private` pragmas, plus
`Self`-typed parameters and return types. Body-less `#schema X with
{ #native foo() -> ... }` declares the method slot for a host-side
implementation supplied through the new `register_method` API.

### Analyzer / evaluator: schema-rooted method dispatch

`value.method(...)` and `Schema.method(...)` now route through a
per-schema method table built by the analyzer. Receiver dispatch
picks `Value::Dict.brand` first and falls back to the primitive type
name (`String`, `Int`, …) so `#extend String with { ... }` and
similar built-in extensions work uniformly. Schema validation
auto-brands a dict with the schema name when it has none, so
`#main(Money m)` parameters dispatch through `Money`'s table without
needing an explicit `#brand` directive.

Cross-module visibility: `#extend X` from any module reachable via
the importer's `#import` graph contributes to that importer's view
of `X` (per-import-chain semantics). Conflict detection emits
`MethodNameConflict` for clashes along the chain.

New diagnostics: `ExtendUnknownSchema`, `MethodNameConflict`,
`UnknownMethod`, `PrivateMethodViolation` (all `Error`).

### Operator lowering

`==` / `!=` lower to `lhs.eq(rhs)` (negated for `!=`) when the
receiver is a branded value with an `eq` witness on its schema.
`<` lowers to `lhs.lt(rhs)`; `>` lowers to `rhs.lt(lhs)` so a
single `lt` witness covers both directions. Primitives and
unbranded values keep the structural / numeric defaults.

### Host API: `register_method`

```rust
ctx.register_method(
    "Money",
    "formatted",
    NativeFnGate::default(),
    Arc::new(MyFormatter),
);
```

`Context::register_method(schema, name, gate, func)` and
`Context::register_pure_method(schema, name, func)` attach a host
implementation to a `#native` method on a specific schema. The
typed `(schema, method)` key replaces the v1 pattern of
`register_fn("Schema.method", ...)`. Capability gating mirrors
`register_fn`. The legacy `register_fn` / `register_pure_fn` paths
remain for free-function dispatch.

### Stdlib method mirror

`stdlib.rs` now double-registers 17 intrinsics on the built-in
schemas:

  * `String` — `split`, `replace`, `upper`, `lower`, `contains`, `len`
  * `List` — `map`, `filter`, `reduce`, `contains`, `join`, `len`
  * `Dict` — `merge`, `keys`, `values`, `has_key`, `len`

Both the legacy free-fn name (`_string_upper`, `len`, …) and the
schema-rooted method (`String::upper`, `String::len`) point at the
same `Arc<dyn RelonFunction>`. `math.*` / `range` / `type` /
`ensure.*` stay as polymorphic free fns (decision 14: a free fn that
accepts heterogeneous types isn't a method).

To call a built-in method from source today you still need to declare
the slot via `#extend String with { #native upper() -> String }` so
the analyzer knows it's there — the «open-box `s.upper()`» convenience
(a built-in stdlib schema carrier) is on roadmap §J.

### Constraints + auto-derive + `<=` / `>=`

`constraints.rs` registers the witness shape of `Equatable` (`eq`),
`Comparable` (`lt`), and `JsonProjectable` (`to_json`). A
`#derive Constraint` placed above a method declaration cross-checks
the method's signature against the registered shape — mismatches
emit `Diagnostic::ConstraintWitnessShapeMismatch`.

Every user schema auto-derives `Equatable` and `JsonProjectable` by
default (analyzer injects placeholder methods); the evaluator's
operator lowering falls back to `Value::PartialEq` when the
placeholder has no host registration, so `a == b` works on every
schema without ceremony. Opt out with `#no_auto_derive Equatable`
(then `a == b` still works — structural fallback — but
`#derive Equatable` against a witness-shaped method is required for
a custom impl). `Comparable` stays opt-in.

`<=` synthesizes from `lt ∨ eq`; `>=` synthesizes from
`(swap lt) ∨ eq`. Missing `lt` drops the entire pair to numeric
comparison (so primitives keep working); missing `eq` falls back to
`Value::PartialEq`. Mirrors the Equatable-on-by-default /
Comparable-opt-in asymmetry.

### Migration

Existing hosts and source files keep working. New language surface
is purely additive at the source level. Hosts wanting to attach a
method to a specific schema should prefer `register_method` over
the older `register_fn("Schema.method", ...)` convention.

---

### Second wave: sandbox hardening + housekeeping

A follow-up batch landed after the schema-rooted phases closed.
Three themes: tighter sandbox enforcement (resource caps, iterator
state, capability surface), build / dependency cleanup, and the
English documentation guide reaching parity with Chinese.

### Sandbox hardening

`max_value_elements` now applies to every native fn return value.
Pre-batch, `check_value_size` only fired at language-level
construction sites (list / dict literals, dict + merge,
comprehensions). Every stdlib intrinsic that returned a `List` or
`Dict` (`range`, `_list_map`, `_list_filter`, `_list_reduce`,
`_string_split`, `_dict_merge`, and their receiver-form mirrors
`xs.map(f)`, `d.merge(b)`, …) bypassed the cap, and `range(0, N)`
could allocate `Vec<Value>` before any check ran — a real OOM
vector.

- `Range::call` now pre-flights `end - start` against the cap and
  refuses oversized requests before allocating.
- `NativeFnCaps` gains `max_value_elements()`; `EvaluatorCaps`
  wires it to `Capabilities`. Both
  `Evaluator::call_function` and `try_call_native_method` run
  `check_value_size` on every native fn return value, covering
  free-form and receiver-form routes uniformly.
- `check_value_size` stays intentionally non-recursive so the
  `Iter` wrapper dict (`{ _kind, _source, _id }`) is sized as
  three keys, not by the source length it iterates over.
- The sandbox guide drops its stdlib exemption: every enforcement
  point is now listed explicitly and the `range` pre-flight is
  documented.

`Iter` cursor state moved off process-global storage into `Context`.
The user-callable `Iter.next()` cursor map and the iter-id counter
used to live in module-level statics, so two concurrent `Context`s
sharing the same process would step each other's cursors. Cursors
now live on `Context`; they clear at the top of every `eval_root` /
`run_main`, so a `Context` reused across top-level runs never
accumulates entries. The id counter intentionally keeps climbing
across runs to avoid colliding with a long-lived `Iter` dict
carried across boundaries.

- `NativeFnCaps` gains `next_iter_id` and
  `iter_cursor_fetch_and_inc` primitives; `EvaluatorCaps` routes
  them to the per-`Context` table.
- Cross-`Context` iter handling follows the implicit-exhausted
  policy: a foreign `_id` with no entry in this `Context`'s table
  surfaces as `None` rather than a new error variant. Concurrent
  iter loops under a watchdog are part of the regression suite.

Capabilities surface collapsed: `Capabilities::allow_native_fn`
(the per-name `HashSet` allowlist) and `Capabilities::allow_all_native_fn`
(the global bypass) are gone. Only the 6 capability bits
(`reads_fs` / `writes_fs` / `network` / `reads_clock` / `reads_env`
/ `uses_rng`) and the 2 resource budgets (`max_steps`,
`max_value_elements`) remain. `Capabilities::all_granted()` flips
the 6 bits directly. After this change, a successful gated-fn call
proves every bit declared on its gate was granted; audit reasoning
no longer has to consider two parallel bypass paths. The
user-facing sandbox / host-integration / spec / architecture docs
are rewritten in step — the recipe for "let a host fn run" is now
uniformly "grant the matching capability bit", never "add the name
to the allowlist".

### Build & dependencies

`thiserror` bumped 1.0 → 2 via `[workspace.dependencies]`. No
source-level changes — existing `#[error]` / `#[from]` /
`#[source]` usages are forward-compatible. Future bumps land in
one place instead of four crate manifests.

`miette` / `serde` / `serde_json` / `clap` / `anyhow` are now
routed through `[workspace.dependencies]` too. Consumers inherit
via `{ workspace = true }` plus their own features. No semantic
change — cargo unifies features across consumers exactly as before.

Bench harness extracted into a standalone `relon-bench` crate
(`publish = false`). The user-facing `cargo run -p relon-cli`
command used to fail with "could not determine which binary to run"
because `relon-cli` shipped two binaries and no `default-run`.
After the split, `relon-cli` carries a single binary and the
`default-run = "relon-cli"` workaround is dropped.

Crate-level `#![allow(unused_assignments)]` on `relon-analyzer` and
`relon-evaluator` works around rust-lang/rust#147648 — a
stable-to-stable regression where the `unused_assignments` lint
fires on every field of a proc-macro-derived enum (`miette::Diagnostic`
/ `thiserror::Error`, plus several other unrelated crates on the
upstream issue). Each `allow` carries a comment pointing at the
issue; drop the `allow` once the rustc fix lands. Restores
`cargo clippy --workspace --all-targets -- -D warnings` as a
working pre-ship gate.

The `[workspace.package].repository` URL is unified to
`https://github.com/kookyleo/relon`. `Cargo.toml` previously
pointed at `relonlang/relon` while the vitepress config, both
locale index pages, and the English introduction all referenced
`kookyleo/relon` — picking the four-vs-one majority avoids a
crates.io repo-mismatch rejection when the first release ships.

### Documentation

English guide reaches parity with Chinese: nine new pages —
`use-cases`, `syntax`, `functions`, `types`, `modules`,
`host-integration`, `sandbox`, `stdlib`, `architecture`. The
vitepress English sidebar is restructured to mirror the Chinese
one (Getting started / Core features / Embedding & sandbox /
Reference); `introduction.md` and `spec.md` cross-links now point
at their English twins, and the obsolete "comprehensive guide is
currently Chinese-first" notes are gone. The English `sandbox`
page mirrors the recent 6-bit + 2-budget surface (no
`allow_native_fn` / `allow_all_native_fn` references) and the
`max_value_elements` enforcement on stdlib intrinsics.

README quickstart and headline example fixed. The headline example
used `@fn(val, symbol)` decorator syntax that does not bind
parameters — it errored with `VariableNotFound` on `val`. The
method-shorthand form (`currency(val, symbol): ...`) replaces it,
matching what `examples/demo.relon` already shipped. A features
bullet referenced "unified closures (`@fn`)", but the language has
no `@fn` syntax — replaced with the actual surface (arrow closures
`(x) => x + 1` and method shorthands `f(x): x + 1`). README also
clarifies the trust model: scripts can't elevate themselves, but
the host can grant everything via auditable, code-visible calls
(`--trust` on the CLI, `Capabilities::all_granted()` from Rust).
The previous "There is no trusted mode" wording contradicted those
APIs and is rephrased.

Chinese syntax / types pages updated for v1.6 and v1.7.
`syntax.md` previously claimed "bare `Dict` (no generics) is still
legal, equivalent to `Dict<Any, Any>`" — v1.7's
`BareGenericContainer` ban rejects that at the analyzer. `types.md`
still listed `Any` as a built-in — v1.6's `ExplicitAnyForbidden`
retired it from user space. Both pages now reflect the current
user-facing surface.

### Third wave: language follow-ups + test hygiene

Closes the three open follow-ups under roadmap §J and seeds two
new test-infrastructure pieces flagged by the post-batch code
review.

`analyzer` follow-ups (roadmap §J):

- `MethodGenericShadowsSchemaGeneric` (Warning) — `#schema Foo<T> with
  { bar<T>(...) }` now surfaces a shadow-warning so a reader can't
  miss the rebound generic key.
- Cross-module value-path resolution — `pkg.alice.region` (and
  nested `pkg.w.inner.value` through generic schemas like
  `Wrapper<Container<Int>>`) now resolves to a concrete type at
  analysis time instead of falling back to `Any`. `walk_path` gains
  a per-hop substitution context so schema-field descent applies
  `T → Int` before lifting the type into the importer's namespace.
  `WorkspaceImportIndex` gains `aliased_values` and
  `imported_schema_generics` indices.
- `MethodGenericArgMismatch` (Error) — call sites like `bag["abc"]`
  against a schema declaring `#derive Indexable index(key: Int) ->
  Option<V>` now flag at analysis instead of waiting for the
  evaluator's runtime `TypeMismatch`.

Test-infrastructure additions:

- `proptest` harness seeded under `crates/relon-evaluator/tests/property_tests.rs`
  with five properties covering arithmetic determinism + numeric
  overflow consistency, `max_value_elements` boundary correctness,
  `BTreeMap`-ordered dict iteration, per-`Context` `Iter` cursor
  isolation, and closure capture determinism. ~2.7 s wall-clock
  for the property suite at 64 cases each.
- Golden snapshot test for the three `#main`-style examples
  (`feature_flag.relon` / `pricing.relon` / `workflow.relon`)
  under `fixtures/golden/examples_main/`. Drives each via
  `Evaluator::run_main` with the canonical `--args` documented in
  each file's header. Catches silent regressions the
  library-mode golden runner can't see.

Authoring hygiene:

- `examples/feature_flag.relon` no longer uses `len(s) % 100` as a
  hash stand-in (deterministic but non-uniform — anti-pedagogical).
  Reframed as a host-integration demo that calls
  `native_hash(s) -> Int`; the bundled `relon-cli` doesn't register
  one, so direct `cargo run` surfaces `FunctionNotFound` by design
  and the snapshot test wires a deterministic in-process stand-in.
- `scripts/pre-commit.sh` (advisory) prints every staged file at
  commit time so multi-task workflows can spot cross-topic strays.
  `scripts/install-hooks.sh` symlinks it into `.git/hooks/`.

### CI + MSRV pin

Closes the "ship gate is local-only / CI absent" finding from the
self-consistency review. The four-step ship gate (`fmt --check`,
`clippy -D warnings`, `cargo test`, `relon-fmt --check` against all
bundled fixtures + examples) is now reproduced verbatim in a
`stable` job under `.github/workflows/ci.yml`, gated on every push
to `main` and every pull request. A second `msrv` job builds the
workspace against the freshly pinned `rust-version = "1.92"`
(declared in `[workspace.package]`) so toolchain drift — and the
day rustc bug `rust-lang/rust#147648` (proc-macro `unused_assignments`
false-positive currently masked by two crate-level `#![allow]`s) gets
fixed upstream — becomes visible in CI rather than silently floating
forward. The MSRV is one minor below the current stable so the gate
has room to breathe without lagging the ecosystem; max dep
`rust-version` across the resolved graph is `1.87`, leaving
comfortable headroom. Default workflow `permissions:` are narrowed
to `contents: read` and `Swatinem/rust-cache@v2` keeps PR feedback
under the patience threshold.

## [Unreleased] — Capability model hardening: 6-bit gate + unified register_fn

### BREAKING: capability bits go from 1 to 6, registration API collapses to one entry point

`NativeFnGate` and `Capabilities` used to carry only one bit (`reads_fs`).
Hosts wired up `network` / `reads_clock` / `reads_env` examples in their
own code as if those bits existed; the docs even showed
`NativeFnGate { network: true, .. }`. They didn't actually exist.

Both structs now carry six bits — `reads_fs`, `writes_fs`, `network`,
`reads_clock`, `reads_env`, `uses_rng` — covering every ambient
capability the embedding model wants to gate. Both are
`#[non_exhaustive]` so adding bit number seven won't be a breaking
change. `Capabilities::all_granted()` flips every one of the six.

The runtime check (`check_native_fn_capability`) and the analyzer's
static check (`capability_check`) now share a `NativeFnGate::missing_bits`
helper. Runtime reports the first miss; analyzer emits one
`Diagnostic::CapabilityRequired` per missing bit, so a fn declaring
`reads_fs + network` with neither granted produces two diagnostics.

The two registration entry points collapse into one:

- `register_fn(name, fn)` (no caps, internal `gated: false` bypass) and
  `register_fn_with_caps(name, gate, fn)` are gone.
- New canonical API: `register_fn(name, gate, fn)` — every fn declares
  its gate. A pure fn carries `NativeFnGate::default()` (every bit zero)
  and the gate check is trivially satisfied even under a fully sandboxed
  `Capabilities::default()`.
- Convenience: `register_pure_fn(name, fn)` is a one-line wrapper for
  `register_fn(name, NativeFnGate::default(), fn)`. Use it for
  deterministic, args-in / value-out fns; the name documents intent.

The internal `gated: bool` flag on the registered-fn record is removed;
every call goes through the same per-bit check, and pure fns pass
because `missing_bits` returns an empty list.

stdlib intrinsics (`stdlib.rs`) now register through `register_pure_fn`.
A new `#[cfg(test)]` purity-guard test scans `stdlib.rs` and blocks
`std::fs` / `std::env` / `std::net` / `std::process` / `SystemTime` /
`Instant::now` / `rand::` / `chrono::` / `tokio::fs` / `tokio::net` /
`reqwest`. Any future ambient capability must live as a gated host-facing
module (e.g. `std/time` exposed via `register_fn(name, NativeFnGate { reads_clock: true, .. }, fn)`),
not as an ungated stdlib intrinsic.

#### Migration

`register_fn_with_caps(name, gate, fn)` → `register_fn(name, gate, fn)`
(rename only).

`register_fn(name, fn)` for a pure fn → `register_pure_fn(name, fn)`.
If the fn does have side effects, pick the right `NativeFnGate` bits
and register through `register_fn(name, gate, fn)` instead.

`NativeFnGate { reads_fs: true }` literal → `NativeFnGate { reads_fs: true, ..NativeFnGate::default() }`
(the struct now has more fields, so partial literals need the spread).
Same for `Capabilities { reads_fs: true, ..Capabilities::default() }`
when toggling individual bits in tests.

## [Earlier-Unreleased] — Sandboxed-by-default facade + spec narrative cleanup

### BREAKING: facade entry points now default to a sandboxed runtime

The default-host entry points — `relon::value_from_str`,
`value_from_file`, `from_str`, `from_file`, `json_from_str`,
`json_from_file`, `project_from_str`, plus the `relon-cli run`
subcommand — used to mount `Capabilities::all_granted()` plus a
trusted `FilesystemModuleResolver`. Anyone copying the README's quick
start was actually getting "all granted by default", which contradicts
the spec's `Sandboxed by default` posture.

These entry points now default to **sandboxed**:

- only `std/*` virtual modules resolve; local `#import "./foo.relon"`
  paths surface as `ModuleNotFound`,
- `Capabilities` stay at the `Context::sandboxed()` defaults — fs
  reads denied, capability-gated native fns denied.

Hosts that need the legacy fully-granted environment now opt in via
explicit `*_trusted` variants:

- `value_from_str_trusted`, `value_from_file_trusted`,
- `from_str_trusted`, `from_file_trusted`,
- `json_from_str_trusted`, `json_from_file_trusted`,
- `project_from_str_trusted`,
- on the CLI: `relon-cli run <file> --trust`.

If your script imports local files, calls `register_fn_with_caps`-
gated host fns, or otherwise needs FS / network capability, switch
the call site to the corresponding `*_trusted` entry. Plain `std/*`
imports and host-owned data (no FS / native-fn dependencies) need no
change.

## [Earlier-Unreleased] — v1.8: Enum / Result first-class + host fn audit + cross-module + tuple-index

v1.7 closed the user-source back-doors (`Any`, bare generics). v1.8
sweeps the remaining surface: the `Enum<...>` slot's
unconditionally-accept behaviour, generic substitution for sum-type
schemas (chiefly `Result<T, E>` / `Option<T>`), host-supplied
signatures that previously bypassed both v1.6 / v1.7 walks,
cross-module `pkg.SchemaName` slot resolution, and structured
positional access on tuples / lists (`pair.0` / `xs.1`).

### v1.8a: Enum<...> alternative-aware subsumption

`InferredType::subsumes_with` for an `Enum<...>` slot used to return
`true` for everything (`"Enum" => true`). v1.8 walks the
alternatives and accepts only when at least one is statically
compatible. Mirrors the runtime's `enum_alt_matches_cheaply` cascade:

- Built-in primitive alternative (`Int`, `String`, `Bool`, …):
  recurse into `subsumes_with`.
- Bareword alternative without generics (parser-stripped string
  literal `"up"` or schema name `Active`): treated as a `String`
  candidate, since the runtime cheap-path matches both shapes
  against `Value::String`.
- Anything else: recurse into `subsumes_with`.

`infer_from_type_node`'s `"Enum"` arm now lifts the slot to the
**join** of all alternative value types (`Enum<"up", "down">` →
`String`; `Enum<Int, Float>` → `Number`; `Enum<Int, String>` →
`Any`), so closure parameters / typed bindings declared as enums
get a precise upper bound instead of `Any`.

### v1.8b: Result<T, E> / variant-generic substitution

When the slot is `Result<Int, String>` and the value is
`Result.Ok { value: 42 }`, the analyzer now substitutes
`T -> Int, E -> String` into the variant's declared field types and
recurses per body field — the same machinery the runtime already
runs in `substitute_generics_in_schema`. A new
`VariantFieldIndex` collects per-variant `(generic_param_names,
field_types)` from `tree.schemas`, plus a `seed_prelude_variants`
pass that injects `Result<T, E>` / `Option<T>` so prelude types
match the runtime's view.

Previously the value `Result.Ok { value: "wrong" }` against
`Result<Int, String>` was caught only at runtime; v1.8 catches it
statically.

### v1.8c: host fn signature audit

`audit_host_fn_signatures` runs over every host-registered
`FnSignature` and pushes the same `ExplicitAnyForbidden` /
`BareGenericContainer` diagnostics the user-source walker emits.
Without this a host could ship `register_fn("foo", sig{ params:
[Any], return: Any })` and bypass the v1.6 / v1.7 bans. Diagnostics
carry `host fn '{name}' parameter '{param}'` /
`host fn '{name}' return type` / `host fn '{name}' variadic tail`
context labels so the operator can pinpoint which integration to
fix.

### v1.8d: cleanup

- The walker module renamed `ban_any.rs` → `ban_unsafe_types.rs`
  to reflect its dual role (ban-`Any` + ban-bare-generic) since v1.7.
- v1.7 evaluator e2e: `check_type` in `schema.rs` gains a `Tuple`
  arm so `(Int, String)`-typed fields are validated at runtime
  (arity check + per-position recursion). Pre-v1.8 a tuple-typed
  field landed in the catchall and raised a spurious
  `TypeMismatch { expected: "Tuple", found: "List" }`.
- `walk_path` adds an explicit `Tuple` arm (descending by name into
  a positional tuple yields `UnknownStep`; tuple-position access
  syntax `pair.0` / `pair.1` would plumb through this branch).
- `walk_path`'s `InferredType::Any` branch now documents that the
  only remaining hit is "non-strict closure parameter without a
  type_hint" (post v1.6 / v1.7 ban).
- `infer_from_type_node`'s `"Closure" | "Fn"` arm now requires
  non-empty generics and lifts properly-shaped `Closure<T1, ...,
  Tn, Ret>` into `Fn(params, ret)` (degenerate cases collapse to
  internal `Any`, never to a fake `Schema(...)`).

### v1.8e: cross-module `pkg.SchemaName` static resolution

Pre-v1.8 a `lib.User` slot in a typed binding collapsed to `Any`
(`infer_from_type_node` returned `Any` for any multi-segment
path), so `lib.User u: 42` silently passed. v1.8 introduces
`infer_from_type_node_with_imports` and
`subsumes_with_imports`, both of which take an
`Option<&WorkspaceImportIndex>`. When the slot is a two-segment
path whose head is a known import alias and whose tail is one of
that alias's exported root-level schemas, the slot folds to
`Schema(tail)` and the rest of the subsumption logic runs as if
the user had written the bare schema name. Callers in
`typecheck::check_typed_binding`, `check_fn_call`, the closure
return-type check, and `main_return::check_main_return` all
forward `tree.workspace_import_index.as_ref()` so cross-module
slots get the same rigour as same-file ones.

The catchall `_ => true` in `subsumes_with` for a single-segment
custom-schema slot also got tightened: a clearly non-schema value
shape (primitive, list, fn, tuple) is now a hard mismatch instead
of silently accepted. This was an opportunistic fix lurking in the
v1.8 cross-module work — `User u: 42` now flags statically in
single-file mode too.

### v1.8f: tuple-position access (`pair.0` / `pair.1`)

The walker stack used to drop every non-`String` segment via
`path_segments`, so `pair.0` walked just `pair` and the static
type stayed `Tuple<...>`. v1.8 introduces `WalkSeg` and
`walk_segments` that preserve `TokenKey::Index` segments, plus
new `walk_path` arms:

- `Tuple, Index(i)` → element at position `i`. Out-of-range
  indices surface `UnknownStep`, which strict mode lifts to
  `UnknownReferenceType`.
- `Tuple, Name(_)` → hard `UnknownStep` (tuples are positional).
- `List<T>, Index(_)` → `T`. Bounds checks stay runtime's job
  (the literal length isn't tracked in `InferredType`).

The runtime side already handled positional access on a
`Value::List` (which is what tuples reduce to at runtime), so no
evaluator changes were needed beyond the v1.7 `("Tuple", List)`
arm in `schema.rs::check_type`.

### Test surface

35 net new tests (was 22 before the v1.8e/v1.8f work):

- 11 fixture tests in `tests/v1_8_fixtures.rs::enum` + `::result`
  covering string-literal alts, heterogeneous alts, primitive
  alts, list-in-enum-slot rejection, `Result.Ok` /
  `Result.Err` correct & mistyped, custom `Pair<T, U>` sum-type
  variant-generic substitution.
- 5 unit tests in `typecheck.rs` for the host fn audit (`Any`
  param / return / variadic tail, bare `List` param, clean-signature
  silent path).
- 6 evaluator e2e tests in `eval_tests.rs::v17_*` for v1.7 tuple
  e2e (typed field, nested in list, runtime arity / position
  mismatch, homogeneous fold, ban-bare diagnostic).
- 3 cross-module fixture tests in `tests/v1_8_fixtures.rs::cross_module`
  (silent pass through `pkg.User`, primitive-vs-pkg-schema
  mismatch, `#main(pkg.User u)` parameter resolution).
- 5 tuple-index fixture tests in `tests/v1_8_fixtures.rs::tuple_index`
  (`pair.0` / `pair.1` positional access, mismatch, out-of-range
  under strict mode, list-index silent fold).
- 2 evaluator e2e tests in `eval_tests.rs::v18_tuple_position_*`
  for runtime + static tuple-position behaviour.

The full v1.0 – v1.7 corpus continues to pass — 753 tests green
after v1.8.

## [Unreleased] — v1.7: tuple types + ban bare generics

v1.6 left two related back-doors open: list literals doubled as tuples
(`[1, "x"]` had no legal annotation after `List<Any>` retired), and
the bare-generic shorthand (`List`, `Dict`, `Closure`, `Fn`, `Enum`)
silently expanded to `<Any>` shapes. v1.7 closes both.

### v1.7a: structured tuple types

A new TypeNode shape `Tuple<T1, T2, ...>` (parser-encoded from the
surface syntax `()`, `(T,)`, `(T1, T2, ...)`) becomes the analyzer's
representation for "fixed-length, mixed-element" data. Highlights:

- The parser accepts `(T1, T2, ...)` as a type wherever a `TypeNode`
  appears: schema fields, dict bindings, `#main` params, closure
  params, generic slots. The 1-tuple uses the trailing-comma form
  `(T,)` to disambiguate from a parenthesized type — `(T)` deliberately
  fails the tuple parser so method-shorthand `helper():` keeps parsing.
- `InferredType::Tuple(Vec<InferredType>)` is the new variant.
  `infer_from_type_node` lifts `Tuple<...>` to it; the printer
  formats it as `()` / `(T,)` / `(T1, T2)` matching the source syntax.
- **List literals now infer as `Tuple<T1, T2, ...>`** instead of
  `List<join(...)>`. `[1, 2, 3]` infers as `Tuple<Int, Int, Int>`,
  preserving each element's precise type.
- **Tuple → List collapse**: a homogeneous tuple subsumes a `List<T>`
  slot (every element subsumes `T`), keeping all pre-v1.7
  homogeneous-list usage working.
- **Tuple → Tuple**: arity check + per-position recursion; a
  mismatch surfaces as `StaticTypeMismatch` pinned to the offending
  position.
- `check_generics` and the path-tail walker recognise `Tuple` as a
  same-outer-container companion to `List` so per-element diagnostics
  can fire with the precise field path.

### v1.7b: ban bare generics

A new diagnostic `BareGenericContainer { type_name, context, range }`
(Error severity, mode-agnostic — same status as v1.6's
`ExplicitAnyForbidden`) fires for every user-written `TypeNode` whose
single-segment head is `List` / `Dict` / `Closure` / `Fn` / `Enum`
and whose `generics` is empty. The check piggybacks on the existing
`scan_typenode_for_any` walker so nested occurrences
(`Dict<String, List>`, `List<Closure>`, …) are caught at every depth.

```relon
{ List items: [1, 2, 3] }              // BareGenericContainer
{ Dict scores: { math: 100 } }         // BareGenericContainer
{ Closure cb: (x) => x }               // BareGenericContainer
{ Dict<String, List> data: ... }       // BareGenericContainer (nested)

{ List<Int> items: [1, 2, 3] }         // OK
{ Dict<String, Int> scores: { ... } }  // OK
```

The legacy v1.3 fixture `bare_dict_still_works.relon` (which asserted
the old `Dict → Dict<Any, Any>` expansion) is rewritten to use
explicit generics, and 11 v1.3/v1.4 fixtures with bare `-> Dict`
return types are updated to `-> Dict<String, Int>` so they no longer
trip the new ban.

### v1.7c: dead-code cleanup in `infer_from_type_node`

With ban-bare in place, the degenerate `"Closure" | "Fn"` arm
(returning `Fn(_, Any)` for empty generics) no longer fires from
user source. The arm now requires non-empty generics and lifts
`Closure<T1, ..., Tn, Ret>` into a properly-typed
`InferredType::Fn(params, ret)`. Bare `Closure` / `Fn` / `Enum` that
slip through pre-source-walk passes collapse to the internal
`InferredType::Any` placeholder, never to a fake `Schema(...)`.

### Test surface

26 new tests:

- 9 unit tests in `typecheck.rs` for tuple subsumption (homogeneous
  list compatibility, heterogeneous rejection, nested matching,
  arity / per-position mismatch).
- 17 fixture tests in `tests/v1_7_fixtures.rs` across two themes:
  `tuple/` (10 fixtures: empty / 1-tuple / pair / nested / mismatch
  / heterogeneous typed) and `ban_bare/` (7 fixtures: bare List /
  Dict / Closure / Fn, nested bare, explicit-generics silent,
  `#main` parameter ban).

The full v1.0–v1.6 corpus continues to pass — 704+17 = 721 tests
green after v1.7.

## [Unreleased] — v1.6: retire `Any` from user space

v1.5 left `Any` as a usable keyword in user code; v1.6 retires it
entirely. The user-facing surface — schema fields, `#main` params,
closure parameters, return-type annotations, typed-binding type
prefixes, nested generics — no longer accepts `Any`. The stdlib's
internal use of `Any` is also gone, replaced with unbound generic
placeholders that flow concrete types through the analyzer pipeline.

### v1.6a: ban `Any` in user code, every mode

A new diagnostic `ExplicitAnyForbidden { context, range }` (Error
severity, mode-agnostic) fires whenever a user-written `TypeNode`
contains a single-segment `Any` head, anywhere in the type tree:

```relon
{ Any payload: 42 }                  // ExplicitAnyForbidden
{ List<Any> xs: [...] }              // ExplicitAnyForbidden (nested)
{ Dict<String, Any> kv: {...} }      // ExplicitAnyForbidden (nested)
{ helper: (Any n) -> Int => 1 }      // ExplicitAnyForbidden (closure param)
{ helper: (Int n) -> Any => n }      // ExplicitAnyForbidden (closure return)
#schema X { Any payload: * }          // ExplicitAnyForbidden (schema field)
#main(Any x) -> Int                   // ExplicitAnyForbidden (#main parameter)
#main(Int n) -> Any                   // ExplicitAnyForbidden (#main return type)
```

Every site where a `TypeNode` reaches user source routes through a
new helper `crate::ban_any::scan_typenode_for_any`, which walks the
node tree (descending into nested generics) and pushes one diagnostic
per occurrence. Multi-segment paths (`pkg.Any`) are silently allowed
— users may genuinely have a schema named `Any` in another module.

### v1.6b: retire `StrictForbidsUntypedMainParam`

The v1.5-specific `StrictForbidsUntypedMainParam` diagnostic (which
only fired under `#strict` for `#main(Any x)`) is removed: the new
generic `ExplicitAnyForbidden` covers the same case in every mode,
making the diagnostic surface simpler and more uniform.

### v1.6c: stdlib-signature rewrite

Every `Any` in `crates/relon-analyzer/src/stdlib_signatures.rs` is
replaced with an unbound generic placeholder. Notable wins:

- `len<T>(T) -> Int`, `_len<T>(T) -> Int`, `type<T>(T) -> String` —
  accept anything without committing to `Any` in the signature surface.
- `_dict_values<V>(Dict<String, V>) -> List<V>` — value type now
  flows end-to-end. `Dict<String, Int>` in produces `List<Int>` out.
- `ensure.int / .string / .bool / .float / .list / .dict /
  .at_least / .at_most / .one_of <T>(T, message?) -> T` — every
  validator now preserves the input type instead of returning `Any`.
- `ensure.required_fields / .requires / .fields_equal <V>(Dict<String,
  V>, ...) -> Dict<String, V>` — Dict shape preserved.
- `_dict_merge<V>` — uniform-V calls bind `V → T` and the return
  type stays `Dict<String, T>`.
- `_string_join<T>(List<T>, String) -> String` — accepts any
  element type without `List<Any>` in the signature.
- `_dict_keys<V>` / `_dict_has_key<V>` — value placeholder unused in
  the return but keeps the signature surface uniform.

Unbound `<T>` / `<V>` is behaviorally equivalent to "accepts any
type" today (Relon doesn't have trait bounds), but the type flow
through the analyzer is now clean: a typed binding consuming a
stdlib result picks up the precise input type instead of being
swallowed by `Any`.

### v1.6d: language-surface retentions

The only `Any` references that survive v1.6 are deliberately
internal:

1. `InferredType::Any` — analyzer's "couldn't infer" placeholder.
   User never sees it; v1.5 strict checks already catch leaks.
2. Generic-placeholder fallback in
   `crates/relon-analyzer/src/generics.rs::collect_bindings` Pass 3:
   unbound `<T>` after unification gets filled with `Any` so
   substitution doesn't leave residual placeholders. Internal.
3. Runtime `Value` is dynamically typed. Implementation detail.

### Test surface

36 new tests across the analyzer (unit + fixture + ban_any helper),
covering:

- 8 ban-Any positions (schema field, #main param, #main return,
  closure param, closure return, typed binding, nested list, nested
  dict)
- 4 stdlib-rewrite regression guards (`_dict_values`, `ensure.int`,
  `_dict_merge`, `len`)
- 7 ban_any helper unit tests (recursion, multi-segment, context
  propagation)
- v1.5 fixtures updated to assert the new `ExplicitAnyForbidden`
  shape where they previously checked `StrictForbidsUntypedMainParam`

The full v1.0–v1.5 corpus continues to pass unchanged.

## [Unreleased] — v1.5: kill the long tail

v1.5 finishes what v1.4 started: under strict mode, every value the
analyzer could derive from source + schemas is now derived. Strict-mode
silent-fallback positions still in v1.4 — comprehension / where / spread
expressions, untyped closure / `#main` params, head-unresolved
variables, multi-segment FnCall — are all closed.

### v1.5a: `comprehension` / `where` / `spread` inference

`infer_type` now walks three new expression shapes:

- **`Expr::Comprehension`** — once the iterable infers as `List<T>` /
  `Dict<V>`, the binding name is typed as the element type inside a
  child scope, and the comprehension's overall type becomes
  `List<element_type>`. Heterogeneous / non-list iterables still fall
  back to `Any` for the binding so the body inference stays well-formed.
- **`Expr::Where`** — the bindings dict is walked, each key's inferred
  value type seeds the body's scope, and the body inference defines the
  expression's type.
- **`Expr::Spread`** as a standalone expression — equals the inner's
  inference result. Used by FnCall args and other expression-position
  spreads.

These three together kill the largest remaining v1.4
"`InferenceLimit`-because-we-haven't-implemented-it" surface.

### v1.5b: closure / `#main` strict typing

Three new diagnostics — every closure / entry parameter must declare a
type stronger than `Any`, and closure bodies must be statically
classifiable:

- `StrictForbidsUntypedClosureParam { param_name }` — a closure
  parameter has no `type_hint`. Pinned on the closure's range.
- `StrictForbidsUntypedMainParam { param_name }` — a `#main(Any x)`
  declaration. Pinned on the param's range.
- `StrictForbidsUnclassifiedClosureBody { role }` — a closure with no
  declared `-> ReturnType` whose body's static inference lands on
  `Any`.

A closure with `(Int n) -> Int => …` keeps passing strict mode silently;
any single drop into `Any` (untyped param, body relying on an unknown
call without a declared return) now produces an explicit error.

### v1.5c: strict-mode head-unresolved escalation

`UnresolvedReference` (warning severity) keeps firing for
non-strict / IDE consumers. Strict mode additionally pushes
`UnknownReferenceType { name, path: [head] }` at error severity, so
strict callers never reach a runtime "name not found" path even for
single-segment lookups that the old path-tail walker couldn't reach.

### v1.5d: multi-segment FnCall inference

`infer_type`'s `Expr::FnCall` arm now routes through
`lookup_signature_path` for paths longer than one segment. This picks
up cross-module `alias.method` calls (already supported by the legacy
typecheck walker) for return-type inference too — a previously dropped
path that landed `obj.method()` calls on `None` and silently leaked
`Any` into surrounding contexts.

### v1.5e: list / dict element strict-aware sweep

The dict and list walker arms now run a strict-aware element sweep:
under strict mode, every untyped list element / untyped dict-field
value whose inference lands on `Any` (and whose surrounding slot
doesn't own the diagnostic via the typed-binding walker) produces an
`InferenceLimit { reason }`.

### Test surface

50 new tests across analyzer (unit + fixture) and evaluator (e2e), in
addition to the v1.4 surface. Fixture tree at
`crates/relon-analyzer/tests/fixtures/v1_5/{comprehension,where_expr,closure_strict,strict_head,main_strict}/`,
driven by `crates/relon-analyzer/tests/v1_5_fixtures.rs`. The full
v1.0–v1.4 corpus continues to pass unchanged.

## [Unreleased] — v1.4: strict completeness

v1.4 closes the three gaps v1.3 left open. The strict-mode contract
("every value must have a derivable static type") is now enforced
across the inference engine end-to-end rather than just at a few
boundary checks.

### v1.4a: `infer.rs` path-tail walking

The expression-level inference engine now walks the full
`Variable(path)` / `Reference { path }` chain instead of dropping every
segment after the head. The new `walk_path` returns one of three
outcomes:

- `Resolved(InferredType)` — every hop succeeded; the final type is
  exposed to callers (return-type checks, typed-binding subsumption,
  spread-source classification, …).
- `UnknownStep { at_segment, running_name }` — the head was visible
  but a later segment could not be classified (missing schema field,
  attempt to descend into a leaf type like `Int`).
- `UnknownHead` — the path's first segment isn't visible in the active
  scope; the resolution layer's `UnresolvedReference` already owns
  this case.

Each step understands four head shapes: `Schema(name)` (looks up the
field on `schema_index`), `Dict<K, V>` (steps onto the value type),
`Optional<T>` (strips the `?` wrapper before retrying), and `Any`
(propagates without failing — strict mode decides whether to flag).
Concretely, `#main(Order o) -> Int\no.id` now infers `Int` and lines
up against the declared return; `#main(Order o) -> String\no.id`
fires a static `MainReturnTypeMismatch` instead of letting the
runtime discover the same problem.

### v1.4b: strict-aware silent-fallback diagnostics

The previously-reserved `UnknownReferenceType` and `InferenceLimit`
diagnostics are now wired up:

- `UnknownReferenceType { name, path }` fires under `#strict` when the
  path-tail walker can't classify some step. `name` is the failing
  segment; `path` is the full source-order chain so consumers can
  reconstruct the lineage without re-walking the AST. (The variant
  shape changed — `path: Vec<String>` is new.)
- `InferenceLimit { reason }` fires under `#strict` for genuinely
  opaque positions that nonetheless demand a derivable type: a typed
  binding whose value is a comprehension / `where` / FnCall without a
  signature, and match arm bodies whose type can't be inferred. The
  `reason` string discriminates sub-cases.

The strict path-tail check is layered on top of the existing v1.0
`check_path_tail` (which kept emitting `UnresolvedReference` for
shape-driven failures); the two are complementary, not duplicated.

### v1.4c: spread-source extension

`spread_source_schema` now lifts two more shapes into the strict
"typed spread" pool:

- path chain: `...o.extras` where `o.extras : Extras` — driven by the
  same `walk_path` machinery as the inference engine.
- FnCall: `...load_extras()` whose static signature returns a
  single-segment `Schema` (or `Dict<K, V>`).

Because sibling closure signatures are needed before the dict's
`check_dict_v1_3` runs, the type-check walker now pre-registers every
`field_name → closure_signature` pair on the dict before performing
the spread/dyn-key checks. Strict mode also accepts a `Dict<K, V>`-
typed spread source out of the box (the value type is fully known
even when the key set is dynamic).

When the source can't be classified, strict mode prefers the more
specific path-tail diagnostic over the generic
`MissingSpreadTypeHint`, so users see the precise failing step
instead of "needs a type hint".

### Test surface

51 new tests across the analyzer (unit + fixture) and evaluator (e2e)
crates. Fixture tree at
`crates/relon-analyzer/tests/fixtures/v1_4/{path_tail,strict_silent_fallback,spread_extension}/`,
driven by `crates/relon-analyzer/tests/v1_4_fixtures.rs`. The full
v1.0–v1.3 corpus (557 tests) continues to pass unchanged.

## [Unreleased] — v1.3: `#main` param injection + `#strict` mode

### v1.3a: `#main(...)` parameters reach the root body's static scope

Previously, the analyzer's `resolve.rs` walker didn't seed the
`#main(...)` parameter names into the root scope frame, so an entry
program like

```relon
#main(Int n) -> String
n + 1
```

would have `n` flagged as `UnresolvedReference` and `infer_type` would
fall back to `Any` — at which point `check_main_return` silently
accepted the body, deferring the obvious mismatch to runtime.

v1.3a fixes the gap: both `resolve::resolve_references` and
`typecheck::typecheck` now build a synthetic root frame populated with
every `#main` parameter (`closure_params` for binding lookup,
`closure_param_types` for inference) before descending into the body.
The frame is shape-agnostic — it applies to atomic / list / dict /
variant / fn-call roots alike — so the body's references resolve and
its inferred type reaches the return-type check intact. The static
`MainReturnTypeMismatch` now fires for the example above.

### v1.3b: `#strict` directive — every value must have a static type

A new bare directive `#strict` opts a file (and, transitively, every
module it reaches via `#import`) into a stricter inference contract:
sites the analyzer would otherwise let pass with an implicit `Any`
fallback now produce errors. The diagnostic kinds:

- `MissingSpreadTypeHint` — `{ ...e }` where `e` isn't a dict literal
  and lacks the new `<T>` typed-spread hint.
- `MissingDynamicKeyTypeHint` — `{ [k]: 1 }` without the typed key
  hint `[<T> k]: 1`.
- `UnresolvedSchema` — typed spread / dynamic key whose `<T>`
  references a name nobody declared.
- `StrictForbidsNativeReturn` — call site of a host-registered native
  fn that has no `host_fn_signatures` entry, so its return type is
  invisible to inference.
- `UnknownReferenceType` / `InferenceLimit` — reserved for future
  silent-fallback sites the analyzer wants to surface.

Strict mode is contagious. The workspace pass detects `#strict` on
the entry, sets `WorkspaceTree.strict_mode`, and threads the bit
through every per-module `analyze_with_options` call so `#import`
dependencies inherit the same rules. This means a strict entry can't
hide silent-fallback shapes inside a non-strict library it imports —
the lib will be stamped strict and the analyzer will report.

### v1.3c: typed spread `...<T> e` and typed dynamic key `[<T> k]: v`

The dict parser now accepts an optional `<T>` immediately after `...`
and immediately after `[`. The type lifts onto the inner Node's
existing `type_hint` slot (no new AST shape — the runtime simply
ignores it, since strict-mode rules are an analyzer contract), so:

```relon
#strict
#schema Extra { Int a: *, Int b: * }
{ src: { a: 1, b: 2 }, ...<Extra> src }      // typed spread
{ k: "key", [<String> k]: 1 }                // typed dynamic key
```

Outside strict mode the hints are still recognized and used for
DuplicateField analysis; they just stop being mandatory.

### v1.3d: `Dict<K, V>` generics

`parse_type_node` already accepted generic type parameters
(`Result<T, E>`, `List<T>`); v1.3 documents and tests that `Dict`
participates the same way. `Dict<String, Int>` validates each value's
inferred type against `Int`; bare `Dict` (no generics) remains
backwards-compatible and means `Dict<Any, Any>`.

### v1.3e: `DuplicateField` for spread-induced collisions

`{ a: 1, ...<Extra> e }` where `Extra`'s schema declares an `a` field
now reports `DuplicateField` regardless of strict mode. Same for two
spreads of dict literals that share a key (`{ ...{a: 1}, ...{a: 2} }`)
or a typed spread that overlaps a named field. Conflicts the analyzer
can't *prove* (untyped non-literal spread) stay silent — the analyzer
won't claim what it can't see.

### Test corpus

`crates/relon-analyzer/tests/fixtures/v1_3/` is the new
fixture-driven test corpus. Each scenario lives in its own `.relon`
file with an `EXPECTED:` comment, and the integration test
`crates/relon-analyzer/tests/v1_3_fixtures.rs` runs them all.

### Counts (this batch)

- 80+ new tests across parser, analyzer, evaluator, and integration
  fixtures (`cargo test --workspace`: 557 / 557 passing).
- 0 clippy warnings (`cargo clippy --workspace --all-targets -D warnings`).
- 0 fmt drift (`cargo fmt --all -- --check`).
- 0 changes to evaluator runtime semantics — strict mode is a
  pure-analyzer contract.

## [Unreleased] — Generic schemas + built-in Result / Option + open root

Schema definitions now accept type parameters, and `Result<T, E>` /
`Option<T>` are seeded into every Context as built-in tagged-enum
schemas — no explicit declaration needed. The document root is also
no longer restricted to dict / list literals — any expression is now
accepted, as long as it evaluates to a JSON value.

### v1.2: open root expression

`parse_base` now accepts any expression at the document root, not
just dict / list literals. Atomic literals (`42`, `"hello"`, `true`,
`null`), arithmetic / pipe / ternary expressions, function calls,
variant constructors (`Result.Ok { value: x }`), references, and the
rest of the expression precedence chain are all valid roots.

This unlocks atomic / variant `#main(...) -> ReturnType` bodies that
the parser previously rejected:

```relon
// Before v1.2: rejected at parse time (root must be dict/list)
// v1.2+: legal — `n + 1` is a binary expression at the root.
#main(Int n) -> Int
n + 1

// Variant constructor as root for `-> Result<T, E>` entries:
#main(Order o) -> Result<Order, String>
Result.Ok { value: o }
```

Pre-v1.2 root forms (dict / list literals) continue to parse
identically — v1.2 is a strict superset, no migration required.

`Closure` / `Schema` / `Type` / `Wildcard` are not JSON values; if
the root evaluates to one, the host's projector
(`relon::JsonProjector`) reports `UnsupportedClosure` /
`UnsupportedSchema` as before. Static `#main(...) -> ReturnType`
mismatches with non-JSON ReturnTypes continue to surface via the
analyzer's `MainReturnTypeMismatch`.

### Generic type parameters on `#schema`

```relon
#schema Box<T> { T value: * }
#schema Pair<A, B> { A first: *, B second: * }
```

The parser accepts an optional `<T, U, ...>` form-parameter list
between the schema name and its body. `DirectiveBody::NameBody`
carries the new `generics: Vec<String>`; the analyzer threads it
through `lower_schema_pure`; `Value::EnumSchema` carries it
alongside the existing `Value::Schema`. The evaluator's `check_type`
performs name-substitution on instantiation (`Box<Int>` →
`{T -> Int}`), so payload fields are validated against the actual
instantiated type. Both root-level (`#schema X<T> Body`) and
dict-embedded (`#schema X<T> Body,`) forms are wired through.

### `Result<T, E>` and `Option<T>` in the prelude

`Context::new` seeds `Result` and `Option` into the per-context
schema table:

```relon
// definitions live in the prelude — users don't write them
#schema Result<T, E>: Enum<Ok { T value: * }, Err { E error: * }>
#schema Option<T>: Enum<Some { T value: * }, None>
```

These are **value-level** types — used inside data, not as the
program's overall return shape. Examples of intended use:

```relon
// As a field type, with payload type-checked
{
    Option<String> nickname: *,
    Result<Int, String> parsed: Result.Ok { value: 42 },
}

// As a return type for a user-defined function/closure
parse(s):
    s == "" ? Result.Err { error: "empty" }
            : Result.Ok { value: 0 + s }
```

Hosts that want a custom `Result` / `Option` can call
`Context::register_schema` to override the prelude entry.

### Result is *not* the entry program's return shape

`#main(...) -> ReturnType` declares the **Json shape** the program
produces, not a Rust-style `Result<T, E>` wrapper. The success-vs-
failure distinction lives at the **host boundary** —
`Evaluator::run_main` already returns `Result<Value, RuntimeError>`
in Rust. So the right pattern is:

```relon
// Good: ReturnType describes the Json the body produces
#main(Order order) -> Order
{ id: order.id, total: order.total * 1.1 }
```

```relon
// Avoid: writing Result at the entry boundary
// — the host already gets Result<Value, RuntimeError> from Rust;
//   wrapping it again in Relon is double-bookkeeping.
#main(Order order) -> Result<Order, String>
...
```

Root bodies remain restricted to dict / list literals (an entry
program returns a Json tree). Variant constructors like
`Result.Ok { ... }` can appear inside dict / list values but not as
the root expression — which falls out cleanly from the layering:
the root *is* the Json, never an `Ok` / `Err` wrapper around it.

## [Unreleased] — Review pass (post-batch-3)

A targeted review pass closing nine evaluator/parser bugs uncovered
after batch 3 landed, plus a `#main` syntax overhaul to align with
the rest of the language.

### Bug fixes

- **Closure path-cache isolation** — A closure body referencing a
  sibling binding (`make(x): { a: x, b: &sibling.a }`) used to
  produce stale results when called more than once: the second call
  reused the first call's `path_cache` namespace and returned the
  earlier `a`. Each closure invocation now gets its own
  `cache_namespace` derived from a per-context call counter.
- **Dynamic-segment reference cache key collision** —
  `&sibling.obj[&sibling.k1]` and `&sibling.obj[&sibling.k2]`
  previously hashed identically because both dynamic key slots
  stringified to `"<dynamic>"`. The cache key now embeds the
  evaluated dynamic key (or skips caching when the key fails to
  evaluate cleanly).
- **`lookup_value_path` resolves dynamic segments** — When a
  reference path crossed into an already-materialized `Dict`/`List`
  (e.g. the result of a function call), trailing `[expr]` segments
  were looked up as the literal string `"<dynamic>"`. They now
  evaluate against the carried scope.
- **`&next` / `&prev` carry their own list scope** — Forcing the
  adjacent element re-used the thunk's stored scope, which lacked
  the target index's `list_context`; legitimate look-aheads like
  `[{ x: &next.y }, { y: &index }]` would fail. The forcing path
  now wraps a child scope with the right `ListContext`.
- **`#private` is invisible across dict boundaries** — Cross-dict
  `&root.secret` reference walks could still find `#private`
  bindings, contradicting the documented "local to owning dict"
  visibility. Reference-path resolution now blocks private hits
  unless the lookup lands on the owning dict's siblings.
- **Hex literals overflowing `i64` are rejected** —
  `0x8000000000000000` previously silently wrapped to a negative
  value via `as i64`. Hex parsing now bounds-checks against the
  signed range and returns a parse error on overflow (with `i64::MIN`
  preserved for `-0x8000000000000000`).
- **Imported modules run the analyzer** — `#import` previously
  parsed module sources but skipped analysis, so structural errors
  (e.g. `#schema Bad { name: * }` missing a type annotation) were
  silently accepted in modules while the same code would fail at
  the entry. Module loading now runs `relon_analyzer::analyze` and
  surfaces error diagnostics through `ModuleParseError`.
- **`step_counter` resets between top-level evaluations** — Hosts
  reusing a single `Evaluator`/`Context` for multiple independent
  runs would inherit the prior run's step count, tripping the
  budget on small follow-up scripts. Both `eval_root` and
  `run_main` now zero the counter on entry.

### `#main` syntax overhaul

- **Parameter style: `Type name`** to align with schema fields
  (`String name: *`). The earlier `name: Type` form is removed.
- **Optional `-> ReturnType` clause** declares the entry's return
  shape. The new `MainReturnTypeMismatch` runtime error fires when
  the body's value doesn't satisfy the declared type.

```relon
// Before
#main(req: Req)
{ greeting: req.name }

// After
#main(Req req) -> Dict
{ greeting: req.name }
```

`MainSignature` now carries `return_type: Option<TypeNode>`; the CLI
and host integration paths are unchanged.

## [Unreleased] — Spec v1 candidate freeze, batch 3

This batch makes the sigil split a hard contract and replaces the
`@input` / `@library` pair from batch 2 with a single unified
`#main(...)` entry-program signature. It is **a breaking change**
across every layer (parser, analyzer, evaluator, fmt, lsp, fixtures,
docs); pre-release semver lets us land it in one go rather than
shipping ten incremental migrations.

### Sigil split: `@` is decorators, `#` is directives

Relon now enforces a hard naming-space division:

| sigil | Purpose | Who can register |
| --- | --- | --- |
| `@name(...)` | **Decorator** — value transform | Built-in (just `@value`) + host + user (any callable binding) |
| `#name ...` | **Directive** — declaration / structure / metadata | Built-in only; fixed set; not user-extensible |

The complete v1 directive set: `#main(...)`, `#schema X Body`,
`#import ... from "..."`, `#private`, `#default`, `#expect`, `#msg`,
`#error`, `#brand X`. Every system attribute that used to be `@name`
is now `#name`. User-defined `@my_fn(arg)` decorators continue to
work — `my_fn` is looked up in the live scope and the value below is
threaded as the last positional arg. Decorator stacks now apply
**bottom-up**: `@a @b v ≡ a(b(v))`.

Five fixed directive shapes: `Bare` (`#private`),
`Value` (`#default 0`, `#brand X`), `NameBody`
(`#schema User { ... }`, no colon), `Import`
(`#import * | name | { a, b as c } from "..."`), `Main`
(`#main(Type name, ...) [-> ReturnType]`). The shape is keyed off
the directive name — the parser's directive table is closed.

### `#main(...)` replaces `@input(...)` and `@library`

Whether a file declares `#main(...)` decides how it's used:

- **`#main(Type name, ...) [-> ReturnType]`**: the file is an
  **entry program**. Hosts must push named arguments via
  `Evaluator::run_main(scope, args)`; the runtime validates each
  arg against the signature before walking the body. The optional
  `-> ReturnType` clause declares the entry's return shape. New
  runtime errors: `NoMainSignature`, `MissingMainArg`,
  `UnexpectedMainArg`, `MainArgTypeMismatch`.
- **No `#main`**: the file is a plain library / data file. It can be
  evaluated directly via `eval_root` and also `#import`-ed by other
  files.

The `@input(slot=Schema)` push contract and the `@library`
file-level marker are both **gone**. Calling `eval_root` on a
`#main` file raises `NoMainSignature`; calling `run_main` on a no-
`#main` file likewise — edge cases caught at the boundary.

### Spec changes

- §1.3 sigil split documented as a hard requirement; conformant
  runtimes MUST NOT allow a single name to coexist as both `@`-form
  and `#`-form.
- §3.1 documents the five directive shapes.
- §6.4 rewritten around `#main(...)`.
- §5 error kinds: `LibraryAsEntry` removed; `NoMainSignature`,
  `MissingMainArg`, `UnexpectedMainArg`, `MainArgTypeMismatch`
  added.

### Migration

Every `@`-form system attribute → `#`-form directive, e.g.:

```relon
// Before
@library
{
    @schema Order: Enum<Pending, Paid>,
    @import("./helpers.relon", spread=true)
    @input(req=Req)
    handler: ...
}

// After
#import * from "./helpers.relon"
#schema Order Enum<Pending, Paid>
#main(Req req)
{
    handler: ...
}
```

Hosts: replace `Context::with_input(value)` /
`Evaluator::eval_root(scope)` with
`Evaluator::run_main(scope, args_map)`.

## [Earlier draft] — Spec v1 candidate freeze, batch 2

This batch fixes a cluster of "structural debt" items called out in the
2026-05-07 critical review (`tmp/critical-review-2026-05-07.md`),
including the introduction of the `@input` push-style data contract.

### Breaking — host-pushed data has a single sanctioned channel

#### 1. `Context.globals` removed; introduce `Context::with_input(value)`

`Context.globals: HashMap<String, Value>` previously served as a
catch-all injection point — business input, host helper constants, and
runtime config all landed in the same map. That made the boundary
between "spec-defined input" and "host-private state" invisible, and
left no place for cross-runtime input contracts.

The replacement is a single push channel:

```rust
// Before
let mut ctx = Context::sandboxed().with_root(node);
ctx.globals.insert("user", to_relon_value(user));
ctx.globals.insert("posts", to_relon_value(posts));

// After
let input = Value::dict(...);  // serde_json::Value also works
let ctx = Context::sandboxed()
    .with_root(node)
    .with_input(input);
```

The script reaches the value through the **reserved root name** `input`:

```relon
{ summary: f"${input.user.name} has ${len(input.posts)} posts" }
```

Hosts that previously stuffed helper constants into `globals` should
namespace them under `input.config.*` etc.

#### 2. `input` is a reserved root identifier

Scripts may not use `input` as a dict field name, closure parameter,
comprehension binder, or `where`-clause name. (Identifiers starting
with `_` no longer have any reserved meaning — see batch 1.)

#### 3. New `@input(name=SchemaRef)` decorator: program input contract

`@input(...)` is a **root-level decorator** declaring one named slot
of the host-pushed input. Multiple decorations merge into a virtual
wrapper schema:

```relon
@input(user=User)
@input(cart=Cart)
{
    @schema User: { String name: * },
    @schema Cart: { Int total: * },
    summary: f"${input.user.name} - ${input.cart.total}"
}
```

Single-slot form:

```relon
@input(req=Req)
{
    @schema Req: {
        String name: *,
        @default(0)
        Int retries: *
    },
    greeting: f"hello ${input.req.name}, retries=${input.req.retries}"
}
```

The runtime validates `Context::with_input(value)` against the
merged wrapper before evaluating the body — type mismatches and
missing required fields are rejected up front; `@default(...)`
fields are filled when host doesn't push them.

Validation rules surface as analyzer errors:
* `DuplicateInputName` — same slot name declared twice.
* `InputDecoratorMissingName` — positional arg used (must be
  `name=SchemaRef`).
* `InputDecoratorEmpty` — bare `@input` with no args.

This is the missing piece that makes cross-runtime determinism (`spec
§1.2`) actually executable: the input contract travels *with the
script*, not separately on every host. **v1 scope**: validation
honors the entry file's `@input(...)` only; cross-file `@input`
aggregation (libraries contributing slots to the merged wrapper) is
deferred to a future revision.

### Breaking — `_` prefix has no special meaning anymore

`_` was an implicit, three-pronged convention (style hint / import
spread skip / not-hidden in JSON). It's now removed; introduce
`@private` instead:

```relon
{
    @private
    helper(v): "<" + v + ">",
    display: helper("hi")     // resolves locally
}
// JSON output: { "display": "<hi>" }   // helper hidden
```

`@private` fields stay bound in the owning dict's local scope (so
siblings can reference them) but never enter the produced
`Value::Dict::map`. That makes them invisible to JSON projection,
unreachable through `&root` / `&sibling` from outside the dict,
unreadable as `lib.foo` after import (alias or spread), and
impossible to leak by accident.

Identifiers may still start with `_` (used by intrinsics like
`_list_map`); the prefix simply has no language-level meaning.

### Non-breaking

* **Root-level `@schema(Name={...})` decorator**: textual layout sugar
  for co-locating schema declarations with `@input(...)`. Lets users
  write
  ```relon
  @schema(Req={ String name: *, Int retries: * })
  @input(req=Req)
  { greeting: f"hello ${input.req.name}" }
  ```
  instead of the previous awkward layout where the `@schema Req: {...}`
  body lived inside the root dict but was referenced from a decorator
  outside it. Semantics are identical to declaring `Req` as a
  `@private @schema` field; the registered schema is visible to both
  the dict body and `@input(...)` SchemaRefs. Spec §6.4.1; new
  diagnostics `RootSchemaDecoratorMissingName`,
  `RootSchemaDecoratorEmpty`, `DuplicateRootSchemaName`,
  `RootSchemaCollidesWithField`, `RootSchemaInvalidValue`.
* **`RecursionLimitExceeded` error kind**: split out from
  `StepLimitExceeded` so `Capabilities::max_steps`-bound semantics
  stop being conflated with type-check / schema-validation recursion
  depth. spec §5 documents both.
* **`register_dict_thunks` propagates dynamic-key errors**: the prepare
  phase no longer silently `continue`s on a dynamic key whose
  expression fails to evaluate. Surfacing the error here is fail-fast;
  the previous behavior re-evaluated the same expression in the
  dict-emit phase and only then reported the failure.
* **`Scope`'s three `reference_root*` fields collapsed into a single
  `RootRef` struct** with `node` / `scope` / `parent_fallback` —
  invariant is now structural, not prose-only.
* **`Evaluator` no longer carries a `'a` lifetime**: it owns
  `Arc<Context>`. The lone `unsafe` (`*const Evaluator as usize` in
  `EvaluatorCaps`) is gone; the crate is now `unsafe`-free.
* **Test layout**: the 95-test `#[cfg(test)] mod tests` block in
  `relon-evaluator/src/lib.rs` was extracted to dedicated
  `eval_tests.rs` / `sandbox_tests.rs` files. lib.rs is back to
  `pub mod` + `pub use` (≈30 lines).
* **Dead `tokio` dev-dep removed.**

### Code counts

* 263 / 263 tests passing.
* 0 clippy warnings (`-D warnings`).
* 0 `unsafe` blocks across the workspace.

---

## [Unreleased - batch 1] — Spec v1 candidate freeze

This release fixes the project's load-bearing positioning to
**Logic-as-Portable-Data**: the Relon source IS the executable
artifact, evaluated identically by any conformant runtime regardless
of host language. The reference runtime in this repo is Rust; the
spec itself is runtime-agnostic.

The full specification lives at [`docs/zh/guide/spec.md`](docs/zh/guide/spec.md)
([English](docs/en/guide/spec.md)). Anything not covered by that
document is runtime-private and not part of the cross-runtime
contract.

### Breaking changes (driven by the new positioning)

#### 1. `Context::trusted()` removed

A "trusted-mode constructor" undermined the spec's core invariant
that capability grants must be explicit and audit-visible. The escape
hatch is gone.

**Migration**:

```rust
// Before:
let ctx = Context::trusted().with_root(node);

// After:
let mut ctx = Context::sandboxed().with_root(node);
ctx.capabilities = Capabilities::all_granted();
ctx.prepend_module_resolver(Arc::new(FilesystemModuleResolver::trusted()));
```

The new helper `Capabilities::all_granted()` makes the "I want
everything" intent explicit. Code reviewers see exactly what is
granted at the call site.

#### 2. Top-level dotted std names no longer registered

`string.split`, `dict.merge`, `list.contains`, `math.clamp`, etc.
were silently registered as runtime-private globals. Scripts that
called them implicitly depended on "this Rust runtime registered
string.split" — a per-runtime detail the spec cannot guarantee
across implementations.

**Migration**:

```relon
// Before:
{ "parts": string.split("a,b,c", ",") }

// After:
@import("std/string", as="string")
{ "parts": string.split("a,b,c", ",") }
```

Every spec-mandated module (`std/list`, `std/string`, `std/dict`,
`std/math`, `std/is`, `std/value`) must be imported explicitly. See
[stdlib.md](docs/zh/guide/stdlib.md).

#### 3. Language-level builtins clearly delineated

Three names remain ambient (no `@import` required), pinned in spec
§6.1: `len`, `range`, `type`. They are metadata operations on data
structures themselves, available unconditionally on every conformant
runtime.

#### 4. `NativeFnGate` replaces `Capabilities` for per-fn metadata

`register_fn_with_caps` now takes `NativeFnGate` (per-function
capability *requirements* — currently `reads_fs: bool`) rather than
`Capabilities` (context-level *grants*). Restoring the distinction
lets each grow independently without dragging context-only fields
like `max_steps` into per-fn metadata.

```rust
// Before:
ctx.register_fn_with_caps(
    "fs.read",
    Capabilities { reads_fs: true, ..Default::default() },
    Arc::new(ReadFs),
);

// After:
ctx.register_fn_with_caps(
    "fs.read",
    NativeFnGate { reads_fs: true },
    Arc::new(ReadFs),
);
```

### Non-breaking improvements

* **Determinism contract written down** (spec §2): `BTreeMap` dict
  iteration, IEEE-754 floats with `OrderedFloat` total ordering, no
  ambient environment access (clock, locale, env vars, RNG).
* **Stable error taxonomy** (spec §5): every `RuntimeError` variant
  has a documented kind label that all conformant runtimes must use.
* **Std module catalog** (spec §6): the canonical list of std modules
  + functions, with each function's reference behavior anchored in
  `crates/relon-evaluator/src/std_relon/<module>.relon`.
* **Implementer guide outline** (spec §9): the path for bringing up
  a Go / TS / Swift / etc. conformant runtime, including reusing the
  reference `.relon` std sources.

### Code-side simplifications (from the parallel `/simplify` rounds)

* `EvaluatorCaps` dedup — cached `Arc<dyn NativeFnCaps>` on the
  evaluator instead of one allocation per native dispatch; removes
  the per-element `Arc<Scope::default()>` allocation in
  `_list_map`/`_list_filter`/`_list_reduce`.
* `is_valid_identifier` operator-precedence bug fixed (used to accept
  `"!!.bad"` because `chars.all(...) || s.contains('.')` short-circuited
  on any dot).
* Heterogeneous `Enum` analyzer: now `return false` after pushing the
  `HeterogeneousEnum` diagnostic so a half-tagged enum doesn't
  pollute `tree.schemas`.
* TOCTOU double-lock on `loading_modules` collapsed into a single
  guard.
* `loading_modules: Mutex<HashSet<String>>` → `Mutex<HashMap<String, usize>>`
  with reference counting, fixing a re-entrant-import edge case
  where the inner `LoadingModuleGuard::drop` would clear the outer
  frame's cycle-detection record.
* `step_counter` reverted from `AtomicUsize` to `AtomicU64` (the
  former silently truncated `Option<u64> max_steps` on 32-bit
  targets, including WASM).
* Cargo `[profile.release]` retuned: fat LTO + `codegen-units = 1`
  for whole-program optimization, with `opt-level = 3` and `unwind`
  panics so downstream embedders get throughput and `Drop`
  semantics. The size-tuned `opt-level = "z"` + `panic = "abort"`
  lives in `[profile.release-small]` for WASM / minimal CLI.
* Workspace test fallback in `FilesystemModuleResolver` is now
  `#[cfg(test)]` instead of leaking into production binaries;
  `canonicalize` errors outside `NotFound` are propagated as
  `IoError` rather than silently masked.

### Counts

* 237 / 237 tests passing.
* 0 clippy warnings.
* `release` CLI: 1.5 MB. `release-small` CLI: 898 KB.

### Deferred to future spec drafts

* `std/time`, `std/regex`, `std/path`, `std/base64` modules.
* LSP cross-runtime story (virtual URIs for std module sources).
* Conformance test corpus split out from `fixtures/` so other-language
  ports can consume it directly.
* Formal grammar in BNF (currently the reference parser IS the
  grammar).
