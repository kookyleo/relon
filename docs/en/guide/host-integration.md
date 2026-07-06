# Host Integration

Relon is not a standalone "install and run" program — it's a
**Rust-embeddable toolkit**. This page covers how to plug it into
your own process: parsing, evaluating, registering native functions,
customizing module resolution, and controlling JSON output.

> Need a security policy for untrusted scripts? Continue to
> [Threat model](./threat-model) and [Sandbox & capabilities](./sandbox)
> after this page.

## First-release embedding paths

Choose one path up front:

| Path | Use when | Backend / trust posture |
| --- | --- | --- |
| Sandboxed facade | The host treats Relon as computed config and pushes all data in. | `relon::from_str` / `EvaluatorBuilder` defaults: sandboxed posture, no local imports or staged host fns. |
| Trusted host-owned script | The host owns the source and needs local imports or staged native fns. | `Backend::TreeWalk` plus explicit trust/capability grants. This is the only first-release path for staged host fn registration. |
| Native performance path | The host wants compiled execution for compatible `#main(...)` programs. | `Backend::Auto` or an explicit compiled backend, without staged host fns. `Backend::Auto + TrustLevel::Trusted` is rejected in the first public release. |

For untrusted plugins, tenants, or uploaded scripts, keep the Relon
source behind a VM/process/container boundary. Relon provides the
capability vocabulary and budget model; Wasmtime/process/container
limits enforce the hard boundary.

## Recommended pattern: push by default

Before integrating, lock in one **architectural decision**: how does
external data enter Relon?

The recommended pattern is **push** — the host completes all I/O
**before** evaluation, materializes the data into `Value`, and pushes
it via `run_main(args)`. The script declares the
expected shape via `#main(...)`. The whole thing stays a pure
function `(source, args) → output`:

```rust
// Recommended: push-style, #main entry program
use std::collections::HashMap;
use relon::{Backend, EvaluatorBuilder, TrustLevel, Value};

let user_data = http_client.get(&format!("/api/user/{user_id}")).await?;
let posts_data = db.query_user_posts(user_id).await?;

// Materialize host-side data into Value
let user_value: Value = serde_json::from_value(user_data)?;
let posts_value: Value = serde_json::from_value(posts_data)?;

let evaluator = EvaluatorBuilder::from_str(source)
    .backend(Backend::Auto)          // the default; auto-dispatches between interpreter and AOT
    .trust(TrustLevel::Sandboxed)    // the default; spell the trust posture out
    .build()?;

let mut args = HashMap::new();
args.insert("user".to_string(), user_value);
args.insert("posts".to_string(), posts_value);

let result = evaluator.run_main(args)?;
```

`serde_json::from_value::<Value>` is targetless: JSON arrays decode as
`Value::List`, and JSON `null` is rejected because `null` is not a Relon
value. When writing a Rust host directly, construct `Value::tuple(...)` for
`#main` tuple parameters and `Value::variant_dict(...)` for enum parameters,
or decode with the `#main` signature. Only a known `Option<T>` target
maps JSON `null` to `None`; non-null input for `Option<T>` is decoded as
`Some(value)`.

The CLI path (`relon run --args '<json>'`) and WASM playground `#main(args)`
already read the entry signature: JSON arrays become `Value::Tuple` for
`Tuple<...>` or tuple-schema targets, remain `Value::List` for `List<T>`
targets before list element validation, and scalar targets reject incompatible
JSON shapes. Enum parameters accept JSON strings for matching unit variants,
and externally tagged objects for payload variants. For example,
`#enum Stat { Up, Down }` with a `Stat` parameter accepts `{ "s": "Up" }`;
`#enum Msg { Email { address: String }, Pair(Int, String) }` accepts
`{ "m": { "Email": { "address": "x@y.z" } } }` and
`{ "m": { "Pair": [7, "x"] } }`. `Option<Int>` accepts `null`, `41`, or
`{ "x": { "Some": { "value": 41 } } }`; `Result<Int, String>` accepts
`{ "r": { "Ok": { "value": 41 } } }` or `{ "r": { "Err": { "error": "bad" } } }`. Payload
variants do not decode from a bare string.

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

