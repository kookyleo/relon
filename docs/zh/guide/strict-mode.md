# 严格模式 (`#relaxed`)

Relon 的 analyzer 默认就是严格模式。所有值必须有静态可推断类型；那些「分析器拿不到信息」的位点（未标类型的 closure 参数、没 signature 的 native fn、不透明表达式等）会以 **error** 级诊断报出来。

模块可以用文件级指令 `#relaxed`（同义词 `#unstrict`）退出严格推断。两种写法完全等价，挑哪个读起来跟你其他指令更协调即可。

```relon
#relaxed
{ ... }
```

写了 `#relaxed` 之后，analyzer 仍然会报所有它能**静态证明**的错误（path 走查失败、未声明 schema、非 dict spread 源等）；只有「真的推不出来」且没法证明是 bug 的位点才沉默——那些位点退回到运行时检查。

> 两种模式共用同一份 parser、同一份 runtime。所有「静态确凿的错误」也共用——严格模式只是**额外**追加「信息不足」类的报错。

## `#import` 链上的传染规则

严格性由 **入口** 决定。入口的模式会被印到整条 `#import` 链上每一个可达模块，使 workspace 端到端只呈现一种模式：

- 严格入口（默认行为，无需写指令）会让每一个可达 import 都按严格分析——哪怕库自己没写 `#relaxed`。这条规则防止严格入口悄悄继承库里的 silent fallback。
- `#relaxed` 入口则把「清零位」印到每一个可达 import 上。一个本来会被严格分析的库，在当前 workspace 构建里会按非严格走，避免严格库意外收紧一个 `#relaxed` 入口的契约。

严格是自顶向下的策略：入口的模式说了算。

## 跨模式错误（两种模式都报）

这些都是 analyzer 仅凭 source + schemas 就能推得出的事实。运行时也会失败，所以它们无论哪种模式都按 error 级触发——`#relaxed` 不会让它们过关。

| 场景 | 诊断 |
|---|---|
| spread 一个静态非 dict 的值（`{ src: 1 + 2, out: { ...src } }`，`src: Int`） | `non_spreadable_source` |
| spread `<未声明 Schema>`（workspace 没声明的 schema 名） | `unresolved_schema` |
| path 下钻到未声明的 schema 字段（`&u.unknown`，`u: User` 没有 `unknown`） | `unknown_reference_type` |
| path 越过叶子类型继续下钻（`&u.id.something`，`u.id: Int`） | `unknown_reference_type` |
| 重复字段——spread 引入了 dict 已经声明的键 | `duplicate_field` |
| 显式 `Any` 标注（`Any x: 1`、`(Any n) => …`、`List<Any>`、嵌套形式） | `explicit_any_forbidden` |
| 不带泛型的容器——`List` / `Dict` / `Closure` / `Fn` 单独写 | `bare_generic_container` |

> `unresolved_reference`（warning 级）仍会为「自由标识符没解析到任何 binding」的情况触发。它保留 warning 级是因为这个名字**可能**会被上游的某个 spread 在运行时补上——analyzer 不知道；严格模式则会在同一位点追加一个 `unknown_reference_type` error。

## 仅严格触发的错误（`#relaxed` 下沉默）

这些位点是「静态信息**真的缺**」。analyzer 既无法证伪也无法推断——严格模式拒绝再往下走；`#relaxed` 模式接受运行时回退。

| 场景 | 诊断 |
|---|---|
| spread 源的静态类型真的未知（未标类型的 closure 参数、未标的 binding 等）且无 `<T>` hint | `spread_source_type_unknown` |
| 动态键缺少 `<T>` hint（`{ [k]: v }`） | `dynamic_key_type_unknown` |
| 自由标识符没 binding（`nowhere`，且没有 spread / import 可能提供） | `unknown_reference_type`（在跨模式的 `unresolved_reference` warning 之上加报） |
| closure 参数没标 type（`(n) => n + 1`），且调用点签名也钉不住其类型——能钉住的豁免（如 `xs.map((x) => ...)` 的 `x` 由 `map` 签名推出，不报） | `closure_param_type_missing` |
| closure body 返回类型推不出（无 `-> ReturnType`，body 落 `Any`） | `closure_return_type_unknown` |
| host 注册了名字但没声明 signature 的 native fn 调用 | `native_fn_signature_missing` |
| 不透明表达式落在类型化 slot 上 | `expression_type_unknown` |

## 完整对照表

下表由 `crates/relon-analyzer/tests/strict_matrix.rs` 生成——每一行都是真实的源码片段，两种模式下分别 `analyze_with_options` 跑过。matrix 测试在 cell 内容变化时会失败，所以这张表与 analyzer 同步。

### Spread `{ ...e }`

