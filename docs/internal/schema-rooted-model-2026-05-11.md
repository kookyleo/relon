# Schema-Rooted Model — Design Decisions (2026-05-11)

> 状态：设计冻结草案，Phase A 已落地（parser AST），Phase B 未启动。
>
> 本文记录将 Relon 的调用模型统一为「每个可调用都有 schema 根」的
> 重大重构所做的全部设计决策。type-constraints 系统
> ([`type-constraints-spec.md`](./archive/type-constraints-spec.md)) 是这套
> 模型的子集 —— constraint / `#derive` / `with { ... }` 在这套模型下
> 自然落位。

## 起点：当前模型的根问题

Relon v1.x 同时存在两条调用 dispatch 路径：

1. **命名空间全局函数**：`string.upper(s)` / `dict.merge(a, b)` /
   `math.abs(n)` / `len(x)` / `range(0, 10)`。这些名字注册到
   `Context.functions: HashMap<String, GatedNativeFn>`，按整段 dotted
   path 字符串查找。`string` / `dict` / `math` 不解析为任何值或
   schema，纯粹是注册名前缀
2. **（提议中的）值方法 dispatch**：`xs.method(args)` 按值的类型查
   schema 方法表

两条路径在 `path FnCall` AST 上无法区分，靠 analyzer「path[0] 是否
绑定到值」判定走哪条。这违反了 Logic-as-Data 的「显式可审计」精神：
读者不读到具体注册名 / 上下文，无法知道调用解析到哪一条路径。

## 原则：每个可调用都有 schema 根

**统一模型**：所有可调用都是 `Schema.method(args)` 或
`value.method(args)` 形态。`<head>.<method>(args)` 的 `<head>` 必须
解析为：

- 一个**值**（参数 / sibling / 闭包捕获 / ...） → `value.method` 走
  值类型的 schema 方法表
- 一个**schema 名** → `Schema.method` 走该 schema 的静态方法
- 都不是 → `UnresolvedReference` 错（**没有 fallthrough 到全局名**）

「全局命名空间函数」这个范畴**消失**。`string` 不再是命名空间，而是
作为 `String` 这个内置 schema 的方法集合体存在；调用形态从
`string.upper(s)` 变成 `String.upper(s)` 或 `s.upper()`。

## 决策摘要

