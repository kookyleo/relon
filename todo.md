# Relon Development Roadmap

## Phase 2: Core Architecture & Performance (High Priority)

- [x] **Memoized Graph Evaluation (Thunks)**: 
    - [x] Add a conservative path cache for repeated `&reference` resolution.
    - [x] Transition dictionary field evaluation from pure AST tree-walking to a lazy memoized model.
    - [x] Wrap dictionary field AST nodes in `Thunks` (AST + definition scope).
    - [x] Cache results once computed to eliminate redundant calculations in deep reference chains.
- [x] **Dynamic Reference Resolution (Spread Penetration)**:
    - Fix the logical gap where `&references` cannot see keys injected via `...` spread operators.
    - Implement navigation through the evaluated value graph rather than raw AST pairs.
    - Enable complex configuration inheritance patterns (e.g., `&sibling.app.port` where `port` is spread into `app`).

## Phase 3: DX & Ecosystem

- [x] **Serde Deserialization Integration**:
    - [x] Implement a seamless bridge to `serde` so developers can deserialize `.relon` files directly into strong-typed Rust structs (e.g., `let config: MyStruct = relon::from_file("config.relon")?`).
- [x] **Standard Library Expansion**:
    - [x] Add high-frequency utility functions for strings (`split`, `join`, `replace`, `to_upper/lower`).
    - [x] Add dictionary utilities (`merge`, `keys`, `values`, `contains`).
    - [x] Expose deterministic native Rust helpers as built-in functions.
- [ ] **Advanced Validation System**:
    - Refine decorator-based schema enforcement.
    - Support custom validation error messages and multi-field cross-validation.

## Phase 4: Tooling (Future)

- [ ] **LSP (Language Server Protocol)**: Implementation for IDE support (syntax highlighting, auto-completion, ref-navigation).
- [ ] **Formatter/Linter**: A dedicated `relon-fmt` tool.
