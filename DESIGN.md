# Relon Design Document (V2.0)

Relon's payload IS executable logic. The language exists so business
rules — validation, pricing, workflow, policy — can be **serialized like
JSON, distributed like JSON, and evaluated identically on any conformant
runtime**. Same source + same input → byte-identical result, whether the
runtime is Rust, Go, TypeScript, Swift, or WASM-in-the-browser.

The reference runtime in this repo is Rust. The *spec* is runtime-agnostic.

## 1. Core Philosophy

### 1.1 Logic-as-Portable-Data (the load-bearing axiom)
Everything else in this document follows from one decision: **logic is
data**. Rules don't compile into binary; they live in databases, config
files, RPC payloads, message queues. Any conformant Relon VM that
parses the same source and is fed the same input must produce the same
output.

This rules out, by design:
- Implicit ambient state (env vars, locale, time-of-day, RNG, FS — all
  must be capabilities the script declares and the host explicitly
  grants).
- Iteration-order leaks (Dict iteration is `BTreeMap`-deterministic, not
  hash-randomized).
- Per-runtime "magic" globals (every name a script references must be
  reachable through the spec — language builtins, `@import`-ed std
  modules, or host-registered functions in the program-declared
  capability set).
- Floating-point ambiguity (IEEE-754 `f64`, no fast-math).

### 1.2 Sandboxed by Default
A Relon script has zero ambient privileges. Capabilities (`reads_fs`,
allow-listed native fns, step / value-size budgets) are granted by the
host at `Context` construction. There is no "trusted" mode the script
can fall back into without the host's explicit consent — that would
break the portability axiom.

### 1.3 Tagged & Self-Describing
Types travel with the payload via `@schema`, sum-type tagged enums,
branded dicts, and computed defaults. A receiver of a Relon document
has everything they need to validate it without out-of-band
documentation.

### 1.4 Expression-Oriented, Pure
No statements, no `return`, no IO primitives, no mutable globals. Every
function body is a single expression; every evaluation is a pure
function from `(source, input, capabilities)` to `Value`.

## 2. Language Features

### 2.1 The Tagged Type System
Types can be explicitly annotated before keys or values. They support generics:
```javascript
{
    String name: "Relon",
    List<String> tags: ["config", "typed"],
    Dict<String, Int> scores: { "math": 100 }
}
```

### 2.2 First-Class Functions (Double-Track Syntax)
Relon treats functions as first-class citizens with two elegant syntax forms:

**1. Method Declarations (Dict Shorthand)**
Ideal for defining structural logic or standard libraries. Supports optional return type prefixes.
```javascript
{
    // Untyped
    sum(a, b): a + b,
    
    // Strongly typed
    Int multiply(Int a, Int b): a * b
}
### 2.2 First-Class Functions (Double-Track Syntax)
...
**2. Arrow Functions (Anonymous Closures)**
Ideal for passing logic as values, mapping over lists, or temporary calculations.
```javascript
{
    doubled: list.map([1, 2, 3], (x) => x * 2),
    filtered: list.filter(data, (Int x) -> Bool => x > 10)
}
```

**3. Pattern Matching (Match Expression)**
Declarative polymorphism based on types or wildcards.
```javascript
result: data match {
    Image: render_image(data),
    Text: render_text(data),
    *: default_value
}
```

### 2.3 Advanced Data Transformation
- **Pipe (`|`)**: Sequential data processing: `[1, 2, 3] | len()`.
- **Spread (`...`)**: Merging Lists and Dicts: `{ ...base, "extra": true }`.
- **Comprehensions**: Powerful iteration: `[x * 2 for x in range(5) if x % 2 == 0]`.
- **List Relative References**: List elements can access their context using `&index`, `&this`, `&prev`, and `&next`.


### 2.4 Modular System
- **`@import("path", as="alias")`**: Namespace-protected import.
- **`@import("path", spread=true)`**: Flat import. Skips `_` prefixed fields by default to protect the namespace.
- **`std/` Virtual Path**: Built-in standard library (e.g., `std/math`, `std/list`).

## 3. Architecture
- **`relon-parser`**: A modern recursive-descent parser built on `winnow`. It outputs a tree-based AST (`Node`/`Expr`) with optional `TypeNode` hints.
- **`relon-evaluator`**: A path-aware tree-walking interpreter with cycle detection and scope pre-scanning.

## 4. Engineering Standards
- **Strict English**: All code and internal documentation are in English.
- **Underline (_) Convention**:
  - **Style Suggestion**: Fields starting with `_` are treated as "internal" or "private" by convention.
  - **Import Spread Protection**: In `@import(..., spread=true)`, fields starting with `_` are **automatically skipped**. They remain accessible via explicit reference (e.g., `lib._helper`).
  - **JSON Export**: Fields starting with `_` are **NOT** automatically hidden. If they contain valid data (non-closures), they are exported to JSON.
- **JSON Projection Rules**:
  - **Closure Filtering**: In a `Dict`, any field whose value is a closure is **hidden** from JSON output. This is the primary mechanism for hiding logic.
  - **Data Consistency**: In a `List` or at the top-level, closures are **not allowed** and will trigger an `UnsupportedClosure` error to prevent silent structural changes.
