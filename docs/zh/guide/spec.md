# Relon 语言规范

> **状态**：v1 候选规范。本文是 Logic-as-Portable-Data 承诺的可执行表
> 述——任何符合本规范的运行时（reference 实现是 Rust）必须按这里描述
> 的语义工作；脚本只能依赖本规范声明的名字与契约。

## 1. 设计承诺

> **同源 + 同输入 → 字节级一致的输出。**

这是规范的承重轴。下面所有约束都是为了让这一句在不同 runtime、不同
机器、不同时间被运行时都依然成立。

### 1.1 Conformant Runtime 的定义

一个实现称为 **conformant** 当且仅当对于本规范覆盖的所有 source +
input 组合：

1. **解析**：接受参考实现接受的所有源；拒绝参考实现拒绝的所有源。
2. **求值**：产出与参考实现字节级一致的 `Value`。
3. **能力模型**：实现 §4 定义的 `Capabilities`，且没有任何让脚本绕
   过它的入口。
4. **标准库**：实现 §6 列举的所有 std 模块，按文档定义的语义。
5. **错误码**：错误的种类标签必须使用 §5 定义的稳定列表（消息文本
   可本地化）。

未明确指定的实现细节（比如内部缓存、线程模型、构建产物大小）由各
runtime 自行决定，不影响 conformance。

### 1.2 跨 runtime 一致性的兑现条件

「同源 + 同输入 → 字节级一致」中的「输入」**特指显式 `Value` 树**
——host 在求值前通过 `Evaluator::run_main(scope, args)` 推入的
named arguments；脚本通过 `#main(...)` 签名声明它期望的形状，并以
参数名直接访问绑定。

Host 通过 `register_fn` 注入的 native fn 的**调用结果**不属于「输
入」。因此：

- **Push 形态**（host 求值前完成 I/O，把数据 materialize 成
  `Value`，通过 `run_main(args)` 推入；脚本用 `#main(...)` 声明契
  约）：跨 runtime 一致性在本规范保障范围内。
- **Pull 形态**（脚本求值期通过 native fn 拉外部数据）：脚本作者
  **主动放弃**了跨 runtime 一致性——不同 host / 不同 runtime / 不同
  时刻的网络与外部状态本就不同，spec 不要求也无法保证一致。

