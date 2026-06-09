# Types & Schema Contracts

In large projects, dynamic languages tend to spiral out of control
when contracts are missing. Relon adds a nominal type system and
structured contracts to ensure data still matches business
expectations after complex dynamic merges and computations.

## Type hints

You can attach a type annotation to nearly any identifier. When the
annotation is present, the Relon engine performs a strict type check
during evaluation; a check failure raises a specific runtime error.

```relon
{
    // Basic type annotations
    String name: "Relon",
    Int port: 8080,

    // Optional type annotation (Option<T>)
    Option<String> optional_desc: None,

    // Generic annotations
    List<Int> scores: [100, 95, 80],
    Dict<String, Bool> flags: { "active": true, "hidden": false }
}
```

Built-in type names include: `Int`, `Float`, `Number` (covers both
Int and Float), `String`, `Bool`, `Option<T>`, `Result<T, E>`,
`List<T>`, `Tuple<T1, T2, ...>`, `Dict<K, V>`, and `Closure<...>`.
Relon has no `null` value; absence is written as `None` and is projected as JSON `null` at the output boundary. (Note:
`Any` was retired from the user-facing surface in v1.6; bare `List` /
`Dict` / `Closure` without generic arguments are rejected by v1.7's
`BareGenericContainer` diagnostic — see spec §6.6.) Enum definitions use the `#enum` form below.

## Enum: Rust-like tagged variants

Relon's public enum syntax is Rust-like `#enum`. It says that a value is one of several mutually exclusive variants. A variant may be unit-like, carry named fields, or carry a tuple payload.

```relon
#enum Notification {
    Email { address: String, subject: String },
    SMS { phone: String },
    Push
}

#enum Packet {
    Pair(Int, String),
    Empty
}
```

Relon does not support string-literal enum variants such as `"up" | "down"`, nor `#enum Stat { "up", "down" }`. For convenient string input from hosts, use the typed JSON string rule below.

### Constructing variants

```relon
{
    a: Notification.Email { address: "x@y.z", subject: "hi" },
    b: Notification.SMS { phone: "+1-555-0100" },
    c: Notification.Push,

    pair: Packet.Pair(7, "x"),
    empty: Packet.Empty
}
```

Rules match Rust's shapes:

- Unit variants are written as `EnumName.Variant`; no empty `{}` is needed.
- Struct variants are written as `EnumName.Variant { field: value }`.
- Tuple variants are written as `EnumName.Variant(value1, value2)`.

Match arms can destructure payloads:

```relon
#main(Packet p) -> Int
p match {
    Pair(n, _): n + 1,
    Empty: 0
}
```

### In-memory shape and JSON output

Internally, an enum value is a tagged `Value::Dict`: `brand` is the variant name and `variant_of` is the enum name. Field access is flat:

```relon
{
    msg: Notification.Email { address: "x@y.z", subject: "hi" },
    addr: msg.address      // -> "x@y.z", no .Email layer
}
```

The default JSON output is externally tagged:

```json
{
  "msg": { "Email": { "address": "x@y.z", "subject": "hi" } },
  "pair": { "Pair": [7, "x"] },
  "empty": { "Empty": {} }
}
```

Both List and tuple project to JSON arrays; tuple-variant payloads also project to JSON arrays.

Lists of variants can be returned directly, built from literals, or produced from `map`, `filter`, or comprehension when the surrounding type is known:

```relon
#enum Stat { Up, Down }
#main(List<Int> xs) -> List<Stat>
xs.map((Int x) => x > 0 ? Stat.Up : Stat.Down)
```

The same rule applies to `List<Option<T>>` and `List<Result<T, E>>`.

### Input: typed JSON for variants

CLI and WASM playground `#main(args)` JSON input reads the entry signature. When the target type is an enum, a JSON string may decode into a unit variant of the same name:

```relon
#enum Stat { Up, Down }
#main(Stat s) -> Stat
s
```

Input:

```json
{ "s": "Up" }
```

Output:

```json
{ "Up": {} }
```