| # | 决策 | 关键含义 |
| --- | --- | --- |
| 1 | **标量即 schema** | Int / Float / String / Bool / Null 是一等内置 schema，与用户 `#schema User` 同种 |
| 2 | **容器都是 schema，泛型参数化** | `List<T>` / `Dict<K,V>` / `Tuple` / `Optional` / `Result` / `Option` 都是 schema，方法以泛型参数化定义 |
| 3 | **schema 静态方法 + 受控 prelude** | 主路径 `Schema.method(args)` / `value.method(args)`；保留 3 个 prelude 糖名：`len(x)` / `range(s, e)` / `type(x)` |
| 4 | **声明 .relon、实现 Rust** | 内置 schema 在 `std_relon/<type>.relon` 用 `#schema X with { ... #native method() -> R }` 声明，host Rust 用 `register_method` 提供 `#native` 实现 |
| 5 | **全部 prelude，无需 import** | 所有内置 schema（标量+容器）默认在作用域内。`#import` 仅用于用户自定义模块 |
| 6 | **register API 立刻收口** | `register_pure_fn(name, fn)` / `register_fn(name, gate, fn)` 退役。新 API：`register_method(schema, name, gate, fn)`。stdlib 36 个 intrinsic 全部迁移 |
| 7 | **@ 全部 schema-rooted，无 tier 1 特例** | 含 `@value` 在内的所有 `@foo(args)` 都是 sugar for `<被装饰值>.foo(args)`，按值类型查方法表。`#expect` / `#default` / `#brand` 等是独立的 `#` 元属性机制（不归装饰器） |
| 8 | **#extend 显式扩展，含 built-in** | `#schema X { ... } with { ... }` 是初始声明（重复报错）；`#extend X with { ... }` 是显式扩展，可针对任意 schema 包括 built-in |
| 9 | **按 import 链可见** | `#extend` 加的方法只在该文件 + `#import` 链能到达它的文件中可见，不全局生效 |
| 10 | **内置 schema 省 body 形态** | `#schema String with { ... }`（无字段集 body） vs `#schema MyType { Int x: * } with { ... }`（dict-shape body）。parser 放松「body 必须存在」要求，无 body 表示「纯方法 holder」 |
| 11 | **不引入 nominal inheritance** | 已有的 `Schema + Schema` 组合 + `Enum<...>` sum types + constraint `#derive` 三条机制覆盖业务建模需求。OO 风格的 `Y extends X` 不在 spec 范畴内 |
| 12 | **`+` 在 schema 产出场景同时合并方法表** | `Schema + Schema` 和 `Schema + Dict_of_fields`（产出新 schema）合并字段 + 合并方法表；`Dict_value + Dict_value`（产出值）只合并字段，方法本来就由值的类型 schema 给，不存在合并 |
| 13a | **方法冲突报错（B 严）** | `Schema L + Schema R` 同名方法 → analyzer 报 `MethodNameConflict`。要覆盖父 schema 方法，结果 schema M 形成后用 `#extend M with { ... }` 显式重写 |
| 13b | **字段冲突 right-wins**（保持现状） | 与 v1.x 字段合并语义一致：`type_hint` 由 R 覆盖、`predicates` 累加（去 Wildcard）、`custom_error` / `default_value` 由 R 覆盖。**不报错**；字段是数据合并，方法是行为合并，二者粒度不同 |
| 14 | **prelude 中集：`len` / `range` / `type`** | 仅这 3 个名字作为顶层裸函数糖：`len(x)` ≡ `x.len()`、`range(s, e)` ≡ `Int.range(s, e)`、`type(x)` ≡ `x.type()`。其它 stdlib 函数（含 `string.upper` / `dict.merge` / `math.abs` 等）必须走 `Schema.method` / `value.method` 形态 |
| 15 | **auto-derive 默认 ON** | 用户 schema 默认结构推导出 `Equatable` 和 `JsonProjectable`（所有字段满足时）；要关闭显式写 `#no_auto_derive Equatable` / `#no_auto_derive JsonProjectable`。`Comparable` 不结构推导，必须显式 `#derive Comparable lt(...)` |

## 决策展开

### 1 + 2：所有类型都是 schema

```relon
// 用户 schema 与内置 schema 在语言层面同等
#schema User { String name: *, Int age: * } with {
    full_name() -> String: self.name
}

// 内置（在 std_relon/string.relon 声明，host 实现）
#schema String with {
    #native upper() -> String
    #native split(sep: String) -> List<String>
}

// 调用形态完全统一
"abc".upper()              // → "ABC"
String.upper("abc")        // 等价静态调用
my_user.full_name()        // → "Ada"
User.full_name(my_user)    // 等价静态调用
```

`InferredType` 中每个原子类型 variant 关联到对应 schema 的方法表；
analyzer 推导出操作数类型后查方法表。

### 3 + 14：方法 dispatch + prelude 糖

dispatch 算法（无 fallthrough）：

```text
解析 path[0]:
1. path[0] 是已绑定值（param / sibling / 闭包变量 / ...）：
   → 取它的静态类型 T，查 T 的 schema 方法表里的 path[1]
   → 命中 → 方法调用
   → 未命中 → MethodNotFound on T
2. path[0] 是 schema 名：
   → 查该 schema 的方法表里的 path[1]
   → 命中 → 静态方法调用
   → 未命中 → SchemaMethodNotFound
3. 都不是 → UnresolvedReference
```

prelude 糖（仅 3 个顶层裸函数名）：

```relon
len(x)         ≡ x.len()                 // dispatch 到 x 类型的 len 方法
range(s, e)    ≡ Int.range(s, e)         // 静态方法
type(x)        ≡ x.type()                // 反射

len([1, 2, 3])     // → 3，dispatch 到 List.len
len("abc")         // → 3，dispatch 到 String.len
len({a: 1, b: 2})  // → 2，dispatch 到 Dict.len
```

prelude 糖的展开发生在 analyzer，runtime 看到的是普通方法调用 AST。

### 4 + 5：声明位与 prelude

```relon
// std_relon/string.relon —— 声明（host 自动加载，无需 import）
#schema String with {
    #native upper() -> String
    #native split(sep: String) -> List<String>
    #native len() -> Int
    // ...
}
```

