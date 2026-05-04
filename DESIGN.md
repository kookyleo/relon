# Relon Design Document (V2.0)

Relon is a programmable configuration language designed to be a functional super-set of JSON. It enables dynamic data generation through a tagged type system, first-class arrow functions, and deep-path references.

## 1. Core Philosophy
- **Everything is a Value**: Every file and expression evaluates to a `Value`.
- **Expression-Oriented**: No statements, no `return` keywords. Every function body is a single expression.
- **Tagged Type System**: Types are optional, prefix-based metadata that provide contracts without muddying the JSON structure.

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
