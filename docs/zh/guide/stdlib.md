# 标准库 (Standard Library)

Relon 标准库是**语言的一部分**——下列模块随运行时一起发布，脚本通
过 `#import <bindspec> from "std/<name>"` 引入，宿主无需额外注册。

> **决定性承诺**：标准库的所有函数都是纯函数；给定相同输入，永远产生
> 字节级一致的输出。无 IO、无网络、无系统时间、无随机数。详见
> [语言规范](./spec.md)。

> **与 capability 模型的关系**：std 模块通过虚拟解析器
> （`StdModuleResolver`）服务，**不消耗** `reads_fs` 能力——它们是
> 规范内容，不属于宿主信任决策。详见 [沙箱与权限](./sandbox.md)。

> **稳定 surface 规则**：稳定用户 API 是下方 manifest 列出的模块 /
> builtin surface。以下划线开头的名字是 std 模块和后端一致性使用的
> implementation intrinsic；它们是可移植的内部契约，不是推荐用户 API。
> tree-walker 运行时注册表里仍有少量历史 free-function 名字，用于内部
> wrapper 和旧 fixture 兼容；这些 runtime-only 名字不是可移植 stdlib API。

## 稳定用户 API manifest

下表是首个公开版本的 stdlib 用户 surface。模块行使用模块路径和导出
成员命名；源码里 import alias 可以任意取名，但稳定 API 是
`std/<module>` 的导出项。

