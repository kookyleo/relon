# Relon Language Specification

> **Status**: v1 candidate. This document is the executable expression
> of the Logic-as-Portable-Data promise. Any runtime claiming
> conformance (the reference implementation is Rust) MUST behave as
> described here; scripts MAY only depend on names and contracts
> declared here.

## 1. Design Commitment

> **Same source + same input → byte-identical output.**

This is the load-bearing axiom. Every constraint below exists so that
sentence holds across runtimes, machines, and time.

### 1.1 What "Conformant Runtime" Means

An implementation is **conformant** iff for every source + input pair
covered by this spec it:

1. **Parses**: accepts every source the reference parser accepts;
   rejects every source the reference parser rejects.
2. **Evaluates**: produces a `Value` byte-identical to the reference
   implementation.
3. **Capabilities**: implements the §4 `Capabilities` model with no
   side door letting scripts bypass it.
4. **Standard library**: ships every std module listed in §6 with the
   semantics defined there.
5. **Error kinds**: uses the stable taxonomy in §5 (human-readable
   messages MAY be localized).

Implementation details outside this scope (internal caches, threading,
binary size) are runtime-private and don't affect conformance.

## 2. Determinism Contract

To make §1's axiom hold, every conformant runtime MUST:

### 2.1 Dict iteration order

`Value::Dict` iterates in **Unicode-codepoint key order** (the
reference implementation uses `BTreeMap`). Hash randomization,
insertion-order preservation, and locale-dependent collation are all
forbidden.

```relon
{ "b": 1, "a": 2 } | dict.keys()  // always ["a", "b"]
```

### 2.2 List iteration order

`Value::List` iterates in insertion order.

### 2.3 Floating point

* The numeric kinds are `Int` (i64) and `Float` (IEEE-754 binary64 /
  `f64`).
* Float comparison uses IEEE-754 total ordering (`OrderedFloat<f64>`):
  * `NaN == NaN` is `true` (different from Rust's `PartialEq`; this
    is a deliberate spec choice so `Dict<String, Float>` keys remain
    equality-comparable).
  * `-0.0 == 0.0` is `true`.
  * Sort order treats `NaN` as greater than every non-NaN.
* Float arithmetic follows IEEE-754; fast-math, automatic FMA fusion,
  and constant-folding that yields different rounding are forbidden.
* Integer arithmetic on `i64` follows Rust release semantics: overflow
  wraps. Saturating or panicking strategies are forbidden by the spec.

### 2.4 Strings

