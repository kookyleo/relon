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
    - [x] Add high-frequency string utilities under `string.*` (`split`, `join`, `replace`, `upper/lower`, `contains`).
    - [x] Add dictionary utilities under `dict.*` (`merge`, `keys`, `values`, `has_key`).
    - [x] Add list utilities under `list.*` (`contains`).
    - [x] Expose deterministic native Rust helpers as built-in functions.
- [x] **Advanced Validation System**:
    - [x] Refine decorator-based schema enforcement.
    - [x] Support custom validation error messages and multi-field cross-validation.

## Phase 4: Core Hardening (High Priority)

- [x] **Unified Parse Pipeline**:
    - [x] Add a public `parse_document` entry point that consumes trailing whitespace/comments and rejects trailing tokens.
    - [x] Route CLI, facade API, module imports, formatter, and tests through the same parse entry point.
- [x] **Runtime Type Safety**:
    - [x] Reject invalid numeric operands instead of silently coercing non-numeric values to `0.0`.
    - [x] Add regression tests for invalid arithmetic, comparisons, and mixed numeric operations.
- [x] **Repository Hygiene**:
    - [x] Remove or correctly wire stale dead code such as `crates/relon-evaluator/src/eval_fns.rs`.
    - [x] Ensure every checked-in Rust source file is compiled by the workspace or intentionally excluded.
- [x] **Import Robustness**:
    - [x] Guarantee `loading_modules` is restored on file-read, parse, and evaluation errors.
    - [x] Canonicalize module paths before caching and cycle detection.
- [x] **Diagnostics & Source Model**:
    - [x] Populate `TokenRange` with line and column, not only byte offsets.
    - [x] Preserve comment/trivia information where tooling needs source-faithful behavior.
    - [x] Improve parser and evaluator errors so CLI and future LSP can show precise diagnostics.
- [x] **Fixture & Golden Tests**:
    - [x] Automatically evaluate `fixtures/` and `examples/` in tests.
    - [x] Add golden outputs for successful fixtures and expected diagnostics for error fixtures.
    - [x] Keep examples formatted by `relon-fmt --check`.
- [x] **Quality Gate**:
    - [x] Make `cargo test` and `cargo clippy --workspace --all-targets -- -D warnings` pass cleanly.
    - [x] Add these checks to the recommended local/CI validation path.

## Phase 5: Standard Library Roadmap (High Priority)

- [x] **Stdlib Scope & Naming Policy**:
    - [x] Treat Python's standard library as a naming and behavior reference, not as a porting target.
    - [x] Keep the core stdlib deterministic and JSON/value oriented: no filesystem, network, clock, random, process, or ambient environment access in the default stdlib.
    - [x] Use readable namespaces with distinct semantics: `ensure.*` validates or fails, `is.*` returns booleans, and data namespaces (`string.*`, `list.*`, `dict.*`, `json.*`, `value.*`, `math.*`) transform or inspect values.
    - [x] Keep only tiny universal helpers as bare built-ins (`len`, `range`, `type`); prefer namespaced APIs for everything else.
    - [x] Prefer `.relon` virtual modules for pure composable helpers; reserve Rust native functions for primitives that need parser/runtime/serde support.
- [ ] **Value & Path Utilities**:
    - [x] Add `std/value.default(value, fallback)` for `null`-only fallback without treating `false`, `0`, or empty collections as missing.
    - [ ] Add `value.get_path(value, path, default?)`, where `path` is a list of keys/indexes such as `["server", "ports", 0]`.
    - [ ] Add `value.has_path(value, path)`, `value.set_path(value, path, new_value)`, and `value.remove_path(value, path)` for structured config rewrites.
    - [x] Add `std/value.equals(a, b)` and `std/value.clone(value)` as pure `.relon` helpers.
