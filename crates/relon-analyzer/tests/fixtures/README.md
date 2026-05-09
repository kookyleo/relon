# Relon analyzer fixtures

Each subdirectory groups `.relon` source files that exercise a specific
analyzer feature. Files are loaded by integration tests under
`crates/relon-analyzer/tests/`. The naming convention is
`<scenario>.relon`; the file's leading comment block declares its
expected diagnostic outcome (`EXPECTED:` line) so a test driver can
assert on it.

## Layout

- `v1_3/` — v1.3 (`#main` param injection + `#strict` + typed spread /
  dynamic key + `Dict<K,V>` generics + `DuplicateField`).
  - `main_injection/` — `#main` parameter handling across atomic /
    dict / list / variant root bodies.
  - `strict_basic/` — basic `#strict` directive on/off behavior.
  - `strict_propagation/` — multi-module workspaces where `#strict`
    contagion is verified (1 hop, 2 hops, diamond).
  - `typehint_spread/` — typed spread `...<T> e` rules under strict.
  - `typehint_dynkey/` — typed dynamic key `[<T> expr]: v` rules under
    strict.
  - `dict_generics/` — `Dict<K, V>` parser + analyzer coverage. Note
    that bare `Dict` is now rejected by v1.7 `BareGenericContainer`;
    the bare-Dict case is rewritten in this directory to use
    `Dict<String, Int>`.
  - `duplicate_field/` — spread-induced field collisions reported
    regardless of strict mode.
- `v1_4/` — strict-completeness sweep: `path_tail/`, `silent_fallback/`,
  `spread_extension/`.
- `v1_5/` — long-tail closure: `comprehension/`, `where/`,
  `closure_strict/`, `head_unresolved/`, `multi_segment_fncall/`,
  `list_dict_strict/`.
- `v1_6/` — retire `Any` from user space: `ban_any/` (every TypeNode
  position), `stdlib_generic/` (rewritten signatures preserve
  concrete types).
- `v1_7/` — tuple types + ban bare generics: `tuple/` (homogeneous
  fold, heterogeneous typed, arity / per-position checks, nesting),
  `ban_bare/` (`List` / `Dict` / `Closure` / `Fn` / `Enum` without
  generic arguments).
- `v1_8/` — Enum / Result first-class typing + cross-module +
  tuple-index: `enum/` (Enum<...> slot checks each alternative;
  heterogeneous, string-literal, and primitive alts), `result/`
  (Result<T, E> / user sum types substitute generic args into
  variant fields), `cross_module/` (`pkg.SchemaName` slot
  resolution through the workspace import index, with `lib.relon`
  as the imported module), `tuple_index/` (positional access on
  `(Int, String)` tuples and `List<T>` via `pair.0` / `xs.0`).

## Conformance contract

These fixtures double as the language's reference test corpus. Other-
language ports of Relon (Go, TS, Swift, ...) consume them to confirm
their own analyzer reaches the same verdicts.