Bare strings apply only to unit variants. Payload variants use the same externally tagged shape as JSON output:

```relon
#enum Msg { Email { address: String }, Pair(Int, String), Push }
#main(Msg m) -> Msg
m
```

```json
{ "m": { "Email": { "address": "x@y.z" } } }
```

Tuple variants use arrays:

```json
{ "m": { "Pair": [7, "x"] } }
```

Built-in `Option` and `Result` use the same target-typed boundary. For
`#main(Option<Int> x)`, input may use `null`, the direct payload `41`, or the
externally tagged form `{ "x": { "Some": { "value": 41 } } }`.
For `#main(Result<Int, String> r)`, use
`{ "r": { "Ok": { "value": 41 } } }` or
`{ "r": { "Err": { "error": "bad" } } }`.

Rust hosts that call `run_main` directly should pass `Value::variant_dict(...)`, or decode their business JSON into a Relon `Value` before calling the evaluator.

### Dispatch with `match`

```relon
{
    msg: Notification.Email { address: "x@y.z", subject: "hi" },
    rendered: msg match {
        Email: f"emailed ${msg.address}",
        SMS:   f"texted ${msg.phone}",
        Push:  "pushed"
    }
}
```

When the analyzer can statically infer the enum type of the matched value, it checks:

| Diagnostic | Trigger |
| --- | --- |
| `NonExhaustiveMatch` | Missing variants and no `_` wildcard |
| `UnknownVariant` | Variant name does not exist |
| `DuplicateMatchArm` | Same variant name appears twice |

When the analyzer cannot infer the type, runtime keeps the verdict. To opt out of exhaustiveness, add a `_: ...` wildcard arm.

### Complete example

```relon
#enum Order {
    Pending { customer: String },
    Shipped { tracking: String },
    Delivered { signed_by: String }
}

{
    summarize(Order o): o match {
        Pending:   f"awaiting shipment: ${o.customer}",
        Shipped:   f"in transit: ${o.tracking}",
        Delivered: f"signed for: ${o.signed_by}"
    }
}
```


## Schema definitions and identity guards

Annotating a dict with `Dict` alone is sometimes not enough — you
often need to validate what the dict looks like inside. That's where
`#schema` comes in.

### 1. Defining a schema

In a `#schema`-defined type, field values are treated as
**predicates**, not plain data. Use `*` to mean "matches anything",
or a closure for custom validation.

`#schema` has two equivalent forms:

```relon
{
    // Form A — standalone declaration (NameBody form):
    #schema ButtonConfig {
        // Must be a String; content matches anything
        String type: *,

        // Custom validation: width must be 10–100
        #expect "Width must be between 10 and 100"
        Int width: (w) => w >= 10 && w <= 100,

        // Default value
        #default false
        Bool disabled: *
    }
}
```

```relon
{
    // Form B — dict-field position (dict-field form):
    // Useful when you want the schema to live alongside regular
    // fields of the same dict.
    #schema ButtonConfig: {
        String type: *,
        #expect "Width must be between 10 and 100"
        Int width: (w) => w >= 10 && w <= 100,
        #default false
        Bool disabled: *
    }
}
```

The two are fully equivalent — they only differ in spelling. Form A
omits the colon and is its own directive; Form B follows the standard
field: value syntax.

### 2. Branding and nominal types

When a plain dict gets prefixed with a schema you defined, the magic
happens:

```relon
{
    // Apply ButtonConfig identity to an anonymous object
    ButtonConfig my_btn: { type: "submit", width: 50 }
}
```

- **Validation**: the engine immediately runs the `ButtonConfig`
  contract over `my_btn`.
- **Default injection**: because the schema declares `#default false`,
  even if `my_btn` omits `disabled`, the evaluated dict will have
  `disabled: false` injected.
- **Identity guard**: `my_btn` is now branded as `ButtonConfig`.
  Anyone who later tries to deep-merge it (via the `+` operator or
  `dict.merge`) re-runs the **full validation** on the merged result.

