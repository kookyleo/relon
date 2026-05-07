# Changelog

## [Unreleased] — Spec v1 candidate freeze, batch 2

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

* 254 / 254 tests passing.
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
