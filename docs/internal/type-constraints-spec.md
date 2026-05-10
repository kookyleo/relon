# Relon Type Constraints Feature Spec

> 状态：草案，尚未实现。
>
> 当前 parser / analyzer / evaluator 还不支持 `#schema ... with { ... }`、
> `#derive`、`#native`、`#no_auto_derive` 或 schema method table。
> 本文是后续 trait-bound / schema-method 系统的候选设计，不是当前
> 语言规范。
>
> 本文是内部 feature spec，不进公开文档侧边栏。目标是固定 Relon
> 语言层的 `Constraint` 语义，以及 `.relon` schema method
> 和 Host 注册实现之间的静态对照规则。
>
> 前置依赖：host capability 模型需要先扩展到 clock / env / RNG /
> network / writes_fs 等能力位，因为本文里的 method 纯度与权限对照
> 需要复用同一套 gate 元数据。

## 背景

Relon 的方向是：尽可能在分析期发现问题，只有依赖运行时输入、host
native 结果或真实数据分支的问题才留到求值期。

因此需要一种轻量的类型约束机制，让 analyzer 能回答：

- 某个类型是否能 JSON 投影？
- 某个类型是否能结构等值比较？
- 某个类型是否能被 `<` / `>` 比较？
- 某个方法是否定义在某个 schema 的 `with { ... }` 方法段里？
- `.relon` 声明和 Host 注册实现是否签名一致？

这个机制命名为 **Constraint**。

注意：本文中的 `Constraint` 是语言层静态类型约束，不是 evaluator
里的 sandbox `Capabilities`。实现时不要把两者混在一个结构里。

## 目标

- 提供封闭、可穷举、可跨 runtime 对齐的语言层 constraint 集合。
- 允许 schema 后紧跟 `with { ... }` method 段，组织绑定在该 schema 上
  的方法。
- 让 analyzer 对 schema method 做静态类型检查。
- 让 analyzer 对 `.relon` method 声明和 Host 注册签名做静态对照。
- 不引入 Rust-style trait / impl resolution。
- 不引入开放式 operator overloading；只允许内置 constraint 明确定义
  的 operator witness。

## 非目标

- 不支持用户定义新 constraint。
- 不支持 `impl Trait for Type`。
- 不支持根据任意业务函数名猜测 constraint；只有内置 constraint 规
  定的 witness 方法名称和签名，并且带 method 级 `#derive` 标记时，
  才参与满足关系。
- 不允许 Host Rust trait 直接泄漏到 `.relon` 类型系统。

## Constraint 模型

Constraint 是语言内置的封闭类型约束集合。用户不能定义新
constraint，也不能写 Rust-style `impl Constraint for Type`。

类型满足 constraint 的来源只有三类：

- 内置类型和容器的固定规则。
- analyzer 对 schema / enum 做结构性推导。
- `with { ... }` 中被 `#derive BuiltinConstraint` 标记的显式
  witness method。

`#derive` 在显式 witness 场景下是 method 级标记，不修饰整个 schema。
如果某个 constraint 需要多个 witness method，每个 method 都要重复
标记对应的 `#derive`，以换取局部清晰。

结构性推导默认开启。需要禁止某个 schema / enum 的结构性推导时，使
用负向声明：

```relon
#schema InternalToken {
    String value: *
} with {
    #no_auto_derive JsonProjectable
}
```

第一版 public constraint 集合如下：

| Constraint | 含义 | 获得方式 |
| --- | --- | --- |
| `Number` | 数值类型 | `Int` / `Float` 内置满足 |
| `Equatable` | 可做确定性等值比较 | 结构推导，或 `#derive Equatable` 标记 `eq` witness |
| `Comparable` | 可做 `<` / `>` / `<=` / `>=` | `Number` 内置满足，或 `#derive Comparable` 标记 `lt` witness |
| `JsonProjectable` | 可投影为 JSON | 默认结构推导，可用 `#no_auto_derive JsonProjectable` 禁用 |
| `Iterable<T>` | 可被迭代 | `List<T>` 内置满足 |
| `Indexable<K, V>` | 可按 key/index 读取 | `List<T>` / `Dict<String, V>` 内置满足 |
| `Callable<Args, Ret>` | 可调用 | closure / host fn / std fn 签名 |