> **Compiled paths — structured inputs.** The native compiled executors
> (cranelift / LLVM) and the compiled-wasm parity target decode
> structured `#main` parameters over their buffer protocol, not just
> scalars. All of the following flow through bit-identically to the
> tree-walk oracle:
>
> - scalar leaves (`Int` / `Float` / `Bool`),
> - **`String`** parameters (e.g. file contents the host read and pushes
>   in),
> - **tuple schema** parameters such as
>   `#schema IPv4 (Int, Int, Int, Int)`, supplied by the host as
>   `Value::Tuple` and decoded positionally,
> - **`List<scalar>`**, **`List<String>`**, **`List<Schema>`**, nested
>   **`List<List<scalar>>`**, and the doubly-nested pointer-array
>   **`List<List<String>>`** / **`List<List<Schema>>`** parameters
>   (consumed through `.length()` / a sibling scalar field read; the inner
>   records — a schema sub-record, an inner string/scalar list record, or
>   an inner pointer-array list — are materialised into the buffer's tail
>   and recursively relocated into the parent's coordinate system),
> - **user-`#schema` struct parameters** whose fields are scalars,
>   `String`, `List<scalar>`, `List<String>`, `List<Schema>`,
>   `List<List<scalar>>`, or the doubly-nested `List<List<String>>` /
>   `List<List<Schema>>` — the whole structured config record, including
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
> - scalar leaves, `String`, and `List<scalar>` (`List<Int/Float/Bool>`)
>   returns — including identity-returning a scalar-list `#main`
>   parameter, whose tail record is a single inline-fixed block,
> - **tuple schema returns** (`#main() -> IPv4 = (127, 0, 0, 1)`) decode
>   to `Value::Tuple` and project to JSON arrays,
> - `List<String>` returns **sourced from an in-source list literal**
>   (`["a", "b", …]`) — a const-pool block whose inner string pointers
>   are contiguous and single-base, so the rigid tail copy relocates them
>   correctly,
> - **`List<List<scalar>>` identity returns from a `#main` parameter**
>   (`#main(List<List<Int|Float|Bool>> xss) -> List<List<…>> = xss`), on
>   **cranelift, llvm, and the compiled-wasm parity target.** This is the first
>   shape carried by the *in-place region-walk return ABI*: instead of
>   copying the nested pointer-array graph, the machine code reports the
>   arena offset of the result root and the host verifies + decodes it in
>   place in its source region (see the design note below); the native
>   compiled paths and wasm target share one host decode pipeline. On wasm
>   the host reads the same arena
>   straight out of the module's **linear memory** and runs the same
>   verifier before decoding (four-way bit-equal: tree-walk == cranelift ==
>   llvm == wasm). A parameter-*field* `List<List<scalar>>`
>   (`#main(W w) -> List<List<Int>> = w.rows`) is **also supported** on the
>   same compiled routes (F4): under the arena-absolute slot convention the field load
>   pushes the field list root's arena-absolute offset directly, so it
>   rides the same in-place return as the identity case,
> - **`List<String>` identity returns from a `#main` parameter**
>   (`#main(List<String> ss) -> List<String> = ss`), on **cranelift,
>   llvm, and the compiled-wasm parity target.** This is the first *per-element
>   pointer-array* shape carried by the in-place region-walk return ABI
>   (the formation that previously segfaulted under the rigid tail copy):
>   the outer `[len][off_i]` header and each `off_i`'s `[len][utf8]` String
>   record live in the input region, so the machine code reports the root
>   offset and the host verifier walks every per-entry String record
>   in-region before decoding it in place — bit-equal to the tree-walk
>   oracle including each string's bytes (CJK / emoji / 4 KiB strings
>   included, and on wasm read out of linear memory through the same
>   verifier). A parameter-*field* `List<String>`
>   (`#main(Outer o) -> List<String> = o.tags`) is **also supported** on
>   the same compiled routes (F4) via the same arena-absolute field-load,
> - **`List<Schema>` identity returns from a `#main` parameter**
>   (`#main(List<Cfg> items) -> List<Cfg> = items`), on **cranelift,
>   llvm, and the compiled-wasm parity target.** This is the deepest in-place
>   region-walk shape: the outer `[len][off_i]` header points at per-element
>   schema sub-records, each of which itself carries `String` /
>   `List<scalar>` / `List<String>` pointer fields (plus inline scalars at
>   varied offsets).
>   The machine code reports the root offset and the host verifier recurses
>   to **every sub-record field pointer** (each entry → its sub-record's
>   fixed area → each String / List field's tail record) before decoding
>   each element into a branded dict in place — bit-equal to the tree-walk
>   oracle including every sub-object's field bytes, on wasm too (the same
>   recursion runs over linear memory). A parameter-*field*
>   `List<Schema>` (`#main(W w) -> List<Cfg> = w.items`) is **also
>   supported** on the same compiled routes (F4) via the same arena-absolute field-load.
>   An element sub-record that itself carries a nested `List<Schema>` /
>   `List<List<…>>` field (e.g. `Team { name: String, members: List<Person>,
>   tags: List<List<Int>> }`) is **also supported, recursively to any depth**
>   (F7): the in-place sub-record reader recurses through the same unified
>   list reader and the IR conversion admission walks the element schema's field
>   types recursively, so nested object arrays and nested lists inside the
>   element schema decode bit-equal at any nesting depth,
> - **deep nested-schema field-walk returns** (`#main(Outer o) ->
>   List<String> = o.inner.tags`, and deeper such as `o.a.b.tags`), on
>   **cranelift, llvm, and the compiled-wasm parity target** (F6). A `≥3`-segment chain
>   whose intermediate segments are nested-schema fields and whose leaf is a
>   pointer-array list (`List<String>` / `List<Int|Float|Bool>` /
>   `List<Schema>` / `List<List<scalar>>`) loads each intermediate
>   sub-record's arena-absolute base, then reads the leaf list root's
>   arena-absolute offset off that base — the same single-root sentinel +
>   multi-region verifier + reader the single-segment walk uses, bit-equal to
>   the tree-walk oracle at any depth. Supported as a top-level return and as
>   an object field (anon-`Dict` / branded struct),
> - **`List<List<String>>` / `List<List<Schema>>` returns from a `#main`
>   parameter** (`#main(List<List<String>> xss) -> List<List<String>> = xss`,
>   and the `List<List<Cfg>>` form), on **cranelift, llvm, and the
>   compiled-wasm parity target** (F5). This is the *doubly-nested* pointer-array
>   shape: the outer `[len][off_i]` header points at inner pointer-array list
>   records, each of which is itself a `[len][inner_off_j]` header whose
>   entries name `String` / schema-sub-record records. The recursive input
>   marshaller writes the whole graph, the relocation walker rebases the
>   inner pointer arrays one level deeper, and the machine code reports the
>   outer root offset; the host verifier recurses to **every innermost
>   record** (outer entry → inner list header → inner entry → String / schema
>   record) before decoding in place — bit-equal to the tree-walk oracle
>   including every inner element's bytes (CJK / empty / long), on wasm too.
>   Supported as a parameter **identity**, a parameter **field** walk
>   (`#main(W w) -> List<List<String>> = w.rows`), and as an object field
>   (anon-`Dict` / branded struct),
> - **`#schema`-branded struct returns** (`#main() -> Cfg { ... }`) whose
>   fields are supported struct-field shapes from the list above
>   (including literal `String` / `List` fields),
> - **anonymous `-> Dict { ... }` returns** — each non-`#internal` field
>   is marshalled into the return record, including `String`,
>   `List<scalar>`, and literal `List<String>` fields. (`#internal` fields
>   stay off the host-visible surface, matching the oracle.)
>
> Still **not yet** supported on the compiled backends (rejected up
> front with a clear `unsupported type in #main` / `layout v1 does not
> yet support list element` error rather than silently falling back —
> use the tree-walk evaluator for those shapes):
>
> - `Dict<_, _>` parameters (the analyzer cannot type `d["x"]` index
>   reads; use a `#schema` struct for structured config instead).
>   Nested list / object-array / nested-schema return shapes (identity,
>   parameter field, deep field-walk, and object field) are supported
>   four-way at any depth (F7); tuple-return caps are listed separately
>   below.
> - tuple returns outside the scalar/literal envelope: nested tuple
>   elements, `List<...>` / `Option<...>` / `Result<...>` tuple elements, or a tuple return body
>   that is not a tuple literal. These stay loud caps until the positional
>   tuple-element work is proven four-way.
>
>   An **object field** sourced by a parameter — whether the object
>   is an **anon-`Dict`** (`-> Dict { servers: servers, n: 1 }`) or a
>   **branded `#schema` struct** (`#schema Wrapper { servers: List<Server>,
>   n: Int }` returned via `-> Wrapper { servers: servers, n: 7 }`) — is
>   supported **four-way** (tree-walk == cranelift == llvm == compiled wasm target).
>   The field may be `List<Schema>`, `List<List<scalar>>`, `List<String>`,
>   or `List<Int|Float|Bool>` (F1b/F2 on cranelift/llvm/wasm for
>   `List<Schema>` / `List<List<scalar>>`; F3 added the branded-struct path
>   and the scalar/String list field types on all four). The source may be a
>   parameter **identity** (`servers`) **or** — F4 — a parameter **field**
>   walk (`o.items`, `o.tags`): both land the field list root's
>   arena-absolute offset in the slot. The object header
>   is built in `out_buf`, but the parameter-sourced field's data lives in
>   `in_buf` — a genuine **cross-region** field pointer. Under the
>   arena-absolute slot convention the field slot stores the parameter list
>   root's arena-absolute offset directly (no copy — note this is distinct
>   from an in-source list **literal** field, e.g. `tags: ["a", "b"]`, which
>   is copied into the `out_buf` tail and is self-contained there); before
>   any decode the host runs the **multi-region** object verifier over the
>   whole arena anchored at `out_ptr`, which classifies the slot pointer into
>   the input region and bounds-checks the entire reachable graph (down to
>   every sub-record String field), then `BufferReader::new_at_base` follows
>   it cross-region — bit-equal to the tree-walk oracle. On wasm the host
>   reads the same arena out of linear memory and runs the same
>   verifier-gated decode, so there is no wasm-specific path. The
>   doubly-nested `List<List<Schema>>` / `List<List<String>>` object field
>   is **also supported** (F5): the inner pointer arrays are relocated,
>   verified, and read one level deeper. The host-side
>   *decode* is in
>   place — `BufferReader` walks the buffer with a single base and
>   reconstructs the nested `Value` recursively (`read_list_record` /
>   `read_list_record_at` for `List<Schema>`, `read_list_list` /
>   `read_list_list_at` for `List<List<scalar>>`, `read_list_string_at`
>   for an in-place `List<String>`). The in-place return wiring covers
>   the *per-element pointer-array* `List<String>` and `List<Schema>`
>   shapes from a parameter **identity**, a parameter **field** walk
>   (above), and object fields; doubly-nested `List<List<String>>` /
>   `List<List<Schema>>` are covered by the same verifier-gated path.
> - **returning a pointer-array list sourced from a `#main` parameter call
>   / arbitrary expression** (rather than a parameter identity, a parameter
>   field walk, or an in-source literal) — such a value is not proven
>   bit-equal for an in-place return, so it stays a loud cap.
>
> All of the above fail **loudly** at compile time; the compiled
> backends never return wrong data or crash for them — route those shapes
> through the tree-walk evaluator.

