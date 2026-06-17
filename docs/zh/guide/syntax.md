# 基础语法

Relon 的语法极度贴近 JSON 和现代 JavaScript，它的设计目标是让你不需要查阅手册就能看懂大部分的配置代码。

## 数据类型 (Primitives)

Relon 支持 JSON 形态的基础值，但语言里没有 JSON `null` 字面量；没有值用 `None`，只有导出 JSON 时才投影为 JSON `null`：

```relon
{
    "string": "Hello, Relon!",
    "raw_string": r#"This is a raw string where \n is literal"#,
    "template": f"The number is ${10 * 2}", // f-string 字符串插值（只认 `${...}`，裸 `{}` 是字面文本）

    "integer": 42,
    "float": 3.14159,
    "hex": 0xFF,
    "binary": 0b1010,

    "boolean": true,
    "none_value": None
}
```

## 集合类型 (Collections)

集合类型在 Relon 中是进行数据建模的核心。

### 列表 (List)

列表是同质的 JSON-array 形集合。所有元素必须有兼容的元素类型；固定位置的异质数据请用 tuple。

```relon
[
    1,
    2,
    3
]
```

### Tuple

tuple 是定长、按位置定型的值。Relon 源码里用小括号，输出到 JSON 时投影成 array。

```relon
(1, "two", true)
```

单元素 tuple 需要尾逗号：`(1,)`。

### Enum

enum 使用 Rust-like 的 `#enum` 声明。unit、struct、tuple 三种变体分别这样构造：

```relon
#enum Stat { Up, Down }
#enum Notification { Email { address: String }, Push }
#enum Packet { Pair(Int, String), Empty }

{
    up: Stat.Up,
    mail: Notification.Email { address: "x@y.z" },
    pair: Packet.Pair(7, "x")
}
```

输出 JSON 时采用外部标签：`Stat.Up` -> `{ "Up": {} }`，`Packet.Pair(7, "x")` -> `{ "Pair": [7, "x"] }`。

`match` arm 可以在 pattern 中绑定 payload 字段。用 `_` 表示忽略某个 payload 槽：

```relon
#enum Packet { Pair(Int, String), Empty }

#main(Packet p) -> Int
p match {
    Pair(n, _): n + 1,
    Empty: 0
}
```

struct variant 按字段名绑定：

```relon
#enum Notification {
    Email { address: String, subject: String },
    Push
}

#main(Notification msg) -> String
msg match {
    Email: msg.address,
    Push: ""
}
```


#### 列表推导式 (List Comprehensions)

这是从 Python 借鉴而来的强大特性，非常适合动态生成数组：

```relon
[x * 2 for x in range(5) if x % 2 == 0]
// 最终求值结果为：[0, 4, 8]
```

### 字典 (Dict)

字典对应 JSON 中的 Object，它的键必须是字符串或能够转为字符串的表达式。

#### 动态键名 (Dynamic Pathing)

在方括号 `[]` 内部，你可以写入任意表达式来动态计算键名：

```relon
{
    // strict 模式下给动态键表达式补一个类型提示
    [<String> "user_" + "42"]: "Alice"
}
```

当前动态键表达式按独立表达式求值，不会像普通字段值那样自动继承同一
dict 的字段作用域；不要在动态键里直接引用 sibling 字段。需要从外部
数据计算键名时，建议先由宿主整理成目标 dict，或暂时使用普通字段承载
计算结果，等动态键作用域实现收口后再内联。

#### 展开运算符 (Spread Operator)

你可以使用 `...` 语法将另一个列表或字典展开合并到当前集合中。如果在字典中展开，后出现的键会覆盖之前的同名键（v1.3 起，被静态检测到的同名冲突会升级为 `DuplicateField` 错误，详见 spec §6.6）。

```relon
{
    base: { host: "localhost", port: 80 },
    prod: {
        ...&sibling.base,
        port: 443 // 覆盖了 base 中的 port（若静态可推断会报 DuplicateField）
    }
}
```

**v1.3 typed spread**：在 `...` 后写 `<T>` 给 spread 源加类型提示：

