# Architecture Overview

> This page is for **contributors** and **deep-integration hosts**:
> code organization, key data structures, extension points, design
> trade-offs.
> If you're reading the rest of the guide as a user, you don't need
> this page.

## Three-layer architecture

```
relon-parser  ──→  relon-analyzer  ──→  relon-evaluator
   (AST)            (side-tables)         (tree-walk)
                          │
                          ▼
                     relon-lsp
                  (IDE diagnostics / go-to / completion)

facade crate: relon  —  exposes evaluate_source / json_from_* to hosts
```

Each layer is a separate crate; downstream crates depend on upstream
ones one-way only.

| Crate | Responsibility | Key exports |
| --- | --- | --- |
| `relon-parser` | Lex + parse → AST. Every `Node` carries a process-wide `NodeId` for cross-layer side-tables | `Node`, `Expr`, `TypeNode`, `Decorator`, `NodeId`, `parse_document` |
| `relon-analyzer` | Four passes (schema / resolve / modules / typecheck) producing the `AnalyzedTree` side-table | `AnalyzedTree`, `SchemaDef`, `ResolvedRef`, `Diagnostic`, `analyze` |
| `relon-evaluator` | Tree-walk evaluation; carries `Context` / `Capabilities` / `Value` / built-in decorators / stdlib | `Context`, `Capabilities`, `Value`, `Evaluator`, `RuntimeError` |
| `relon` (facade) | Stitches parse → analyze → eval; `Projector` controls JSON output shape | `evaluate_source`, `value_from_str`, `json_from_str`, `Error` |
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

## The analyzer's four passes

The execution order is fixed; each pass can read its predecessors'
output:

1. **`schema`**: recognize `#schema Name { ... }` /
   `#schema Name: { ... }` / `#schema Name Enum<...>` and lower them
   to `SchemaDef`. Tagged-enum sum-type variant lists are extracted
   here too.
2. **`resolve`**: bind `Reference` / `Variable` nodes to target
   fields' `NodeId`. Conservative strategy: closure params and dict
   spreads mark the frame as dynamic; references aren't forcibly
   flagged as errors.
3. **`modules`**: scan top-level `#import ... from "..."` directives
   and collect import edges.
4. **`main_sig`**: recognize the root-level
   `#main(Type name, ...) [-> ReturnType]` directive and build
   `MainSignature`.
5. **`typecheck`**: aggregate diagnostics —
   `UnresolvedReference`, `StaticTypeMismatch`,
   `NonExhaustiveMatch`, `UnknownVariant` (with did-you-mean),
   `DuplicateMatchArm`, `HeterogeneousEnum`, `SchemaBodyNotDict`, …

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

The host can register four kinds of objects on `Context`, forming
Relon's plugin surface:

| Interface | Use | Trait / type |
| --- | --- | --- |
| **Native fn** | Let `.relon` call a host-side function | `RelonFunction` + `Context::register_fn` / `register_pure_fn` |
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
- **Performance layer**: bytecode IR + cranelift JIT is a long-term
  goal, to be addressed once correctness and the ecosystem stabilize.