<!-- relon-stdlib-user-manifest:start -->
| 名字 | 签名 | 类别 | 可移植状态 | 关键错误语义 |
|---|---|---|---|---|
| `len` | `forall T: (value: T) -> Int` | 语言 builtin | stable portable | 运行时拒绝不支持的 receiver 形状。 |
| `range` | `(end: Int) -> List[Int]` / `(start: Int, end: Int) -> List[Int]` | 语言 builtin | stable portable | 拒绝非 `Int` 边界。 |
| `type` | `forall T: (value: T) -> String` | 语言 builtin | stable portable | 对所有 Relon 值都有定义。 |
| `std/list.map` | `forall T, U: (list: List[T], f: Closure[(T) -> U]) -> List[U]` | list 模块 | stable portable | closure 错误会终止调用。 |
| `std/list.filter` | `forall T: (list: List[T], f: Closure[(T) -> Bool]) -> List[T]` | list 模块 | stable portable | predicate 必须返回 `Bool`；closure 错误会终止调用。 |
| `std/list.reduce` | `forall T, U: (list: List[T], init: U, f: Closure[(U, T) -> U]) -> U` | list 模块 | stable portable | closure 错误会终止调用。 |
| `std/list.contains` | `forall T: (list: List[T], needle: T) -> Bool` | list 模块 | stable portable | 使用 Relon 值相等语义。 |
| `std/list.sum` | `(list: List[Number]) -> Number` | list 模块 | stable portable | 数值错误从 `+` 传播。 |
| `std/list.avg` | `(list: List[Number]) -> Number` | list 模块 | stable portable | 除法错误从 `/` 传播；空列表会触发除零。 |
| `std/list.len` | `forall T: (list: List[T]) -> Int` | list 模块 | stable portable | receiver 形状同 `len`。 |
| `std/list.first` | `forall T: (list: List[T]) -> T` | list 模块 | stable portable | 空列表抛 index/list 错误。 |
| `std/list.last` | `forall T: (list: List[T]) -> T` | list 模块 | stable portable | 空列表抛 index/list 错误。 |
| `std/list.compact` | `forall T: (list: List[T]) -> List[T]` | list 模块 | stable portable | 移除 `None` 元素。 |
| `std/list.flatten` | `forall T: (list: List[List[T]]) -> List[T]` | list 模块 | stable portable | 非列表元素抛类型错误。 |
| `std/dict.merge` | `forall V: (base: Dict[String, V], overlay: Dict[String, V]) -> Dict[String, V]` | dict 模块 | stable portable | overlay key 覆盖 base key。 |
| `std/dict.keys` | `forall V: (dict: Dict[String, V]) -> List[String]` | dict 模块 | stable portable | 返回确定性的 map 顺序。 |
| `std/dict.values` | `forall V: (dict: Dict[String, V]) -> List[V]` | dict 模块 | stable portable | 顺序与 `std/dict.keys` 一致。 |
| `std/dict.has_key` | `forall V: (dict: Dict[String, V], key: String) -> Bool` | dict 模块 | stable portable | 对字典总是有定义。 |
| `std/string.split` | `(s: String, sep: String) -> List[String]` | string 模块 | stable portable | 字节级字符串语义。 |
| `std/string.join` | `forall T: (list: List[T], sep: String) -> String` | string 模块 | stable portable | 元素按 Relon 值格式渲染。 |
| `std/string.replace` | `(s: String, from: String, to: String) -> String` | string 模块 | stable portable | 字节级字符串语义。 |
| `std/string.upper` | `(s: String) -> String` | string 模块 | stable portable | Unicode 大小写映射跟随 Rust `String`。 |
| `std/string.lower` | `(s: String) -> String` | string 模块 | stable portable | Unicode 大小写映射跟随 Rust `String`。 |
| `std/string.contains` | `(s: String, needle: String) -> Bool` | string 模块 | stable portable | 字节级子串搜索。 |
| `std/math.abs` | `(n: Number) -> Number` | math 模块 | stable portable | Int overflow 与 Float NaN 行为由测试固定。 |
| `std/math.max` | `(a: Number, b: Number) -> Number` | math 模块 | stable portable | 分支语义，包括 NaN 顺序，由测试固定。 |
| `std/math.min` | `(a: Number, b: Number) -> Number` | math 模块 | stable portable | 分支语义，包括 NaN 顺序，由测试固定。 |
| `std/math.clamp` | `(v: Number, lo: Number, hi: Number) -> Number` | math 模块 | stable portable | 反向边界与 NaN 行为由测试固定。 |
| `std/is.int` | `forall T: (value: T) -> Bool` | predicate 模块 | stable portable | 对所有 Relon 值都有定义。 |
| `std/is.string` | `forall T: (value: T) -> Bool` | predicate 模块 | stable portable | 对所有 Relon 值都有定义。 |
| `std/is.bool` | `forall T: (value: T) -> Bool` | predicate 模块 | stable portable | 对所有 Relon 值都有定义。 |
| `std/is.float` | `forall T: (value: T) -> Bool` | predicate 模块 | stable portable | 对所有 Relon 值都有定义。 |
| `std/is.list` | `forall T: (value: T) -> Bool` | predicate 模块 | stable portable | 对所有 Relon 值都有定义。 |
| `std/is.dict` | `forall T: (value: T) -> Bool` | predicate 模块 | stable portable | 对所有 Relon 值都有定义。 |
| `std/is.number` | `forall T: (value: T) -> Bool` | predicate 模块 | stable portable | 对 `Int` 或 `Float` 返回 true。 |
| `std/is.empty` | `forall T: (value: T) -> Bool` | predicate 模块 | stable portable | 使用 `len`；不支持的 receiver 形状抛同类错误。 |
| `std/value.default` | `forall T: (value: T or None, fallback: T) -> T` | value 模块 | stable portable | 只有 `None` 选择 fallback。 |
<!-- relon-stdlib-user-manifest:end -->

`std/string.glob_match` 被刻意排除在稳定用户 manifest 之外。它目前只是
legacy/runtime 兼容 surface；后续若要升级，必须按普通稳定 API 一样补
analyzer 覆盖、公开文档和后端测试。

## Implementation intrinsic 与 schema 内部契约

