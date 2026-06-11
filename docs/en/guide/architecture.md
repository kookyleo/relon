# Architecture Overview

> This page is for **contributors** and **deep-integration hosts**:
> code organization, key data structures, extension points, design
> trade-offs.
> If you're reading the rest of the guide as a user, you don't need
> this page.

## Three-layer architecture

```
relon-parser  ──→  relon-analyzer  ──┬─→  relon-evaluator (tree-walk)
   (AST)            (side-tables)    │
                          │          └─→  relon-ir ──→ relon-codegen-cranelift
                          ▼                       └──→ relon-codegen-llvm
                     relon-lsp                       (AOT compiled backends)
                  (IDE diagnostics / go-to / completion)

facade crate: relon  —  exposes from_str / json_from_* / EvaluatorBuilder (Backend::Auto dispatch)
```

Each layer is a separate crate; downstream crates depend on upstream
ones one-way only.

| Crate | Responsibility | Key exports |
| --- | --- | --- |
| `relon-parser` | Lex + parse → AST. Every `Node` carries a process-wide `NodeId` for cross-layer side-tables | `Node`, `Expr`, `TypeNode`, `Decorator`, `NodeId`, `parse_document` |
| `relon-analyzer` | Many passes (schema / extend / main_sig / modules / resolve / typecheck, …) producing the `AnalyzedTree` side-table | `AnalyzedTree`, `SchemaDef`, `ResolvedRef`, `Diagnostic`, `analyze` |
| `relon-evaluator` | Tree-walk evaluation; carries `Context` / `Capabilities` / `Value` / built-in decorators / stdlib | `Context`, `Capabilities`, `Value`, `Evaluator`, `RuntimeError` |
| `relon-ir` + `relon-codegen-cranelift` / `relon-codegen-llvm` | Lower AST + side-tables to IR, then AOT-compile declared supported shapes to native machine code; matched against tree-walk | IR module, per-backend entry points |
| `relon` (facade) | Stitches parse → analyze → eval; `EvaluatorBuilder` selects the backend (default `Backend::Auto`); `Projector` controls JSON output shape | `from_str`, `value_from_str`, `json_from_str`, `EvaluatorBuilder`, `Error` |
| `relon-lsp` | Synchronous lsp-server; reuses analyzer `Diagnostic` and side-tables | binary `relon-lsp` |

## Data flow

```
source string
     │
     ▼ parse_document
AST: Node { id: NodeId, expr, decorators, type_hint, range }
     │
     ▼ analyze
AnalyzedTree {
    schemas:    HashMap<NodeId, SchemaDef>,
    references: HashMap<NodeId, ResolvedRef>,
    node_index: HashMap<NodeId, Arc<Node>>,
    imports:    Vec<ModuleImport>,
    diagnostics: Vec<Diagnostic>,
    is_library: bool,
}
     │
     ▼ Context::with_root(...).with_analyzed(...)
evaluation (tree-walk)
     │
     ▼ Projector
plain JSON
```

`AnalyzedTree` is a **read-only side-table** that doesn't mutate the
AST. The evaluator queries whatever it needs; if an entry is missing,
it falls back (e.g. schemas not pre-lowered get lowered on demand via
`lower_schema_pure`).

The compiled backends run a parallel pipeline: the same AST +
`AnalyzedTree` is lowered to IR by `relon-ir`, then compiled to
native machine code by the cranelift / LLVM backends. Tree-walk stays
the full-surface reference; compiled backends are matched against it on
their declared supported surface and must report the same observable
sandbox errors.

## The analyzer's pass pipeline

The execution order is fixed (entry point `analyze_with_options`);
each pass can read its predecessors' output. Grouped by
responsibility:

1. **Host signature audit**: `audit_host_fn_signatures` — signatures
   the host registered for its native fns go through the same `Any` /
   bare-generic ban, so a misconfigured host integration surfaces
   here.
