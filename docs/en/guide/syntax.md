# Syntax Basics

Relon's syntax stays very close to JSON and modern JavaScript. The
design goal is that you can read most config code without consulting
the manual.

## Primitives

Relon natively supports every JSON primitive, with a few additions:

```relon
{
    "string": "Hello, Relon!",
    "raw_string": r#"This is a raw string where \n is literal"#,
    "template": f"The number is {10 * 2}", // f-string interpolation

    "integer": 42,
    "float": 3.14159,
    "hex": 0xFF,
    "binary": 0b1010,

    "boolean": true,
    "null_value": null
}
```

## Collections

Collections are the core of data modeling in Relon.

### List

A list is a homogeneous JSON-array-shaped collection. All elements
must have a compatible element type; use a tuple for fixed-position
heterogeneous data.

```relon
[
    1,
    2,
    3
]
```

### Tuple

A tuple is a fixed-length, position-typed value. It uses parentheses
in Relon source and projects to a JSON array at output.

```relon
(1, "two", true)
```

One-element tuples require a trailing comma: `(1,)`.

#### List comprehensions

A powerful Python-inspired feature, ideal for generating arrays:

```relon
[x * 2 for x in range(5) if x % 2 == 0]
// Evaluates to: [0, 4, 8]
```

### Dict

A dict corresponds to a JSON object. Its keys must be strings or
expressions that evaluate to strings.

#### Dynamic keys

Inside `[]`, you can write any expression to compute a key name:

```relon
{
    prefix: "user_",
    id: 42,
    // Use a dynamic key to concatenate
    [&sibling.prefix + &sibling.id]: "Alice"
}
```

#### Spread operator

Use `...` to splice another list or dict into the current collection.
In dicts, later keys overwrite earlier ones (since v1.3, statically
detected collisions are escalated to `DuplicateField` errors — see
spec §6.6).

```relon
{
    base: { host: "localhost", port: 80 },
    prod: {
        ...&sibling.base,
        port: 443 // overrides base's port (a static collision would fire DuplicateField)
    }
}
```

**v1.3 typed spread**: write `<T>` after `...` to type-hint the
spread source:

```relon
#schema Extra { Int a: *, Int b: * }
{ src: { a: 1, b: 2 }, ...<Extra> src }
{ ...<Dict<String, Int>> kv }
```

Under strict mode (the default), a spread source that isn't a dict
literal and lacks a `<T>` hint triggers `SpreadSourceTypeUnknown`.

**v1.3 typed dynamic key**: write `<T>` after `[` to type-hint the
dynamic key:

```relon
{ k: "key", [<String> k]: 1 }
{ idx: 0, [<Int> idx]: "row0" }
```

Under strict mode (the default), a dynamic key missing `<T>`
triggers `DynamicKeyTypeUnknown`.

#### `Dict<K, V>` generics (formally specified in v1.3)

```relon
{ Dict<String, Int> scores: { math: 100, art: 90 } }
{ Dict<String, Result<Int, String>> tasks: { ... } }
```

Bare `Dict` (without generics) is rejected by v1.7's
`BareGenericContainer` diagnostic — explicit generics are mandatory.

## Document root

Each `.relon` file evaluates to a single JSON value — Object, Array,
String, Number, Bool, or Null. **The root may be any expression**:
dict / list / tuple literal, atomic literal, binary / ternary / pipe
expression, function call, variant constructor, reference,
where / match, … as long as the final value lands in the JSON type
set. Directives, decorators, comments, and whitespace may precede it.

```relon
// Legal: dict root
{ "value": 42 }

// Legal: list root
[1, 2, 3]

// Legal: tuple root, projected as a JSON array
(1, "x")

// Legal: atomic literals are JSON values
42
"hello"
true
null

// Legal: top-level directives are allowed
#import string from "std/string"
{ "shouted": string.upper("hi") }

// Legal: in an entry program, root may reference #main params
#main(Int n) -> Int
n + 1

// Legal: variant constructor as the root
#main(Order o) -> Result<Order, String>
Result.Ok { value: o }
```

> Historical note: v1.1 and earlier accepted only dict / list literals
> at the root (a bare scalar or expression was rejected by the parser).
> v1.2 widened this to any expression (a superset extension); legacy
> scripts are unaffected. This lets `#main(Int n) -> Int` write `n + 1`
> directly and `#main(...) -> Result<T, E>` write `Result.Ok { ... }`
> directly — no longer needing a `{ value: ... }` wrapper dict.

