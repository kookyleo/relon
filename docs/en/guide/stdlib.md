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

## Capability-gated builtins

Unlike the pure builtins above (and every std module), these language-level
builtins are **effectful** — they read ambient, non-deterministic sources,
so they are **not** covered by the determinism guarantee and are gated by
the [capability model](./sandbox):

| Function | Returns | Capability | wasm backend lowering |
|---|---|---|---|
| `clock()` | `Int` — wall-clock time in nanoseconds | `reads_clock` | standard WASI `clock_time_get` |
| `random()` | `Int` — a non-deterministic 64-bit value | `uses_rng` | standard WASI `random_get` |
| `read_file(path)` | `String` — the file's UTF-8 contents | `reads_fs` | standard WASI preview1 `path_open` / `fd_read` / `fd_close` |
| `read_dir(path)` | `List<String>` — the directory's entry names, **sorted** | `reads_fs` | not yet implemented (native-only) |

```relon
{
    now: clock(),                 // needs reads_clock, else CapabilityDenied
    nonce: random(),              // needs uses_rng
    config: read_file("app.toml") // needs reads_fs
}
```

`read_file(path)` resolves `path` against a single host-configured **filesystem
sandbox root** and refuses any path that escapes it (`../`, absolute paths,
symlinks out of root → `CapabilityDenied`). The root is the native analogue of
the directory a WASI host **preopens** for the wasm backend — relative paths
resolve against the same root across every executor, so the result is
byte-identical. (`read_file` is byte-equal across all four backends — tree-walk,
cranelift-native, llvm-native, and wasm32: the wasm arm lowers to the standard
preview1 fd protocol — `path_open` / `fd_read` / `fd_close` against the
preopened dir — so any off-the-shelf WASI host runs it.)

`read_dir(path)` lists a directory's bare entry file names against the same
sandbox root (same escape refusal). The entry names are **sorted**
byte-lexicographically — `read_dir` / `fd_readdir` iteration order is
OS-unspecified, and the sort is what makes every backend return a
byte-identical list. Non-UTF-8 names are skipped (the wire String layout is
UTF-8 only). `read_dir` is **native-only** for now — byte-equal across the
three native backends (tree-walk, cranelift-native, llvm-native); the wasm32
arm (the standard preview1 `fd_readdir` dirent-stream protocol) is deferred and
raises a loud codegen error rather than emit an incorrect listing.

They are built into the language (no `#import`), but the host must grant the
matching capability bit — the **same gate** as host-registered native fns,
so an ungranted call raises `CapabilityDenied`. On the native backends they
call the host runtime (`SystemTime` / OS RNG); on the **wasm backend** they
lower to **standard WASI imports**, so the emitted module runs on any
standard WASI host (wasmtime / browser / …) and that host grants the clock /
randomness — relon's `requires <cap>` lines up with the WASI capability
grant. See [Sandbox & capabilities](./sandbox).

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