```rust
// host 启动时
ctx.register_method("String", "upper",
    NativeFnGate::default(),       // 纯函数
    Arc::new(StringUpper));
ctx.register_method("String", "len",
    NativeFnGate::default(),
    Arc::new(StringLen));
// ...
```

用户 .relon：

```relon
#main(String name) -> String:
    name.upper()        // String 自动在作用域，无需 #import
```

### 6：统一 register API

```rust
// 退役
ctx.register_pure_fn(name, fn);                    // 删
ctx.register_fn(name, gate, fn);                   // 删

// 新 canonical
ctx.register_method(schema_name, method_name, gate, fn);

// 例
ctx.register_method("String", "upper",
    NativeFnGate::default(), Arc::new(StringUpper));
ctx.register_method("Money", "fetch_rate",
    NativeFnGate { network: true, ..NativeFnGate::default() },
    Arc::new(FetchRate));
```

stdlib 36 个 intrinsic 全部迁移到 `register_method`。stdlib 纯度守门
测试同步更新（`stdlib.rs` 的 ban-list 不变，但所有调用都换形态）。

### 7：装饰器 = schema 方法

```relon
#schema Float with {
    #native round(n: Int) -> Float
}

#schema User {
    @round(2)
    Float weight: 70.5
}
// analyzer 看到 @round(2) 装饰一个 Float 字段值
// → lower 成 weight.round(2)，调用 Float.round 方法
```

`@value` 不再是特殊 plugin —— 它就是每个 schema auto-derived 的
`value(replacement) -> Self` 方法（body 是 `replacement`），analyzer
可以平凡化为「直接产出 replacement」。

`#default` / `#expect` / `#msg` / `#error` / `#brand` / `#private` 是
独立的 `#` directive 元属性机制，**不**走装饰器路径，host 通过
`register_directive_meta` 之类的独立 API 注册（不复用
`register_method`）。

### 8 + 9：扩展类型

```relon
// my-extensions.relon
#extend String with {
    is_email() -> Bool: ...    // 用户给 built-in String 加方法
}

#extend MyType with {
    summarize() -> String: ...
}
```

```relon
// other-file.relon
#import "my-extensions"

#main(String s) -> Bool:
    s.is_email()        // ✅ 通过 import 链可见
```

```relon
// no-import.relon
#main(String s) -> Bool:
    s.is_email()        // ✗ MethodNotFound on String —— 没 import
```

`#schema X { ... } with { ... }` 是 X 的初次声明（重复报
`SchemaRedefined`）；`#extend X with { ... }` 是后续扩展。analyzer 在
workspace 闭合时跨 module 合并方法表，按 import 可达性确定每个文件
能看到的方法集。

### 10：省 body 形态

```relon
// 内置 schema：纯方法 holder
#schema String with { #native upper() -> String }
#schema Int with { #native abs() -> Int }
#schema List<T> with { #native map<U>(f: Closure<(T) -> U>) -> List<U> }

// 用户 schema：dict-shape body + 可选 with
#schema User { String name: * } with {
    full_name() -> String: self.name
}
```

parser 改动：`#schema X with { ... }`（无 body）合法。Phase A 当前要
求 body 必须存在，需要在 Phase B 前微调。

### 11：不引入 inheritance

业务建模 6 类常见用例（content / role / event variant / form field /
strategy / hierarchy）的现有覆盖：

| 用例 | 现有机制 |
| --- | --- |
| 内容层级 | `Schema + Schema` 字段加 + 方法合（决策 12）+ constraint |
| 角色 / 权限 | constraint + `#derive` |
| 事件 / 状态 / 变体 | `Enum<...>` sum types |
| 表单字段 | sum types 或 composition |
| Strategy | sum types + constraint |
| 严格嵌套 | composition 链 |

唯一覆盖不干净的是「严格 4+ 层嵌套 hierarchy」 —— 实际很少。决策 12
让 `+` 同时合并方法表后，3 层以内 hierarchy 已经覆盖。

### 12 + 13：`+` 合并语义

| 操作数 | 结果 | 字段 | 方法 |
| --- | --- | --- | --- |
| Schema L + Schema R | Schema | right-wins + predicates 累加 | 同名报错（B 严） |
| Schema + typed-fields dict | Schema | right-wins + predicates 累加 | 继承左边（dict 一侧无方法） |
| Dict_value + Dict_value | Dict_value | right-wins + predicates 累加 + Identity Guard 重校验 | n/a（值无方法表） |