```relon
#schema Extra { Int a: *, Int b: * }
#main(Extra src, Dict<String, Int> kv) -> Dict<String, Int>
{
    ...<Extra> src,
    ...<Dict<String, Int>> kv
}
```

严格模式下（默认行为），spread 源若不是 dict 字面量且没有 `<T>` 提示，
analyzer 会报 `SpreadSourceTypeUnknown`。

**v1.3 typed dynamic key**：在 `[` 后写 `<T>` 给动态键加类型提示：

```relon
{
    [<String> "key"]: "value",
    [<Int> 0]: "row0"
}
```

严格模式下（默认行为），动态键缺少 `<T>` 时报 `DynamicKeyTypeUnknown`。

#### `Dict<K, V>` 泛型（v1.3 显式纳入）

```relon
{ Dict<String, Int> scores: { math: 100, art: 90 } }
{ Dict<String, Result<Int, String>> tasks: { ... } }
```

`Dict`、`List`、`Closure`、`Fn` 都必须带泛型参数——v1.7 起，bare `Dict` / `List` / `Closure` 等会被分析器以 `BareGenericContainer` 拒绝（`Enum` 则是保留类型名，写在类型位置报 `ReservedTypeName`）。配合 v1.6 的 `Any` 退出用户面（`ExplicitAnyForbidden`），用户必须写明键值 / 元素类型。

## 文档根 (Document Root)

每个 `.relon` 文件求值后得到一个可投影为 JSON 的 Relon 值——Object、Array、String、Number、Bool，或投影为 JSON null 的 `None`。**根可以是任意表达式**：dict / list / tuple literal、原子字面量、二元 / 三元 / pipe 运算、函数调用、变体构造、引用、where / match 等，只要最终求值结果落在上述 JSON 类型集合内（前面允许有指令、装饰器、注释、空白）。

```text
// 合法：dict 根
{ "value": 42 }

// 合法：list 根
[1, 2, 3]

// 合法：tuple 根，输出投影为 JSON array
(1, "x")

// 合法：原子字面量也是 JSON 值
42
"hello"
true
None                  // 导出 JSON 时为 null

// 合法：顶层可以挂指令
#import string from "std/string"
{ "shouted": string.upper("hi") }

// 合法：在入口程序里 root 可以引用 #main 参数
#main(Int n) -> Int
n + 1

// 合法：变体构造直接作为根
#main(Order o) -> Result<Order, String>
Ok(o)
```

> 历史说明：v1.1 及之前版本仅允许 dict / list literal 作为根（裸标量或表达式会被 parser 拒绝）。v1.2 起放开为任意表达式（superset 扩展），旧脚本完全不受影响。这一放开使得 `#main(Int n) -> Int` body 可以直接写成 `n + 1`、`#main(...) -> Result<T, E>` body 可以直接写成 `Ok(...)`，而不必再用 `{ value: ... }` 这样的 dict 包一层。

`Closure` / `Schema` / `Type` 不属于 JSON 值；如果根表达式求值产出它们，host 端的 projector（如内置 `JsonProjector`）会以错误兜底（`UnsupportedClosure` / `UnsupportedSchema`）。在静态侧，`#main(...) -> ReturnType` 写裸 `Closure` 会先被 `BareGenericContainer` 拦截、未声明的类型名报 `UnknownTypeName`；body 静态类型与声明不符时 analyzer 报 `MainReturnTypeMismatch`。

## `@` 与 `#` —— 装饰器 vs 指令

Relon 把「附加在节点之上的元信息」分成两个互不重叠的命名空间：

- `@name(...)` —— **装饰器**：值变换（value transform）。如内置的
  `@value(...)` 或用户定义的任意可调用对象（`@my_fn(arg)` 等价于把
  下方的值传进 `my_fn` 的最后一个位置参数）。装饰器栈自下而上应用：
  `@a @b v ≡ a(b(v))`。
