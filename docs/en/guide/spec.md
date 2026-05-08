# Relon Language Specification

> **Status**: v1 candidate. This document is the executable
> formulation of Relon's Logic-as-Portable-Data promise — any conformant
> runtime (the reference implementation is in Rust) MUST behave per the
> semantics described here; scripts may rely only on the names and
> contracts the spec declares.

## 1. Design commitment

> **Same source + same input → byte-identical output.**

This is the load-bearing axis of the spec. Every constraint below
exists to make that single sentence remain true across runtimes,
machines, and time.

### 1.1 Conformant runtime

An implementation is **conformant** if and only if, for every
source + input combination covered by this spec:

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
build-artifact size) are up to each runtime and don't affect
conformance.

### 1.2 Conditions for cross-runtime determinism

In "same source + same input → byte-identical output", **input** means
the explicit `Value` tree the host pushes via
`Evaluator::run_main(scope, args)` before evaluation; the script
declares the expected shape via a `#main(...)` signature and accesses
the bindings by parameter name.

Results returned from native functions registered via `register_fn`
are **not** input. Therefore:

- **Push form** (host completes I/O before evaluation, materializes
  the data into a `Value`, and pushes it via `run_main(args)`; the
  script declares the contract with `#main(...)`): cross-runtime
  determinism is in scope.
- **Pull form** (the script pulls external data through native
  functions during evaluation): the author has **deliberately
  given up** cross-runtime determinism — different host /
  runtime / time inherently sees different network and external
  state, and the spec neither requires nor can guarantee parity.

See `docs/zh/guide/host-integration.md` for the implementer guide
(the comprehensive guide is currently Chinese-first).

### 1.3 sigil split: `@` vs `#`

Relon separates "metadata stacked on a node" into two disjoint
namespaces. This is a hard spec requirement — a conformant runtime
MUST NOT allow a single name to exist in both `@`-form and `#`-form.

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

## 2. Determinism contract

To honor §1, every conformant runtime MUST:

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
* Integer arithmetic on `i64` follows Rust semantics: overflow wraps
  in release. The spec mandates this wrap behavior — saturating /
  panicking implementations are non-conformant.

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

A conformant runtime MUST accept every source the reference parser
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

## 4. Capability model

### 4.1 Default-zero

A freshly constructed `Context` has **no capabilities**. Scripts:

* Cannot read the filesystem
  (`#import x from "./local.relon"` → `CapabilityDenied`).
* Cannot call any native function registered via
  `register_fn_with_caps`.
* Have no step / value-size budget (`None` means "unenforced", but
  hosts SHOULD set both based on trust level).

### 4.2 Explicit grants

The host grants via `Capabilities` fields:

```rust
let mut ctx = Context::sandboxed();
ctx.capabilities.reads_fs = true;                          // permit #import on real FS
ctx.capabilities.allow_native_fn.insert("fs.read".into());  // permit calls to a named host fn
ctx.capabilities.max_steps = Some(1_000_000);               // step budget
```

Or grant everything at once via `Capabilities::all_granted()` — but
that's an explicit, auditable grant rather than an implicit "trusted"
mode. **The spec forbids any `trusted()`-style shortcut constructor**:
scripts must be able to observe what the host did and didn't grant on
any conformant runtime.

### 4.3 std modules' special status

`#import * from "std/list"`, `#import string from "std/string"`, etc.
resolve through a virtual `StdModuleResolver` and **do not consume**
the `reads_fs` capability. This is intentional — std is part of the
spec, not a trust decision.

## 5. Error kinds

Conformant runtimes MUST use these stable tags:

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
| `StepLimitExceeded` | `max_steps` budget exhausted |
| `RecursionLimitExceeded` | Type-check / schema-validate recursion exceeds the runtime's safety cap (separate budget from `max_steps`) |
| `ValueTooLarge` | `max_value_bytes` exceeded |
| `NoMainSignature` | File lacks `#main(...)` but `run_main` was called |
| `MissingMainArg` | Host did not push a value for a declared `#main` parameter |
| `UnexpectedMainArg` | Host pushed an arg name not in the `#main` signature |
| `MainArgTypeMismatch` | Pushed value doesn't match the declared parameter type |
| `UnsupportedOperator` | Invalid operation or type combination |