下列名字是 std 模块、schema lowering 和后端一致性使用的 analyzer-backed
callable 契约。记录它们是为了让实现保持一致；用户代码应优先使用上方
稳定用户 API，schema 校验则使用 `#expect`。

<!-- relon-stdlib-internal-manifest:start -->
| 名字 | 签名 | 类别 | 可移植状态 | 关键错误语义 |
|---|---|---|---|---|
| `_len` | `forall T: (value: T) -> Int` | backing intrinsic | portable internal | 同 `len`。 |
| `_list_map` | `forall T, U: (list: List[T], f: Closure[(T) -> U]) -> List[U]` | backing intrinsic | portable internal | closure 错误会终止调用。 |
| `_list_filter` | `forall T: (list: List[T], f: Closure[(T) -> Bool]) -> List[T]` | backing intrinsic | portable internal | predicate 必须返回 `Bool`；closure 错误会终止调用。 |
| `_list_reduce` | `forall T, U: (list: List[T], init: U, f: Closure[(U, T) -> U]) -> U` | backing intrinsic | portable internal | closure 错误会终止调用。 |
| `_list_contains` | `forall T: (list: List[T], needle: T) -> Bool` | backing intrinsic | portable internal | 使用 Relon 值相等语义。 |
| `_string_split` | `(s: String, sep: String) -> List[String]` | backing intrinsic | portable internal | 字节级字符串语义。 |
| `_string_join` | `forall T: (list: List[T], sep: String) -> String` | backing intrinsic | portable internal | 元素按 Relon 值格式渲染。 |
| `_string_replace` | `(s: String, from: String, to: String) -> String` | backing intrinsic | portable internal | 字节级字符串语义。 |
| `_string_upper` | `(s: String) -> String` | backing intrinsic | portable internal | Unicode 大小写映射跟随 Rust `String`。 |
| `_string_lower` | `(s: String) -> String` | backing intrinsic | portable internal | Unicode 大小写映射跟随 Rust `String`。 |
| `_string_contains` | `(s: String, needle: String) -> Bool` | backing intrinsic | portable internal | 字节级子串搜索。 |
| `_dict_merge` | `forall V: (base: Dict[String, V], ...Dict[String, V]) -> Dict[String, V]` | backing intrinsic | portable internal | 后面的 key 覆盖前面的 key。 |
| `_dict_keys` | `forall V: (d: Dict[String, V]) -> List[String]` | backing intrinsic | portable internal | 返回确定性的 map 顺序。 |
| `_dict_values` | `forall V: (d: Dict[String, V]) -> List[V]` | backing intrinsic | portable internal | 顺序与 `_dict_keys` 一致。 |
| `_dict_has_key` | `forall V: (d: Dict[String, V], key: String) -> Bool` | backing intrinsic | portable internal | 对字典总是有定义。 |
| `_math_abs` | `(n: Float) -> Float` | backing intrinsic | portable internal | Float 语义跟随 `f64::abs`；Int dispatch 在 `std/math` 中实现。 |
| `_math_max` | `(a: Number, b: Number) -> Number` | 历史 backing intrinsic | portable internal | 为 analyzer/backend parity 保留；用户代码应调用 `std/math.max`。 |
| `_math_min` | `(a: Number, b: Number) -> Number` | 历史 backing intrinsic | portable internal | 为 analyzer/backend parity 保留；用户代码应调用 `std/math.min`。 |
| `_math_clamp` | `(v: Number, lo: Number, hi: Number) -> Number` | 历史 backing intrinsic | portable internal | 为 analyzer/backend parity 保留；用户代码应调用 `std/math.clamp`。 |
| `ensure.int` | `forall T: (value: T, message?: String) -> T` | schema 内部契约 | portable internal | 不匹配时抛 schema/runtime validation 错误。 |
| `ensure.string` | `forall T: (value: T, message?: String) -> T` | schema 内部契约 | portable internal | 不匹配时抛 schema/runtime validation 错误。 |
| `ensure.bool` | `forall T: (value: T, message?: String) -> T` | schema 内部契约 | portable internal | 不匹配时抛 schema/runtime validation 错误。 |
| `ensure.float` | `forall T: (value: T, message?: String) -> T` | schema 内部契约 | portable internal | 不匹配时抛 schema/runtime validation 错误。 |
| `ensure.list` | `forall T: (value: T, message?: String) -> T` | schema 内部契约 | portable internal | 不匹配时抛 schema/runtime validation 错误。 |
| `ensure.dict` | `forall T: (value: T, message?: String) -> T` | schema 内部契约 | portable internal | 不匹配时抛 schema/runtime validation 错误。 |
| `ensure.at_least` | `forall T: (value: T, min: Number, message?: String) -> T` | schema 内部契约 | portable internal | 低于下界时抛 schema/runtime validation 错误。 |
| `ensure.at_most` | `forall T: (value: T, max: Number, message?: String) -> T` | schema 内部契约 | portable internal | 高于上界时抛 schema/runtime validation 错误。 |
| `ensure.one_of` | `forall T: (value: T, allowed: List[T], message?: String) -> T` | schema 内部契约 | portable internal | 不在 allowed 集合中时抛 schema/runtime validation 错误。 |
| `ensure.required_fields` | `forall V: (dict: Dict[String, V], fields: List[String], message?: String) -> Dict[String, V]` | schema 内部契约 | portable internal | 缺字段时抛 schema/runtime validation 错误。 |
| `ensure.requires` | `forall V: (dict: Dict[String, V], field: String, required: String, message?: String) -> Dict[String, V]` | schema 内部契约 | portable internal | 缺依赖字段时抛 schema/runtime validation 错误。 |
| `ensure.fields_equal` | `forall V: (dict: Dict[String, V], left: String, right: String, message?: String) -> Dict[String, V]` | schema 内部契约 | portable internal | 两个字段不相等时抛 schema/runtime validation 错误。 |
<!-- relon-stdlib-internal-manifest:end -->

