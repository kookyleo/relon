# Relon 语言规范

> **状态**：v1 候选规范。本文是 Logic-as-Data 承诺的可执行表述——
> 实现必须按这里描述的语义工作；脚本只能依赖本规范声明的名字与契
> 约。当前唯一的 reference 实现是仓库内的 Rust crates。

## 1. 设计承诺

> **同源 + 同输入 → 字节级一致的输出。**

这是规范的承重轴。下面所有约束都是为了让这一句在不同机器、不同时间
都依然成立——同一段 `.relon` 跑两次必须得到同一个结果，可重放、可
hash、可缓存。

### 1.1 实现契约（Implementation contract）

一个实现满足本规范当且仅当对于本规范覆盖的所有 source + input 组合：

1. **解析**：接受 reference 实现接受的所有源；拒绝 reference 实现
   拒绝的所有源。
2. **求值**：产出与 reference 实现字节级一致的 `Value`。
3. **能力模型**：实现 §4 定义的 `Capabilities`，且没有任何让脚本
   绕过它的入口。
4. **标准库**：实现 §6 列举的所有 std 模块，按文档定义的语义。
5. **错误码**：错误的种类标签必须使用 §5 定义的稳定列表（消息文本
   可本地化）。

未明确指定的实现细节（比如内部缓存、线程模型、构建产物大小）由实现
自行决定，不影响契约。

### 1.2 求值确定性的边界：push vs pull

「同源 + 同输入 → 字节级一致」中的「输入」**特指显式 `Value` 树**
——host 在求值前通过 `Evaluator::run_main(scope, args)` 推入的
named arguments；脚本通过 `#main(...)` 签名声明它期望的形状，并以
参数名直接访问绑定。

Host 通过 `register_fn` 注入的 native fn 的**调用结果**不属于「输
入」。因此：

- **Push 形态**（host 求值前完成 I/O，把数据 materialize 成
  `Value`，通过 `run_main(args)` 推入；脚本用 `#main(...)` 声明契
  约）：求值确定性在本规范保障范围内——同一份 args 跑两次结果必然
  一致，可重放、可 hash、可缓存。
- **Pull 形态**（脚本求值期通过 native fn 拉外部数据）：脚本作者
  **主动放弃**了求值确定性——不同时刻的网络与外部状态本就不同，
  spec 不要求也无法保证一致。

