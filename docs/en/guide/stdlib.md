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

> **Stable surface rule**: the stable user API is the module/builtin
> surface listed in the manifest below. Names with a leading `_` are
> implementation intrinsics for std modules and backend parity; they are
> portable internal contracts, not recommended user API. Some historical
> free-function names still exist in the tree-walker runtime registry for
> compatibility with internal wrappers and old fixtures; those
> runtime-only names are not portable stdlib API.

## Stable user API manifest

This table is the first-release public stdlib surface. Module rows name
the module path and exported member; users may choose any import alias
in source, but the stable API is the `std/<module>` export.

<!-- relon-stdlib-user-manifest:start -->
| Name | Signature | Category | Portable status | Key error semantics |
|---|---|---|---|---|
| `len` | `forall T: (value: T) -> Int` | language builtin | stable portable | Runtime rejects unsupported receiver shapes. |
| `range` | `(end: Int) -> List[Int]` / `(start: Int, end: Int) -> List[Int]` | language builtin | stable portable | Rejects non-`Int` bounds. |
| `type` | `forall T: (value: T) -> String` | language builtin | stable portable | Total over Relon values. |
| `std/list.map` | `forall T, U: (list: List[T], f: Closure[(T) -> U]) -> List[U]` | list module | stable portable | Closure errors abort the call. |
| `std/list.filter` | `forall T: (list: List[T], f: Closure[(T) -> Bool]) -> List[T]` | list module | stable portable | Predicate must return `Bool`; closure errors abort the call. |
| `std/list.reduce` | `forall T, U: (list: List[T], init: U, f: Closure[(U, T) -> U]) -> U` | list module | stable portable | Closure errors abort the call. |
| `std/list.contains` | `forall T: (list: List[T], needle: T) -> Bool` | list module | stable portable | Uses Relon value equality. |
| `std/list.sum` | `(list: List[Number]) -> Number` | list module | stable portable | Numeric errors propagate from `+`. |
| `std/list.avg` | `(list: List[Number]) -> Number` | list module | stable portable | Division errors propagate from `/`; empty lists divide by zero. |
| `std/list.len` | `forall T: (list: List[T]) -> Int` | list module | stable portable | Same receiver shape as `len`. |
| `std/list.first` | `forall T: (list: List[T]) -> T` | list module | stable portable | Empty lists raise an index/list error. |
| `std/list.last` | `forall T: (list: List[T]) -> T` | list module | stable portable | Empty lists raise an index/list error. |
| `std/list.compact` | `forall T: (list: List[T]) -> List[T]` | list module | stable portable | Removes `None` elements. |
| `std/list.flatten` | `forall T: (list: List[List[T]]) -> List[T]` | list module | stable portable | Non-list elements raise type errors. |
| `std/dict.merge` | `forall V: (base: Dict[String, V], overlay: Dict[String, V]) -> Dict[String, V]` | dict module | stable portable | Overlay keys overwrite base keys. |
| `std/dict.keys` | `forall V: (dict: Dict[String, V]) -> List[String]` | dict module | stable portable | Returns deterministic map order. |
| `std/dict.values` | `forall V: (dict: Dict[String, V]) -> List[V]` | dict module | stable portable | Order matches `std/dict.keys`. |
| `std/dict.has_key` | `forall V: (dict: Dict[String, V], key: String) -> Bool` | dict module | stable portable | Total over dictionaries. |
| `std/string.split` | `(s: String, sep: String) -> List[String]` | string module | stable portable | Byte-oriented string semantics. |
| `std/string.join` | `forall T: (list: List[T], sep: String) -> String` | string module | stable portable | Elements are rendered with Relon value formatting. |
| `std/string.replace` | `(s: String, from: String, to: String) -> String` | string module | stable portable | Byte-oriented string semantics. |
| `std/string.upper` | `(s: String) -> String` | string module | stable portable | Unicode case mapping follows Rust `String`. |
| `std/string.lower` | `(s: String) -> String` | string module | stable portable | Unicode case mapping follows Rust `String`. |
| `std/string.contains` | `(s: String, needle: String) -> Bool` | string module | stable portable | Byte-oriented substring search. |
| `std/math.abs` | `(n: Number) -> Number` | math module | stable portable | Int overflow and Float NaN behavior are pinned by tests. |
| `std/math.max` | `(a: Number, b: Number) -> Number` | math module | stable portable | Branch semantics, including NaN order, are pinned by tests. |
| `std/math.min` | `(a: Number, b: Number) -> Number` | math module | stable portable | Branch semantics, including NaN order, are pinned by tests. |
| `std/math.clamp` | `(v: Number, lo: Number, hi: Number) -> Number` | math module | stable portable | Inverted bounds and NaN behavior are pinned by tests. |
| `std/is.int` | `forall T: (value: T) -> Bool` | predicate module | stable portable | Total over Relon values. |
| `std/is.string` | `forall T: (value: T) -> Bool` | predicate module | stable portable | Total over Relon values. |
| `std/is.bool` | `forall T: (value: T) -> Bool` | predicate module | stable portable | Total over Relon values. |
| `std/is.float` | `forall T: (value: T) -> Bool` | predicate module | stable portable | Total over Relon values. |
| `std/is.list` | `forall T: (value: T) -> Bool` | predicate module | stable portable | Total over Relon values. |
| `std/is.dict` | `forall T: (value: T) -> Bool` | predicate module | stable portable | Total over Relon values. |
| `std/is.number` | `forall T: (value: T) -> Bool` | predicate module | stable portable | True for `Int` or `Float`. |
| `std/is.empty` | `forall T: (value: T) -> Bool` | predicate module | stable portable | Uses `len`; unsupported receiver shapes raise the same error. |
| `std/value.default` | `forall T: (value: T or None, fallback: T) -> T` | value module | stable portable | Only `None` selects the fallback. |
<!-- relon-stdlib-user-manifest:end -->

