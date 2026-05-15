# Relon Language Specification

> **Status**: v1 candidate. This document is the executable
> formulation of Relon's Logic-as-Data promise — implementations MUST
> behave per the semantics described here; scripts may rely only on
> the names and contracts the spec declares. The only reference
> implementation today is the Rust crates in this repo.

## 1. Design commitment

> **Same source + same input → byte-identical output.**

This is the load-bearing axis of the spec. Every constraint below
exists to make that single sentence remain true across machines and
time — running the same `.relon` twice MUST produce the same result,
so evaluation can be replayed, hashed, and cached.

### 1.1 Implementation contract

An implementation satisfies the spec if and only if, for every
source + input combination covered by it:

1. **Parse**: it accepts every source the reference parser accepts and
   rejects every source it rejects.
2. **Evaluate**: it produces a `Value` byte-identical to the reference.
3. **Capability model**: it implements the `Capabilities` defined in
   §4 with no escape hatch that lets a script bypass them.
4. **Standard library**: it implements every std module listed in §6
   with the documented semantics.
5. **Error codes**: error kind tags use the stable list in §5 (the
   human-readable text may be localized).

Implementation details left unspecified (internal caches, threading,
build-artifact size) are up to the implementation and don't affect
the contract.

### 1.2 Determinism boundary: push vs pull

In "same source + same input → byte-identical output", **input** means
the explicit `Value` tree the host pushes via
`Evaluator::run_main(scope, args)` before evaluation; the script
declares the expected shape via a `#main(...)` signature and accesses
the bindings by parameter name.

Results returned from native functions registered via `register_fn`
are **not** input. Therefore:

- **Push form** (host completes I/O before evaluation, materializes
  the data into a `Value`, and pushes it via `run_main(args)`; the
  script declares the contract with `#main(...)`): determinism is in
  scope — the same args evaluated twice MUST produce the same result,
  replay/hash/cache safe.
- **Pull form** (the script pulls external data through native
  functions during evaluation): the author has **deliberately given
  up** determinism — network and external state vary over time, and
  the spec neither requires nor can guarantee parity.

See [Host integration](./host-integration) for the implementer guide.

### 1.3 sigil split: `@` vs `#`

Relon separates "metadata stacked on a node" into two disjoint
namespaces. This is a hard spec requirement — an implementation MUST
NOT allow a single name to exist in both `@`-form and `#`-form.

| sigil | Purpose | Who can register |
| --- | --- | --- |
| `@name(...)` | **Decorator** — value transform | Built-in + host + user (any callable binding) |
| `#name ...` | **Directive** — declaration / structure / metadata | Built-in only; fixed set; not user-extensible |

The complete v1 directive set: `#main(...)`, `#schema X Body`,
`#import ... from "..."`, `#private`, `#default`, `#expect`,
`#msg`, `#error`, `#brand X`.

The complete v1 built-in decorator set: `@value(...)`. Any other
`@name(...)` is parsed as "look up `name` in the current scope; pass
the value below as the last positional argument".

### 1.4 Static-analysis-first principle

Relon's baseline error-handling rule:

> **Any error whose detection only depends on source / module graph
> / schema / stdlib signatures MUST be reported at the analyzer
> stage; only errors that depend on host-pushed values, native-fn
> return values, or data-driven branch outcomes are permitted to
> remain runtime errors.**

This is the same design tilt as Rust: catch what you can before the
program runs. In Relon, "compile time" concretely means the
`parser → analyzer` static pipeline; "runtime" is the evaluator's
walk.

Every `RuntimeError` variant — when added or modified — must be
audited against this rule:

- If the question "why didn't analyzer catch this?" has an answer
  (the check needs runtime data), it stays runtime.
- If it doesn't, the check must be moved into analyzer as a new
  diagnostic.
- Errors analyzer already covers (e.g. `UnresolvedReference`,
  `StaticTypeMismatch`, `NonExhaustiveMatch`) MUST NOT be
  re-reported as a separate runtime error; analyzer is the
  authority.

Known v1 gaps (tracked under the staging roadmap): expression-level
type inference covers only literals; closure-body reference
resolution still leans runtime; there's no static reachability
analysis for capabilities. These gaps are scheduled for staged
hardening.

## 2. Determinism contract

To honor §1, every implementation MUST:

### 2.1 Dict iteration order

`Value::Dict` iterates in **Unicode codepoint lexicographic order of
the keys** (the reference implementation uses `BTreeMap`). Hash
randomization, insertion-order preservation, and locale-dependent
sorting are forbidden.