推导规则：

- `Number`：`Int`、`Float` 满足。`Number` 也是现有类型系统中的数值
  slot，constraint 系统复用这个名字。
- `Comparable`：`Number` 内置满足。用户 schema 只有在
  `lt(other: Self) -> Bool` method 上标记 `#derive Comparable`，
  且自身满足 `Equatable` 时才满足。`Comparable` **没有结构性默认推
  导** —— 类型不会自动满足，必须显式 `#derive`。Analyzer 用 `lt`
  和 `eq` 把 `>` / `<=` / `>=` 脱糖为下面的规范 lowering，runtime
  不做动态分派：

  ```text
  a < b   ==>  a.lt(b)
  a > b   ==>  b.lt(a)
  a <= b  ==>  a.lt(b) or a.eq(b)
  a >= b  ==>  b.lt(a) or a.eq(b)
  a == b  ==>  a.eq(b)        // 走 Equatable witness 或结构等值
  a != b  ==>  not a.eq(b)
  ```

  `<=` / `>=` 形式上调用了 `eq` 与 `lt` 两次：analyzer 在 const-fold
  阶段会消除显然不可能的分支，但 lowering 形式本身不省略，便于代码
  生成与诊断对齐。
- `Equatable`：`Null`、`Bool`、`Int`、`Float`、`String` 满足；
  `List<T>` 在 `T: Equatable` 时满足；`Dict<String, V>` 在
  `V: Equatable` 时满足。schema / enum 默认按字段或 payload 递归
  推导，除非声明 `#no_auto_derive Equatable`。也可以在
  `eq(other: Self) -> Bool` method 上标记 `#derive Equatable`，覆盖
  默认结构等值语义。
  `Closure`、`Schema`、`EnumSchema` 这类 runtime-only 值不满足。
- `JsonProjectable`：JSON 基础类型满足；`List<T>` 在
  `T: JsonProjectable` 时满足；`Dict<String, V>` 在
  `V: JsonProjectable` 时满足。schema / enum 默认按字段或 payload
  递归推导，除非声明 `#no_auto_derive JsonProjectable`。非有限 `Float`
  是值级错误，仍由 projector/runtime 报。

#### 结构性推导的规则细节

`Equatable` / `JsonProjectable` 的结构推导按下列规则展开（`Comparable`
没有结构性默认，不在此列）：

- **schema 字段**：公开声明字段（含 optional `T?` 与默认值字段）纳
  入推导。当前语言已有 `#private` 字段可见性语义；本草案暂定
  `#private` 字段不参与默认 `JsonProjectable` / `Equatable` 结构性
  推导。需要让私有状态参与等值语义时，应写显式 `eq` witness。
  `with { ... }` 内部的 method 不引入字段。
- **enum 变体**：每个变体的 payload schema 必须满足同一个 constraint。
  payload 的字段顺序**不影响**推导结果（结构性 constraint 看的是字段
  集合，不是顺序）—— 但 `eq` 的 witness 派生比较时仍按 schema 声明
  顺序逐字段比对，方便保留可读的 trace。
- **递归失败**：任一字段（或任一变体的 payload）不满足该 constraint
  时，整个 schema / enum **不满足**该 constraint。Analyzer 报对应
  `Missing*` 诊断时，**只指向第一个失败的字段**（深度优先、声明
  顺序），避免在嵌套很深时刷屏。
- **递归类型**：自递归的 schema（如 `Tree { children: List<Tree> }`）
  按「乐观假设」推导 —— 先认为 `Tree` 满足 constraint，再校验所有字
  段；只要不出现「除自递归外的不满足字段」就接受。这与 Rust 的
  `derive(PartialEq)` 处理 `Vec<Self>` 的方式一致。
- `Iterable<T>`：v1 只有 `List<T>` 满足。`Dict` 通过 `dict.keys` /
  `dict.values` 显式转成 list。
- `Indexable<K, V>`：`List<T>` 满足 `Indexable<Int, T>`；
  `Dict<String, V>` 满足 `Indexable<String, V>`。
- `Callable<Args, Ret>`：由 `.relon` 函数签名、stdlib 签名或 Host
  注册签名给出。

Analyzer 可以有内部分类，例如 `SchemaLike` / `EnumLike`，但它们不
是 public constraint，不能出现在用户签名里。

