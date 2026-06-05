# Host Integration

Relon is not a standalone "install and run" program — it's a
**Rust-embeddable toolkit**. This page covers how to plug it into
your own process: parsing, evaluating, registering native functions,
customizing module resolution, and controlling JSON output.

> Need a security policy for untrusted scripts? Continue to
> [Sandbox & capabilities](./sandbox) after this page.

## Recommended pattern: push by default

Before integrating, lock in one **architectural decision**: how does
external data enter Relon?

The recommended pattern is **push** — the host completes all I/O
**before** evaluation, materializes the data into `Value`, and pushes
it via `Evaluator::run_main(scope, args)`. The script declares the
expected shape via `#main(...)`. The whole thing stays a pure
function `(source, args) → output`:

```rust
// Recommended: push-style, #main entry program
use std::collections::HashMap;
use std::sync::Arc;
use relon_evaluator::{Context, Evaluator, Scope, Value};

let user_data = http_client.get(&format!("/api/user/{user_id}")).await?;
let posts_data = db.query_user_posts(user_id).await?;

// Materialize host-side data into Value
let user_value: Value = serde_json::from_value(user_data)?;
let posts_value: Value = serde_json::from_value(posts_data)?;

let analyzed = relon_analyzer::analyze(&parsed_node);
let mut ctx = Context::sandboxed().with_root(parsed_node);
ctx.analyzed = Some(Arc::new(analyzed));

let mut args = HashMap::new();
args.insert("user".to_string(), user_value);
args.insert("posts".to_string(), posts_value);

let result = Evaluator::new(Arc::new(ctx))
    .run_main(&Arc::new(Scope::default()), args)?;
```

Pair it with a `#main(...)` signature on the script side, describing
the shape the host must push:

```relon
#main(User user, PostList posts)
{
    #schema User { String name: *, String tier: * },
    #schema Post { String title: * },
    #schema PostList List<Post>,
    summary: f"${user.name} has ${len(posts)} posts",
    eligible: len(posts) > 10 && user.tier == "gold"
}
```

`#main(Type name, ...) [-> ReturnType]` is the file's **entry
signature**; each parameter declares one host-pushed slot:

- The parameter name is directly visible at the root scope (note:
  **not** `input.user`, just `user`).
- The parameter type must be a declared `#schema` or a primitive type.
- Before running the body, the runtime validates `args` against the
  signature: missing field → `MissingMainArg`; extra field →
  `UnexpectedMainArg`; type mismatch → `MainArgTypeMismatch`.

> **Compiled backends — structured inputs.** The compiled executors
> (cranelift-native / llvm-native / compiled wasm) decode structured
> `#main` parameters over their buffer protocol, not just scalars. All
> of the following flow through bit-identically to the tree-walk oracle:
>
> - scalar leaves (`Int` / `Float` / `Bool` / `Null`),
> - **`String`** parameters (e.g. file contents the host read and pushes
>   in),
> - **`List<scalar>`**, **`List<String>`**, **`List<Schema>`**, and
>   nested **`List<List<scalar>>`** parameters (consumed through
>   `.length()` / a sibling scalar field read; the inner records — a
>   schema sub-record / an inner list record — are materialised into the
>   buffer's tail and relocated into the parent's coordinate system),
> - **user-`#schema` struct parameters** whose fields are scalars,
>   `String`, `List<scalar>`, `List<String>`, `List<Schema>`, or
>   `List<List<scalar>>` — the whole structured config record, including
>   string, list, list-of-record, and nested-list fields,
> - **nested-`#schema` struct fields** read through a multi-segment walk
>   (`o.inner.x`, and deeper such as `c.b.a.v`). Both field-declaration
>   spellings work — the value-position `inner: Inner` and the prefix
>   `Inner inner: *` — and each intermediate segment rebases to its
>   sub-record before the leaf field read.
>
> On the **return** side the compiled backends marshal the body's output
> `Value` back into the buffer bit-identically to the tree-walk oracle
> for:
>
> - scalar leaves, `String`, `List<scalar>`, and `List<String>` returns,
> - **`#schema`-branded struct returns** (`#main() -> Cfg { ... }`) whose
>   fields are any of the above (including `String` / `List` fields),
> - **anonymous `-> Dict { ... }` returns** — each non-`#internal` field
>   is marshalled into the return record, including `String`,
>   `List<scalar>`, and `List<String>` fields. (`#internal` fields stay
>   off the host-visible surface, matching the oracle.)
>
> Still **not yet** supported on the compiled backends (rejected up
> front with a clear `unsupported type in #main` / `layout v1 does not
> yet support list element` error rather than silently falling back —
> use the tree-walk evaluator for those shapes):
>
> - `Dict<_, _>` parameters (the analyzer cannot type `d["x"]` index
>   reads; use a `#schema` struct for structured config instead),
> - inner pointer-array element lists (`List<List<String>>` /
>   `List<List<Schema>>`) — the recursive per-entry relocation isn't
>   modelled,
> - **returning** a `List<Schema>` / `List<List<…>>` from `#main`, or an
>   anon-`Dict` field of those types (the input decode is supported; the
>   recursive output-store relocation is not — it fails loudly, never
>   returns wrong data).