```relon
{ "b": 1, "a": 2 } | dict.keys()  // always ["a", "b"]
```

### 2.2 List iteration order

`Value::List` iterates in insertion order. No surprises.

### 2.3 Floats

* Numeric types: `Int` (i64) and `Float` (IEEE-754 binary64 / `f64`).
* Float comparison uses the IEEE-754 total order
  (`OrderedFloat<f64>`):
  * `NaN == NaN` is `true` (a deliberate spec choice that lets
    `Dict<String, Float>` etc. round-trip).
  * `-0.0 == 0.0` is `true`.
  * In sorts, `NaN` is greater than every non-NaN.
* Float arithmetic obeys IEEE-754; fast-math, automatic FMA fusing,
  and compile-time constant folding that changes rounding are
  forbidden.
* Integer arithmetic on `i64` is checked: `+`, `-`, `*`, `/`, `%`,
  and unary `-` must raise `NumericOverflow` whenever the result
  would exceed `i64`'s representable range. The spec forbids wrap,
  saturate, panic, or any Rust debug-vs-release dependence.

### 2.4 Strings

* All strings are UTF-8 encoded; comparison and ordering are by
  Unicode codepoint.
* String operations like `string.split` are **byte-based** (matching
  Rust's `String::split`). For grapheme-cluster operations the host
  must expose a native function explicitly.

### 2.5 Invisible environment

Scripts CANNOT read:

* The system clock (`now()`, `SystemTime::now()`, …). If you need
  time, push it via `#main`.
* System timezone or locale.
* Environment variables.
* Random numbers (`rand`, `/dev/urandom`).
* Process ID, CPU count, etc.
* HashMap hash seeds (allowed for runtime-internal data structures
  but never exposed to scripts).

### 2.6 Error determinism

The error **kind tag** (`TypeMismatch`, `ModuleNotFound`,
`CapabilityDenied`, …) and the trigger location (`TokenRange`)
MUST be identical across runtimes; only the human-readable text may be
localized.

## 3. Lexical / syntax

Reference implementation: `crates/relon-parser`.

An implementation MUST accept every source the reference parser
accepts and reject every source it rejects. The grammar corpus is
defined by `fixtures/`, `examples/`, and `crates/relon-parser/tests/`.

### 3.1 The five directive shapes

Every `#name ...` directive matches one of five fixed shapes. The
shape is determined by the directive name (looked up in a parser
table) and is not user-extensible:

| Shape | Form | Example | Used for |
| --- | --- | --- | --- |
| Bare | `#name` | `#private` | Field flag |
| Value | `#name <expr>` | `#default 0`, `#expect "must be ≥0"`, `#brand Color` | Metadata / value transform |
| NameBody | `#name <ident> <body>` | `#schema User { String name: * }` | Named declaration (no colon) |
| Import | `#import <bindspec> from "<path>"` | `#import * from "std/list"` | Import |
| Main | `#main(Type name, ...) [-> ReturnType]` | `#main(User u, Cart cart) -> Order` | Entry signature |

`<bindspec>` is one of: a single ident (namespace), `*` (spread), or
`{ a, b as c }` (destructuring).

`#schema X: Body` is dict-field-position sugar — the `:` belongs to
the dict-field grammar, not the directive grammar; semantically it's
equivalent to `#schema X Body`.

`#relaxed` (synonym `#unstrict`) is the strict-mode opt-out; see
§6.6. Both are `Bare`-shape directives.

## 4. Capability model

### 4.1 Default-zero

A freshly constructed `Context` has **no capabilities**. Scripts:

* Cannot read the filesystem
  (`#import x from "./local.relon"` → `CapabilityDenied`).
* Cannot call any native function registered via
  `register_fn(name, gate, fn)` whose `NativeFnGate` declares any
  capability bit (pure fns registered via `register_pure_fn(name, fn)`
  carry an empty gate and pass under the sandbox).
* Have no step / value-size budget (`None` means "unenforced", but
  hosts SHOULD set both based on trust level).

### 4.2 Explicit grants

The host grants via `Capabilities` fields:

```rust
let mut ctx = Context::sandboxed();
ctx.capabilities.reads_fs = true;                           // permit #import on real FS and any host fn gated on reads_fs
ctx.capabilities.max_steps = Some(1_000_000);               // step budget
```

Or grant everything at once via `Capabilities::all_granted()` — but
that's an explicit, auditable grant rather than an implicit "trusted"
mode. **The spec forbids any `trusted()`-style shortcut constructor**:
scripts must be able to observe what the host did and didn't grant.

### 4.3 std modules' special status

`#import * from "std/list"`, `#import string from "std/string"`, etc.
resolve through a virtual `StdModuleResolver` and **do not consume**
the `reads_fs` capability. This is intentional — std is part of the
spec, not a trust decision.

## 5. Error kinds

Implementations MUST use these stable tags:

| Kind | Trigger |
|---|---|
| `Parse` | Lexical / syntactic error |
| `Analyze` | Semantic-analysis error aggregate (`#schema` heterogeneity, untyped fields, …) |
| `TypeMismatch` | Runtime value violates declared type |
| `VariableNotFound` | Reference to undefined name (schema, alias, function) |
| `FunctionNotFound` | Call to unregistered native fn or closure |
| `CircularImport` | `#import` cycle |
| `ModuleNotFound` | No resolver returned the module |
| `ModuleParseError` | Module file failed to parse |
| `IoError` | Genuine I/O error (within an allowed `reads_fs` op) |
| `CapabilityDenied` | Blocked by §4 |
| `NumericOverflow` | Integer arithmetic exceeds `i64`'s representable range |
| `StepLimitExceeded` | `max_steps` budget exhausted |
| `RecursionLimitExceeded` | Type-check / schema-validate recursion exceeds the runtime's safety cap (separate budget from `max_steps`) |
| `ValueTooLarge` | `max_value_elements` exceeded |
| `NoMainSignature` | File lacks `#main(...)` but `run_main` was called |
| `MissingMainArg` | Host did not push a value for a declared `#main` parameter |
| `UnexpectedMainArg` | Host pushed an arg name not in the `#main` signature |
| `MainArgTypeMismatch` | Pushed value doesn't match the declared parameter type |
| `UnsupportedOperator` | Invalid operation or type combination |

## 6. Standard library (spec-mandated)

Every implementation MUST provide these std modules. Scripts import
them via `#import <bindspec> from "std/<name>"`.

### 6.1 Language-level builtins (no import needed)

Three names belong to the **language**, not std modules — they are
metadata operations on the data structures themselves and are
available unconditionally:

* `len(value)` — element count of a `String` / `List` / `Dict`
  (`Int`).
* `range(end)` / `range(start, end)` — half-open `Int` list.
* `type(value)` — the value's type name (`"Int"`, `"Float"`,
  `"String"`, `"Bool"`, `"List"`, `"Dict"`, `"Closure"`, `"Null"`).