第一版不提供 `.relon` 语法去定义新 constraint 或 Rust-style impl：

```relon
#impl Comparable for Money
```

如果业务需要 Money 比较，业务必须在 `with { ... }` 中显式提供
`Comparable` 所需 witness 方法，并在该 method 上用
`#derive Comparable` 标记它是内置 constraint 的 witness。

## Constraint Witness

部分 constraint 有固定 witness 方法。Witness 方法是 `with { ... }`
下的普通方法，自带 `self: Self`，但名称和签名由 constraint 定义。
Analyzer 会把方法脱糖为带显式 receiver 的内部签名：

```text
Money.lt(other: Money) -> Bool
==> Money.lt(self: Money, other: Money) -> Bool
```

| Constraint | 必需 witness | 说明 |
| --- | --- | --- |
| `Equatable` | `eq(other: Self) -> Bool` | 显式等值语义；没有显式 witness 时可按结构推导 |
| `Comparable` | `lt(other: Self) -> Bool`，且 `Self: Equatable` | `<` 直接调用 `lt`；其它比较由 `lt` + `eq` 派生 |

Witness 识别由 method 级 `#derive` 触发。也就是说，只有 method
紧前方标记了 `#derive Equatable` / `#derive Comparable`，analyzer
才会把 `eq` / `lt` 当作 constraint witness 检查。

在 `#derive` 触发后，witness 必须精确匹配：owner、函数名、参数数
量、参数类型、返回类型、纯度/权限元信息都要符合 constraint 定义。
名字相同但签名不匹配时必须报 analyzer 错误，不能降级当普通 schema
method 处理。

没有对应 method 级 `#derive` 时，`eq` / `lt` 只是普通 schema
method。它们不会覆盖结构性 `Equatable`，不会让类型满足
`Comparable`，也不会启用 `<` 这类 operator。

如果一个内置 constraint 将来需要多个 witness method，每个 method
都必须重复标记该 constraint。也就是说，不引入“上一行 `#derive`
修饰后续多个 method”的语义。

用于 operator 的 witness 方法必须是 pure，不能声明 `reads_fs` / network
这类 host capability。需要外部状态的业务比较应保持显式函数调用，
不要让类型满足 `Comparable`。

示例：

```relon
#schema Money {
    Int cents: *
    String currency: *
} with {
    #derive Equatable
    eq(other: Self) -> Bool:
        self.currency == other.currency && self.cents == other.cents

    #derive Comparable
    lt(other: Self) -> Bool:
        self.currency == other.currency && self.cents < other.cents
}
```

`#derive Equatable` 让 analyzer 检查 `Money.eq`，并让 `Money` 满足
`Equatable`；`#derive Comparable` 让 analyzer 检查 `Money.lt`，加上
`Money: Equatable` 后让 `Money` 满足 `Comparable`。Analyzer 可以
因此允许：

```relon
price == limit
price < limit
```

但这个 operator 绑定是静态的：analyzer 在类型已知时把 operator
绑定到内置 constraint 的 witness，不存在运行期 trait lookup。

Analyzer 能强制的是 witness 的签名、纯度和可达性；无法证明 `eq`
满足自反/对称/传递，也无法证明 `lt` 满足严格弱序。业务代码和 Host
native 实现必须承担这些语义律。

## 命名与冲突规则

Constraint 名称是全局保留名。用户不能定义同名 schema、module
alias、import alias、顶层函数或 schema method owner。

Schema method 使用完整名：

```text
<schema-name>.<fn-name>
```

因此下面两个方法不冲突：

```relon
#schema Money {
    Int cents: *
} with {
    format() -> String: f"${self.cents}"
}

#schema User {
    String name: *
} with {
    format() -> String: self.name
}
```

它们的完整名分别是 `Money.format` 和 `User.format`。

同一个 owner 下不能重名：

```relon
#schema Money {
    Int cents: *
} with {
    format() -> String: "a"
    format() -> String: "b" // analyzer error
}
```

Schema method 和普通顶层函数可以同名，因为调用路径不同：

```relon
format(v) -> String: "global"

#schema Money {
    Int cents: *
} with {
    format() -> String: "money"
}
```

这里 `format(x)` 和 `price.format()` 是不同调用。