冲突逃生：

```relon
#schema L with { summary() -> String: "L" }
#schema R with { summary() -> String: "R" }

#schema M &sibling.L + &sibling.R   // ✗ MethodNameConflict on summary

// 修复方式 1：M 不组合，M 直接 #extend 写
#schema M &sibling.L
#extend M with { summary() -> String: "M-version" }   // 改写父方法

// 修复方式 2：rename 其中一边
#schema R with { summary_r() -> String: "R" }         // R 改名
#schema M &sibling.L + &sibling.R                     // 不再冲突
```

### 14：prelude 糖

仅 `len` / `range` / `type`。其它一切（含 v1.x 当前的 `string.upper`
等）必须改写为 `Schema.method(...)` / `value.method(...)`。stdlib 重组
是 Phase B 的主体工作之一。

### 15：auto-derive

| Constraint | 默认 | 关闭方式 |
| --- | --- | --- |
| Equatable | ON（结构推导） | `with { #no_auto_derive Equatable }` |
| JsonProjectable | ON（结构推导） | `with { #no_auto_derive JsonProjectable }` |
| Comparable | OFF | 必须 `#derive Comparable` 显式提供 `lt(other: Self) -> Bool` 方法 |

结构推导规则与 type-constraints-spec §"结构性推导规则细节" 一致。

## 实施阶段

| 阶段 | 内容 | 状态 |
| --- | --- | --- |
| **A** | parser AST：`with { ... }` 块、`#derive` / `#native` / `#no_auto_derive` 三个 directive、`SchemaMethod` AST | ✅ 落地（commit `1f...`，2026-05-10） |
| **A.1** | parser 微调：`#schema X with { ... }` 省 body 合法 + `#extend X with { ... }` 新 directive | 待启动 |
| **B** | analyzer 方法表 + dispatch 算法（值方法 + 静态方法 + prelude 糖展开 + 跨 module #extend 合并 + `+` 合并方法）；evaluator 方法调用 | 待启动 |
| **C** | constraint 模型 + 结构化推导（Equatable / JsonProjectable auto-derive）+ witness 签名校验（`#derive Equatable/Comparable`）+ operator lowering（`<` `>` 等到 `lt` / `eq` 调用） | 待启动 |
| **D** | host API：`register_method` / `register_directive_meta` 落地 + stdlib 36 个 intrinsic 全部迁移 + 内置 schema `.relon` 声明文件创建 | 待启动 |
| **E** | 用户文档：spec / host-integration / sandbox 全部对齐新模型；CHANGELOG 重大破坏性条目 | 待启动 |

## 与 type-constraints-spec 的关系

[`type-constraints-spec.md`](./archive/type-constraints-spec.md) 描述
`#derive Constraint` / `with { ... }` / 7 个内置 constraint /
operator lowering 等机制。

本文是上层模型，type-constraints 自然落位为：

- `with { ... }` —— 本模型的核心语法（决策 1/2/4/8/10）
- `#derive C` —— 决策 15 的显式入口（用于 `Comparable` 强制声明、
  以及 user 给类型显式 opt-in 自定义 constraint 时）
- `#no_auto_derive C` —— 决策 15 的关闭开关
- 7 个 constraint —— 在新模型下，每个 constraint 是 schema 方法表的
  一个「期望子集」描述

实施时 type-constraints-spec 的内容大部分自动落实，但要重新核对：

- `#native` 关键字的语义已经被本模型用于「方法体在 host」（决策 4）
- 内置类型（Int / String 等）的 constraint 满足规则要更新为「这些
  schema 默认提供 `eq` / `lt` 等 witness 方法」（用 host
  `register_method` 注册）

## 已决细节（5 条尾巴）

### 16：方法 visibility — `#private` = schema 内部可见

```relon
#schema Money { Int cents: * } with {
    format() -> String:
        self.amount_string() + " " + self.currency_code()    // ✅ 同 schema 其它方法可调

    #private
    amount_string() -> String: f"${self.cents / 100}"        // 内部 helper

    #private
    currency_code() -> String: "USD"
}

#main(Money m) -> String:
    m.format()              // ✅ 公开方法
    m.amount_string()       // ✗ MethodNotFound: amount_string is private
```