### 6.2 std module catalog

| Module | Functions | Notes |
|---|---|---|
| `std/list` | `map`, `filter`, `reduce`, `contains`, `sum`, `avg`, `len`, `first`, `last`, `compact`, `flatten` | Functional list ops |
| `std/dict` | `merge`, `keys`, `values`, `has_key` | Dict meta ops |
| `std/string` | `split`, `join`, `replace`, `upper`, `lower`, `contains` | String ops |
| `std/math` | `abs`, `max`, `min`, `clamp` | Numeric ops |
| `std/is` | `int`, `string`, `bool`, `float`, `list`, `dict`, `number`, `empty` | Type predicates |
| `std/value` | `default` | Value guards (null-coalesce, …) |

Each function's exact contract is defined by the reference
implementation's `crates/relon-evaluator/src/std_relon/<name>.relon`
sources; those `.relon` files are themselves part of the spec
(reference behavior of the std modules).

### 6.3 `ensure.*` validators

The `#schema` machinery depends internally on `ensure.*` functions
(`ensure.int`, `ensure.string`, etc.). They are an implementation
detail and not part of the user-facing API — but the implementation
MUST provide them with the spec'd semantics, otherwise `#schema`
will diverge.

### 6.4 Root expression — the document root may be any expression

A `.relon` file evaluates to **one JSON value** — Object, Array,
String, Number, Bool, or Null. The root **may be any expression**:
dict / list literal, atomic literal, binary / ternary / pipe
expression, function call, variant constructor, reference,
where / match — provided the final value falls in the JSON type
set.

```relon
// Legal root forms
{ id: 1, total: 99 }              // dict literal
[1, 2, 3]                          // list literal
n + 1                              // binary expression (in a #main entry)
"hello"                            // string literal
42                                 // integer
true                               // bool
null                               // null
Result.Ok { value: order }         // variant constructor
range(0, 10)                       // function call
@projector { ... }                 // decorated dict
```

Implementation requirement: implementations MUST accept every root
shape the reference parser accepts. Pre-v1.2 implementations that
only accepted dict / list literals at the root must extend to the
full expression chain.

