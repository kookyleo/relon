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

## 能力门控 builtin

与上面的纯 builtin(以及所有 std 模块)不同,下列语言级 builtin 是
**effectful(有副作用)**——读取环境的非确定来源,因此**不在决定性
承诺之内**,并受 [capability 模型](./sandbox.md)门控:

| 函数 | 返回 | 能力 | wasm 后端降级 |
|---|---|---|---|
| `clock()` | `Int` —— 当前 wall-clock 纳秒 | `reads_clock` | 标准 WASI `clock_time_get` |
| `random()` | `Int` —— 非确定 64 位值 | `uses_rng` | 标准 WASI `random_get` |
| `read_file(path)` | `String` —— 文件的 UTF-8 内容 | `reads_fs` | 标准 WASI preview1 `path_open` / `fd_read` / `fd_close` |
| `read_dir(path)` | `List<String>` —— 目录的条目名,**已排序** | `reads_fs` | 尚未实现(仅 native) |

```relon
{
    now: clock(),                 // 需 reads_clock,否则 CapabilityDenied
    nonce: random(),              // 需 uses_rng
    config: read_file("app.toml") // 需 reads_fs
}
```

`read_file(path)` 把 `path` 解析到单一的宿主配置**文件系统沙箱根**,
并拒绝任何逃逸该根的路径(`../`、绝对路径、指向根外的符号链接 →
`CapabilityDenied`)。该根是 wasm 后端 WASI host **preopen** 目录的
native 等价物——相对路径在每个执行器里解析到同一根,故结果逐字节相
同。(`read_file` 在四后端——tree-walk、cranelift-native、llvm-native、
wasm32——逐字节相等:wasm 臂降成标准 preview1 fd 协议(对 preopen 目录的
`path_open` / `fd_read` / `fd_close`),故任意现成 WASI host 均可运行。)

`read_dir(path)` 按同一沙箱根列举目录的条目文件名(同样拒绝逃逸)。条
目名按字节序**排序**——`read_dir` / `fd_readdir` 的迭代顺序由 OS 决定、
不确定,排序是让各后端返回逐字节相同列表的前提。非 UTF-8 名被跳过(线
上 String 布局只支持 UTF-8)。`read_dir` 目前**仅 native**——在三个
native 后端(tree-walk、cranelift-native、llvm-native)逐字节相等;
wasm32 臂(标准 preview1 `fd_readdir` dirent 流协议)暂缓,会抛出响亮的
codegen 错误而非产出错误列表。

它们内建于语言(无需 `#import`),但宿主必须授予对应能力位——与宿主
注册的原生函数**同一道门**,未授权调用即 `CapabilityDenied`。native
后端调宿主 runtime(`SystemTime` / OS RNG);**wasm 后端**降成**标准
WASI import**,故产出的模块可在任何标准 WASI host(wasmtime / 浏览器
/ …)运行、由该 host 授予时钟/随机——relon 的 `requires <cap>` 与
WASI 能力授予对齐。详见 [沙箱与权限](./sandbox.md)。

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
