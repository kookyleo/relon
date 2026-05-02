# Relon Design Document (V1.2)

Relon is a programmable configuration language designed to be a functional super-set of JSON. It enables dynamic data generation through meta-decorators, unified closures, and deep-path references.

## 1. Core Philosophy
- **Everything is a Value**: Every file and expression evaluates to a `Value`.
- **Relon-in-Relon**: The language is self-descriptive. Logic is defined using meta-decorators (`@fn`).
- **Unified Logic**: Functions and Decorators share the same underlying closure mechanism.

## 2. Language Features

### 2.1 The Unified `@fn`
Define logic once, use it everywhere:
```javascript
@fn(val, offset)
"add_offset": val + offset
```
- **As Function**: `add_offset(10, 5)` -> `15`
- **As Decorator**: `@add_offset(5) 10` -> `15` (The decorated value is implicitly passed as the first argument).

### 2.2 Advanced Data Transformation
- **Pipe (`|`)**: Sequential data processing: `[1, 2, 3] | len()`.
- **Spread (`...`)**: Merging Lists and Dicts: `{ ...base, "extra": true }`.
- **Comprehensions**: Powerful iteration: `[x * 2 for x in range(5) if x % 2 == 0]`.

### 2.3 Modular System
- **`@import("path", as="alias")`**: Namespace-protected import.
- **`@import("path", spread=true)`**: Flat import (merges into current scope).
- **`std/` Virtual Path**: Built-in standard library (e.g., `std/math`, `std/list`).

## 3. Architecture
- **`relon-parser`**: A modern recursive-descent parser built on `winnow`. It outputs a tree-based AST (`Node`/`Expr`).
- **`relon-evaluator`**: A path-aware tree-walking interpreter with cycle detection and scope pre-scanning.

## 4. Engineering Standards
- **Strict English**: All code and internal documentation are in English.
- **Private Fields**: Keys starting with `_` are used for internal logic and hidden in final JSON output.