- `#name ...` —— **指令**：声明 / 结构 / 元数据。完整集合是
  `#main(...)`、`#schema X Body`、`#enum X { ... }`、`#import ... from "..."`、
  `#internal`、`#default`、`#expect`、`#msg`、`#error`、`#brand X`、
  `#relaxed`（同义词 `#unstrict`）、`#derive`、`#no_auto_derive`、
  `#native`、`#extend`。指令名是固定的、由宿主注册，不可由用户定义。

> 一个简单的判断：如果它**改变值**，那是 `@`；如果它**改变形状或
> 元信息**，那是 `#`。

## 字段可见性 — `#internal`

由于配置通常需要对外导出为纯净的 JSON，内部的逻辑需要被隐藏。Relon
通过 `#internal` 指令显式声明字段不对外可见：

```relon
{
    #internal
    String helper(String v): "<" + v + ">",
    String display: helper("hi")
}
// JSON 输出：{ "display": "<hi>" }   // helper 被隐藏
```

`#internal` 字段：

- 仍然存活在所属 dict 的局部作用域里，**同一个 dict 的其它字段可以
  引用它**（上例中 `display` 调用 `helper` 即正常）。
- **不会写入** dict 的导出 map：所以
  - 不会出现在 JSON 输出中；
  - 跨 dict 的 `&root` / `&sibling` 引用看不到它；
  - 通过 `#import lib from "..."` 后访问 `lib.private_field` 会以
    `VariableNotFound` 失败；
  - 通过 `#import * from "..."` spread 形式也不会被复制进当前作用域。

字典中如果值是一个**闭包（函数）**，默认 JSON 投影器会自动过滤掉它
——这是 `#internal` 之外的另一道防线，专门处理「Value 没有 JSON 表
示」的情况。

> 历史说明：早期版本曾用 `_` 前缀做隐式约定，并使用 `@private`
> 装饰器形式。两者都已**完全取消**：标识符仍然可以以 `_` 开头（例
> 如内部 intrinsic `_list_map`），但不代表任何可见性、导入或投影
> 行为；可见性请使用 `#internal` 指令。

## 严格模式 — 用 `#relaxed` 退出

Relon 的 analyzer 默认就是严格模式：当前文件以及它通过 `#import`
触达的所有模块都按「严格静态推断」校验。模块可以用文件级指令
`#relaxed`（同义词 `#unstrict`）退出：

```relon
#relaxed
#import * from "./lib.relon"
{ ok: 1 }
```

严格模式下，原本 analyzer 走 silent fallback 的位置（推不出来
时退化为 `Any` 让 runtime 兜底）会改报错：

- 没有类型提示的 spread（`{ ...e }`）→ `SpreadSourceTypeUnknown`
- 没有类型提示的动态键（`{ [k]: v }`）→ `DynamicKeyTypeUnknown`
- typed spread / dynamic key 的 `<T>` 引用未声明的 schema →
  `UnresolvedSchema`
- 调用未在 `host_fn_signatures` 登记返回类型的 native fn →
  `NativeFnSignatureMissing`

**按模块各管各的**：严格性是文件级的——每个模块默认严格，只能用它自己
头部的 `#relaxed` 退出。入口的 `#relaxed` 不会被印到它 import 的库上：
没写指令的库始终按严格校验，无论谁导入它。整程序「不漏 `Any`」靠边界
拦截——严格模块不会静态接受 `#relaxed` 依赖产出的 `Any`，会在使用点报错。

完整的严格模式语义、诊断列表、`<T>` typehint 语法、`Dict<K, V>`
泛型，详见规范 §6.6。

## 算术与逻辑运算符

Relon 支持标准的算术 (`+`, `-`, `*`, `/`, `%`) 和逻辑比较运算符 (`>`, `<`, `>=`, `<=`, `==`, `!=`, `&&`, `||`)，以及一元取反 `!`。另外，你还可以使用三元运算符处理条件：

```relon
#main(Int status_code, Bool is_active) -> Dict
{
    status: status_code == 200 ? "OK" : "Error",
    inactive: !is_active
}
```

此外还有管道运算符 `|`——把左侧的值作为右侧调用的第一个实参：
`xs | map((x) => x * 2)` 等价于 `map(xs, (x) => x * 2)`。