`Closure` / `Schema` / `Type` are not JSON values. If the root
expression evaluates to one of these, the host-side projector (e.g.
the built-in `JsonProjector`) reports an error
(`UnsupportedClosure` / `UnsupportedSchema`). Declaring
`#main(...) -> ReturnType` as a non-JSON type makes the analyzer emit
`MainReturnTypeMismatch` per the standard type-check rules.

## `@` vs `#` — decorators vs directives

Relon splits "metadata attached to a node" into two disjoint
namespaces:

- `@name(...)` — **decorator**: a value transform. Either the
  built-in `@value(...)` or any user-defined callable
  (`@my_fn(arg)` is equivalent to passing the value below as the
  last positional argument of `my_fn`). Decorator stacks apply
  bottom-up: `@a @b v ≡ a(b(v))`.
- `#name ...` — **directive**: declaration / structure / metadata.
  The full set is `#main(...)`, `#schema X Body`,
  `#import ... from "..."`, `#internal`, `#default`, `#expect`,
  `#msg`, `#error`, `#brand X`, `#relaxed` (synonym `#unstrict`),
  `#derive`, `#no_auto_derive`, `#native`, and `#extend`. Directive
  names are fixed and registered by the runtime — not user-extensible.

> Rule of thumb: if it **changes the value**, it's `@`; if it
> **changes the shape or metadata**, it's `#`.

## Field visibility — `#internal`

Since configs typically need to export clean JSON, internal logic must
be hideable. Relon uses the `#internal` directive to declare that a
field is not externally visible:

```relon
{
    #internal
    helper(v): "<" + v + ">",
    display: helper("hi")
}
// JSON output: { "display": "<hi>" }   // helper is hidden
```

An `#internal` field:

- Lives in the local scope of its dict — **other fields in the same
  dict can reference it** (the `display` field calling `helper` above
  works fine).
- **Is not written** into the dict's export map. So:
  - It doesn't appear in JSON output.
  - Cross-dict `&root` / `&sibling` references can't see it.
  - After `#import lib from "..."`, accessing `lib.private_field`
    fails with `VariableNotFound`.
  - Spread-form `#import * from "..."` also doesn't copy it into the
    current scope.

If a dict value is a **closure (function)**, the default JSON
projector automatically filters it out. That's another defense beyond
`#internal`, specifically for "values that have no JSON representation".

> Historical note: early versions used a `_` prefix as an implicit
> convention and an `@private` decorator. Both are **fully retired**:
> identifiers may still start with `_` (e.g. the internal intrinsic
> `_list_map`), but it carries no visibility, import, or projection
> meaning. Use `#internal` for visibility.

## Strict mode — opt out with `#relaxed`

Relon's analyzer is strict by default: the file *and every module
reachable through its `#import` graph* runs under "strict static
inference" mode. A module opts out with the file-level directive
`#relaxed` (or its synonym `#unstrict`):

```relon
#relaxed
#import * from "./lib.relon"
{ ok: 1 }
```

Under strict mode, sites where the analyzer would normally silently
fall back to `Any` (and defer to runtime) produce errors:

- A spread without a type hint (`{ ...e }`) → `SpreadSourceTypeUnknown`.
- A dynamic key without a type hint (`{ [k]: v }`) →
  `DynamicKeyTypeUnknown`.
- A typed spread / dynamic-key `<T>` referencing an undeclared schema
  → `UnresolvedSchema`.
- A call to a native fn whose return type isn't registered in
  `host_fn_signatures` → `NativeFnSignatureMissing`.

**Contagion**: strict is decided at the entry. A strict entry (the
default — no directive needed) makes every imported library run under
strict rules, even one that didn't write `#relaxed` itself. A
`#relaxed` entry stamps the cleared bit on every reachable import, so
the workspace presents a single mode end to end.

For the full strict semantics, the complete diagnostic list, `<T>`
typehint syntax, and `Dict<K, V>` generics, see spec §6.6.

## Arithmetic and logical operators

Relon supports the standard arithmetic (`+`, `-`, `*`, `/`, `%`) and
comparison / logical operators (`>`, `<`, `>=`, `<=`, `==`, `!=`,
`&&`, `||`). You can also use the ternary operator for conditionals:

```relon
{
    "status": status_code == 200 ? "OK" : "Error"
}
```