详见 [host-integration.md §推荐范式：Push-by-default](./host-integration.md#推荐范式push-by-default)。

### 1.3 sigil 划分：`@` 与 `#`

Relon 把「附加在节点之上的元信息」分进两个互不重叠的命名空间。这
是规范的硬约束：实现不得允许某个名字同时以 `@` 和 `#` 形式存在。

| sigil | 用途 | 谁可以注册 |
| --- | --- | --- |
| `@name(...)` | **装饰器**——值变换（value transform） | 内置 + host + 用户（任何可调用绑定都行） |
| `#name ...` | **指令**——声明 / 结构 / 元数据 | 仅由内置注册，固定集合，用户不可扩展 |

完整指令集合（v1）：`#main(...)`、`#schema X Body`、`#import ... from "..."`、
`#private`、`#default`、`#expect`、`#msg`、`#error`、`#brand X`。

完整内置装饰器集合（v1）：`@value(...)`。其它任何 `@name(...)` 都
解析为「在当前作用域查找 `name`、把下方值作为最后一个位置参数传
入」。

### 1.4 静态分析优先原则

Relon 的错误处理基线：

> **凡是只依赖 source / module graph / schema / stdlib 签名的信
> 息，错误必须在 analyzer 阶段报；只有依赖 host-pushed value /
> native fn 返回值 / 数据相关分支结果的错误，才允许留到 runtime。**

这是与 Rust 共享的设计取向：能在编译期挡住的问题就别推到运行期。
在 Relon 里「编译期」具体指 parser → analyzer 这条静态链路；
「运行期」指 evaluator 的求值过程。

每个 `RuntimeError` variant 在新增或修改时都要按此准则审一遍：

- 「这个错为什么没在 analyzer 抓？」答得出来（依赖运行期数据）就
  保留 runtime；答不出来就要求前移到 analyzer，作为新的诊断。
- analyzer 已能查的错（如 `UnresolvedReference`、
  `StaticTypeMismatch`、`NonExhaustiveMatch`）不允许 evaluator 再
  独立报一遍——一致性由 analyzer 做权威。

短板（v1 已识别、按 stage 推进静态化）：表达式级类型推断只覆盖字
面量；closure 体内的引用解析仍偏运行期；capability 没有静态可达
性分析。这些短板的补齐方向以工程 roadmap 体现。

## 2. 决定性契约（Determinism Contract）

为了保证 §1 的承重轴成立，实现必须遵守：

### 2.1 字典迭代序

`Value::Dict` 的迭代顺序是**键的 Unicode 码点字典序**（reference
实现用 `BTreeMap` 实现）。**禁止**任何形式的哈希随机化、插入序保
留、或 locale-dependent 排序。

```relon
{ "b": 1, "a": 2 } | dict.keys()  // 永远是 ["a", "b"]
```

### 2.2 列表迭代序

`Value::List` 按插入序遍历。无意外。

### 2.3 浮点

* 数值类型只有 `Int`（i64）和 `Float`（IEEE-754 binary64 / `f64`）。
* 浮点比较使用 IEEE-754 总序（`OrderedFloat<f64>`）：
  * `NaN == NaN` 为 `true`（与 Rust 的 `PartialEq` 不同——这是规
    范的显式选择，让 `Dict<String, Float>` 等可序列化）
  * `-0.0 == 0.0` 为 `true`
  * 排序中 `NaN` 视为大于一切非 NaN。
* 浮点运算遵守 IEEE-754；**禁止** fast-math、FMA 自动融合或编译期
  常量折叠产生不同舍入。
* 整数算术在 `i64` 上执行确定性 checked 运算：`+`、`-`、`*`、
  `/`、`%`、一元 `-` 只要越界就必须抛 `NumericOverflow`。禁止依赖
  Rust debug/release 差异、wrap、饱和或 panic 行为。

### 2.4 字符串

* 所有字符串按 UTF-8 编码、按 Unicode 码点比较与排序。
* `string.split` 等基于「字符串」的操作以**字节**为单位（reference
  实现的 `String::split` 行为）；如果脚本需要按字符串图元（grapheme
  cluster）操作，必须由 host 通过 native fn 显式提供。

### 2.5 不可见的环境

脚本**不可读**：

* 系统时钟（`now()`、`SystemTime::now()` 等）。如果需要时间，host
  通过 `#main` 推入。
* 系统时区、locale。
* 环境变量。
* 随机数（`rand`、`/dev/urandom`）。
* 进程 ID、CPU 数等。
* HashMap 哈希种子（运行时内部数据结构允许，但绝不暴露给脚本）。

### 2.6 错误确定性

错误的**种类标签**（`TypeMismatch`、`ModuleNotFound`、`CapabilityDenied`
等）和触发位置（`TokenRange`）必须在所有 runtime 上完全相同；只有
人类可读的消息文本允许本地化。

## 3. 词法 / 语法

参考实现：`crates/relon-parser`。

实现必须接受 reference parser 接受的所有 source、拒绝它拒绝的所有
source。语法 corpus 由 `fixtures/` 与 `examples/` 中的样例 +
`crates/relon-parser/tests/` 共同定义。

> 详细语法见 [基础语法](./syntax.md)。

### 3.1 五种指令形态

每个 `#name ...` 指令必须满足下列五种 shape 之一。shape 由名字决
定（在 parser 内查表），不可由用户扩展：

| shape | 形态 | 例 | 用于 |
| --- | --- | --- | --- |
| Bare | `#name` | `#private` | 标记字段属性 |
| Value | `#name <expr>` | `#default 0`、`#expect "must be ≥0"`、`#brand Color` | 元数据 / 值变换 |
| NameBody | `#name <ident> <body>` | `#schema User { String name: * }` | 命名声明（无冒号） |
| Import | `#import <bindspec> from "<path>"` | `#import * from "std/list"` | 导入 |
| Main | `#main(Type name, ...) [-> ReturnType]` | `#main(User u, Cart cart) -> Order` | 入口签名 |

`<bindspec>` 是以下三者之一：单个 ident（命名空间）、`*`（spread）、
`{ a, b as c }`（析构）。

`#schema X: Body` 这种形态是**字段位置**（dict-field 形）的语法
糖——`:` 属于 dict 字段语法而非指令语法；语义上等价于
`#schema X Body`。

v1.3 新增的 `#strict` 也是 `Bare` 形态指令；详见 §6.6。

## 4. 能力模型（Capabilities）

### 4.1 默认零特权

新构造的 `Context` 默认**没有任何能力**。脚本：

* 无法读文件系统（`#import x from "./local.relon"` → `CapabilityDenied`）
* 无法调用任何 `register_fn(name, gate, fn)` 注册时声明了非空
  `NativeFnGate` 的 host 原生函数（pure fn 通过 `register_pure_fn`
  注册时携带空 gate，沙箱下也能跑）
* 没有执行步数 / value 体积上限（`None` 表示「不强制」，但 host 应
  根据信任程度显式设置）

### 4.2 显式授权才放行

Host 通过 `Capabilities` 字段显式授权：

```rust
let mut ctx = Context::sandboxed();
ctx.capabilities.reads_fs = true;                           // 允许 #import 真实文件，同时让声明 reads_fs 的 host fn 通过 gate
ctx.capabilities.max_steps = Some(1_000_000);               // 限制求值步数
```

或一次性授权全部（`Capabilities::all_granted()`）——但这是显式的、
可审计的赋权，不是隐式的「trusted 模式」。**规范禁止任何 `trusted()`
或类似的「全开」捷径构造器**：脚本必须能观察到 host 授予了什么、没
授予什么。

### 4.3 std 虚拟模块的特殊位置

`#import * from "std/list"`、`#import string from "std/string"` 等
std 模块通过**虚拟解析器**（`StdModuleResolver`）服务，**不消耗**
`reads_fs` 能力。这是规范的有意设计：std 是规范的一部分，对它的访
问不属于 host 信任决策。

## 5. 错误种类（Error Kinds）

实现必须使用以下稳定标签：

| Kind | 触发条件 |
|---|---|
| `Parse` | 词法 / 语法错误 |
| `Analyze` | 语义分析阶段聚合错误（`#schema` 异构、未类型化字段等）|
| `TypeMismatch` | 运行时值不符合声明类型 |
| `VariableNotFound` | 引用未定义的名字（含 schema 名、模块 alias、函数名）|
| `FunctionNotFound` | 调用未注册的原生函数或闭包 |
| `CircularImport` | `#import` 形成环 |
| `ModuleNotFound` | 没有 resolver 返回该模块 |
| `ModuleParseError` | 模块文件解析失败 |
| `IoError` | 真实 I/O 错误（被允许的 `reads_fs` 操作中发生）|
| `CapabilityDenied` | 受 §4 拦截 |
| `NumericOverflow` | 整数算术超出 `i64` 可表示范围 |
| `StepLimitExceeded` | 触发 `max_steps`（求值步数预算耗尽）|
| `RecursionLimitExceeded` | 类型检查 / schema 验证递归深度超过运行时安全上限（与 `max_steps` 是不同维度的预算，hosts 不能通过调高 `max_steps` 缓解）|
| `ValueTooLarge` | 触发 `max_value_elements` |
| `NoMainSignature` | 文件没有 `#main(...)` 但被 `run_main` 调用 |
| `MissingMainArg` | host 没有为 `#main` 声明的某个参数推入值 |
| `UnexpectedMainArg` | host 推入了 `#main` 签名中没有的参数名 |
| `MainArgTypeMismatch` | 推入值不匹配 `#main` 参数类型 |
| `UnsupportedOperator` | 无效操作或类型组合 |

## 6. 标准库目录（Spec-mandated）

实现必须提供以下 std 模块。脚本通过
`#import <bindspec> from "std/<name>"` 引入。

### 6.1 语言级 builtins（无需 import）

这三个名字属于**语言**而非 std 模块——它们是数据结构本身的元操
作，所有运行时无条件提供：

* `len(value)` — 返回 `String` / `List` / `Dict` 的元素数（`Int`）。
* `range(end)` / `range(start, end)` — 返回半开区间 `Int` 列表。
* `type(value)` — 返回值的类型名（`String`：`"Int"`、`"Float"`、
  `"String"`、`"Bool"`、`"List"`、`"Dict"`、`"Closure"`、`"Null"`）。

### 6.2 std 模块清单

| 模块 | 函数 | 备注 |
|---|---|---|
| `std/list` | `map`、`filter`、`reduce`、`contains`、`sum`、`avg`、`len`、`first`、`last`、`compact`、`flatten` | 函数式列表操作 |
| `std/dict` | `merge`、`keys`、`values`、`has_key` | Dict 元操作 |
| `std/string` | `split`、`join`、`replace`、`upper`、`lower`、`contains` | 字符串操作 |
| `std/math` | `abs`、`max`、`min`、`clamp` | 数值操作 |
| `std/is` | `int`、`string`、`bool`、`float`、`list`、`dict`、`number`、`empty` | 类型谓词 |
| `std/value` | `default` | 值守卫（null-coalesce 等） |

每个函数的精确契约由参考实现的 `crates/relon-evaluator/src/std_relon/<name>.relon`
源码定义；这些 `.relon` 文件本身**也是规范的一部分**（它们是 std
模块的 reference 行为定义）。

### 6.3 「ensure.\*」校验器

`#schema` 内部依赖一组 `ensure.*` 函数（`ensure.int`、
`ensure.string` 等）。这些是 schema 系统的实现细节，不暴露给脚本
直接调用——但实现必须确保它们存在且按规范工作，否则 `#schema` 行
为会发散。

### 6.4 Root expression —— 文件根可以是任意表达式

一个 `.relon` 文件求值后产出**一个 JSON 值**——Object、Array、String、
Number、Bool 或 Null。Root **可以是任意表达式**：dict / list literal、
atomic literal、二元 / 三元 / pipe 运算、函数调用、变体构造、引用、
where / match 等，只要其最终求值结果落在上述 JSON 类型集合内。

```relon
// 合法 root 形态示例
{ id: 1, total: 99 }              // dict literal
[1, 2, 3]                          // list literal
n + 1                              // 二元运算（在有 #main 的入口程序里）
"hello"                            // string literal
42                                 // 整数
true                               // bool
null                               // null
Result.Ok { value: order }         // variant constructor
range(0, 10)                       // 函数调用
@projector { ... }                 // 装饰过的 dict
```

实现要求：必须接受 reference parser 接受的所有 root 形态。pre-v1.2
仅接受 dict / list literal 的实现需要扩展到完整 expression 链。

`Closure` / `Schema` / `Type` / `Wildcard` 不属于 JSON 值。如果用户
让 root 求值出这些 host 端无法 JSON 序列化的值，host-side projector
（如内置 `JsonProjector`）会以错误兜底（`UnsupportedClosure` /
`UnsupportedSchema`）。在静态侧，`#main(...) -> ReturnType` 中声明非
JSON ReturnType（如 `Closure`、`Schema`）时，analyzer 的
`check_main_return` 会按已有 type-check 规则发出
`MainReturnTypeMismatch`。

> 历史说明：spec v1.0 / v1.1 仅允许 dict / list literal 作为 root。
> v1.2 放开为任意表达式（superset 扩展）；旧脚本完全不受影响。这一
> 放开使得 `#main(Int n) -> Int` body 可以直接写成 `n + 1`、
> `#main(...) -> Result<T, E>` body 可以直接写成
> `Result.Ok { ... }`，而不必再用 `{ value: ... }` 这样的 dict 包
> 一层。

### 6.5 `#main(Type name, ...) [-> ReturnType]` —— 入口签名

`#main(...)` 是**根级指令**（写在文件根表达式之前），声明这个文件
是一个**入口程序**：宿主必须通过 `Evaluator::run_main(scope, args)`
推入与签名匹配的 named arguments，runtime 在跑 body 之前完成校验。
形态：

```relon
#main(Req req)
{
    #schema Req {
        String name: *,
        #default 0
        Int retries: *
    },
    greeting: f"hello ${req.name}, retries=${req.retries}"
}
```

多个参数并列声明：

```relon
#main(User user, Cart cart)
{
    #schema User { String name: * },
    #schema Cart { Int total: * },
    summary: f"${user.name} - ${cart.total}"
}
```

可选的 `-> ReturnType` 子句声明 body 产生的 **Json 形态**——一个
原子值、dict 或 list 的 schema/类型。省略时不校验返回值。

**`ReturnType` 不该写成 `Result<T, E>`**：成功 vs 失败的区分发生
在**宿主边界**——`Evaluator::run_main` 在 Rust 一侧已经返回
`Result<Value, RuntimeError>`，在 Relon 文件里再包一层 Result 是
重复记账。Relon 内置的 `Result<T, E>` / `Option<T>`（见 §X 内置
schema）是**值层**概念，用于建模数据里某个字段「可能没有」/「可能
失败」，不该出现在入口签名的返回位置。

```relon
// 正确：ReturnType 描述 body 产生的 Json
#main(Order order) -> Order
{ id: order.id, total: order.total * 1.1 }

// v1.2 起：root 可以是任意表达式，atomic ReturnType 直接可用
#main(Int n) -> Int
n + 1

// 应避免：在入口边界写 Result —— 与宿主侧的 Rust Result 重复
#main(Order order) -> Result<Order, String>
...
```

**语义要求**（实现必须按此提供）：

1. `#main(...)` 必须是**根级指令**（写在文件根表达式之前）；写在
   嵌套 dict 上无意义。
2. 每个参数必须是 `Type name` 形式（与 `#schema` 字段写法对齐）：
   - 同一参数名被多次声明 → `Analyze` 错误 `DuplicateMainParam`。
   - 类型必须解析到一个已声明的 `#schema` 或基础类型。
3. 求值前 `Evaluator::run_main(scope, args)` 推入的数据必须按签名校
   验：
   - 缺参数 → `MissingMainArg`；
   - 多参数 → `UnexpectedMainArg`；
   - 类型不匹配 → `MainArgTypeMismatch`。
4. 校验通过后，每个参数按 **参数名直接绑定到根作用域 locals**——脚
   本里直接以 `req`、`user` 等名字访问，不需要 `input.` 前缀。
5. 文件**没有 `#main(...)`** 时调用 `run_main` 报 `NoMainSignature`。
   反之，`#main` 文件被 `eval_root` 当库求值时也报 `NoMainSignature`
   ——edge case 立即在边界截住。
6. **跨文件 `#main` 聚合**（lib 中的 `#main(...)` 也参与 entry 总契
   约）不在 v1 范围内——只校验 entry 文件的 `#main(...)`。lib 文件
   通常不写 `#main`，由 entry 通过 `#import` 引用。

`#main(...)` 把「入口契约」写进 .relon 源码而非 host 端：脚本能脱离
host 独立审计，host 推数据形状不匹配时分析器/求值器在边界即时拦截。
这是 §1.2 求值确定性边界的关键拼图。

**v1.3 起**，`#main(...)` 形参也参与文件根表达式的静态分析：每个声
明的形参在 root scope frame 里被 seed 为已解析引用 + 已知类型，因此
atomic / dict / list / variant / 函数调用等任意根表达式形态都能直接
按名字访问形参，并参与 `infer_type`。这填上了 v1.2 之前在 atomic
root 下「`n+1` 的 `n` 只在运行时才知道是 `Int`」造成的静态分析空
档——`#main(Int n) -> String\nn+1` 现在静态报
`MainReturnTypeMismatch`，不再下沉到 runtime。

### 6.6 `#strict` —— 严格静态推断模式

`#strict` 是一条 v1.3 新增的根级 Bare 指令，开启后整个文件以及它
**`#import` 链上的所有模块**都按「每个值必须有可静态推断出的类型」
校验。原本 analyzer 走 silent fallback 的位置（推不出来时退化为
`Any` 让 runtime 兜底）会改报错。

```relon
#strict
{ ... }
```

**传染规则**：strict 是**入口决定**的——只在 entry 文件写 `#strict`，
workspace 分析器会把 strict bit 传播给该 entry 的所有可达 `#import`
模块。被 import 的 lib 即使自己没写 `#strict` 也按 strict 校验。这
是为了防止「非 strict lib 把 silent fallback 偷渡给 strict entry」。

**新增诊断（strict 下报 Error 级别）**：

| Diagnostic | 触发条件 |
|---|---|
| `MissingSpreadTypeHint` | `{ ...e }` 中 `e` 不是 dict 字面量、也没有 `<T>` typehint、且无法从 path-tail（`...x.y`）或 FnCall 签名（`...f()`）推出 schema/Dict 类型 |
| `MissingDynamicKeyTypeHint` | `{ [k]: v }` 缺少 `<T>` typehint |
| `UnresolvedSchema` | typed spread / dynamic key 中 `<T>` 引用了未声明的 schema |
| `UnknownReferenceType { name, path }` | path-tail 推断在某段失败：头部或中段类型未知（如 `o.unknown`），中段是无字段的 leaf 类型（如 `o.id.something`），或 v1.5 起 strict 下 head 完全 unresolved。`name` 是失败段，`path` 是源码顺序的完整段链 |
| `InferenceLimit { reason }` | 真正不可静态分类的位点：FnCall 无签名落在 typed slot、list 元素 / dict 字段值不可推、match arm body 不可推。`reason` 描述具体子类 |
| `StrictForbidsNativeReturn` | 调用一个被 host 注册了名字、但没在 `host_fn_signatures` 里登记返回类型的 native fn |
| `StrictForbidsUntypedClosureParam { param_name }` | v1.5：closure 参数没标 type，body 推断会被默认 `Any` 污染 |
| `StrictForbidsUnclassifiedClosureBody { role }` | v1.5：closure 既未声明 `-> ReturnType`，body 推断又落到 `Any`，签名最终带 Any 返回 |
| `ExplicitAnyForbidden { context }` | v1.6：用户在源码任意类型位置写了 `Any`（包括嵌套 `List<Any>` / `Dict<String, Any>`）。**这一条与 strict 模式无关**，所有模式都报 |
| `BareGenericContainer { type_name, context }` | v1.7：用户写了不带泛型参数的 `List` / `Dict` / `Closure` / `Fn` / `Enum`——pre-v1.7 它们静默展开为 `<Any>`。**与 strict 模式无关**，与 `ExplicitAnyForbidden` 同等地位 |
| `DuplicateField` | spread 引入的 key 与另一个 named field 或 spread 冲突（**这一条 strict 与否都报**）|

**v1.4 path-tail 推断细则**（针对 `Variable` / `Reference` 多段路径）：

* `Schema(name)` 头：下一段必须是该 schema 声明过的字段；不存在 → strict 下 `UnknownReferenceType`。
* `Dict<K, V>` 头：每段都解析成 V（值类型一致），strict 接受。
* `Optional<T>` 头：在尝试下一段前自动 strip 一层 `?` 包装，再按 T 处理。
* `Any` 头：v1.6/v1.7 双禁之后，唯一仍能命中此分支的位点是「非 strict 模式下未类型化的 closure 参数」（strict 会被 `StrictForbidsUntypedClosureParam` 提前拦截）；继续传播 `Any`，由 runtime 兜底。
* `Tuple<T1, T2, ...>` 头（v1.7）：tuple 是位置型而非名字型，按名字下钻一律 `UnknownStep`（未来若引入 `pair.0` / `pair.1` 位置访问语法，会从这条分支接入）。
* `Int` / `String` / `Bool` / `List<...>` 等 leaf 头：不能继续下钻，strict 下 `UnknownReferenceType`。

**v1.4 typed spread 扩展接受的源**（在 `<T>` 显式 typehint 之外）：

* path 链：`...o.extras` —— path-tail 推到 `Schema` 或 `Dict<K,V>` 即可豁免 hint
* FnCall：`...load_extras()` —— 静态签名返回 single-segment Schema 或 `Dict<K,V>` 即可豁免 hint
* 同级 typed 字段：`...e`（`Type e: ...` 已是 v1.3 行为）
* dict literal：`...{ a: 1 }`（v1.3 行为）

推不出时优先报 path-tail 的具体诊断（如 `UnknownReferenceType`），避免与 generic `MissingSpreadTypeHint` 重叠。

**v1.5 推断升级**——以下表达式从「runtime 才能定」变为「analyzer 静态推断」：

* **list comprehension** `[elem for x in iter if cond]`：iter 推为 `List<T>` /
  `Dict<V>` 后，`x` 在 element body 的 scope 中类型为 T（或 V），结果整体推为
  `List<element_type>`。
* **where 表达式** `expr where { k1: v1, k2: v2 }`：bindings 中每个 key 的值类型
  seed 进 body scope，body 推断结果即整个表达式类型。
* **`Expr::Spread(inner)`**（作为表达式）：等于 inner 的推断结果。
* **`#main(...)` / closure 形参**：strict 下任何参数标为 `Any` 或未标都会被报错；
  closure body 在没声明 `-> ReturnType` 时若推断落到 `Any` 也会被报错。
* **head-unresolved 引用**：strict 下从 warning 升级为 `UnknownReferenceType` Error。
* **FnCall 多段路径**（`alias.method`）：通过 `lookup_signature_path` 推签名，覆盖
  cross-module 与 sibling-method 形态。

经过 v1.5 后，**strict 模式下唯一的 silent fallback 仅剩**：（i）host 注册了名字
但没声明签名的 native fn（已由 `StrictForbidsNativeReturn` 覆盖）；（ii）真正动态
键 / spread 等用户主动选择的 untyped 位点，已由 `Missing*Hint` 系列覆盖。所有
「能从 source + schemas 推得出」的位点都被静态检查捕获。

**v1.6：`Any` 类型从用户空间彻底退役**

v1.5 仍然允许用户在源码里写 `Any` 这个类型字面量；v1.6 把它从用户语言面禁掉，
所有模式（strict 和非 strict）一视同仁报 `ExplicitAnyForbidden`：

* `Any field: ...`
* `#main(Any x)` / `#main(...) -> Any`
* `(Any n) => ...` / `(...) -> Any => ...`
* `#schema X { Any payload: * }`
* 嵌套形：`List<Any>`、`Dict<String, Any>`、`List<Dict<String, Any>>` 等任意层

替代方案：用具体类型（`Int` / `String` / `Bool`）、参数化容器（`List<T>` /
`Dict<String, V>`）、`Enum<...>` sum 类型、或自定义 `#schema`。「我接受任何
shape」的真实需求由具体 schema 表达。

**v1.6 stdlib 签名同步重写**：以前 stdlib 内部用 `Any` 表示「任何输入都收」
的位置，全部换成 unbound 泛型：

* `len<T>(T) -> Int` / `_len<T>(T) -> Int` / `type<T>(T) -> String`
* `_string_join<T>(List<T>, String) -> String`
* `_dict_merge<V>(Dict<String, V>, ...) -> Dict<String, V>`
* `_dict_keys<V>(Dict<String, V>) -> List<String>`
* `_dict_values<V>(Dict<String, V>) -> List<V>` —— **value 类型现在端到端流过**
* `_dict_has_key<V>(Dict<String, V>, String) -> Bool`
* `ensure.int / .string / ...<T>(T, message?) -> T` —— **保留输入类型，不再被 Any 吞掉**
* `ensure.at_least<T> / .at_most<T> / .one_of<T>` 同形
* `ensure.required_fields<V> / .requires<V> / .fields_equal<V>` 同形

unbound 泛型在 Relon 没有 trait bound 系统的当下行为等同于「接受任何类型」，
但**类型流是干净的**：调用 site 绑定具体类型，下游 typed slot 能拿到精确信息
（如 `Int n: ensure.int(x)` 的 `n` 现在直接是 `Int`，pre-v1.6 会被 `Any` 接管）。

**仅保留的 `Any` 内部用途**：

1. 分析器内部的 `InferredType::Any` 占位（用户看不到）
2. generic placeholder 推断 fallback（Pass 3：未绑定 `<T>` → 临时填 Any）
3. 运行时 `Value` 没有强类型标签（实现细节）

这三处用户都看不到，「Any」这个词从源码、诊断、文档示例中彻底消失。

**v1.7：Tuple 类型 + bare 泛型禁用**

v1.6 之前 List 同时承担「同质数组」和「异质 tuple」两种角色——`[1, "x"]`
要么必须放进 `List<Any>`（已被 v1.6 禁用），要么没有合法的标注方式。v1.7
为「定长、异质」数据引入正经的 Tuple 类型：

```relon
// 写法（trailing-comma 区分单元素 tuple 与括号类型）
() unit: []
(Int,) one: [1]
(Int, String) pair: [42, "hello"]
List<(String, Int, Bool)> rows: [
  ["alice", 3, true],
  ["bob", 1, false]
]
```

语义要点：

* List 字面量推断从 `List<join(...)>` 改为 `Tuple<T1, T2, ...>`，**保留每个
  位置的精确类型**。`[1, 2, 3]` 推为 `Tuple<Int, Int, Int>`，`[1, "x"]`
  推为 `Tuple<Int, String>`。
* **Tuple → List 折叠**：同质 tuple 仍可放进 `List<T>` 槽位（每个元素都
  subsume `T`），保持 v1.6 之前所有「homogeneous list」用法不变。
* **Tuple → Tuple**：先比 arity，再 per-position 校验；任何一项不符即
  报 `StaticTypeMismatch`，路径定位到具体位置。
* 嵌套合法：`List<(Int, String)>` / `(List<Int>, String)` / `((Int, Int), String)`。

**bare 泛型禁用**：v1.7 同步关闭了 `List` / `Dict` / `Closure` / `Fn` /
`Enum` 不带泛型参数的写法——pre-v1.7 它们会静默扩展为
`List<Any>` / `Dict<Any, Any>` / `Fn(_, Any)` 等，构成 v1.6 ban-`Any`
之后唯一仍能在源码里偷渡 `Any` 的后门。新诊断 `BareGenericContainer`
在源码、`#main` 参数、closure 参数、schema 字段、嵌套泛型槽位等所有
TypeNode 出现的位置触发；唯一的解决方式是显式补全泛型参数。

```relon
{ List items: [1, 2, 3] }              // BareGenericContainer
{ Dict scores: { math: 100 } }         // BareGenericContainer
{ Closure cb: (x) => x }               // BareGenericContainer
{ Dict<String, List> data: ... }       // BareGenericContainer（嵌套 List）

{ List<Int> items: [1, 2, 3] }         // OK
{ Dict<String, Int> scores: { ... } }  // OK
```

`BareGenericContainer` 与 strict 模式无关，**所有模式一视同仁报 Error**——
和 v1.6 的 `ExplicitAnyForbidden` 同等地位。

**v1.8：Enum / Result 一等公民 + host fn 走查**

v1.7 把用户面的 `Any` / bare 泛型彻底关掉之后，剩下三处仍能在分析阶段
偷渡静默通过的地方在 v1.8 一起收口：

* **`Enum<...>` slot 按替代项检查**：以前 `subsumes_with` 对 `Enum`
  头无脑返回 `true`，把 `42` 塞进 `Enum<"up", "down">` 也合法。v1.8
  改为遍历每个替代项，要求至少有一项静态兼容。bareword 替代项
  （字面量被 parser 剥引号后的 `up` 或 schema 名 `Active`）按
  `String` 候选处理，与 runtime cheap-path 一致。
* **`Result<T, E>` / `Option<T>` 泛型替换**：以前
  `Result<Int, String> r: Result.Ok { value: "wrong" }` 只有 runtime
  能抓住；v1.8 在分析阶段对变体字段做 `T -> Int, E -> String` 替换并
  递归校验，所有用户声明的带泛型 sum schema 同等享受这条流水线
  （`#schema Pair<T, U> Enum<Both { left: T, right: U }>` 也走这条
  路径）。`Result` / `Option` 的变体形状由 `seed_prelude_variants`
  注入到分析器索引，与 evaluator prelude 对齐。
* **host 注册 fn 签名走 ban 走查**：`audit_host_fn_signatures` 对
  `AnalyzeOptions::host_fn_signatures` 中每个签名的 params /
  return_type / variadic_tail 跑同一个 `scan_typenode_for_any`，
  诊断 context 标记为 `host fn '{name}' parameter '{param}'` 等，
  防止宿主以 `register_fn` 形式偷渡 `Any` / 裸泛型。
* **跨模块 `pkg.SchemaName` 静态解析**：以前
  `lib.User u: 42` 会因为多段路径在 `infer_from_type_node` 落到
  `Any` 而静默通过；v1.8 新增 `subsumes_with_imports` /
  `infer_from_type_node_with_imports`，把 `WorkspaceImportIndex`
  穿到 typecheck 里。两段路径中如果头部是已知 import alias、尾部
  是该 alias 导出的 schema 名，槽位折叠为单段 `Schema(name)`,
  和同文件 `User u: 42` 走同一条流水线。同时把单段 `_ => true`
  的兜底改紧：明显不是 schema 形态的值（primitive / list / fn /
  tuple）撞上 schema 槽位现在直接静态报错。
* **Tuple 位置访问 `pair.0` / `pair.1`**：parser 一直就支持
  `TokenKey::Index`，但分析器侧 `path_segments` 会把所有非 String
  段丢弃，导致 `pair.0` 静态类型还是整个 `Tuple<...>`。v1.8 引入
  `WalkSeg::Name | Index` 保留位置段，`walk_path` 新增
  `Tuple, Index(i)` →「第 i 个元素的类型」、`Tuple, Name(_)` →
  硬失败、`List<T>, Index(_)` → `T`。位置越界在 strict 模式下升级
  为 `UnknownReferenceType`；List 越界仍由 runtime 兜底（字面量长
  度没在 InferredType 里追踪）。runtime 行为不变，tuple 在运行时
  仍然只是 `Value::List`。

**v1.3 typed-spread / typed-dynkey 语法**（strict 启用 / 兼容性扩
展）：

```relon
// typed spread —— 在 `...` 后用 `<T>` 标注
{ ...<Extra> e }
{ ...<Dict<String, Int>> kv }

// typed dynamic key —— 在 `[` 后用 `<T>` 标注
{ [<String> key_expr]: value }
{ [<Int> idx]: row }
```

非 strict 模式也接受这两种语法（写了就被静态利用，等价于局部
strict）；不写时静态退化为 `Any`，由 runtime 负责。strict 模式下不
写就报上面对应的 `Missing*Hint`。

**`Dict<K, V>` 泛型**（v1.3 显式纳入规范）：parser 接受 `Dict` 一
组或两组泛型参数（与 `List<T>` / `Result<T, E>` 同形）。`Dict<String, Int>`
会逐字段校验值类型；`Dict<String, Result<Int, String>>` 等嵌套形也合
法。**v1.7 起**，bare `Dict`（不带泛型参数）已被 `BareGenericContainer`
诊断禁用，必须写完整泛型。

```relon
{ Dict<String, Int> scores: { math: 100, art: 90 } }
```

## 7. Host 可注册扩展的边界

Host 可以通过 `register_fn` / `register_pure_fn` /
`register_decorator` 注入：

* 原生函数（数据进、数据出）
* 装饰器插件（自定义 `@value` 替代品、领域专属变换器）

**Host 注入的名字不属于规范**——脚本如果依赖了 host 注入的名字，
它就脱离了规范保证范围，只在该 host 配置下行为可预期。

最佳实践：

* 业务库写在 `.relon` 中（不带 `#main` 的纯库），通过 `#import`
  分发；这样的库不依赖任何 host 注入名，行为完全由 spec + 源码决
  定。
* 只在「必须用宿主能力」的场景注册原生函数（FS、数据库、HTTP），
  通过 `register_fn(name, gate, fn)` 在 `NativeFnGate` 上声明所需的
  能力 bit；纯函数走 `register_pure_fn(name, fn)`，gate 为空，沙箱
  下自动通过。

## 8. 版本化

* 本文档对应 **spec v1**。
* std 模块按 semver 演进：函数语义变更必须升 major；新增函数升
  minor。
* `#import * from "std/<name>"` 默认绑定到 runtime 实现的最新兼容
  版本。未来可能引入 `#import * from "std/<name>@1.x"` 显式绑定。
* runtime 必须在元数据（`relon --version` 或等价 API）中声明它
  实现的 spec 版本。

## 附录 A：与「configuration language」叙事的告别

历史上 Relon 文档把自己描述为「typed business-config DSL」。这是
**不准确**的：那种叙事下，每个 host 都自由扩展、脚本自由依赖
ambient state，求值确定性无从谈起。

Logic-as-Data 取代了这个叙事。它意味着：

* 没有「trusted mode」让脚本绕开沙箱
* 没有 ambient 全局名让脚本隐式依赖（host 注入名不在规范保证范围
  内，由作者自行选择是否使用）
* 没有未规范的浮点 / 迭代序行为
* std 是语言的一部分，不是可选扩展

这些选择都是为了同一个目标：**逻辑像 JSON 一样可存可传，由沙箱内
确定地求值。**
