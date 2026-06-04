# Standard Library

The Relon standard library is **part of the language** — the modules
below ship with the runtime; scripts import them via
`#import <bindspec> from "std/<name>"`, and the host registers
nothing extra.

> **Determinism guarantee**: every stdlib function is pure; the same
> input always produces byte-identical output. No I/O, no network,
> no system time, no random numbers. See the
> [Language spec](./spec) for details.

> **Relationship to the capability model**: std modules are served
> by a virtual resolver (`StdModuleResolver`) and **do not consume**
> `reads_fs` — they're spec content, not a host trust decision. See
> [Sandbox & capabilities](./sandbox).

## Language-level builtins (no import needed)

These three names belong to the **language**, not std modules — they
are metadata operations on data structures themselves, unconditionally
available:

| Function | Meaning |
|---|---|
| `len(value)` | Element count of a `String` / `List` / `Dict`. |
| `range(end)` / `range(start, end)` | Half-open `Int` list. |
| `type(value)` | The value's type name (`"Int"`, `"String"`, `"List"`, …). |

```relon
{
    n: len([1, 2, 3]),       // 3
    nums: range(5),          // [0, 1, 2, 3, 4]
    kind: type("hi")         // "String"
}
```

## No effectful language builtins

Relon the language has **no** effectful builtins — there is no `clock()`,
`random()`, `read_file()`, `read_dir()`, or `stat()`. Relon is a pure
function `f(inputs) -> output`: it never reaches out to the world during
evaluation. Any effectful value (the current time, a random nonce, a file's
contents, a directory listing, file metadata, an environment variable) is
**taken by the host** and fed in as an **input** to the evaluation.

A host that needs to expose an effectful operation does so explicitly via a
`#native` function it registers (and gates with a capability bit) — that is
the host's own, audited escape hatch, not a language builtin. See
[the ADR](../../internal/adr-effectful-io-builtins-2026-06-04) for the
rationale and [Sandbox & capabilities](./sandbox) for the capability model.

## std/list

```relon
#import list from "std/list"
{
    doubled: list.map([1, 2, 3], (x) => x * 2),     // [2, 4, 6]
    evens: list.filter(range(10), (x) => x % 2 == 0),
    sum: list.reduce([1, 2, 3], 0, (acc, x) => acc + x),
    has_two: list.contains([1, 2, 3], 2),           // true
    total: list.sum([1, 2, 3]),                     // 6
    mean: list.avg([1, 2, 3]),                      // 2
    n: list.len([1, 2, 3]),                         // 3
    head: list.first([10, 20, 30]),                 // 10
    tail: list.last([10, 20, 30]),                  // 30
    cleaned: list.compact([1, null, 2]),            // [1, 2]
    flat: list.flatten([[1, 2], [3]])               // [1, 2, 3]
}
```

## std/dict

```relon
#import dict from "std/dict"
{
    base: { a: 1 },
    over: { b: 2 },
    merged: dict.merge(&sibling.base, &sibling.over),  // { a: 1, b: 2 }
    keys: dict.keys(&sibling.merged),                  // ["a", "b"] (BTreeMap order)
    values: dict.values(&sibling.merged),              // [1, 2]
    has_a: dict.has_key(&sibling.merged, "a")          // true
}
```

## std/string

```relon
#import string from "std/string"
{
    parts: string.split("a,b,c", ","),     // ["a", "b", "c"]
    joined: string.join(["a", "b"], "-"),  // "a-b"
    fixed: string.replace("hi world", "world", "relon"),
    upper: string.upper("relon"),          // "RELON"
    lower: string.lower("RELON"),          // "relon"
    has_x: string.contains("hello", "ell") // true
}
```

`string.*` operations are **byte-based** (e.g. `string.split` matches
Rust's `String::split`). For grapheme-cluster operations, the host
must expose a native fn explicitly.

## std/math

```relon
#import math from "std/math"
{
    a: math.abs(-3),             // 3
    hi: math.max(2, 5),          // 5
    lo: math.min(2, 5),          // 2
    bound: math.clamp(15, 0, 10) // 10
}
```

## std/is

```relon
#import is from "std/is"
{
    is_int: is.int(42),         // true
    is_str: is.string("a"),     // true
    is_num: is.number(1.5),     // true (Int or Float)
    is_empty: is.empty([]),     // true
    // ... also bool / float / list / dict
}
```

## std/value

```relon
#import value from "std/value"
{
    a: value.default(null, "fallback"),  // "fallback"
    b: value.default(false, true)        // false (only null triggers fallback)
}
```

## ensure.\* — used internally by #schema

`#schema` depends internally on a set of `ensure.*` functions:
`ensure.int`, `ensure.string`, `ensure.required_fields`,
`ensure.at_least`, `ensure.one_of`, … These are an implementation
detail of the schema system, with semantics locked down by spec §6.3
— **but they are not API the user script should call directly**.
Inside your own schema, use the `#expect` directive instead.

## Roadmap

The following modules are under spec evaluation and not yet frozen:

- `std/time`: read the host's current time — necessarily a
  capability operation; will be exposed through an explicit
  capability channel rather than as a pure std function.
- `std/regex`: regex match and extract. Spec needs to first pin the
  regex dialect (PCRE and RE2 behave differently).
- `std/path`: pure path-string operations (join, normalize).
- `std/base64`: encode / decode.

> Until spec freezes these modules, the host can register them via
> `register_fn(name, gate, fn)` (declaring the corresponding bits on
> `NativeFnGate`, e.g. `std/time` uses `reads_clock`) — but scripts
> using them depend on host-injected names and step outside the
> spec's semantic guarantees (behavior is only predictable under that
> host configuration). See [Host integration](./host-integration).