## 6. Standard library (spec-mandated)

Every conformant runtime MUST implement these std modules. Scripts
import them via `#import <bindspec> from "std/<name>"`.

### 6.1 Language-level builtins (no import needed)

Three names belong to the **language**, not std modules — they are
metadata operations on the data structures themselves and every
runtime ships them unconditionally:

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
detail and not part of the user-facing API — but conformant runtimes
MUST provide them with the spec'd semantics, otherwise `#schema`
will diverge.

### 6.4 `#main(Type name, ...) [-> ReturnType]` — entry signature

`#main(...)` is a **root-level directive** (placed before the file's
root dict). It declares the file as an **entry program**: the host
must push named arguments matching the signature via
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

// Avoid: writing Result at the entry boundary — the host already
// gets Result<Value, RuntimeError> from Rust.
#main(Order order) -> Result<Order, String>
...
```

**Semantic requirements** (every conformant runtime MUST implement):

1. `#main(...)` MUST be a **root-level directive** (placed before the
   file's root dict); writing it on a nested dict is meaningless.
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
rather than the host, so any conformant runtime sees the same
script and validates against the same schema — the keystone of §1.2's
cross-runtime determinism guarantee.

## 7. Boundary of host-registered extensions

The host can inject via `register_fn` / `register_fn_with_caps` /
`register_decorator`:

* Native functions (data in, data out).
* Decorator plugins (custom `@value` replacements, domain-specific
  transformers).

But **the spec does not require other runtimes to provide these
host-injected names**. A script that depends on a host-injected name
forfeits cross-runtime portability for that scope and only works on
that host.

Best practice:

* Ship business libraries as `.relon` files (libraries without
  `#main`) and distribute them via `#import`. They are portable
  across runtimes by construction.
* Register native functions only when "needs host capability"
  applies (FS, DB, HTTP), and tag them via `register_fn_with_caps`
  with the appropriate `NativeFnGate`.

## 8. Versioning

* This document tracks **spec v1**.
* std modules evolve via semver: behavior changes bump major;
  additions bump minor.
* `#import * from "std/<name>"` binds to the runtime's latest
  compatible version. A future direction is
  `#import * from "std/<name>@1.x"` for explicit pinning.
* Runtimes MUST report the spec version they implement in metadata
  (`relon --version` or equivalent API).

## 9. Building a new runtime

If you want to write a Go / TS / Swift / your-language conformant
runtime:

1. **Start from the grammar corpus**: ensure your parser accepts every
   `.relon` in `fixtures/` and `examples/` and produces an AST
   isomorphic to the reference.
2. **Reuse the std `.relon` sources**:
   `crates/relon-evaluator/src/std_relon/*.relon` files are the
   reference behavior of std modules; you only need to implement the
   `_*` intrinsics (`_list_map`, …) as native; the rest is plain
   Relon and shared across runtimes.
3. **Pass conformance tests**: `fixtures/golden/` lists reference
   outputs; any conformant runtime running the same source MUST
   produce the same JSON.
4. **Align error codes** per §5.

## Appendix A: Saying goodbye to the "configuration language" framing

Historically Relon docs framed it as a "typed business-config DSL".
That framing was **inaccurate**: with each host extending freely and
scripts depending on ambient state, cross-runtime parity has nothing
to stand on.

Logic-as-Portable-Data replaces that framing. It means:

* No "trusted mode" lets scripts bypass the sandbox.
* No runtime-private global names for scripts to depend on
  implicitly.
* No unspecified float / iteration-order behavior.
* std is part of the spec, not an optional extension.

Each choice serves the same goal: **logic flows between systems like
JSON, with completely deterministic results.**