同一作用域下 schema 名、module alias、import alias 不能同名。否则
`Money.format` 无法稳定判断 `Money` 是 schema owner 还是模块绑定。

Host 注册 schema method 也必须使用 `(owner, name)` 二元组。默认规
则：

- `.relon` 有函数体，Host 又注册同名 `(owner, name)`：冲突。
- `.relon` 用 `#native` 只声明 method 签名：Host 必须注册同名实现。
- `.relon #native` 声明和 Host 注册签名不一致：冲突。
- Host 注册了源码中不存在的 method：默认诊断，除非 host 显式启
  用“外部扩展 namespace”模式。

## Schema Method 语法

目标语法：

```relon
#schema Money {
    Int cents: *
    String currency: *
} with {
    #derive Equatable
    eq(other: Self) -> Bool:
        self.currency == other.currency && self.cents == other.cents

    #derive Comparable
    lt(other: Self) -> Bool:
        self.currency == other.currency && self.cents < other.cents

    format() -> String:
        f"${self.cents} ${self.currency}"
}
```

语义：

- `#schema Money { ... }` 定义数据形状。
- `with { ... }` 定义 `Money` 的方法 namespace。
- `self` 由 analyzer 注入，类型为当前 schema。
- `format` 是普通 schema 方法；`lt` 因为 method 紧前方标记了
  `#derive Comparable`，且名称和签名匹配 `Comparable` 的 witness
  定义，所以也是 constraint witness。
- 方法调用写成 `price.format()` / `price.lt(limit)`；analyzer 会在
  `price` 的静态类型已知时脱糖为 `Money.format(price)` /
  `Money.lt(price, limit)`。
- operator 语法只在对应 constraint witness 已静态满足时可用。
- `Money` 的 `Comparable` 来自 `lt` method 上的 `#derive
  Comparable`；`Money` 的 `Equatable` 可以来自结构推导，也可以来自
  `eq` method 上的 `#derive Equatable`。

`with { ... }` 里允许两类 item：

- **method**：核心声明，形如 `name(args) -> ReturnType: body`，自带
  `self`。是否真的写出 `body` 由其前面的 attribute 决定。
- **schema-level item**：目前只有 `#no_auto_derive BuiltinConstraint`，
  作用于整个 schema / enum 的结构性推导开关，不绑定 method。

method 前可连续堆叠任意数量的 **method attribute**，按出现顺序作
用于紧随其后的同一个 method declaration（中间不留空行）：

- `#derive BuiltinConstraint`：标记该 method 是某个内置 constraint
  的 witness。可连续多行声明多个 constraint，每行一个，避免一行混
  合多个语义。
- `#native`：标记该 method 不写 `.relon` body，由 Host 通过
  `register_native_method` 注册实现。不带 `#native` 的 method 必
  须写出 `.relon` body。

`#derive` 与 `#native` 之间没有顺序约束 —— 两者都是「装饰 next
method declaration」的 attribute，按 attribute 模型语义等价于一组无
序集合。Style guide 推荐先写 `#derive` 再写 `#native`（与 Rust 的
`#[derive(...)]` + `extern "C"` 习惯一致），但 analyzer 不强制。

例：

```relon
#schema Money {
    Int cents: *
    String currency: *
} with {
    #derive Comparable
    #native
    lt(other: Self) -> Bool

    #derive Equatable
    eq(other: Self) -> Bool:
        self.cents == other.cents && self.currency == other.currency
}
```

第一个 method 同时被 `#derive Comparable` 和 `#native` 装饰：声明 `lt`
是 `Comparable` 的 witness，且实现由 Host 注册（无 body）。第二个
method 被 `#derive Equatable` 装饰，写出 `.relon` body。

`self` 和 `Self` 只在当前 schema 的 `with { ... }` 段内有效；离开
该段后必须写出具体 schema 类型。

### `#derive` / `#no_auto_derive`

第一版支持 method 级 `#derive` 和 schema 级 `#no_auto_derive`，二者都只
能引用内置 constraint。

规则：

- `#derive Equatable`：必须紧挨着 `eq(other: Self) -> Bool`，使用
  显式 witness 覆盖默认结构等值语义。
- `#derive Comparable`：必须紧挨着 `lt(other: Self) -> Bool`，并且
  类型满足 `Equatable`；否则 analyzer 报错。