`Closure` / `Schema` / `Type` / `Wildcard` are not JSON values. If
the user evaluates the root to one of these, host-side projectors
(e.g. the built-in `JsonProjector`) report errors
(`UnsupportedClosure` / `UnsupportedSchema`). On the static side,
declaring a non-JSON `ReturnType` (e.g. `Closure`, `Schema`) on
`#main(...) -> ReturnType` causes the analyzer's
`check_main_return` to emit `MainReturnTypeMismatch` as it does
today.

> Historical note: spec v1.0 / v1.1 allowed only dict / list
> literals at the root. v1.2 widens this to any expression
> (superset extension); legacy scripts are unaffected. This makes
> `#main(Int n) -> Int` writeable as `n + 1` directly and
> `#main(...) -> Result<T, E>` writeable as `Result.Ok { ... }`
> directly, no longer needing a `{ value: ... }` wrapper dict.

### 6.5 `#main(Type name, ...) [-> ReturnType]` — entry signature

`#main(...)` is a **root-level directive** (placed before the file's
root expression). It declares the file as an **entry program**: the
host must push named arguments matching the signature via
`Evaluator::run_main(scope, args)`, and the runtime validates them
before the body walk. Form:

```relon
#main(Req req)
{
    #schema Req {
        String name: *,
        #default 0
        Int retries: *
    },
    greeting: f"hello ${req.name}, retries=${req.retries}"
}
```

Multiple parameters are listed side-by-side:

```relon
#main(User user, Cart cart)
{
    #schema User { String name: * },
    #schema Cart { Int total: * },
    summary: f"${user.name} - ${cart.total}"
}
```

The optional `-> ReturnType` clause declares the **Json shape** the
body produces — an atom, dict, or list schema/type. Omitting it
leaves the return value unchecked.

**Don't write `ReturnType` as `Result<T, E>`.** The success-vs-
failure distinction lives at the **host boundary** —
`Evaluator::run_main` already returns `Result<Value, RuntimeError>`
on the Rust side, so wrapping that again in Relon is double
bookkeeping. The built-in `Result<T, E>` / `Option<T>` (see §X on
prelude schemas) are **value-level** concepts for modelling
"this field may be missing / may have failed" inside data — not for
the entry's overall return position.

```relon
// Good: ReturnType describes the Json the body produces
#main(Order order) -> Order
{ id: order.id, total: order.total * 1.1 }

// v1.2+: the root may be any expression, so atomic ReturnTypes
// are now usable directly:
#main(Int n) -> Int
n + 1

// Avoid: writing Result at the entry boundary — the host already
// gets Result<Value, RuntimeError> from Rust.
#main(Order order) -> Result<Order, String>
...
```

**Semantic requirements** (every implementation MUST provide):