详见 [host-integration.md §推荐范式：Push-by-default](./host-integration.md#推荐范式push-by-default)。

### 1.3 sigil 划分：`@` 与 `#`

Relon 把「附加在节点之上的元信息」分进两个互不重叠的命名空间。这
是规范的硬约束：conformant runtime 不得允许某个名字同时以 `@` 和
`#` 形式存在。

| sigil | 用途 | 谁可以注册 |
| --- | --- | --- |
| `@name(...)` | **装饰器**——值变换（value transform） | 内置 + host + 用户（任何可调用绑定都行） |
| `#name ...` | **指令**——声明 / 结构 / 元数据 | 仅由内置注册，固定集合，用户不可扩展 |

完整指令集合（v1）：`#main(...)`、`#schema X Body`、`#import ... from "..."`、
`#private`、`#default`、`#expect`、`#msg`、`#error`、`#brand X`。

完整内置装饰器集合（v1）：`@value(...)`。其它任何 `@name(...)` 都
解析为「在当前作用域查找 `name`、把下方值作为最后一个位置参数传
入」。

## 2. 决定性契约（Determinism Contract）

为了保证 §1 的承重轴成立，所有 conformant runtime 必须遵守：

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
* 整数算术在 `i64` 上遵循 Rust 语义：溢出在 release 模式 wrap。
  这是参考实现的 wrap 行为；规范层面禁止改成「饱和」或「panic」。

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

任何 conformant runtime 必须接受 reference parser 接受的所有
source、拒绝它拒绝的所有 source。语法 corpus 由
`fixtures/` 与 `examples/` 中的样例 + `crates/relon-parser/tests/`
共同定义。

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
| Main | `#main(name: Type, ...)` | `#main(u: User, cart: Cart)` | 入口签名 |

`<bindspec>` 是以下三者之一：单个 ident（命名空间）、`*`（spread）、
`{ a, b as c }`（析构）。

`#schema X: Body` 这种形态是**字段位置**（dict-field 形）的语法
糖——`:` 属于 dict 字段语法而非指令语法；语义上等价于
`#schema X Body`。

## 4. 能力模型（Capabilities）

### 4.1 默认零特权

新构造的 `Context` 默认**没有任何能力**。脚本：

* 无法读文件系统（`#import x from "./local.relon"` → `CapabilityDenied`）
* 无法调用任何 `register_fn_with_caps` 注册的 host 原生函数
* 没有执行步数 / value 体积上限（`None` 表示「不强制」，但 host 应
  根据信任程度显式设置）

### 4.2 显式授权才放行

Host 通过 `Capabilities` 字段显式授权：

```rust
let mut ctx = Context::sandboxed();
ctx.capabilities.reads_fs = true;                          // 允许 #import 真实文件
ctx.capabilities.allow_native_fn.insert("fs.read".into());  // 允许调用具名 host fn
ctx.capabilities.max_steps = Some(1_000_000);               // 限制求值步数
```

或一次性授权全部（`Capabilities::all_granted()`）——但这是显式的、
可审计的赋权，不是隐式的「trusted 模式」。**规范禁止任何 `trusted()`
或类似的「全开」捷径构造器**：脚本必须能在任何 runtime 上观察到
host 授予了什么、没授予什么。

### 4.3 std 虚拟模块的特殊位置

`#import * from "std/list"`、`#import string from "std/string"` 等
std 模块通过**虚拟解析器**（`StdModuleResolver`）服务，**不消耗**
`reads_fs` 能力。这是规范的有意设计：std 是规范的一部分，对它的访
问不属于 host 信任决策。

## 5. 错误种类（Error Kinds）

所有 conformant runtime 必须使用以下稳定标签：

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
| `StepLimitExceeded` | 触发 `max_steps`（求值步数预算耗尽）|
| `RecursionLimitExceeded` | 类型检查 / schema 验证递归深度超过运行时安全上限（与 `max_steps` 是不同维度的预算，hosts 不能通过调高 `max_steps` 缓解）|
| `ValueTooLarge` | 触发 `max_value_bytes` |
| `NoMainSignature` | 文件没有 `#main(...)` 但被 `run_main` 调用 |
| `MissingMainArg` | host 没有为 `#main` 声明的某个参数推入值 |
| `UnexpectedMainArg` | host 推入了 `#main` 签名中没有的参数名 |
| `MainArgTypeMismatch` | 推入值不匹配 `#main` 参数类型 |
| `UnsupportedOperator` | 无效操作或类型组合 |

## 6. 标准库目录（Spec-mandated）

每个 conformant runtime 必须实现以下 std 模块。脚本通过
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
直接调用——但 conformant runtime 必须确保它们存在且按规范工作，
否则 `#schema` 行为会发散。

### 6.4 `#main(name: Type, ...)` —— 入口签名

`#main(...)` 是**根级指令**（写在文件根 dict 之前），声明这个文件
是一个**入口程序**：宿主必须通过 `Evaluator::run_main(scope, args)`
推入与签名匹配的 named arguments，runtime 在跑 body 之前完成校验。
形态：

```relon
#main(req: Req)
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
#main(user: User, cart: Cart)
{
    #schema User { String name: * },
    #schema Cart { Int total: * },
    summary: f"${user.name} - ${cart.total}"
}
```

**语义要求**（每个 conformant runtime 必须按此实现）：

1. `#main(...)` 必须是**根级指令**（写在文件根 dict 之前）；写在
   嵌套 dict 上无意义。
2. 每个参数必须是 `name: Type` 形式：
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

`#main(...)` 把「入口契约」写进 .relon 源码而非 host 端，使任何
conformant runtime 看同一份脚本都按相同 schema 校验——这是 §1.2 跨
runtime 一致性兑现的关键拼图。

## 7. Host 可注册扩展的边界

Host 可以通过 `register_fn` / `register_fn_with_caps` /
`register_decorator` 注入：

* 原生函数（数据进、数据出）
* 装饰器插件（自定义 `@value` 替代品、领域专属变换器）

但 **conformant 规范不要求其它 runtime 提供同名扩展**——脚本如果
依赖了 host 注入的名字，它就脱离了「跨 runtime 可移植」的承诺，
仅在该 host 上保证行为。

最佳实践：

* 业务库写在 `.relon` 中（不带 `#main` 的纯库），通过 `#import`
  分发；这样的库自动跨 runtime 可移植。
* 只在「必须用宿主能力」的场景注册原生函数（FS、数据库、HTTP），
  并用 `register_fn_with_caps` 标记所需 `NativeFnGate`。

## 8. 版本化

* 本文档对应 **spec v1**。
* std 模块按 semver 演进：函数语义变更必须升 major；新增函数升
  minor。
* `#import * from "std/<name>"` 默认绑定到 runtime 实现的最新兼容
  版本。未来可能引入 `#import * from "std/<name>@1.x"` 显式绑定。
* runtime 必须在元数据（`relon --version` 或等价 API）中声明它
  实现的 spec 版本。

## 9. 实现新 runtime 的入口

如果你想为 Go / TS / Swift / 你的语言写一个 conformant runtime：

1. **从语法 corpus 开始**：确保你的 parser 接受 `fixtures/` 与
   `examples/` 中所有 `.relon` 源、产出与 reference 同构的 AST。
2. **复用 std `.relon` 源**：`crates/relon-evaluator/src/std_relon/`
   下的 `.relon` 文件是 std 模块的 reference 行为；你只需要把
   `_*` intrinsic（`_list_map` 等）实现为 native，剩下的 std
   函数都是用纯 relon 写的、跨 runtime 共享。
3. **过 conformance 测试**：`fixtures/golden/` 下是参考输出；任何
   conformant runtime 跑同一份 source 必须产出相同 JSON。
4. **错误码要对齐**：见 §5。

> 详细 implementer guide 在 `docs/zh/guide/host-integration.md`
> 与本规范配合阅读。

## 附录 A：与「configuration language」叙事的告别

历史上 Relon 文档把自己描述为「typed business-config DSL」。这是
**不准确**的：那种叙事下，每个 host 都自由扩展、脚本自由依赖
ambient state，跨 host 一致性无从谈起。

Logic-as-Portable-Data 取代了这个叙事。它意味着：

* 没有「trusted mode」让脚本绕开沙箱
* 没有 runtime-private 的全局名让脚本隐式依赖
* 没有未规范的浮点 / 迭代序行为
* std 是规范的一部分，不是可选扩展

这些选择都是为了同一个目标：**逻辑像 JSON 一样在系统之间流动，结
果完全确定。**