* All strings are UTF-8 and compared by Unicode code point.
* `string.split` etc. operate at the **byte** level (the reference
  implementation's `String::split`); grapheme-cluster operations must
  be supplied by the host as native fns when needed.

### 2.5 The unobservable environment

Scripts MAY NOT read:

* System clock (`now()`, `SystemTime::now()` …). Pass time through
  input.
* System timezone, locale.
* Environment variables.
* Random sources (`rand`, `/dev/urandom`).
* Process metadata (PID, CPU count).
* HashMap hash seeds. Internal data-structure usage is fine; nothing
  may surface to the script.

### 2.6 Error determinism

Error **kind labels** (`TypeMismatch`, `ModuleNotFound`,
`CapabilityDenied`, …) and trigger locations (`TokenRange`) MUST be
identical across runtimes; only human-readable messages MAY be
localized.

## 3. Lexical / Syntactic Layer

Reference implementation: `crates/relon-parser`.

A conformant runtime MUST accept every source the reference parser
accepts and reject every source it rejects. The grammar corpus is
defined by `fixtures/` + `examples/` + the parser's own test suite.

> See [Syntax](./syntax.md) for the friendly tour.

## 4. Capability Model

### 4.1 Zero ambient privileges by default

A freshly-constructed `Context` grants nothing. Scripts cannot:

* Read the filesystem (`@import("./local.relon")` →
  `CapabilityDenied`).
* Call any host fn registered via `register_fn_with_caps`.
* Run unbounded (the host SHOULD set `max_steps` /
  `max_value_bytes`).

### 4.2 Explicit grant or nothing

Hosts grant capabilities by mutating `Capabilities` fields
explicitly:

```rust
let mut ctx = Context::sandboxed();
ctx.capabilities.reads_fs = true;                          // permit @import on real FS
ctx.capabilities.allow_native_fn.insert("fs.read".into());  // permit a specific native fn
ctx.capabilities.max_steps = Some(1_000_000);               // bound step count
```

Or all at once via `Capabilities::all_granted()` — but this is an
explicit, audit-visible grant, not an implicit "trusted mode". **The
spec forbids any `trusted()`-style shortcut constructor**: scripts
must be able to observe what the host did and didn't grant on every
runtime.

### 4.3 Std virtual modules are exempt from `reads_fs`

`@import("std/list")` etc. resolve through a virtual `StdModuleResolver`
and do not consume `reads_fs`. This is deliberate: std is part of the
spec, not something the host audits.

## 5. Error Kinds (Stable Taxonomy)

| Kind | Trigger |
|---|---|
| `Parse` | lexical / syntactic error |
| `Analyze` | aggregated semantic-analysis errors |
| `TypeMismatch` | runtime value violates declared type |
| `VariableNotFound` | reference to an undeclared name |
| `FunctionNotFound` | call to an unregistered native fn / closure |
| `CircularImport` | `@import` cycle |
| `ModuleNotFound` | no resolver returned the module |
| `ModuleParseError` | imported file failed to parse |
| `IoError` | real I/O error during a permitted FS op |
| `CapabilityDenied` | §4 rejection |
| `StepLimitExceeded` | `max_steps` exhausted |
| `ValueTooLarge` | `max_value_bytes` exceeded |
| `LibraryAsEntry` | `@library` file evaluated as host entry |
| `UnsupportedOperator` | invalid operation or type combination |

## 6. Standard Library Catalog (Spec-mandated)

A conformant runtime MUST implement these. Scripts access them via
`@import("std/<name>", as="<alias>")`.

### 6.1 Language-level builtins (no import required)

Three names belong to the **language**, not to a std module — they are
metadata operations on data structures themselves and are
unconditionally available:

* `len(value)` — element count of a `String` / `List` / `Dict`
  (`Int`).
* `range(end)` / `range(start, end)` — half-open `Int` list.
* `type(value)` — type name as `String`: `"Int"`, `"Float"`,
  `"String"`, `"Bool"`, `"List"`, `"Dict"`, `"Closure"`, `"Null"`.

### 6.2 Std module catalog

| Module | Functions | Notes |
|---|---|---|
| `std/list` | `map`, `filter`, `reduce`, `contains`, `sum`, `avg`, `len`, `first`, `last`, `compact`, `flatten` | functional list ops |
| `std/dict` | `merge`, `keys`, `values`, `has_key` | dict meta-ops |
| `std/string` | `split`, `join`, `replace`, `upper`, `lower`, `contains` | string ops |
| `std/math` | `abs`, `max`, `min`, `clamp` | numeric ops |
| `std/is` | `int`, `string`, `bool`, `float`, `list`, `dict`, `number`, `empty` | type predicates |
| `std/value` | `default` | value guards (null-coalesce, etc.) |

Each function's precise contract is defined by the reference Relon
source at `crates/relon-evaluator/src/std_relon/<name>.relon`; **those
`.relon` files are part of the spec**.

### 6.3 The `ensure.*` validators

`@schema` machinery depends internally on `ensure.*` functions. These
are implementation details of the schema system and are not part of
the user-facing API — but a conformant runtime MUST still register
them with the spec'd semantics, otherwise `@schema` will diverge.

## 7. Host-Registered Extensions

Hosts MAY inject through `register_fn` / `register_fn_with_caps` /
`register_decorator`:

* Native functions (data in, data out).
* Decorator plugins (`@expect`, custom `@brand` behaviors, …).

But **the spec does not require other runtimes to provide
same-named extensions** — a script that depends on host-injected
names has stepped outside the portability promise; behavior is
guaranteed only on that host.

Best practice:

* Ship business libraries as `.relon` files (mark with `@library`)
  distributed via `@import`. They are portable across runtimes by
  construction.
* Reach for native fns only when host capabilities are required (FS,
  database, HTTP), and declare them with `register_fn_with_caps` +
  `NativeFnGate`.

## 8. Versioning

* This document is **spec v1**.
* Std modules version under semver: a function-semantic change is a
  major bump; new functions are minor.
* `@import("std/<name>")` binds to the runtime's latest compatible
  version by default. Future revisions may add
  `@import("std/<name>", version="1.x")` for explicit pinning.
* Runtimes MUST publish (via `relon --version` or equivalent) the
  spec version they implement.

## 9. Implementing a New Runtime

To bring up a Go / TS / Swift / your-language conformant runtime:

1. **Start from the syntax corpus**: ensure your parser accepts every
   `.relon` under `fixtures/` and `examples/` and produces an AST
   isomorphic to the reference's.
2. **Reuse the std `.relon` sources**: the files under
   `crates/relon-evaluator/src/std_relon/` ARE the std module's
   reference behavior. You only need to implement the `_*`
   intrinsics (`_list_map`, `_string_split`, …) as natives; the rest
   of the std functions are pure relon and are shared across
   runtimes.
3. **Pass the conformance suite**: `fixtures/golden/` holds reference
   outputs. Any conformant runtime running the same source MUST
   produce identical JSON.
4. **Align error kinds**: see §5.

> Detailed implementer guide is in [host-integration](./host-integration.md);
> read it side-by-side with this spec.

## Appendix A: Departing from the "configuration language" framing

Earlier docs described Relon as a "typed business-config DSL". That
framing is **incorrect** in retrospect: under it, each host extends
freely, scripts depend on ambient state, and cross-host consistency
is unknowable.

Logic-as-Portable-Data replaces it. The implication is:

* No "trusted mode" letting scripts bypass the sandbox.
* No runtime-private global names letting scripts depend implicitly.
* No unspecified float / iteration-order behavior.
* The std library is part of the spec, not an optional extension.

These choices all serve one goal: **logic flows between systems like
JSON does, with byte-identical results.**