1. `#main(...)` MUST be a **root-level directive** (placed before the
   file's root expression); writing it on a nested dict is
   meaningless.
2. Each parameter MUST be `Type name` (matching the `#schema` field
   convention):
   - The same parameter name declared twice → `Analyze` error
     `DuplicateMainParam`.
   - The type MUST resolve to a declared `#schema` or a built-in
     type.
3. Before the body walk, the data pushed via
   `Evaluator::run_main(scope, args)` MUST be validated against the
   signature:
   - Missing arg → `MissingMainArg`.
   - Extra arg → `UnexpectedMainArg`.
   - Type mismatch → `MainArgTypeMismatch`.
4. After validation, each parameter is bound **directly into the
   root scope's locals by parameter name** — scripts access them
   directly as `req`, `user`, etc., not via an `input.` prefix.
5. **No `#main(...)`** in the file: calling `run_main` raises
   `NoMainSignature`. Conversely, calling `eval_root` on a `#main`
   file (treating it as a library) also raises `NoMainSignature`
   — edge cases are caught at the boundary.
6. **Cross-file `#main` aggregation** (i.e., `#main(...)` in
   imported libraries also affects the entry's contract) is out of
   scope for v1 — only the entry file's `#main(...)` is validated.
   Library files typically don't declare `#main`, and the entry
   references them via `#import`.

`#main(...)` writes the entry contract into the `.relon` source
rather than the host, so the script can be audited independently and
the boundary checker rejects mis-shaped pushes before any body
evaluation — the keystone of §1.2's determinism boundary.

**v1.3** extends static analysis to cover `#main(...)` parameters
inside the root body: every declared parameter is seeded into the
root scope frame with its declared `TypeNode`, so atomic / dict /
list / variant / fn-call root forms can all reference parameters by
name and have them participate in `infer_type`. This closes the
v1.2-era gap where `#main(Int n) -> String\nn+1` would let the
mismatch slip through to runtime — `MainReturnTypeMismatch` now
surfaces statically.

### 6.6 Strict static-inference mode

Relon's analyzer is strict by default. The file *and every module
its `#import` graph reaches* requires every value to have a
statically inferable type. Sites the analyzer would otherwise let
pass with an implicit `Any` fallback now produce errors.

A module opts out by writing the file-level `Bare` directive
`#relaxed` (or its synonym `#unstrict`) at the top:

```relon
#relaxed
{ ... }
```

**Contagion rule**: strict mode is decided at the **entry**. The
entry's mode is stamped onto every reachable `#import` target, so
the workspace presents a single mode end to end. A strict entry
analyses every reachable library under strict rules — preventing a
relaxed library from sneaking silent fallbacks into a strict entry.
A `#relaxed` entry stamps the cleared bit on every reachable import,
so a strict library doesn't tighten a relaxed entry by accident.

**Diagnostic kinds** (all Error severity). Strict-mode checks split
into *cross-mode* and *strict-only* buckets — see the
[strict-mode reference](./strict-mode.md) for the full matrix.

Cross-mode (fire regardless of mode):

| Diagnostic | Trigger |
|---|---|
| `NonSpreadableSource { source_type }` | `{ ...e }` where `e`'s static type is known but isn't dict-shaped (e.g. `Int`, `Bool`, `List<T>`). No `<T>` hint can make this valid — the program is wrong in any mode |
| `UnresolvedSchema` | `<Schema>` annotation (typed spread, dynamic-key hint, etc.) names a schema that isn't declared in the workspace |
| `UnknownReferenceType { name, path }` | path-tail walker has positive knowledge a step is broken: descend into an undeclared schema field (`o.unknown`), descend past a leaf type (`o.id.something`), or — under strict — descend into `Any` |
| `DuplicateField` | spread contributes a key already declared on the dict, or two spreads contribute the same key |
| `ExplicitAnyForbidden { context }` | v1.6: user wrote `Any` somewhere in source (including nested `List<Any>` / `Dict<String, Any>`). `Any` is retired from the user-facing surface in every mode |
| `BareGenericContainer { type_name, context }` | v1.7: user wrote `List` / `Dict` / `Closure` / `Fn` / `Enum` without generic arguments |

Strict-only (silent under `#relaxed`; the underlying information is
*genuinely* missing rather than statically known):

| Diagnostic | Trigger |
|---|---|
| `SpreadSourceTypeUnknown` | `{ ...e }` where the analyzer can't determine `e`'s static shape (untyped closure parameter, untyped binding, etc.) and there's no `<T>` hint. Fix is to annotate the spread or type the source |
| `DynamicKeyTypeUnknown` | `{ [k]: v }` without a `<T>` typehint |
| `ExpressionTypeUnknown { reason }` | a genuinely opaque expression in a position that demands a derivable type: FnCall without a static signature in a typed slot, list element / dict field value that can't be inferred, match-arm body that can't be inferred |
| `NativeFnSignatureMissing { fn_name }` | call to a host-registered native fn with no `host_fn_signatures` entry |
| `ClosureParamTypeMissing { param_name }` | closure parameter has no declared type, leaking `Any` into the body scope |
| `ClosureReturnTypeUnknown { role }` | closure has neither a declared `-> ReturnType` nor an inferable body, so the synthesized signature would return `Any` |

**v1.4 path-tail walking** (applies to `Variable` / `Reference` paths
with multiple segments):

* `Schema(name)` head → next segment must be a declared field of that
  schema; missing → `UnknownReferenceType` (cross-mode).
* `Dict<K, V>` head → every key step yields V (homogeneous values).
* `Optional<T>` head → strip the `?` wrapper before stepping again,
  matching the runtime's `T?.x` semantics.
* `Any` head → after the v1.6/v1.7 double ban, the only path-head that
  can still land here is a closure parameter without a `type_hint`
  (strict raises `ClosureParamTypeMissing` and never reaches the
  walker). Propagate `Any` so non-strict callers defer to runtime.
* `Tuple<T1, T2, ...>` head (v1.7) → tuples are positional, not named;
  descending by name always yields `UnknownStep`. (Positional indexing
  syntax like `pair.0` / `pair.1` would plumb through this branch.)
* `Int` / `String` / `Bool` / `List<...>` and other leaves → cannot
  descend; `UnknownReferenceType` (cross-mode).

**v1.4 typed-spread sources** accepted in addition to an inline
`<T>` typehint:

* path chain: `...o.extras` — path-tail walks to a `Schema` or
  `Dict<K,V>` and the spread is accepted without a hint.
* FnCall: `...load_extras()` — the static signature's return type is
  a single-segment `Schema` or `Dict<K,V>`.
* sibling typed field: `...e` (already a v1.3 case via `Type e: ...`).
* dict literal: `...{ a: 1 }` (v1.3 case).

When the source can't be classified, strict mode prefers the more
specific path-tail diagnostic (`UnknownReferenceType`) over the
generic `SpreadSourceTypeUnknown`.

**v1.5 inference upgrades** — these expressions move from "decided at
runtime" to "statically inferable":

* **list comprehension** `[elem for x in iter if cond]` — once `iter`
  infers as `List<T>` (or `Dict<V>`), `x` is typed as T (or V) inside
  the element body, and the whole expression becomes
  `List<element_type>`.
* **where expression** `expr where { k1: v1, k2: v2 }` — every binding's
  inferred value type seeds the body's scope; the expression's type is
  the body's inferred type.
* **`Expr::Spread(inner)`** as a standalone expression — equals the
  inner inference result.
* **`#main(...)` / closure parameters** — strict mode forbids any
  parameter whose declared type is missing or `Any`; closures without
  a declared `-> ReturnType` whose body inference falls to `Any` also
  fire under strict mode.
* **head-unresolved references** — strict escalates the legacy
  `UnresolvedReference` warning to `UnknownReferenceType` at error
  severity.
* **multi-segment FnCall paths** (`alias.method`) — route through
  `lookup_signature_path`, covering cross-module and sibling-method
  forms uniformly.

After v1.5 the only silent fallbacks left under strict mode are: (i)
host-registered native fns with no declared signature (covered by
`NativeFnSignatureMissing`), and (ii) explicitly-untyped sites the
user opted into (covered by `SpreadSourceTypeUnknown` /
`DynamicKeyTypeUnknown`). Everything else that's derivable from
source + schemas is caught statically — v2 hardens this further by
moving every "statically known to be wrong" check (non-dict spread
source, undeclared schema, broken path step) to cross-mode error
severity.

**v1.6: retire `Any` from the user-facing surface entirely**

v1.5 still let the user write `Any` as a type annotation; v1.6 bans it
in *every mode* (strict and non-strict alike) by reporting
`ExplicitAnyForbidden`:

* `Any field: ...`
* `#main(Any x)` / `#main(...) -> Any`
* `(Any n) => ...` / `(...) -> Any => ...`
* `#schema X { Any payload: * }`
* nested forms — `List<Any>`, `Dict<String, Any>`, `List<Dict<String, Any>>`,
  any depth

Replacements: concrete types (`Int` / `String` / `Bool`), parameterized
containers (`List<T>` / `Dict<String, V>`), `Enum<...>` for sum types,
or a custom `#schema`. The "I'll accept any shape" use case is
expressed by declaring the schema explicitly — there is no
all-purpose escape hatch any more.

**v1.6 stdlib-signature rewrite**: every internal `Any` slot in the
stdlib is now an unbound generic placeholder so the language surface
no longer mentions the keyword internally either:

* `len<T>(T) -> Int` / `_len<T>(T) -> Int` / `type<T>(T) -> String`
* `_string_join<T>(List<T>, String) -> String`
* `_dict_merge<V>(Dict<String, V>, ...) -> Dict<String, V>`
* `_dict_keys<V>(Dict<String, V>) -> List<String>`
* `_dict_values<V>(Dict<String, V>) -> List<V>` — **value type now
  flows end-to-end**
* `_dict_has_key<V>(Dict<String, V>, String) -> Bool`
* `ensure.int / .string / ...<T>(T, message?) -> T` — **preserves the
  input type instead of collapsing to `Any`**
* `ensure.at_least<T>` / `.at_most<T>` / `.one_of<T>` — same shape
* `ensure.required_fields<V>` / `.requires<V>` / `.fields_equal<V>` —
  same shape

Unbound `<T>` is behaviorally equivalent to "accepts any type" today
(Relon doesn't have trait bounds yet) but the type flow is clean: the
call site binds a concrete type, and downstream typed slots see the
precise shape (`Int n: ensure.int(x)` lands `n: Int` instead of being
swallowed by `Any`).

**The only remaining `Any` retentions** are all internal:

1. The analyzer's `InferredType::Any` placeholder for "couldn't infer"
   (never user-visible).
2. Generic-placeholder fallback (Pass 3 in `collect_bindings` fills
   unbound `<T>` with `Any` for substitution — also internal).
3. Runtime `Value` is dynamically typed (implementation detail).

None of these reach source code, diagnostics, or documentation
examples — `Any` is gone from the user-facing surface.

**v1.7: Tuple types + bare-generic ban**

Through v1.6, list literals carried two roles: homogeneous arrays and
heterogeneous tuples. With `List<Any>` retired, `[1, "x"]` no longer
had a legal annotation. v1.7 introduces a proper `Tuple` type for
fixed-length, mixed-element data:

```relon
// Trailing-comma form disambiguates a 1-tuple from a parenthesized type
() unit: []
(Int,) one: [1]
(Int, String) pair: [42, "hello"]
List<(String, Int, Bool)> rows: [
  ["alice", 3, true],
  ["bob", 1, false]
]
```

Semantics:

* List-literal inference now produces `Tuple<T1, T2, ...>` (instead of
  `List<join(...)>`), preserving each element's precise type.
  `[1, 2, 3]` infers as `Tuple<Int, Int, Int>`; `[1, "x"]` infers as
  `Tuple<Int, String>`.
* **Tuple → List collapse**: a homogeneous tuple still subsumes
  `List<T>` (every element subsumes `T`), so all pre-v1.7 homogeneous-
  list usage keeps working.
* **Tuple → Tuple**: arity check first, then per-position recursion.
  Any mismatch raises `StaticTypeMismatch` pinpointed to the position.
* Nesting is fine: `List<(Int, String)>`, `(List<Int>, String)`,
  `((Int, Int), String)`.

**Bare-generic ban**: v1.7 also closes the bare-generic shorthand for
`List` / `Dict` / `Closure` / `Fn` / `Enum` (no generic arguments).
Pre-v1.7 they silently expanded to `List<Any>` / `Dict<Any, Any>` /
`Fn(_, Any)` / etc. — the only remaining back-door for `Any` after
v1.6's ban. The new `BareGenericContainer` diagnostic fires at every
TypeNode site (source code, `#main` parameters, closure parameters,
schema fields, nested generic slots); the only fix is to write the
explicit type arguments.

```relon
{ List items: [1, 2, 3] }              // BareGenericContainer
{ Dict scores: { math: 100 } }         // BareGenericContainer
{ Closure cb: (x) => x }               // BareGenericContainer
{ Dict<String, List> data: ... }       // BareGenericContainer (nested)

{ List<Int> items: [1, 2, 3] }         // OK
{ Dict<String, Int> scores: { ... } }  // OK
```

`BareGenericContainer` is mode-independent — **every mode reports it
as an Error**, just like v1.6's `ExplicitAnyForbidden`.

**v1.8: Enum / Result first-class + host fn audit**

After v1.7 closed the user-source back-doors (`Any`, bare generics),
three positions still let things slip through statically. v1.8 closes
all three:

* **`Enum<...>` slot now alternative-aware**: previously
  `subsumes_with` returned `true` unconditionally for an `Enum` head,
  so `42` would happily land in `Enum<"up", "down">`. v1.8 walks the
  alternatives and accepts only when at least one matches statically.
  Bareword alternatives (parser strips quotes from `"up"`, leaving
  `up`; same shape as a schema name `Active`) are treated as `String`
  candidates, mirroring the runtime cheap-path.
* **`Result<T, E>` / `Option<T>` generic substitution**: previously
  `Result<Int, String> r: Result.Ok { value: "wrong" }` was caught
  only at runtime. v1.8 substitutes `T -> Int, E -> String` into the
  variant's declared field types and recurses into the body. Every
  user-declared sum schema with generics rides the same code path
  (`#schema Pair<T, U> Enum<Both { left: T, right: U }>` works the
  same way). `Result` / `Option` variant shapes are injected via
  `seed_prelude_variants` so the analyzer's view matches the runtime.
* **Host fn signature audit**: `audit_host_fn_signatures` runs
  `scan_typenode_for_any` over every `AnalyzeOptions::host_fn_signatures`
  entry's params / return / variadic-tail. Diagnostics carry
  `host fn '{name}' parameter '{param}'` etc. so a host shipping
  `register_fn("foo", sig{ params: [Any], … })` can't bypass v1.6 /
  v1.7's user-source bans.
* **Cross-module `pkg.SchemaName` static resolution**: pre-v1.8 a
  multi-segment slot like `lib.User u: 42` collapsed to `Any` in
  `infer_from_type_node`, so the typed-binding check passed
  silently. v1.8 introduces `infer_from_type_node_with_imports`
  and `subsumes_with_imports`, both threading
  `Option<&WorkspaceImportIndex>` through the analyzer. A
  two-segment slot whose head is a known import alias and whose
  tail is one of that alias's exported root-level schemas is
  folded to a single-segment `Schema(tail)` slot — the rest of
  the subsumption logic runs as if the user had written the bare
  schema name. The same-file `_ => true` catchall for a
  non-matching schema slot also got tightened: a clearly
  non-schema value shape (primitive, list, fn, tuple) is now a
  hard mismatch instead of silently accepted.
* **Tuple-position access (`pair.0` / `pair.1`)**: pre-v1.8 the
  walker stack dropped every non-`String` segment via
  `path_segments`, so `pair.0` walked just `pair` and the static
  type stayed `Tuple<...>`. v1.8 introduces `WalkSeg::Name |
  Index` and a new `walk_segments` builder; `walk_path` gains
  `Tuple, Index(i)` → element at position `i`,
  `Tuple, Name(_)` → hard `UnknownStep`,
  `List<T>, Index(_)` → `T`. Out-of-range tuple indices surface
  `UnknownStep` (strict mode lifts to `UnknownReferenceType`);
  list bounds checks stay runtime's job. Runtime behaviour is
  unchanged — tuples are still `Value::List` at runtime, so
  positional access goes through the existing list-index lookup.

**v1.3 typed-spread / typed-dynkey syntax** (used by strict mode,
also accepted in non-strict mode for opt-in static checking):

```relon
// typed spread — `<T>` after `...`
{ ...<Extra> e }
{ ...<Dict<String, Int>> kv }

// typed dynamic key — `<T>` after `[`
{ [<String> key_expr]: value }
{ [<Int> idx]: row }
```

Under `#relaxed` the hints are still recognized — when present, the
analyzer uses them; when absent, the affected slot falls back to
`Any` and runtime owns the verdict. Strict (the default) escalates
the absent-hint case to an error (`Missing*Hint`).

**`Dict<K, V>` generics** (formally specified in v1.3): the parser
accepts `Dict` with one or two generic arguments (mirroring `List<T>`
/ `Result<T, E>`). `Dict<String, Int>` validates each value against
`Int`; nested forms like `Dict<String, Result<Int, String>>` are
also accepted. **Starting in v1.7**, bare `Dict` (no generics) is
rejected by the `BareGenericContainer` diagnostic — explicit generics
are now mandatory.

```relon
{ Dict<String, Int> scores: { math: 100, art: 90 } }
```

## 7. Boundary of host-registered extensions

The host can inject via `register_fn` / `register_pure_fn` /
`register_decorator`:

* Native functions (data in, data out).
* Decorator plugins (custom `@value` replacements, domain-specific
  transformers).

**Host-injected names are not part of the spec**. A script that
depends on a host-injected name steps outside the spec's guarantees
and only behaves predictably on that host configuration.

Best practice:

* Ship business libraries as `.relon` files (libraries without
  `#main`) and distribute them via `#import`. They depend only on
  the language and std, so behavior is fully determined by spec +
  source.
* Register native functions only when "needs host capability"
  applies (FS, DB, HTTP). Use `register_fn(name, gate, fn)` and
  declare the required bits on the `NativeFnGate`; pure fns go through
  `register_pure_fn(name, fn)` (empty gate, trivially satisfied under
  the sandbox).

## 8. Versioning

* This document tracks **spec v1**.
* std modules evolve via semver: behavior changes bump major;
  additions bump minor.
* `#import * from "std/<name>"` binds to the runtime's latest
  compatible version. A future direction is
  `#import * from "std/<name>@1.x"` for explicit pinning.
* The runtime MUST report the spec version it implements in metadata
  (`relon --version` or equivalent API).

## Appendix A: Saying goodbye to the "configuration language" framing

Historically Relon docs framed it as a "typed business-config DSL".
That framing was **inaccurate**: with each host extending freely and
scripts depending on ambient state, evaluation determinism has
nothing to stand on.

Logic-as-Data replaces that framing. It means:

* No "trusted mode" lets scripts bypass the sandbox.
* No ambient global names for scripts to depend on implicitly
  (host-injected names are out of spec scope; authors choose
  whether to use them).
* No unspecified float / iteration-order behavior.
* std is part of the language, not an optional extension.

Each choice serves the same goal: **logic stored and shipped like
JSON, evaluated deterministically inside a sandbox.**