- [ ] **Dictionary / Object Utilities**:
    - [x] Add `dict.merge`, `dict.keys`, `dict.values`, and `dict.has_key` native primitives.
    - [x] Add `std/dict` wrappers for `merge`, `keys`, `values`, and `has_key`.
    - [ ] Add `dict.get(dict, key, default?)`.
    - [ ] Add `dict.deep_merge(base, overlay)` for layered configuration overlays.
    - [ ] Add `dict.pick(dict, keys)`, `dict.omit(dict, keys)`, and `dict.rename(dict, from, to)`.
    - [ ] Add `dict.entries(dict)` and `dict.from_entries(entries)` for interop with list transforms.
    - [ ] Add `dict.compact(dict)` to remove `null` values while preserving falsey but meaningful values.
- [ ] **List / Array Utilities**:
    - [x] Add `list.contains` native primitive.
    - [x] Add `std/list.contains`, `std/list.first`, and `std/list.compact`.
    - [ ] Add `list.last`, `list.take`, and `list.drop`.
    - [ ] Add `list.unique`, `list.sort`, `list.reverse`, and `list.flatten`.
    - [x] Add `list.compact(list)` to remove `null` items in `std/list`.
    - [ ] Add `list.any(list, fn)`, `list.all(list, fn)`, and `list.find(list, fn)` if closure arguments stay ergonomic.
    - [ ] Keep `std/list` virtual helpers (`map`, `filter`) aligned with native list naming.
- [ ] **String Utilities**:
    - [x] Add `string.split`, `string.join`, `string.replace`, `string.upper`, `string.lower`, and `string.contains`.
    - [x] Add `std/string` wrappers for `split`, `join`, `replace`, `upper`, `lower`, and `contains`.
    - [ ] Add `string.trim`, `string.trim_start`, and `string.trim_end`.
    - [ ] Add `string.starts_with`, `string.ends_with`, and `string.strip_prefix` / `string.strip_suffix`.
    - [ ] Add `string.to_int`, `string.to_float`, and `string.to_bool` only with strict, explicit parse semantics.
    - [ ] Add `string.matches` only after choosing a deterministic regex engine and clear error model.
- [ ] **JSON Serialization Utilities**:
    - [ ] Add `json.parse(string)` to turn JSON text into Relon values.
    - [ ] Add `json.stringify(value)` for compact deterministic output.
    - [ ] Add `json.pretty(value)` for stable human-readable output.
    - [ ] Add `json.canonical(value)` only if stable object-key ordering becomes a user-facing contract.
- [ ] **Predicates & Validation**:
    - [x] Add readable validation decorators under `ensure.*`.
    - [x] Add predicate helpers `std/is.null`, `std/is.bool`, `std/is.int`, `std/is.float`, `std/is.number`, `std/is.string`, `std/is.list`, and `std/is.dict`.
    - [x] Add `std/is.empty(value)` for strings, lists, and dicts.
    - [ ] Add validators `ensure.number`, `ensure.non_empty`, `ensure.len_at_least`, `ensure.len_at_most`, and `ensure.matches`.
    - [ ] Add structure validators `ensure.has_key`, `ensure.has_path`, and `ensure.only_keys` for config shape checks.
- [ ] **Math & Numeric Utilities**:
    - [x] Keep `std/math` helpers such as `abs`, `min`, and `max`.
    - [x] Add `std/math.clamp(value, min, max)`.
    - [ ] Add `math.floor`, `math.ceil`, and `math.round` with explicit int/float return semantics.
    - [ ] Add `math.sum(list)` and `math.mean(list)` only after numeric error behavior is fully consistent.
- [ ] **Deferred / Non-Core Capabilities**:
    - [ ] Keep `env.*`, filesystem access, process execution, current time, random values, HTTP, URL fetching, and secrets out of the default stdlib.
    - [ ] Revisit those as explicit host-provided capability modules after the deterministic JSON/config stdlib is stable.

## Phase 6: Tooling (Future)

- [ ] **LSP (Language Server Protocol)**: Implementation for IDE support (syntax highlighting, auto-completion, ref-navigation).
- [x] **Formatter/Linter**: A dedicated `relon-fmt` tool.