```relon
{
    // The merge raises: "Width must be between 10 and 100"
    // — preventing invalid attributes from polluting your business shape
    invalid_btn: &sibling.my_btn + { width: 999 }
}
```

### 3. Directive-position branding: `#brand X`

Field-level type annotations `Type field: { ... }` can only be
written to the left of the key. Some positions don't have a key —
list elements, the document root, dicts wrapped by other directives
(e.g. spread-form `#import`). For those, use `#brand X`:

```relon
{
    #schema Weather {
        String location: *,
        Int temperature: *
    },

    // Equivalent to `Weather typed: { ... }`, written at directive position
    decorated: #brand Weather {
        location: "Tokyo",
        temperature: 18
    },

    // List elements can't carry a field-level hint, so we use #brand
    cities: [
        #brand Weather { location: "Paris",  temperature: 20 },
        #brand Weather { location: "Sydney", temperature: 24 }
    ]
}
```

`#brand` exactly mirrors the runtime behavior of field-level hints —
same `check_type` validation, same brand-writing logic — so
`Weather typed: { ... }` and `decorated: #brand Weather { ... }`
behave identically for identity guards, `match` dispatch, and JSON
output.

The argument can take these shapes — basically the same shapes you
can write as a field-level type prefix:

- **Bareword**: `#brand Weather`, `#brand geo.Location` (paths use
  `.` as separator).
- **String literal**: `#brand "Weather"`, parsed as the same type
  name as the bareword form.
- **Generic forms**: `#brand Dict<String, Int>`,
  `#brand List<Weather>`, `#brand Foo<T>`.
- **Optional form**: `#brand Option<Weather>` — same behavior as the
  field-level `Option<Weather> w: ...` form. `None` passes; other
  values follow the original type check.

> About generic brand strings: the string written into `dict.brand`
> matches `format_type_node` output.
> - Single-segment, non-built-in type: `Weather`.
> - Multi-segment path: `geo.Location`.
> - Generic: `Dict<String, Int>`, `Foo<T>`.
> - Optional: `Option<Weather>`.
>
> In `match` arms the bareword form (`Weather: ...`) only matches
> single-segment non-built-in brands. To match by full-string brand
> equality on a generic, use `&self.brand == "Dict<String, Int>"` or
> redesign the schema to wrap it under a named type
> (`#schema Counters Dict<String, Int>`).

**Validation boundary**:

- Applied to a dict: when `check_type` passes, the brand is written
  to `dict.brand`. For built-in type names (`Int`/`String`/…) in the
  **single-segment, no-generic, no-`?`** form, the check runs but no
  brand is written — identical to field-level hints.
- Built-in container generics (`Dict<K, V>`, `List<T>`):
  `check_type`'s existing rules recurse; on success the brand string
  uses the full generic expression (e.g. `"Dict<String, Int>"`). Tagged
  sums should use Rust-like `#enum`.
- Custom type + generic parameters (e.g. `Foo<T>`): the runtime
  currently runs `check_custom_schema` keyed on `Foo` alone; generic
  parameters are preserved in the brand string but **don't
  participate** in runtime validation. Same as field-level type
  prefixes.
- Applied to a non-dict: validation only — there's no brand storage.
- Writing both a field-level hint and `#brand` on the same position
  (e.g. `Foo x: #brand Bar { ... }`) is rejected — same intent
  written twice; drop one.
- `#brand Unknown` raises `VariableNotFound` if `Unknown` isn't in
  scope, identical to `Unknown x: { ... }`.
- ⚠️ `#brand Map<...>` **does not work**: Relon's built-in containers
  are `Dict` / `List`, not `Map`. `Map<...>` falls through
  `check_custom_schema` looking for a schema named `Map`; if missing,
  `VariableNotFound` is raised.

#### Inside schema fields

`#brand X` can also appear on a field inside a `#schema` body — there
it's a synonym for the field-level type prefix `X`:

```relon
{
    // These two schemas are fully equivalent:
    #schema A {
        String name: *,
        Dict<String, Int> counters: *
    },

    #schema B {
        #brand String name: *,
        #brand Dict<String, Int> counters: *
    },

    // Instance validation runs the same path
    A inst1: { name: "x", counters: { hits: 1 } },
    B inst2: { name: "y", counters: { misses: 2 } }
}
```

Extra rules when used inside a schema field:

- Writing both a type prefix and `#brand` on the same field is
  rejected: `#schema S { #brand Bar Foo x: * }` raises
  `SchemaFieldBrandConflict`. Don't write the same intent twice.
- `#brand` composes with other meta-directives like `#expect` /
  `#default` / `#msg` / `#error`. `#default 0 #brand Int age: *`
  works.
- `#brand` on a schema field affects only the schema's field type
  declaration — it **doesn't** auto-brand the nested dict in an
  instance. Same as the `Type field: *` form. If you want the
  nested dict in the instance to carry a brand too, write `#brand` or
  a type prefix again on the instance side.

## Schema mixins and composition

In component libraries you often extend a base config into an
advanced one. Since schemas are first-class values, you can compose
them with `+`:

```relon
{
    #schema BaseControl {
        String id: *,
        #default false Bool disabled: *
    },

    // Inherit BaseControl's constraints; mix in extra fields
    #schema IconControl &sibling.BaseControl + {
        String icon_path: *
    },

    // The final instance has the full set of constraints
    IconControl final_btn: { id: "btn_1", icon_path: "/icons/save.png" }
}
```

## Recursive schemas

Schemas can reference themselves — natural for menu trees, file
directories, ASTs, and other recursive structures:

```relon
{
    #schema Menu {
        String title: *,
        Option<List<Menu>> children: *
    },

    Menu root: {
        title: "Home",
        children: [
            { title: "Products", children: [] },
            { title: "About" }
        ]
    }
}
```

> The implementation caps recursive validation depth at 20, far beyond
> most business nesting. If you hit it, your data shape is probably
> the problem — revisit the schema first.

## Custom validation messages (`#expect`)

By default, the engine assembles an error message from the predicate
string when validation fails; readability is mediocre. `#expect "..."`
lets you provide a business-readable message explicitly:

```relon
{
    #schema Server {
        #expect "Port must be between 0 and 65535"
        Int port: (p) => p > 0 && p < 65536
    },

    Server s: { port: 70000 }
    // → TypeMismatch { expected: "Port must be between 0 and 65535", ... }
}
```

`#expect` must accompany a predicate closure — applying it to `*` is
meaningless.

## Required, optional, default value, computed default

Relon's schema fields pack these four semantics into one
declaration, distinguished by **modifier combinations**:

```relon
{
    #schema User {
        // 1. Required (default): missing → error
        String name: *,

        // 2. Optional (Option<T>): the field may be absent; use None for no value
        Option<String> bio: *,

        // 3. Literal default (#default value): missing → this constant
        #default "user"
        String role: *,

        // 4. Computed default (#default (self) => ...)
        // Called when the field is missing; self is "the partial
        // instance with all known fields already filled in"
        #default (self) => self.name + " <unset>"
        String display_name: *
    },

    // Usage
    User u: { name: "Ada" }
    // u.bio may be absent; optional lookup returns None
    // u.role         == "user"
    // u.display_name == "Ada <unset>"
}
```

A few details to keep in mind:

- An explicitly written field value **always wins** over a default —
  literal or computed.
- Computed defaults are **lazy**: they only fire when the field is
  actually missing; no wasted calls.
- A computed default's `self` sees "explicitly written fields +
  fields already filled by literal defaults", but **not other
  computed-default fields** — they don't observe each other,
  preventing evaluation cycles.

## Next

- Package schemas and helpers into reusable libraries: [Modules &
  scope](./modules).
- Register your own native functions for schemas to use:
  [Host integration](./host-integration).
- Use schemas with `#expect` as your first line of defense when
  running untrusted scripts: [Sandbox & capabilities](./sandbox).