`#private` 是方法上的标记 directive（method-level pragma，跟 `#derive`
/ `#native` 同位置）。可见性按 **schema 身份**判定，不论文件 / module
位置：所有 `format/amount_string/currency_code` 都属于 `Money` schema 的
方法表，分两层 —— 公开层（脚本可见）与私有层（仅其它同 schema 方法体
内可见）。

### 17：Tuple 位置访问保留为语言原语

`xs.0` / `xs.1` 这种位置访问由 parser/analyzer 作 `TupleIndex` AST 节点
处理，**不走方法表 dispatch**。`Tuple<T1, T2, ...>` 仍是 schema（占位
角色），允许未来挂少量普通方法（如 `.len()` 返回 arity，`.swap()`
等），位置访问与方法访问语义独立。

```relon
#main((Int, String) pair) -> String:
    pair.0          // ✅ TupleIndex(0) AST，编译期 arity 检查
    pair.1          // ✅
    pair.len()      // ✅ Tuple schema 上的方法
    pair.0()        // ✗ TupleIndex 不可调用
```

### 18：`#extend` + `#derive` 的耦合

只能在 `#extend` 里 `#derive` 那些**原始 `#schema` 声明里未 derive
过**的 constraint。

```relon
// fileA.relon
#schema MyData { Int v: * }              // 没写 #derive

// fileB.relon
#import "fileA"
#extend MyData with {
    #derive Equatable
    eq(other: Self) -> Bool: self.v == other.v
}                                        // ✅ MyData 之前没 derive Equatable

// fileC.relon
#import "fileB"
#extend MyData with {
    #derive Equatable                    // ✗ 已经 derive 过（在 fileB），冲突
    eq(other: Self) -> Bool: ...
}
```

实务等价：每个 constraint 在一个 schema 上**最多 derive 一次**，无论
原始声明位还是 `#extend` 位。冲突由 analyzer 在 import 闭合时跨 module
检测。

### 19：conflict detection 时机 — 每文件 import 闭包视角

冲突判定**不是 workspace 全局**，而是**每个文件视角**：

```text
对每个模块 M：
  collapse_extend_table = ∅
  for each module N in M's import closure:
    for each #extend declaration in N:
      merge into collapse_extend_table
      if same (schema, method) name with different signature → MethodNameConflict at M
      if same (schema, constraint) #derive twice → DeriveConflict at M
  M 的方法解析使用这张合并后的表
```

含义：

- 两个独立 lib L1 / L2 各自 `#extend String with { sanitize() ... }` 实现不同 → 各自合法
- 文件 F 同时 `#import L1` + `#import L2` → F 的视角看到冲突，analyzer 在 F 这个模块的 pass 里报错
- 文件 G 只 `#import L1` → G 没事

### 20：decorator 链 lowering — 链式方法调用

`@a @b @c v` 等价 `a(b(c(v)))`，schema-rooted 形态：

```text
@c v       →  v.c()                         // 按 v 的类型查 c 方法
@b (...)   →  v.c().b()                     // 按 v.c() 返回类型查 b 方法
@a (...)   →  v.c().b().a()                 // 按 v.c().b() 返回类型查 a 方法
```

每一步独立做类型推导 + 方法表查找。a / b / c **可以在不同的
schema 上** —— 譬如 `@uppercase @parse_int "42"` 流是
`String → Int → Int`，第一步在 String 上找 `parse_int`，第二步在 Int
上找 `uppercase`（如果 Int 没有 uppercase 就在第二步报
`MethodNotFound on Int`）。

## 4 个剩余 constraint 的 lowering（决策 21-24）

`Equatable` / `Comparable` / `JsonProjectable` 已经定型（决策 15、18），
constraint 体系闭合还差 4 项：迭代、索引、可调用、算术。逐项 design out。

### 21：Iterable — `iter() -> Iter<T>` 真迭代器

`for x in c` / 列表推导式 `[for x in c: ...]` 编译到 `c.iter()` 拿
一个 `Iter<T>`，循环里反复 `it.next()` 直到 `None`。

```relon
// constraint witness
#extend MyCollection with {
    #derive Iterable
    iter() -> Iter<Item>: ...
}

// 新增 prelude schema（与 Optional / Result 同级）
#schema Iter<T> with {
    #native next() -> Optional<T>
}

// 编译路径：
for x in c: body
// ↓ desugar
let it = c.iter()
loop {
    match it.next() {
        Some(x) => body,
        None => break,
    }
}
```