`std/string.glob_match` is intentionally absent from the stable user
manifest. It exists only as a legacy/runtime compatibility surface until
it receives the same promotion path as any other stable API: analyzer
coverage, public docs, and backend tests.

## Implementation intrinsics and schema internals

The following names are analyzer-backed callable contracts used by std
modules, schema lowering, and backend parity. They are documented so
implementations stay aligned; user code should prefer the stable user API
above and `#expect` for schemas.

<!-- relon-stdlib-internal-manifest:start -->
| Name | Signature | Category | Portable status | Key error semantics |
|---|---|---|---|---|
| `_len` | `forall T: (value: T) -> Int` | backing intrinsic | portable internal | Same as `len`. |
| `_list_map` | `forall T, U: (list: List[T], f: Closure[(T) -> U]) -> List[U]` | backing intrinsic | portable internal | Closure errors abort the call. |
| `_list_filter` | `forall T: (list: List[T], f: Closure[(T) -> Bool]) -> List[T]` | backing intrinsic | portable internal | Predicate must return `Bool`; closure errors abort the call. |
| `_list_reduce` | `forall T, U: (list: List[T], init: U, f: Closure[(U, T) -> U]) -> U` | backing intrinsic | portable internal | Closure errors abort the call. |
| `_list_contains` | `forall T: (list: List[T], needle: T) -> Bool` | backing intrinsic | portable internal | Uses Relon value equality. |
| `_string_split` | `(s: String, sep: String) -> List[String]` | backing intrinsic | portable internal | Byte-oriented string semantics. |
| `_string_join` | `forall T: (list: List[T], sep: String) -> String` | backing intrinsic | portable internal | Elements are rendered with Relon value formatting. |
| `_string_replace` | `(s: String, from: String, to: String) -> String` | backing intrinsic | portable internal | Byte-oriented string semantics. |
| `_string_upper` | `(s: String) -> String` | backing intrinsic | portable internal | Unicode case mapping follows Rust `String`. |
| `_string_lower` | `(s: String) -> String` | backing intrinsic | portable internal | Unicode case mapping follows Rust `String`. |
| `_string_contains` | `(s: String, needle: String) -> Bool` | backing intrinsic | portable internal | Byte-oriented substring search. |
| `_dict_merge` | `forall V: (base: Dict[String, V], ...Dict[String, V]) -> Dict[String, V]` | backing intrinsic | portable internal | Later keys overwrite earlier keys. |
| `_dict_keys` | `forall V: (d: Dict[String, V]) -> List[String]` | backing intrinsic | portable internal | Returns deterministic map order. |
| `_dict_values` | `forall V: (d: Dict[String, V]) -> List[V]` | backing intrinsic | portable internal | Order matches `_dict_keys`. |
| `_dict_has_key` | `forall V: (d: Dict[String, V], key: String) -> Bool` | backing intrinsic | portable internal | Total over dictionaries. |
| `_math_abs` | `(n: Float) -> Float` | backing intrinsic | portable internal | Float semantics follow `f64::abs`; Int dispatch lives in `std/math`. |
| `_math_max` | `(a: Number, b: Number) -> Number` | historical backing intrinsic | portable internal | Kept for analyzer/backend parity; user code should call `std/math.max`. |
| `_math_min` | `(a: Number, b: Number) -> Number` | historical backing intrinsic | portable internal | Kept for analyzer/backend parity; user code should call `std/math.min`. |
| `_math_clamp` | `(v: Number, lo: Number, hi: Number) -> Number` | historical backing intrinsic | portable internal | Kept for analyzer/backend parity; user code should call `std/math.clamp`. |
| `ensure.int` | `forall T: (value: T, message?: String) -> T` | schema internal contract | portable internal | Raises schema/runtime validation error on mismatch. |
| `ensure.string` | `forall T: (value: T, message?: String) -> T` | schema internal contract | portable internal | Raises schema/runtime validation error on mismatch. |
| `ensure.bool` | `forall T: (value: T, message?: String) -> T` | schema internal contract | portable internal | Raises schema/runtime validation error on mismatch. |
| `ensure.float` | `forall T: (value: T, message?: String) -> T` | schema internal contract | portable internal | Raises schema/runtime validation error on mismatch. |
| `ensure.list` | `forall T: (value: T, message?: String) -> T` | schema internal contract | portable internal | Raises schema/runtime validation error on mismatch. |
| `ensure.dict` | `forall T: (value: T, message?: String) -> T` | schema internal contract | portable internal | Raises schema/runtime validation error on mismatch. |
| `ensure.at_least` | `forall T: (value: T, min: Number, message?: String) -> T` | schema internal contract | portable internal | Raises schema/runtime validation error when below bound. |
| `ensure.at_most` | `forall T: (value: T, max: Number, message?: String) -> T` | schema internal contract | portable internal | Raises schema/runtime validation error when above bound. |
| `ensure.one_of` | `forall T: (value: T, allowed: List[T], message?: String) -> T` | schema internal contract | portable internal | Raises schema/runtime validation error when not in allowed set. |
| `ensure.required_fields` | `forall V: (dict: Dict[String, V], fields: List[String], message?: String) -> Dict[String, V]` | schema internal contract | portable internal | Raises schema/runtime validation error when a field is missing. |
| `ensure.requires` | `forall V: (dict: Dict[String, V], field: String, required: String, message?: String) -> Dict[String, V]` | schema internal contract | portable internal | Raises schema/runtime validation error when a dependency is missing. |
| `ensure.fields_equal` | `forall V: (dict: Dict[String, V], left: String, right: String, message?: String) -> Dict[String, V]` | schema internal contract | portable internal | Raises schema/runtime validation error when fields differ. |
<!-- relon-stdlib-internal-manifest:end -->

Legacy runtime-only free functions are intentionally absent from both
manifests. They remain a compatibility detail of the tree-walk evaluator
until each one is promoted by adding analyzer coverage, public docs, and
backend tests.

## Language-level builtins (no import needed)

These three names belong to the **language**, not std modules — they
are metadata operations on data structures themselves, unconditionally
available:

| Function | Meaning |
|---|---|
| `len(value)` | Element count of a `String` / `List` / `Dict`. |
| `range(end)` / `range(start, end)` | Half-open `Int` list. |
| `type(value)` | The value's type name (`"Int"`, `"String"`, `"List"`, `"Tuple"`, …). |

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
the host's own, audited escape hatch, not a language builtin. This keeps
the language itself a pure function — same input, same output, cacheable
and replayable. See [Sandbox & capabilities](./sandbox) for the
capability model.

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
    cleaned: list.compact([1, None, 2]),            // [1, 2]
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
    a: value.default(None, "fallback"),  // "fallback"
    b: value.default(false, true)          // false (only None triggers fallback)
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