2. **Built-in carrier injection**: `inject_core_schemas` — installs
   the built-in `String` / `List<T>` / `Dict<K, V>` / `Iter<T>`
   method tables so `s.upper()` dispatches through the same path as
   user-declared methods (skippable via
   `AnalyzeOptions::skip_core_schemas` / CLI `--lite`).
3. **Schema collection**: `collect_schemas` + `collect_root_schemas`
   — recognize `#schema Name { ... }`, `#schema Name: { ... }`,
   `#enum Name { ... }`, and root-level `#schema A Body` forms, and
   convert them to `SchemaDef`. Tagged enum variant lists are
   extracted here too.
4. **Methods and constraints**: `collect_extends`
   (`#extend X with { ... }`), duplicate-method / generic-shadowing
   checks, `#derive` witness shape checking, Equatable /
   JsonProjectable auto-derive, and method-signature-table
   synthesis.
5. **Entry and modules**: `collect_main` (root-level
   `#main(Type name, ...) [-> ReturnType]` → `MainSignature`) and
   `collect_imports` (collect `#import` edges for the workspace
   pass).
6. **Resolution and type checking**: `resolve_references` — bind
   `Reference` / `Variable` nodes to target fields' `NodeId`, with
   the conservative strategy that closure params and dict spreads
   mark the frame as dynamic instead of forcing errors; then
   `typecheck` + `check_main_return` — aggregate diagnostics:
   `UnresolvedReference`, `StaticTypeMismatch`,
   `NonExhaustiveMatch`, `UnknownVariant` (with did-you-mean),
   `DuplicateMatchArm`, `SchemaBodyNotDict`, … Callers can append
   the optional static capability-reachability check
   (`capability_check`, enabled by the compiled backends).

There is also a **trivial-scalar-`#main` short-circuit pipeline**:
when the source is classified as a trivial scalar `#main` shape,
every pass except `collect_main` + `check_main_return` is provably a
no-op, so the analyzer skips them wholesale and produces a
side-table byte-for-byte equivalent to the full pipeline's output —
this is part of `Backend::Auto`'s cold-start path.

Diagnostics have two levels: `Severity::Error` blocks evaluation;
`Severity::Warning` is informational and the evaluator still runs.

## Key invariants

- **Process-wide unique `NodeId`**: assigned by `AtomicU32::fetch_add`;
  used as the side-table key. AST clones don't reallocate the id
  (`Node::PartialEq` skips the id).
- **`Value::Dict` carries `brand: Option<String>`**: a successful
  `#schema` type check stamps the brand. Re-merging triggers a
  re-validation; user code can't bypass the schema.
- **`Value::Dict` carries `variant_of: Option<String>`**: only
  sum-type variants carry it, marking the parent enum's name. The
  `Projector` uses it to decide externally tagged output.
- **JSON output closed-loop**: the default `JsonProjector` silently
  drops runtime-only Values (closures, schemas, EnumSchemas, types,
  wildcards) inside a Dict; `#internal` fields go one step further —
  they never enter `Value::Dict::map`, so the projector can't see
  them. But in a **List**, **Tuple**, or at the document **root**, encountering
  a closure raises `UnsupportedClosure` instead of being silent: a
  list/tuple is a data sequence, and silently dropping an element would
  make indices and length lie. `#internal` / closure filtering /
  `UnsupportedClosure` are three layered defenses: the first two
  hide "things that shouldn't appear" position-by-position; the
  last fires explicitly when silent hiding would change the
  structure.

## Extension points

The host can register six kinds of objects on `Context`, forming
Relon's plugin surface:

| Interface | Use | Trait / type |
| --- | --- | --- |
| **Native fn** | Let `.relon` call a host-side function | `RelonFunction` + `Context::register_fn` / `register_pure_fn` |
| **Native method** | Attach a host implementation to a method declared `#native` on a schema, dispatched by brand (`m.cents_value()`) | `RelonFunction` + `Context::register_method` / `register_pure_method` |
| **Host schema** | Register a host-constructed schema by name for evaluation-time reference | `Context::register_schema` |
| **Decorator plugin** | Author new decorators, plugged into `pre_eval` / `wrap` / `schema_field_meta` | `DecoratorPlugin` + `Context::register_decorator` |
| **Module resolver** | Control `#import ... from "..."` (sandbox, virtual fs, registry) | `ModuleResolver` + `Context::prepend_module_resolver` |
| **Projector** | Adjust JSON output shape (default `JsonProjector`) | `Projector` trait |

See [Host integration](./host-integration).

## Sandbox model

`Capabilities` defines the evaluator's boundary. The struct is
`#[non_exhaustive]`; it falls into two categories:

- **Capability bits (six)**: `reads_fs` / `writes_fs` / `network` /
  `reads_clock` / `reads_env` / `uses_rng`, matching same-named bits
  on `NativeFnGate`. Whatever bits a host fn declares must be
  granted by the host in `Capabilities` for the sandbox to allow
  the call. That's the only authorization path — no by-name
  allowlist, no global short-circuit.
- **Budgets**: `max_steps` (exceeded -> `StepLimitExceeded`),
  `max_value_elements` (list/tuple/dict construction checkpoints).

The filesystem's real enforcement point is the resolver:
`FilesystemModuleResolver` denies by default; `with_root_dir`
restricts to a root; `trusted()` opens everything. `reads_fs` /
`writes_fs` are just capability-layer bits.

`Context::sandboxed()` defaults to denying the filesystem and any
native fn declaring a bit; for "all open" set
`Capabilities::all_granted()` (flips all 6 bits at once) and install
`FilesystemModuleResolver::trusted()`. `Context::new()` is the
lightweight base constructor: virtual std modules + built-in pure
fns only, **not** a "fully trusted" mode.

See [Sandbox & capabilities](./sandbox).

## Design-trade-off log

- **Three crates instead of one**: the cost is some cross-crate
  references; the payoff is that LSPs and the evaluator can
  independently consume the analyzer's side-table without dragging
  in the whole evaluator.
- **Conservative reference resolution**: closure params / dict
  spreads aren't aggressively erroring out, to avoid false
  positives on dynamic features. The cost is some errors deferred
  to runtime.
- **Match exhaustiveness is an error, not a warning**: missing a
  variant on a sum type is a hard `NonExhaustiveMatch` Error —
  errors surface early.
- **Externally tagged JSON over internally tagged**: in memory the
  dict stays flat with a brand; the projector wraps at
  serialization time. Business authors write
  `notification.address` directly, no `notification.Email.address`.
- **Entry / library split keyed on `#main(...)`**: a file with
  `#main` is an entry program that must be driven by
  `run_main(args)` from the host; a file without `#main` can be
  evaluated by `eval_root` directly or imported via `#import`. The
  two paths don't cross — running a library file as an entry
  raises `NoMainSignature` immediately.

## Areas still evolving

- **Cross-language host**: the v1 roadmap is a C ABI cdylib (JSON
  in, JSON out); v2 adds native-fn callbacks; v3 considers PyO3 /
  napi-rs wrappers if there's demand. We do **not** plan
  cross-language type / decorator registration.
- **Stdlib breadth**: currently six modules with about 30 functions;
  `time` / `regex` / `path` / `base64` are on the roadmap.
- **Analyzer inference depth**: today the typechecker's "matched
  expression type inference" only covers Reference chains; more
  complex expressions are skipped.
- **Performance layer**: landed, no longer a long-term goal — the
  Cranelift AOT and LLVM AOT compiled backends exist alongside
  tree-walk, and `Backend::Auto` is the SDK default dispatch
  (trivial-scalar short-circuit + lazy compilation + loud fallback
  on unsupported shapes). What's still evolving is the cold-start
  chain: the `.o` object cache is ready (written on every compile
  and round-trip-verified on load), while executing that object
  directly via dlopen is deferred to a later phase. See
  [Performance & execution backends](./performance.md).