历史 runtime-only free function 被刻意排除在两个 manifest 之外。它们只是
tree-walk evaluator 的兼容细节；每个名字后续必须先补 analyzer 覆盖、
公开文档与后端测试，才能升级为稳定 stdlib。

## 语言级 builtin（无需 import）

下列三个名字属于**语言**而非 std 模块——是数据结构本身的元操作，
无条件可用：

| 函数 | 语义 |
|---|---|
| `len(value)` | 返回 `String` / `List` / `Dict` 的元素数。 |
| `range(end)` / `range(start, end)` | 返回半开区间 `Int` 列表。 |
| `type(value)` | 返回值的类型名（`"Int"`、`"String"`、`"List"`、`"Tuple"` 等）。 |

```relon
{
    n: len([1, 2, 3]),       // 3
    nums: range(5),          // [0, 1, 2, 3, 4]
    kind: type("hi")         // "String"
}
```

## 语言没有 effectful builtin

Relon 语言**没有**任何 effectful builtin —— 没有 `clock()`、`random()`、
`read_file()`、`read_dir()`、`stat()`。Relon 是一个纯函数
`f(inputs) -> output`:它在求值期间永不向外伸手取数据。一切 effectful 的
值(当前时间、随机 nonce、文件内容、目录列表、文件元信息、环境变量)都
由**宿主取好、作为 input 喂进来**。

宿主若需要暴露某个 effectful 操作,通过它自己注册并门控(带能力位)的
`#native` 函数显式做到 —— 这是宿主自己的、可审计的逃生舱,而非语言
builtin。这保证语言本身始终是纯函数 —— 同输入必同输出、可缓存可重放;
能力模型见 [沙箱与权限](./sandbox.md)。

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
    a: value.default(None, "fallback"),  // "fallback"
    b: value.default(false, true)          // false（仅 None 触发 fallback）
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