**为什么不选 `to_list()` desugar**：lazy 表达力是 Iterable 区别于
List 的核心；用 `to_list()` 会让大集合 / 无限序列 / IO-driven 流
退化为 eager List。`Iter<T>` 与 immutable 模型不冲突 —— `next()`
返回 `Optional<T>`，外部传 `it`（Iter 本身是个值），调一次拿一个元素，
迭代器的「状态推进」由 host `#native` 实现内部封装（host 端可以是
mutable，但 relon 源码层面只看到「函数返回下一个元素」）。这条
设计与 Closure 已经能持有捕获环境的语义对称。

**List / Dict / String 的内置 `iter()`**：在 `core.relon`（决策
21' 见下）声明，host 提供 `register_pure_method("List", "iter", ...)`
等实现。

### 22：Indexable — `index(key: K) -> Optional<V>`

`a[i]` 编译到 `a.index(i)`；返回类型固定是 `Optional<V>`。用户必须
显式 `a[i]?` / `a[i] ?? d` 才能拿到 V，与 path-tail 可选访问语义
（`obj.field?`）一致。

```relon
// constraint witness
#extend Sparse<K, V> with {
    #derive Indexable
    index(key: K) -> Optional<V>: ...
}

// 用户层
let v: V          = a[i]?           // panic if None
let v: V          = a[i] ?? default
let vo: Optional<V> = a[i]          // 不 unwrap

// List / Dict / String 内置 index 一律走 Optional
let s: Optional<String> = arr[3]
let n: Optional<Int>    = dict["count"]
```

**为什么不允许 `index() -> V` panic 风格**：

1. 与现有 `a.field?` path-tail 语义对齐 —— 同一个「访问可能失败」直觉，
   不应该按容器形态分裂两套规则。
2. panic 风格会让 a[i] 的可失败性从类型上消失，static analysis 无从
   下手；强制 `Optional<V>` 把「可能失败」推到类型签名。
3. `a[i]?` 一行就能恢复 panic 风格，写起来不贵。

### 23：Callable — 不引入

用户 schema **不能**直接 `f(args)` 调用；保留「只有 `Closure` 是可调用值」
的语义。需要「值作为函数」体验的 builder / functor 模式，用具名 method：

```relon
// builder 模式
#schema Greeter { String prefix: * } with {
    invoke(name: String) -> String:
        f"${self.prefix} ${name}"
}

let g = { Greeter: { prefix: "hi" } }
let s = g.invoke("Ada")    // ✅ 具名 method
// let s = g("Ada")        // ✗ 编译错：g 不是 Closure
```

**为什么不引入**：

1. `f(args)` 已经是 FnCall 的字面形态，加 `Callable` 会让「path[0]
   是不是 Closure」的判断变成「path[0] 的 schema 上有没有 call witness」
   的多分支查表 —— hot path 复杂化，错误信息也更难指给用户看（「这个
   不是函数」 vs 「这个 schema 没实现 Callable」）。
2. Closure 已经能捕获状态，覆盖 functor 用例。builder 模式
   `g.invoke(...)` 比 `g(...)` 多打 7 个字符，但语义更显式（读者立刻
   看出 g 是 schema 不是 fn）。
3. 这是 «keep the closed door closed» 决策 —— 不开 trait 多态调用，
   就少一个 dispatch 路径需要维护、文档化、教学。

constraint 注册表把 `Callable` 项目**从 spec 删除**，constraint
体系 = {Equatable, Comparable, JsonProjectable, Iterable, Indexable,
Addable, Subtractable, Multiplicable, Divisible, Modable}，共 10 项。

### 24：Number — 拆细为 5 个独立 constraint

`+` `-` `*` `/` `%` 各走独立 constraint，按需 `#derive`：

| Operator | Constraint     | Witness                          |
|----------|----------------|----------------------------------|
| `+`      | Addable        | `add(other: Self) -> Self`       |
| `-`      | Subtractable   | `sub(other: Self) -> Self`       |
| `*`      | Multiplicable  | `mul(other: Self) -> Self`       |
| `/`      | Divisible      | `div(other: Self) -> Self`       |
| `%`      | Modable        | `rem(other: Self) -> Self`       |