- `JsonProjectable` v1 没有 method witness，所以不能写在 method 级
  `#derive` 上；它由 analyzer 默认结构推导。
- 标记显式 witness method 时，`#derive` 必须紧挨 method，中间不留
  空行。
- 同一个 method 可以按需连续标记多行 `#derive`。每一行只声明一个
  内置 constraint，避免一行里混合多个语义。
- 结构性推导默认开启。需要禁止时使用 `#no_auto_derive`，它是 schema 级
  独立 item，不绑定 method：

```relon
#schema User {
    String name: *
    Int age: *
} with {
    #no_auto_derive JsonProjectable
}
```

`#no_auto_derive` 只关闭结构性推导，不影响显式 witness method：

```relon
#schema UserId {
    String value: *
} with {
    #no_auto_derive Equatable

    #derive Equatable
    eq(other: Self) -> Bool:
        self.value == other.value
}
```

`#derive` / `#no_auto_derive` 仍只能引用内置 constraint，不能携带自定义
代码。

## Host 注册模型

Host 可以注册 schema native method，但必须投影成 Relon 可见签名：

```rust
ctx.register_schema_method(
    "GeoPoint",
    "distance_to",
    MethodSignature::new()
        .receiver(Type::schema("GeoPoint"))
        .param("other", Type::schema("GeoPoint"))
        .returns(Type::Float)
        .pure(),
    Arc::new(GeoDistanceTo),
);
```

Analyzer 只看 Relon-visible 元数据：

```text
SchemaMethod {
  owner: GeoPoint,
  name: distance_to,
  signature: (self: GeoPoint, other: GeoPoint) -> Float,
  source: RelonBody | HostNative,
}
```

Rust 侧的 `impl RelonFunction for GeoDistanceTo` 只是执行细节。
Analyzer 不理解也不依赖 Rust trait。

## `.relon` 与 Host 的对照规则

同一个 schema method 可以有两种来源：

- `.relon` body：源代码中直接定义函数体。
- Host native：Host 注册签名和执行实现。

对照规则：

1. 如果 `.relon` 定义了函数体，Host 不需要注册同名实现；如果 Host
   也注册同名实现，默认报冲突。
2. 如果 `.relon` 只声明 native method，Host 必须注册同名实现。
3. 如果 `.relon` 声明和 Host 注册同名 method，签名必须一致。
4. 参数数量、参数类型、返回类型都必须一致。
5. sandbox 权限/纯度元信息必须一致；例如 `.relon` 声明 pure，
   Host 不能注册需要 `reads_fs` 的实现。
6. 冲突必须在 analyzer 阶段报错，不允许推迟到 evaluator。

可选 native 声明语法草案：

```relon
#schema GeoPoint {
    Float lat: *
    Float lon: *
} with {
    #native
    distance_to(other: Self) -> Float
}
```

`#native` 表示该 method 没有 `.relon` body，必须由 Host 提供实现。
这类写法主要用于需要宿主库、平台能力或性能优化的普通 method。也可
以把 `#native` method 用作 constraint witness，但那是高级用法；它
仍必须由 method 级 `#derive` 激活，并满足 witness 的签名和纯度要求。

## Analyzer 责任

Analyzer 必须构建 method table：

```text
MethodTable:
  Money.lt     -> MethodSignature + source location + source kind
  Money.format -> MethodSignature + source location + source kind
```

并执行：

- schema owner 存在性检查。
- method 重名检查。
- 参数类型解析。
- 返回类型解析。
- `.relon` method body 类型检查。
- 调用点参数检查。
- 调用点返回类型推断。
- Host 注册签名对照。
- native method 缺实现诊断。
- Host 注册多余实现诊断，除非 Host 明确允许扩展 namespace。

## Evaluator 责任

Evaluator 不做 trait/constraint resolution。

Evaluator 只根据 analyzer 产物执行：

- `RelonBody`：按脱糖后的普通 function 执行。
- `HostNative`：调用 Host 注册实现。

如果 analyzer 已经通过，evaluator 不应该再发现“函数不存在 / 签名不
匹配”这类结构性错误。运行期只保留：

- host native 执行失败。
- 数据相关分支触发的 validation error。
- sandbox permission denied。
- step/value budget error。