| 场景 | 非严格 | 严格 |
|---|---|---|
| 字典字面量直接 spread：`{ ...{a: 1} }` | — | — |
| spread 一个声明类型的兄弟字段（`Extra e: {...}` → `{ ...e }`） | — | — |
| spread 一个静态非 dict 的引用（`{ src: 1 + 2, out: { ...src } }`） | `non_spreadable_source` | `non_spreadable_source` |
| spread 一个类型未知的引用（如未标类型的 closure 参数） | — | `spread_source_type_unknown` |
| 无类型源 + 显式 hint（`{ ...<Extra> e }`） | — | — |
| spread `<未声明 Schema>` | `unresolved_schema` | `unresolved_schema` |

### 动态字典键 `{ [expr]: ... }`

| 场景 | 非严格 | 严格 |
|---|---|---|
| `{ [k]: 1 }` 无 `<T>` hint | — | `dynamic_key_type_unknown` |
| `{ [<String> k]: 1 }` 带 hint | — | — |

这张表只说明 analyzer 是否产生“缺少 key 类型”诊断。当前运行期动态键
表达式不承诺继承同一 dict 的 sibling 作用域，也不适合作为读取
`#main` 参数的公开写法；用户文档里的可运行示例只使用自足表达式
（例如 `[<String> "user_" + "42"]`）。

### Native fn 调用

| 场景 | 非严格 | 严格 |
|---|---|---|
| 宿主注册的 native fn，**有** signature | — | — |
| 宿主注册的 native fn，**无** signature | — | `expression_type_unknown` + `native_fn_signature_missing` |

### Closure

| 场景 | 非严格 | 严格 |
|---|---|---|
| `(Int n) => n + 1`（参数有类型） | — | — |
| `(n) => n + 1`（参数无类型） | — | `closure_param_type_missing` + `closure_return_type_unknown` |
| `(n) => ext_call(n)`（参数 + body 都无法分类） | — | `closure_param_type_missing` + `closure_return_type_unknown` + `native_fn_signature_missing` |

### 引用 path

| 场景 | 非严格 | 严格 |
|---|---|---|
| `&sibling.u.name`，`u: User`、`User { String name }` | — | — |
| `&sibling.u.unknown`（schema 没这个字段） | `unresolved_reference` + `unknown_reference_type` | `unresolved_reference` + `unknown_reference_type` |
| `&sibling.u.id.something`（沿叶子类型继续下钻） | `unknown_reference_type` | `unknown_reference_type` |
| `nowhere`（自由标识符，没有 binding） | `unresolved_reference` | `unresolved_reference` + `unknown_reference_type` |

### `#main(...)` 参数

| 场景 | 非严格 | 严格 |
|---|---|---|
| `#main(Int x) -> Dict<String, Int>` | — | — |
| `#main(x) -> ...`（参数无类型） | *（被 parser 直接拒绝——`#main` 参数无论哪种模式都必须带类型标注）* | — |

## 何时用哪种

**严格（默认）** 适用于：

- 生产用规则文件（定价、风控、校验）——你想要构建期保证「没有任何值悄悄落到 `Any`」。
- 给严格入口准备的 workspace 库——传染规则会把 `#relaxed` 库也按严格分析，库里不能藏 silent fallback。
- 走 AOT 编译路径的代码：[架构概览](./architecture.md) 里描述的类型驱动优化，只有「每个形状都静态可知」时才会触发。

**用 `#relaxed` 退出严格** 适用于：

- 快速实验、playground——想写一行立刻能跑。
- 中间数据形态还在演进的胶水代码。
- 宿主接入早期，还没来得及把每个 native fn 注册 signature。

## 修复手册

| 诊断 | 典型修法 |
|---|---|
| `non_spreadable_source` | spread 源的类型本来就错——换成 dict 字面量、schema-typed binding 或 `Dict<K, V>` 表达式。`<T>` hint 救不了它。 |
| `spread_source_type_unknown` | 给 spread source 加类型（`Extra e: ...` → `{ ...e }`），或写内联 hint：`{ ...<Extra> e }`。 |
| `dynamic_key_type_unknown` | 加 key-type hint：`{ [<String> k]: v }`。 |
| `unresolved_schema` | 引用前先声明 schema（`#schema Missing { ... }`），或把 `<Missing>` 标注去掉。 |
| `unknown_reference_type` | 检查 path：字段必须在 head 的 schema 中声明，且不能越过叶子类型继续 `.x`（`Int.something` 之类）。 |
| `expression_type_unknown` | 给外层 binding 加类型让推断有目标，或重写表达式让类型可推。 |
| `native_fn_signature_missing` | 在 `host_fn_signatures` 里给 native fn 注册带返回类型的 signature，或在该模块加 `#relaxed` 接受动态回退。 |
| `closure_param_type_missing` | 给参数加类型：`(Int n) => ...`。 |
| `closure_return_type_unknown` | 在 closure 上声明 `-> ReturnType`，或重写 body 让推断能拿到类型。 |

所有诊断都会指向出问题的源代码位置；其中一部分（目前是 `spread_source_type_unknown` 和路径走查类的子集）IDE quick-fix 也会给出补全建议。