> [!NOTE]
> **Why the output store is the hard half.** The
> arena is one contiguous block: `[const_data | in_buf @ in_ptr | out_buf
> @ out_ptr | scratch]`. Inside the running machine code every pointer is
> *arena-base-relative* (`arena_base + ptr` dereferences it), so a param
> graph in `in_buf` and a const-pool literal share one coordinate system.
> For a normal return the host decodes by handing `BufferReader` the
> **out_buf slice** — so a returned pointer is read as *out_buf-relative*.
> A param-identity pointer-array return (`#main(List<P> xs) -> List<P> =
> xs`) instead lives entirely in `in_buf` with `in_buf`-relative inner
> offsets; the old return path tried to *copy* that graph into `out_buf`
> with a single rigid delta, which only works for a contiguous,
> single-base const-pool block and segfaulted on the scattered param
> graph.
>
> **In-place region-walk return ABI (the honest fix; `List<List<scalar>>`,
> `List<String>`, and `List<Schema>` parameter-identity now landed on both
> the cranelift and llvm backends).** Instead of copying, the machine
> code reports the **arena-absolute offset of the result root** to the
> host via a negative return sentinel: a `run_main` return value `>= 0` is
> the usual `bytes_written` (decode at `out_ptr`), while a value `< 0`
> encodes `-(root_abs + 1)` — "the return is in place; its root header is
> at arena offset `root_abs`." The host then:
>
> 1. **selects the region** `root_abs` falls in by comparing it against
>    the arena layout boundaries (`const_data` / `in_buf` / `out_buf` /
>    `scratch`) — the single-region invariant guarantees the value is
>    self-contained in exactly one, so a value's inner offsets are
>    region-relative inside that region's slice;
> 2. **runs the bounds verifier** (`verifier::verify_value_at`) over the
>    whole reachable graph, confined to that region. A pointer that
>    escapes the region — or any length / offset that runs off the end —
>    is a **loud error**, never a wild read. This is the gate: the host
>    does not decode an unverified in-place return;
> 3. only on a clean verify **decodes in place** via the same
>    `BufferReader` the out_buf path uses (`read_list_list_at` for a
>    nested-list root, `read_list_string_at` for a `List<String>` root,
>    `read_list_record_at` for a `List<Schema>` root — each sub-record
>    drained into a branded dict), against the region slice the verifier
>    certified.
>
> This keeps the load-bearing single-region wall intact (no cross-region
> copy, no whole-buffer rigid relocation) and turns the entire class of
> "wrong base / scattered graph" bugs from *silent miscompile* into
> *explicit verifier failure*. The host decode pipeline (sentinel →
> region-select → verifier → decode) lives once in
> `relon_eval_api::inplace_return` and is shared by both AOT backends, so
> cranelift and llvm walk the exact same gate. The reader, verifier, and
> the `List<List<scalar>>`, `List<String>`, and `List<Schema>`
> parameter-identity cases on **both** native backends are wired (the
> verifier recurses through every `List<Schema>` sub-record field pointer);
> the remaining work is extending the same ABI to object-field returns of
> these shapes and wasm linear memory — each landed only once it is proven
> bit-equal to the oracle.