```relon
#schema Vec2 { Float x: *, Float y: * } with {
    #derive Addable
    add(other: Self) -> Self:
        { Vec2: { x: self.x + other.x, y: self.y + other.y } }
    #derive Subtractable
    sub(other: Self) -> Self:
        { Vec2: { x: self.x - other.x, y: self.y - other.y } }
}

let u: Vec2 = ...
let v: Vec2 = ...
let w = u + v   // 命中 Vec2.add
let z = u - v   // 命中 Vec2.sub
// let r = u * v  // ✗ 编译错：Vec2 not Multiplicable
```

**为什么拆细，不要一个统一的 `Number`**：

1. 大量场景只需要部分算子。向量加法不该被迫定义 `*`（点乘？外积？
   两者语义不同），写「我们就不让 `*` 编译」比「随便实现一个」更安全。
2. 与 Iterable / Indexable / Equatable 单 witness 的粒度对齐 ——
   constraint 体系里「一个 constraint = 一个 method」是统一形状，
   `Number` 5 method 一组反而是异类。
3. analyzer 的 `ConstraintWitnessShapeMismatch` 检查每次扫一个
   witness，5 个并立的 constraint 比 5-method 单 constraint 错误
   信息更精准（直接说「Subtractable 的 sub method 形状不对」而不是
   「Number 的第 2 项不对」）。

**primitives 不参与**：Int / Float / String 的 `+` `-` `*` `/`
现有语义（含 String concat、numeric promotion）保留 fallback 路径，
constraint dispatch 仅当 LHS 是 branded value 且 schema 上有
对应 witness 时才命中。与决策 15 「Equatable on by default for
user schema」对称：constraint 体系**只给用户 schema 添加行为**，
不替换 primitives 的 hardcoded 语义。

**unary minus**：`-x` 独立成 `Negatable` constraint
（`neg(self) -> Self`），不计入这 5 个里 —— `Subtractable` 是二元，
`Negatable` 是一元，witness shape 不同。本批延后到「真有需要」再开。

### 21'：core.relon 载体（统一 dispatch entry point）

跟决策 4 呼应：内置 schema 的方法在 `crates/relon-evaluator/src/core/*.relon`
（或单一 `core.relon`）声明，编译期 `include_str!` 内嵌，analyzer 启动时
always-on 加载到 `tree.schema_methods`。host Rust 通过
`register_pure_method` 提供 `#native` 实现。

```relon
// core/string.relon（schema-rooted 载体）
#schema String with {
    #native upper() -> String
    #native lower() -> String
    #native split(sep: String) -> List<String>
    #native replace(from: String, to: String) -> String
    #native contains(sub: String) -> Bool
    #native len() -> Int
}

// core/list.relon
#schema List<T> with {
    #native map<U>(f: Closure<(T) -> U>) -> List<U>
    #native filter(f: Closure<(T) -> Bool>) -> List<T>
    #native reduce<U>(init: U, f: Closure<(U, T) -> U>) -> U
    #native contains(x: T) -> Bool
    #native iter() -> Iter<T>
    #native len() -> Int
}

// core/dict.relon
#schema Dict<K, V> with {
    #native merge(other: Self) -> Self
    #native keys() -> List<K>
    #native values() -> List<V>
    #native has_key(k: K) -> Bool
    #native iter() -> Iter<Tuple<K, V>>
    #native len() -> Int
}

// core/iter.relon
#schema Iter<T> with {
    #native next() -> Optional<T>
}
```

用户写 `s.upper()` / `lst.map(f)` 直接命中 —— analyzer 看到 core
schema 的 method 声明，evaluator 走 `register_pure_method` 提供的
host 实现。零 `#extend` boilerplate，零特例：core 库与用户库走同
一条 dispatch path。

## 一句话定性

Relon v1.x 的两条调用 dispatch 路径（命名空间全局函数 / 值方法）合并
为单一「schema-rooted」路径。每个可调用都有一个 schema 根；调用形态
统一为 `Schema.method(args)` 或 `value.method(args)`。这把「调用如何
解析」从「读者要查上下文 + 注册表」简化为「读者按 path[0] 类型查方法
表」 —— 与 Logic-as-Data 的「显式可审计」原则一致。
