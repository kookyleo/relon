# 标准库 (Standard Library)

Relon 标准库是**语言的一部分**——下列模块随运行时一起发布，脚本通
过 `#import <bindspec> from "std/<name>"` 引入，宿主无需额外注册。

> **决定性承诺**：标准库的所有函数都是纯函数；给定相同输入，永远产生
> 字节级一致的输出。无 IO、无网络、无系统时间、无随机数。详见
> [语言规范](./spec.md)。

> **与 capability 模型的关系**：std 模块通过虚拟解析器
> （`StdModuleResolver`）服务，**不消耗** `reads_fs` 能力——它们是
> 规范内容，不属于宿主信任决策。详见 [沙箱与权限](./sandbox.md)。

## 语言级 builtin（无需 import）

下列三个名字属于**语言**而非 std 模块——是数据结构本身的元操作，
无条件可用：

| 函数 | 语义 |
|---|---|
| `len(value)` | 返回 `String` / `List` / `Dict` 的元素数。 |
| `range(end)` / `range(start, end)` | 返回半开区间 `Int` 列表。 |
| `type(value)` | 返回值的类型名（`"Int"`、`"String"`、`"List"` 等）。 |

```relon
{
    n: len([1, 2, 3]),       // 3
    nums: range(5),          // [0, 1, 2, 3, 4]
    kind: type("hi")         // "String"
}
```

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
    keys: dict.keys(&sibling.merged),                  // ["a", "b"] (BTreeMap 序)
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

`string.*` 操作以**字节**为单位（如 `string.split` 行为同 Rust 的
`String::split`）。如果脚本需要按 grapheme cluster 操作，必须由 host
通过 native fn 显式提供。

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
    is_num: is.number(1.5),     // true（Int 或 Float）
    is_empty: is.empty([]),     // true
    // ... 还有 bool / float / list / dict
}
```

## std/value

```relon
#import value from "std/value"
{
    a: value.default(null, "fallback"),  // "fallback"
    b: value.default(false, true)        // false（仅 null 触发 fallback）
}
```

## ensure.\* —— #schema 内部使用

`#schema` 内部依赖一组 `ensure.*` 函数：`ensure.int`、
`ensure.string`、`ensure.required_fields`、`ensure.at_least`、
`ensure.one_of` 等。这些是 schema 系统的实现细节，由 spec §6.3 锁
定语义——但**不属于用户脚本应直接调用的 API**。在你自己的 schema
里通过 `#expect` 指令使用即可。

## 路线图（Roadmap）

下列模块在 spec 评估中，尚未冻结：

- `std/time`：取宿主当前时间——必然涉及 capability，将通过显式
  capability 通道暴露而非纯 std 函数。
- `std/regex`：正则匹配、提取。需先在 spec 里钉死正则方言（PCRE
  与 RE2 行为不同）。
- `std/path`：纯路径字符串操作（join、normalize）。
- `std/base64`：编解码。

> 在 spec 冻结这些模块之前，宿主端可以用 `register_fn(name, gate, fn)`
> 自己注册（按需声明 `NativeFnGate` 上对应的 bit，例如 `std/time` 走
> `reads_clock`）——但这样的脚本会依赖宿主注入名，脱离规范的语义保
> 证范围（行为只在该 host 配置下可预期）。详见
> [嵌入宿主](./host-integration.md)。