### Boundary Result vs Relon value-level Result

The host's `run_main` call returns a Rust-side
`Result<Value, RuntimeError>`: on success `Ok(json_value)`; on failure
`Err(...)` (schema validation, runtime overflow, capability denial,
…). This **boundary Result is the Rust side's responsibility** — the
script author doesn't perceive it.

The `ReturnType` in `#main(...) -> ReturnType` describes the **Json
shape the body produces** (an atomic value, dict, or list), not a
Result wrapper. Relon's built-in `Result<T, E>` / `Option<T>` are
**value-level** concepts (modelling "this field may be missing / may
have failed" inside data); they don't belong in the entry signature's
return position.

```relon
// Good: ReturnType describes the body's Json output
#main(Order order) -> Order
{ id: order.id, total: order.total * 1.1 }

// Avoid: writing Result at the entry boundary — duplicates the Rust side's Result
#main(Order order) -> Result<Order, String>
...
```

Host code:

```rust
match evaluator.run_main(&scope, args) {
    Ok(value) => /* value is the Json described by ReturnType */,
    Err(e)    => /* validation / eval / capability error */,
}
```

Push-by-default has several consistent benefits:
- The "external data contract" lives in the `.relon` file, statically
  checked by `#schema`.
- A missing field or type mismatch on the host's push fails before
  evaluation begins.
- Multiple schemas naturally compose into the entry signature (each
  slot is name-isolated).

The contrary — **not recommended** as the default — is:

```rust
// ⚠️ pull-style: I/O moves inside evaluation
ctx.register_fn("http.get",
    NativeFnGate { network: true, ..Default::default() },
    Arc::new(HttpGet),
);
```

```relon
// The script pulls data on its own
{
    user: http.get("/api/user/" + user_id),
    posts: db.query("SELECT * FROM posts WHERE author = " + user.id)
}
```

### Why push is preferred

| Axis | push | pull |
|---|---|---|
| Honors "same source + same input → byte-identical output"? | ✅ args is an explicit `Value` tree, replay / diff / hash-able | ❌ args implicitly include `http.get`'s network state at the time |
| Testing | Construct args | Mock http / db client |
| Caching / pre-compilation / fuzz | Truly pure — memoizable | Any cache is tied to time and external state |
| Auditing "what data does this logic read?" | One look at `#main(...)` | Trace every host-fn reachability |
| Evaluation determinism (spec §1) | ✅ Same args → same result | ❌ Network / external state varies; not replayable |
| Mental partitioning | Host owns I/O across boundaries; script owns data composition | The two intertwine |

### Pull isn't forbidden — it's a "deliberate surrender of determinism"

In these scenarios pull is still reasonable:

- **Lazy loading**: dataset too large to fully push ("filter from 1M
  users").
- **Dynamic queries**: query conditions depend on intermediate
  computations in the script.
- **Side-effect actions**: a rules engine triggers email / log /
  webhook after deciding — side effects are the point.
- **Observability**: a `@log("...")` decorator used for debugging,
  with no effect on the result.

In these cases register the host fn with
[`register_fn`](#capability-gated-registration), declaring the right
bits via `NativeFnGate { reads_clock: true, network: true, ..Default::default() }`
as needed. **It's a deliberate trade**: the script author gives up
"running the same args twice always produces the same result" in
exchange for "can pull data dynamically". Spec §1's determinism
guarantee only covers push.

> **Bottom line**: push when you can. Use pull only when push is
> impractical (data volume, dynamic-ness, side effects), and accept
> that part of the logic is no longer replayable.

## Entry programs vs libraries

Whether a file declares `#main(...)` decides **how it's used**:

| Declaration | Usage | Entry evaluation |
| --- | --- | --- |
| `#main(...)` | Entry program | `Evaluator::run_main(scope, args)`; calling `eval_root` directly raises `NoMainSignature` |
| No `#main` | Pure-data / shared-schema library | `Evaluator::eval_root(scope)` works; the file may also be `#import`-ed |

Library files don't need `#main` when imported — `#import` only takes
their exports. Benefits:
- A clean line between libraries and entries; the host won't
  accidentally run a library as an entry (rejected at the door).
- The entry program's args contract lives in source, so the host
  needs no extra agreement.

## Minimal example

The most common need: "read a `.relon` file, get JSON out." Three
lines:

```rust
use relon;

let json = relon::json_from_file("config/app.relon")?;
println!("{}", serde_json::to_string_pretty(&json)?);
```

If the source is already in memory:

```rust
let json = relon::json_from_str(r#"{ host: "localhost", port: 8080 }"#)?;
```

> The top-level `relon::*` API takes the "no-`#main` library / data
> file" fast path (calls `eval_root` internally). To run an entry
> program with `#main(...)`, use `Evaluator::run_main(...)` directly.

Want a strongly typed Rust struct instead? Use serde:

```rust
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ServerConfig {
    host: String,
    port: u16,
}

let cfg: ServerConfig = relon::from_file("config/app.relon")?;
```

`relon::from_str` / `from_file` are internally `json_from_*` +
`serde_json::from_value`.

## Top-level API at a glance

| Function | Behavior |
| --- | --- |
| `value_from_str(src) -> Value` | Parse + eval, return the Relon in-memory value (may contain closures, schemas, etc. that can't go to JSON directly) |
| `value_from_file(path) -> Value` | Same, from a file |
| `json_from_str(src) -> serde_json::Value` | Eval + project through the default `JsonProjector` to JSON |
| `json_from_file(path) -> serde_json::Value` | Same, from a file |
| `from_str::<T>(src) -> T` | Eval + project + serde-deserialize to a custom type |
| `from_file::<T>(path) -> T` | Same, from a file |
| `analyze_from_str(src) -> AnalyzedTree` | Run **only** parser + analyzer, no evaluation — for LSPs / CI static diagnostics |
| `project_with(&projector, &value) -> P::Output` | Project an already-evaluated `Value` through a custom `Projector` |
| `project_from_str(src, &projector) -> P::Output` | Parse + eval + project in one shot |

## What `Context` is

When you use the top-level `relon::*` API, `Context` is constructed
internally. To register native functions, decorators, custom module
resolvers, or capabilities, build `Context` directly:

```rust
use relon_evaluator::{Context, Evaluator, Scope};
use relon_parser::parse_document;
use std::sync::Arc;

let node = parse_document(source).unwrap();
let mut ctx = Context::sandboxed().with_root(node);

// (register functions / decorators / swap module resolver here)

let value = Evaluator::new(Arc::new(ctx)).eval_root(&Arc::new(Scope::default()))?;
```

`Context` holds:

- **`functions`** — the native-fn table populated by `register_fn`
  (pure fns use the convenience wrapper `register_pure_fn`).
- **`decorators`** — the decorator plugins registered via
  `register_decorator`.
- **`module_resolvers`** — the resolver chain `#import` walks;
  `Context::sandboxed()` defaults to
  `[StdModuleResolver, FilesystemModuleResolver::default()]`.
- **`capabilities`** — sandbox / resource budgets (see
  [Sandbox & capabilities](./sandbox) for details).
- **`root_node`** + **`analyzed`** — the root AST and the analyzer
  side-table (which includes the `#main` signature).
- **Multiple caches** (path / module / loading) — to avoid redundant
  evaluation.

> Historical note: early versions provided
> `Context.input: Option<Value>` and `with_input(value)` as a push
> entry; both have been **removed** — push is now uniformly
> `Evaluator::run_main(scope, args)`. An even earlier
> `Context.globals: HashMap<String, Value>` general-purpose injection
> point was also removed: mixing semantics in one map scattered the
> failure modes. Today there's a single entry point + `#main`
> contract.

There are two construction tracks:

| Constructor | Default safety level |
| --- | --- |
| `Context::sandboxed()` | Fully sandboxed: filesystem denied, all capability bits off, only `std/...` virtual modules survive |
| `Context::new()` | Lightweight base constructor: virtual std modules + built-in pure fns; for real workloads prefer `Context::sandboxed()` with explicit grants |
| `Capabilities::all_granted()` + `FilesystemModuleResolver::trusted()` | The host's own scripts can flip everything on explicitly: filesystem unrestricted, all gated native fns pass, no step / size budget |

## Registering a native function

The most common request: expose a constant or pure function computed
by Rust to `.relon`.

```rust
use relon_evaluator::{Context, NativeArgs, RelonFunction, Value, RuntimeError};
use relon_parser::TokenRange;
use std::sync::Arc;

struct AppVersion;

impl RelonFunction for AppVersion {
    fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
        Ok(Value::String(env!("CARGO_PKG_VERSION").to_string()))
    }
}

let mut ctx = Context::new();
ctx.register_pure_fn("app_version", Arc::new(AppVersion));
```

In `.relon`:

```relon
{
    version: app_version()
}
```

Key points:

- `register_pure_fn` is the convenience wrapper for
  `register_fn(name, NativeFnGate::default(), fn)`: it declares an
  empty gate that any `Capabilities` trivially satisfies, so pure
  functions can be called even under the sandbox.
- `NativeArgs` splits positional and named arguments: `args.get(0)`
  gets a positional arg; `args.get_named("name")` gets a named arg.
- The function returns `Value` — Relon's in-memory value type. To
  build a dict / list use `Value::Dict` / `Value::List`.

## Capability-gated registration

For functions with side effects — file reads, network calls,
environment reads — register them with `register_fn` and set the
corresponding `NativeFnGate` bit:

```rust
use relon_evaluator::{Context, NativeFnGate, NativeArgs, RelonFunction, Value, RuntimeError};
use relon_parser::TokenRange;
use std::sync::Arc;

struct ReadSecret;

impl RelonFunction for ReadSecret {
    fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
        let secret = std::fs::read_to_string("/etc/myapp/secret").unwrap_or_default();
        Ok(Value::String(secret))
    }
}

let mut ctx = Context::sandboxed();
ctx.register_fn(
    "secret.read",
    NativeFnGate { reads_fs: true, ..Default::default() },
    Arc::new(ReadSecret),
);

// How to allow this under the sandbox: grant every bit the gate declared
ctx.capabilities.reads_fs = true;
```

Every native function goes through the same gate check: every bit the
function declares must be granted in `Capabilities`, or
`CapabilityDenied`. Pure fns registered through `register_pure_fn`
declare an empty gate — zero bits required — so they run without any
capability grant. `register_fn(name, gate, fn)` requires explicit
host grants for every bit set in `gate`.
`Capabilities::all_granted()` flips all six bits at once. See
[Sandbox & capabilities](./sandbox).

## Module resolution

`#import <bindspec> from "path"` doesn't read files directly — it
asks each resolver in `Context::module_resolvers` "can you resolve
this path?" The first to return `Some(ModuleSource)` wins; an `Err`
aborts immediately.

The default chain:

1. **`StdModuleResolver`** — resolves virtual modules like
   `std/list`, `std/string` (embedded in the binary, zero IO).
2. **`FilesystemModuleResolver`** — reads from disk:
   - Host-owned scripts can install
     `FilesystemModuleResolver::trusted()` for no root restriction.
   - Under `Context::sandboxed()` the default is
     `FilesystemModuleResolver::default()`, which **denies
     everything** — replace it or append a `with_root_dir(...)`
     instance.

Replacement example:

```rust
use relon_evaluator::{Context, FilesystemModuleResolver, StdModuleResolver};
use std::sync::Arc;

let mut ctx = Context::sandboxed();
ctx.module_resolvers = vec![
    Arc::new(StdModuleResolver),
    Arc::new(FilesystemModuleResolver::with_root_dir("/var/relon-configs")),
];
```

`with_root_dir` canonicalizes the root path and, on every import,
checks that the target resolves under the root (preventing symlink
escapes too) — see
[Sandbox & capabilities](./sandbox#filesystemmoduleresolver-behavior)
for details.

To insert a custom resolver (e.g. "read from memory", "read from an
OCI registry"), implement the `ModuleResolver` trait:

```rust
ctx.prepend_module_resolver(Arc::new(MyResolver)); // run first
// As a fallback: just push to the end of ctx.module_resolvers
ctx.module_resolvers.push(Arc::new(FallbackResolver));
```

## Decorator plugins

**`@name(...)` decorators** are only for value transforms, distinct
from `#name ...` directives (which handle structure / metadata —
see [Syntax basics](./syntax)):

- Built-in: `@value(...)` is the only decorator name the runtime
  provides.
- User-defined: `@my_fn(arg)` is equivalent to passing the value
  below as the last positional argument of `my_fn`. `my_fn` can be a
  closure in the same dict, a function imported via `#import`, or a
  host-registered native fn — any callable binding works.
- Host-registered: implement the `DecoratorPlugin` trait and register
  a name.

```rust
use relon_evaluator::{Context, DecoratorPlugin};
// Trait implementation omitted — all three hooks default to no-op
ctx.register_decorator("my_org.audit", Arc::new(MyAuditPlugin));
```

`DecoratorPlugin` exposes three hooks, all default no-op; override
the ones you need:

| Hook | When it fires | Typical use |
| --- | --- | --- |
| `pre_eval` | Before the decorated node evaluates | Inject scope / override the result outright |
| `wrap` | After the decorated node evaluates | Validation, transformation (e.g. `@ensure.int`) |
| `schema_field_meta` | When extracting a field from a schema dict | Attach metadata to the field |

The full trait signature lives in
`crates/relon-evaluator/src/decorator.rs`; we won't repeat it here —
most hosts only need `wrap`.

## `Projector`: customize JSON output

The default `JsonProjector` projects `Value` to `serde_json::Value`,
with these specifics:

- Closures, schemas, types, wildcards inside a dict are **silently
  dropped** (kept at runtime, not in JSON).
- The same shapes at the top level **raise an error** (no JSON
  representation).
- Non-finite floats (`Infinity` / `NaN`) raise an error.
- Sum-type variants output in **externally tagged** form:
  `{ "Email": { ... } }`.
- Regular branded dicts stay **flat** — a `#schema User`-branded dict
  isn't wrapped in an outer layer.

To use a different shape — e.g. sum types with internally tagged
`{ "type": "Email", "address": "..." }`, or direct BSON / Protobuf —
implement the `Projector` trait:

```rust
use relon::Projector;
use relon_evaluator::Value;

struct InternallyTaggedJson;

#[derive(Debug, thiserror::Error)]
#[error("projection failed: {0}")]
struct ProjErr(String);

impl Projector for InternallyTaggedJson {
    type Output = serde_json::Value;
    type Error = ProjErr;

    fn project(&self, value: &Value) -> Result<Self::Output, Self::Error> {
        // Walk it yourself, inspect brand/variant_of on Value::Dict, rewrite shape...
        todo!()
    }
}

let json = relon::project_from_str(source, &InternallyTaggedJson)?;
```

> **Scope note**: `Projector` is a knob for fine-tuning JSON shape,
> not an escape hatch out of JSON. Relon's output always lands in
> JSON — that's a hard constraint. If you want YAML / TOML / XML,
> that's another tool's job (e.g. Pkl).

## Error types

`relon::Error` is the unified error of the facade crate:

| Variant | Source |
| --- | --- |
| `Error::Parse(String)` | Lexical / syntactic error |
| `Error::Analyze(Vec<Diagnostic>)` | Analyzer errors returned **in batch** (all four passes run together) |
| `Error::Eval(RuntimeError)` | Evaluation-time error: type mismatch, unresolved reference, capability denial, step overrun, … |
| `Error::Io { path, source }` | File-read failure |
| `Error::Deserialize(serde_json::Error)` | `from_str::<T>`-class API deserialization failure |
| `Error::NonFiniteFloat(f64)` | `Infinity` / `NaN` encountered during JSON projection |
| `Error::UnsupportedClosure` / `UnsupportedSchema` | The top-level value is a closure or schema, can't be projected |

Under the sandbox, `RuntimeError` may also be `CapabilityDenied`,
`StepLimitExceeded`, or `ValueTooLarge` — see
[Sandbox & capabilities](./sandbox). Entry programs add
`NoMainSignature` (a library file run as an entry),
`MissingMainArg` / `UnexpectedMainArg` / `MainArgTypeMismatch` (args
don't match the `#main` signature).

## Next

- Security strategy for untrusted scripts:
  [Sandbox & capabilities](./sandbox).
- Make `.relon` use the functions you registered: wrap them inside
  schemas / libraries — see [Types & schema contracts](./types).
- Miette-friendly error formatting: pass `RuntimeError` /
  `Diagnostic` straight to `miette::Report`.