## 调用语义

推荐调用形式：

```relon
price.lt(limit)
price.format()
```

Analyzer 在 receiver 静态类型已知时脱糖：

```text
price.lt(limit)  ==> Money.lt(price, limit)
price.format()   ==> Money.format(price)
```

也允许使用完整名调用，便于消歧和生成代码：

```relon
Money.lt(price, limit)
Money.format(price)
```

不支持开放式 operator overloading。只允许内置 constraint 定义的
operator：

```relon
price < limit
```

`<` 只接受满足内置 `Comparable` constraint 的类型。对用户 schema，
这意味着 `lt(other: Self) -> Bool` method 必须标记
`#derive Comparable`，并且 analyzer 能静态证明该 schema 满足
`Equatable`，无论它来自结构推导还是显式 `eq` witness。

## 错误类型草案

Analyzer 新增诊断建议：

| 诊断 | 条件 |
| --- | --- |
| `UnknownMethodOwner` | `with` 绑定的 schema 不存在 |
| `DuplicateSchemaMethod` | 同一 owner 下 method 重名 |
| `SchemaMethodTypeMismatch` | method body 返回值不匹配声明返回类型 |
| `UnknownSchemaMethod` | 调用不存在的 schema method |
| `SchemaMethodArgTypeMismatch` | 调用参数不匹配 |
| `ConstraintWitnessSignatureMismatch` | witness 名称存在但签名不符合 constraint 定义 |
| `MissingConstraintDerive` (Error) | 类型被要求满足**没有结构性默认**的 constraint（当前只有 `Comparable`），schema 有候选 witness method，但没有贴对应 `#derive` |
| `IgnoredConstraintWitness` (Lint / Warning) | 类型被要求满足**有结构性默认**的 constraint（如 `Equatable` / `JsonProjectable`），结构推导成功，但 schema 还有一个名字 / 签名匹配的 witness method 没贴 `#derive` —— 提示用户「你大概想用这个 witness 而不是默认推导」，但不阻塞编译 |
| `MissingConstraintWitness` | 类型被要求满足 constraint，但缺少必要 witness（结构性推导也不满足） |
| `InvalidDeriveTarget` | `#derive` 引用的 constraint 没有 method witness，或紧随的 method 不可能成为该 witness |
| `NoAutoDeriveConflict` | `#no_auto_derive` 引用的 constraint 没有结构性默认（如 `Comparable`），声明无意义 |
| `MissingHostSchemaMethod` | `#native` 声明无 Host 实现 |
| `HostSchemaMethodSignatureMismatch` | Host 注册签名和 `.relon` 声明不一致 |
| `HostSchemaMethodPermissionMismatch` | Host 权限/纯度声明和 `.relon` 不一致 |

## 实现阶段

### Phase 1：纯 `.relon` schema method

- Parser 支持 `#schema ... with { ... }`。
- Analyzer 建 method table。
- Analyzer 检查 method body 和调用点。
- Evaluator 执行 `value.method(...)` 和 `Schema.method(value, ...)`。

### Phase 2：Host schema method

- Host API 增加 `register_schema_method`。
- Analyzer 输入增加 Host signature registry。
- 支持 `#native` method 声明。
- 检查 Host 签名和 `.relon` 声明一致。

### Phase 3：Constraint bounds

- stdlib 和 schema method 签名支持内置 constraint bound。
- 例如 `T: Equatable`、`T: JsonProjectable`。
- 只允许引用内置 constraint。

### Phase 4：derive 控制

- `with { ... }` 支持 method 级 `#derive BuiltinConstraint`。
- `with { ... }` 支持 schema 级 `#no_auto_derive BuiltinConstraint`。
- `#derive Equatable` 必须对照 `eq(other: Self) -> Bool` witness。
- `#derive Comparable` 必须对照 `lt(other: Self) -> Bool` witness。
- 不允许用户自定义 derive 实现。

## 设计原则

- Constraint 是封闭集合，写进 spec。
- Schema 定义数据形状；行为必须显式写在 `with { ... }` method 段
  或 Host native method registry 中。
- Host trait 留在 Rust 边界内。
- Analyzer 对 source、schema、signature、host registry 能知道的事
  必须静态报错。
- Evaluator 不承担静态结构错误兜底。
