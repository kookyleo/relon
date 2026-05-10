# Schema-Rooted Model — Design Decisions (2026-05-11)

> 状态：设计冻结草案，Phase A 已落地（parser AST），Phase B 未启动。
>
> 本文记录将 Relon 的调用模型统一为「每个可调用都有 schema 根」的
> 重大重构所做的全部设计决策。type-constraints 系统
> ([`type-constraints-spec.md`](./type-constraints-spec.md)) 是这套
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
| 2 | **容器都是 schema，泛型参数化** | List<T> / Dict<K,V> / Tuple / Optional / Result / Option 都是 schema，方法以泛型参数化定义 |
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

[`type-constraints-spec.md`](./type-constraints-spec.md) 描述
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

## 未决问题

以下不影响 Phase A.1 / B 启动，但实施时要决定：

- **visibility on extension methods**：`#extend X with { #private foo() ... }` 是否合法？私有作用域如何（仅文件、仅 schema 定义模块）？
- **Tuple 的方法表表达**：`Tuple<T1, T2, ...>` arity 不定，方法（如 `.0` `.1` 位置访问）怎么落到统一方法表？或位置访问保留为语言原语，不走方法表？
- **conflict detection 在 import 闭合时的具体算法**：跨 module `#extend` 合并的冲突应在哪一阶段触发（每个 module 独立分析后合并、还是 workspace 全局收口时检测）？
- **decorator 链 `@a @b v` 的 lowering 形态**：`a(b(v))` 还是 `v.b().a()`？两者等价但 AST 形态影响 analyzer 推导
- **constraint 与 `#extend` 的耦合**：用户 `#extend X with { #derive Equatable eq() ... }` 是否给 X 添加 Equatable derive？

## 一句话定性

Relon v1.x 的两条调用 dispatch 路径（命名空间全局函数 / 值方法）合并
为单一「schema-rooted」路径。每个可调用都有一个 schema 根；调用形态
统一为 `Schema.method(args)` 或 `value.method(args)`。这把「调用如何
解析」从「读者要查上下文 + 注册表」简化为「读者按 path[0] 类型查方法
表」 —— 与 Logic-as-Data 的「显式可审计」原则一致。