### Boundary Result vs Relon value-level Result

The host's `run_main` call returns a Rust-side
`Result<Value, RuntimeError>`: on success `Ok(value)`; on failure
`Err(...)` (schema validation, runtime overflow, capability denial,
…). This **boundary Result is the Rust side's responsibility** — the
script author doesn't perceive it.

The `ReturnType` in `#main(...) -> ReturnType` describes the **JSON
shape the body produces** (an atomic value, dict, list, or tuple), not a
Result wrapper. Relon's built-in `Result<T, E>` / `Option<T>` are
**value-level** concepts (modelling "this field may be missing / may
have failed" inside data); they don't belong in the entry signature's
return position.

```relon
// Good: ReturnType describes the body's JSON output
#main(Order order) -> Order
{ id: order.id, total: order.total * 1.1 }

// Avoid: writing Result at the entry boundary — duplicates the Rust side's Result
#main(Order order) -> Result<Order, String>
...
```

Host code:

```rust
match evaluator.run_main(args) {
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
let mut gate = NativeFnGate::default();
gate.require(CapabilityBit::Network);
ctx.register_fn("http.get", gate, Arc::new(HttpGet));
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
bits on a `NativeFnGate` via `gate.require(CapabilityBit::Network)`
etc. as needed. **It's a deliberate trade**: the script author gives up
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
| `#main(...)` | Entry program | Push args via `run_main(args)`; `eval_root` does **not** check for `#main` — it evaluates the root expression as usual, with the parameters unbound (referencing them fails as undefined names) |
| No `#main` | Pure-data / shared-schema library | `eval_root(scope)` works; the file may also be `#import`-ed; calling `run_main` on it raises `NoMainSignature` |

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
> program with `#main(...)`, build with `EvaluatorBuilder` and call
> `run_main(args)`.

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

Every evaluating entry point above runs in the **sandboxed posture**
(filesystem `#import` denied, gated native fns blocked). Each has a
matching `*_trusted` variant — `from_str_trusted` /
`from_file_trusted` / `json_from_str_trusted` /
`json_from_file_trusted` / `value_from_str_trusted` /
`value_from_file_trusted` / `project_from_str_trusted` — that
evaluates in the trusted posture (equivalent to
`TrustLevel::Trusted`: filesystem `#import` allowed), intended
**only** for host-owned scripts.

### `EvaluatorBuilder`: pick a backend, a trust posture, host fns

When you need more control than "one line to JSON" (choosing the
execution backend, running a `#main` entry, registering native fns),
use the facade's `EvaluatorBuilder`:

```rust
use relon::{Backend, EvaluatorBuilder, ResourceBudget, TrustLevel};

let evaluator = EvaluatorBuilder::from_str(source)   // or from_file(path)
    .backend(Backend::Auto)        // Auto (default) / TreeWalk / CraneliftAot / LlvmAot
    .trust(TrustLevel::Sandboxed)  // Sandboxed (default) / Trusted
    .build()?;                     // -> Box<dyn relon::Evaluator>

let json_value = evaluator.eval_root(&Arc::new(relon::Scope::default()))?;
// or, for an entry program: evaluator.run_main(args)?
```

- `Backend::Auto` (the default) short-circuits trivial scalar `#main`
  programs to the tree-walker, lazily compiles everything else with
  cranelift AOT, and falls back loudly to the tree-walker for shapes
  the compiler doesn't support — see [Performance](./performance).
- `Backend::Auto + TrustLevel::Trusted` is not wired in the first public
  release; the builder rejects it instead of guessing. Use
  `Backend::TreeWalk` for trusted local imports or staged host fns, or
  select an explicit compiled backend for host-owned sources that do not
  need staged host fns.
- `register_native_fn(name, gate, fn)` / `register_pure_native_fn`
  are tree-walker-only in the current builder surface. Use
  `Backend::TreeWalk` when staging host fns; `Backend::Auto` /
  `CraneliftAot` / `LlvmAot` fail loudly instead of ignoring them.
- `.grant(CapabilityBit)` adds one capability bit on top of the
  `Sandboxed` baseline — the least-privilege path for capability-gated
  host fns (see
  [Capability-gated registration](#capability-gated-registration)).
  Grants follow the same backend rule as host fns: `Backend::TreeWalk`
  only, other backends fail loudly at `build()`. Under
  `TrustLevel::Trusted` every bit is already granted, so extra
  `.grant` calls are accepted as redundant no-ops (the effective grant
  set is the union).
- `max_source_bytes(n)` rejects source above `n` bytes before parsing.
  This is the parser/input guardrail; it is separate from evaluator
  step/value budgets.
- `resource_budget(ResourceBudget::dev())` / `ResourceBudget::untrusted()`
  installs evaluator-side step/value guardrails. In the initial API this
  requires `Backend::TreeWalk`; other backends fail loudly instead of
  ignoring the budget. Hard untrusted execution should use a wasm runtime
  and engine-level limits. Use
  `relon host-policy --target wasmtime --profile untrusted` for the
  Wasmtime starting point; see
  [Threat model](./threat-model) and
  [Wasmtime host policy](./wasmtime-host-policy).

```rust
let guarded = EvaluatorBuilder::from_str(source)
    .backend(Backend::TreeWalk)
    .trust(TrustLevel::Sandboxed)
    .max_source_bytes(256 * 1024)
    .resource_budget(ResourceBudget::untrusted())
    .build()?;
```

## What `Context` is

When you use the top-level `relon::*` API or `EvaluatorBuilder`,
`Context` is constructed internally. Trust posture, per-bit
capability grants (`.grant(CapabilityBit)`), native-fn registration,
and resource budgets are all covered by the builder, so most hosts
never touch `Context`. Building it directly is usually not needed;
reach for it only for the knobs the builder does not carry yet —
registering decorators or mounting a custom module-resolver chain —
by handing it to the concrete backend type `TreeWalkEvaluator`
(`Evaluator` is the trait all backends share):

```rust
use relon_evaluator::{Context, Scope, TreeWalkEvaluator};
use relon_parser::parse_document;
use std::sync::Arc;

let node = parse_document(source).unwrap();
let mut ctx = Context::sandboxed().with_root(node);

// (register functions / decorators / swap module resolver here)

let value = TreeWalkEvaluator::new(Arc::new(ctx))
    .eval_root(&Arc::new(Scope::default()))?;
```

`Context` holds:

- **`functions`** — the native-fn table populated by `register_fn`
  (pure fns use the convenience wrapper `register_pure_fn`).
- **`decorators`** — the decorator plugins registered via
  `register_decorator`.
- **`module_resolvers`** — the resolver chain `#import` walks;
  `Context::sandboxed()` defaults to
  `[StdModuleResolver, FilesystemModuleResolver::default()]`.
- **`capabilities`** — granted host authority bits (see
  [Sandbox & capabilities](./sandbox) for details).
- **resource budgets** — currently bridged through `ResourceBudget` into the
  evaluator compatibility fields on `Capabilities`.
- **`root_node`** + **`analyzed`** — the root AST and the analyzer
  side-table (which includes the `#main` signature).
- **Multiple caches** (path / module / loading) — to avoid redundant
  evaluation.

> Historical note: early versions provided
> `Context.input: Option<Value>` and `with_input(value)` as a push
> entry; both have been **removed** — push is now uniformly
> `run_main(args)`. An even earlier
> `Context.globals: HashMap<String, Value>` general-purpose injection
> point was also removed: mixing semantics in one map scattered the
> failure modes. Today there's a single entry point + `#main`
> contract.

There are two construction tracks:

| Constructor | Default safety level |
| --- | --- |
| `Context::sandboxed()` | Sandboxed posture: filesystem denied, all capability bits off, only `std/...` virtual modules survive; not a tenant boundary by itself |
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
        Ok(Value::String(env!("CARGO_PKG_VERSION").into()))
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
  build a dict / list / tuple use `Value::Dict` / `Value::List` /
  `Value::tuple(...)`.

## Capability-gated registration

For functions with side effects — file reads, network calls,
environment reads — declare the corresponding bit on the fn's
`NativeFnGate`, and grant exactly that bit at construction time with
`EvaluatorBuilder::grant`. The evaluator stays sandboxed; only the
named authority is added on top:

```rust
use relon::{Backend, CapabilityBit, EvaluatorBuilder, NativeFnGate, TrustLevel};
use relon_eval_api::{NativeArgs, RelonFunction, RuntimeError, Value};
use relon_parser::TokenRange;
use std::sync::Arc;

struct ReadSecret;

impl RelonFunction for ReadSecret {
    fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
        let secret = std::fs::read_to_string("/etc/myapp/secret").unwrap_or_default();
        Ok(Value::String(secret.into()))
    }
}

// The fn declares what it needs…
let mut gate = NativeFnGate::default();
gate.require(CapabilityBit::ReadsFs);

// …and the host grants exactly that bit on the sandboxed baseline.
let evaluator = EvaluatorBuilder::from_str(source)
    .backend(Backend::TreeWalk)
    .trust(TrustLevel::Sandboxed)          // default; shown for clarity
    .grant(CapabilityBit::ReadsFs)         // least privilege: this bit only
    .register_native_fn("secret.read", gate, Arc::new(ReadSecret))
    .build()?;
```

Notes on `.grant`:

- Grants are strictly additive on the `Sandboxed` baseline: nothing
  else widens. In particular, `grant(CapabilityBit::ReadsFs)` does
  **not** enable filesystem `#import` — the sandboxed module-resolver
  chain still denies filesystem paths; the bit only satisfies
  native-fn gates.
- Under `TrustLevel::Trusted` every bit is already granted, so extra
  `.grant` calls are redundant no-ops (accepted, not rejected — the
  requested authority is in effect either way).
- Grants are applied by `Backend::TreeWalk` only; staging one under
  `Backend::Auto` or a compiled backend makes `build()` fail loudly
  instead of silently ignoring it.

The same wiring is possible without the builder by assembling
`Context` directly — usually not needed now that the builder carries
per-bit grants, but still legitimate when you are already deep in the
lower-level crates (e.g. combining grants with custom decorators or
resolver chains):

```rust
use relon_evaluator::{Capabilities, CapabilityBit, Context, NativeFnGate};
use std::sync::Arc;

let mut caps = Capabilities::default();
caps.grant(CapabilityBit::ReadsFs);

let mut gate = NativeFnGate::default();
gate.require(CapabilityBit::ReadsFs);

let mut ctx = Context::sandboxed().with_capabilities(caps);
ctx.register_fn("secret.read", gate, Arc::new(ReadSecret));
```

Every native function goes through the same gate check regardless of
which registration path it came in on (builder or `Context`): every
bit the function declares must be granted in `Capabilities`, or the
call fails with `CapabilityDenied`. Pure fns registered through
`register_pure_fn` / `register_pure_native_fn` declare an empty gate
— zero bits required — so they run without any capability grant.
Gated registration requires explicit host grants for every bit set in
the gate. `Capabilities::all_granted()` flips all six bits at once
(that is what `TrustLevel::Trusted` uses). See
[Sandbox & capabilities](./sandbox).

## Module resolution

`#import <bindspec> from "path"` doesn't read files directly — it
asks each resolver in the `Context` resolver chain (readable via
`module_resolvers()`) "can you resolve this path?" The first to
return `Some(ModuleSource)` wins; an `Err` aborts immediately.

The default chain:

1. **`StdModuleResolver`** — resolves virtual modules like
   `std/list`, `std/string` (embedded in the binary, zero IO).
2. **`FilesystemModuleResolver`** — reads from disk:
   - Host-owned scripts can install
     `FilesystemModuleResolver::trusted()` for no root restriction.
   - Under `Context::sandboxed()` the default is
     `FilesystemModuleResolver::default()`, which **denies
     everything** — mount a `with_root_dir(...)` instance in front
     of it to allow anything.

Mounting example (the rooted resolver sits ahead of the
default-denying one; first match wins):

```rust
use relon_evaluator::{Context, FilesystemModuleResolver};
use std::sync::Arc;

let mut ctx = Context::sandboxed();
ctx.prepend_module_resolver(Arc::new(
    FilesystemModuleResolver::with_root_dir("/var/relon-configs"),
));
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
// As a fallback: append to the end of the chain, consulted only
// when no earlier resolver claims the path
ctx.append_module_resolver(Arc::new(FallbackResolver));
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
- `List` and `Tuple` both project to JSON arrays.
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
        Err(ProjErr("custom traversal omitted from the guide".into()))
    }
}

let json = relon::project_from_str(source, &InternallyTaggedJson)?;
```

> **Scope note**: `Projector` is a knob for fine-tuning JSON shape,
> not an escape hatch out of JSON. Relon's output always lands in
> JSON — that's a hard constraint. If you want YAML / TOML / XML,
> that's another tool's job (e.g. Pkl).

## Build-time AOT: `include_relon!` and relon-rs-*

Beyond runtime embedding, you can compile `.relon` sources into
relocatable object files at **build time** and link them into your
Rust binary — `#main` becomes an ordinary Rust function call, with no
parse / eval cost at runtime. Three crates are involved:

- **`relon-rs-build`** — the build.rs-side `Compiler`, which compiles
  each `.relon` source into one ELF object file (exporting a single
  extern symbol) plus one generated binding `.rs`;
- **`relon-rs-macro`** — the `include_relon!` proc-macro that
  stitches the matching binding into your source file;
- **`relon-rs-shims`** — the runtime host shims (`SandboxState`, the
  buffer-protocol entry, string operators, …).

```rust
// build.rs
fn main() {
    let out_dir = std::env::var_os("OUT_DIR").unwrap();
    relon_rs_build::Compiler::new()
        .source("src/foo.relon")
        .emit_all(&out_dir)
        .unwrap();
}
```

```rust
// src/main.rs
relon_rs_macro::include_relon!("src/foo.relon");
// or aliased: relon_rs_macro::include_relon!("src/foo.relon" as compute);

fn main() {
    let state = relon_rs_shims::SandboxState::default();
    println!("{}", foo::main(&state, 42)); // #main(Int n) -> Int
}
```

The accepted leaf types for `#main` parameters and the return slot
are currently `Int` / `Float` / `Bool` / `String` / `List<Int>` (the
authoritative list is `relon-rs-build`'s `rust_type_for` table). See
`crates/relon-rs-demo` for an end-to-end example.

## Error types

`relon::Error` is the unified error of the facade crate:

| Variant | Source |
| --- | --- |
| `Error::Parse(String)` | Lexical / syntactic error |
| `Error::AnalyzeWorkspace { workspace, modules }` | Analyzer errors returned **in batch** (all passes run together): workspace-level findings (cycles, missing imports, cross-module collisions) plus per-module diagnostics |
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
  [Threat model](./threat-model) and [Sandbox & capabilities](./sandbox).
- Make `.relon` use the functions you registered: wrap them inside
  schemas / libraries — see [Types & schema contracts](./types).
- Miette-friendly error formatting: pass `RuntimeError` /
  `Diagnostic` straight to `miette::Report`.
