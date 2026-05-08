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
——host 在求值前通过 `Context::with_input(value)` 注入的数据，脚本
通过保留名 `input` 读取。

Host 通过 `register_fn` 注入的 native fn 的**调用结果**不属于「输
入」。因此：

- **Push 形态**（host 求值前完成 I/O，把数据 materialize 成 `Value`
  通过 `with_input` 注入；脚本可选地用 `@input` 装饰器声明契约）：
  跨 runtime 一致性在本规范保障范围内。
- **Pull 形态**（脚本求值期通过 native fn 拉外部数据）：脚本作者
  **主动放弃**了跨 runtime 一致性——不同 host / 不同 runtime / 不同
  时刻的网络与外部状态本就不同，spec 不要求也无法保证一致。

详见 [host-integration.md §推荐范式：Push-by-default](./host-integration.md#推荐范式push-by-default)。

### 1.3 保留根级名

下列标识符是**保留根级名**，conformant runtime 在这些名字上必须实
现 spec 规定的语义；脚本不得将它们用作 dict 字段名、闭包参数名、
或 `where` 子句名：

| 名字 | 语义 |
|---|---|
| `input` | 当前文件的 push-style 外部输入（见 §1.2）。引用形态 `input.foo.bar`。host 未推数据且文件无 `@input` 时读 `input.foo` 失败 (`VariableNotFound`)。 |

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
  传入 input。
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

## 4. 能力模型（Capabilities）

### 4.1 默认零特权

新构造的 `Context` 默认**没有任何能力**。脚本：

* 无法读文件系统（`@import("./local.relon")` → `CapabilityDenied`）
* 无法调用任何 `register_fn_with_caps` 注册的 host 原生函数
* 没有执行步数 / value 体积上限（`None` 表示「不强制」，但 host 应
  根据信任程度显式设置）

### 4.2 显式授权才放行

Host 通过 `Capabilities` 字段显式授权：

```rust
let mut ctx = Context::sandboxed();
ctx.capabilities.reads_fs = true;                          // 允许 @import 真实文件
ctx.capabilities.allow_native_fn.insert("fs.read".into());  // 允许调用具名 host fn
ctx.capabilities.max_steps = Some(1_000_000);               // 限制求值步数
```

或一次性授权全部（`Capabilities::all_granted()`）——但这是显式的、
可审计的赋权，不是隐式的「trusted 模式」。**规范禁止任何 `trusted()`
或类似的「全开」捷径构造器**：脚本必须能在任何 runtime 上观察到
host 授予了什么、没授予什么。

### 4.3 std 虚拟模块的特殊位置

`@import("std/list")`、`@import("std/string")` 等 std 模块通过
**虚拟解析器**（`StdModuleResolver`）服务，**不消耗** `reads_fs`
能力。这是规范的有意设计：std 是规范的一部分，对它的访问不属于
host 信任决策。

## 5. 错误种类（Error Kinds）

所有 conformant runtime 必须使用以下稳定标签：

| Kind | 触发条件 |
|---|---|
| `Parse` | 词法 / 语法错误 |
| `Analyze` | 语义分析阶段聚合错误（`@schema` 异构、未类型化字段等）|
| `TypeMismatch` | 运行时值不符合声明类型 |
| `VariableNotFound` | 引用未定义的名字（含 schema 名、模块 alias、函数名）|
| `FunctionNotFound` | 调用未注册的原生函数或闭包 |
| `CircularImport` | `@import` 形成环 |
| `ModuleNotFound` | 没有 resolver 返回该模块 |
| `ModuleParseError` | 模块文件解析失败 |
| `IoError` | 真实 I/O 错误（被允许的 `reads_fs` 操作中发生）|
| `CapabilityDenied` | 受 §4 拦截 |
| `StepLimitExceeded` | 触发 `max_steps`（求值步数预算耗尽）|
| `RecursionLimitExceeded` | 类型检查 / schema 验证递归深度超过运行时安全上限（与 `max_steps` 是不同维度的预算，hosts 不能通过调高 `max_steps` 缓解）|
| `ValueTooLarge` | 触发 `max_value_bytes` |
| `LibraryAsEntry` | 试图把 `@library` 文件当 host entry |
| `UnsupportedOperator` | 无效操作或类型组合 |

## 6. 标准库目录（Spec-mandated）

每个 conformant runtime 必须实现以下 std 模块。脚本通过
`@import("std/<name>", as="<alias>")` 引入。

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

`@schema` 装饰器内部依赖一组 `ensure.*` 函数（`ensure.int`、
`ensure.string` 等）。这些是 schema 系统的实现细节，不暴露给脚本
直接调用——但 conformant runtime 必须确保它们存在且按规范工作，
否则 `@schema` 行为会发散。

### 6.4 `@input(name=SchemaRef)` —— 程序输入契约

`@input(...)` 是**根级装饰器**（装饰文件的根 dict），声明 host-pushed
input 中的一个**命名 slot**。每个 slot 用 `name=SchemaRef` 形式给出：
slot 名是 input wrapper 中的字段名，SchemaRef 是已声明的 `@schema`
（本文件或 imported 的）。形态：

```relon
@input(req=Req)
{
    @schema Req: {
        String name: *,
        @default(0)
        Int retries: *
    },
    greeting: f"hello ${input.req.name}, retries=${input.req.retries}"
}
```

多个 slot 可以并列声明，runtime 自动合并成一个 wrapper schema
`{ <slot1>: <schema1>, <slot2>: <schema2>, ... }`：

```relon
@input(user=User)
@input(cart=Cart)
{
    @schema User: { String name: * },
    @schema Cart: { Int total: * },
    summary: f"${input.user.name} - ${input.cart.total}"
}
```

**语义要求**（每个 conformant runtime 必须按此实现）：

1. `@input(...)` 必须是**根级装饰器**（写在文件根 dict 之前）；装饰
   字段或非根 dict 时无意义。
2. 每个参数必须是 `name=SchemaRef` 形式：
   - 缺 name（位置参数）→ `Analyze` 错误 `InputDecoratorMissingName`。
   - 同一 slot name 被多次声明 → `Analyze` 错误 `DuplicateInputName`。
   - 完全没参数（`@input`）→ `Analyze` 错误 `InputDecoratorEmpty`。
3. 求值 `Context::with_input(value)` 注入的数据**前**，必须按合并后
   的 wrapper schema 校验：
   - host-pushed value 必须是 `Value::Dict`；否则 `TypeMismatch`。
   - 每个声明的 slot 必须出现在 pushed dict 中；否则
     `TypeMismatch`（`expected: input slot '<name>'`，`found: missing`）。
   - 每个 slot 的值按对应 SchemaRef 求值出的 `Value::Schema` 校验：
     字段类型不匹配 / 缺必填字段 → `TypeMismatch`；带 `@default(...)`
     的字段 host 未推时用默认值填充。
4. 校验后的 input 树绑定到保留根级名 `input`（§1.3），脚本通过
   `input.<slot>.<field>` 访问。
5. 文件**没有 `@input(...)`** 时，`with_input` 推入的数据按原样绑定
   到 `input`，不做 schema 校验；脚本读 `input.foo` 时若数据缺字段
   则退化为运行时 `VariableNotFound`。
6. **跨文件 `@input` 聚合**（即 lib 中的 `@input(...)` 也参与 entry
   的总契约）暂不在 v1 范围内——v1 只校验 entry 文件的
   `@input(...)`。lib 中的 `@input(...)` 当前由 evaluator 视作识别
   到的根装饰器但不参与 host input 校验；建议 lib 只导出 `@schema`，
   由 entry 通过 `@input(slot=lib.Schema)` 引用。

`@input(...)` 把「外部数据契约」写进 .relon 源码而非 host 端，使任何
conformant runtime 看同一份脚本都按相同 schema 校验——这是 §1.2 跨
runtime 一致性兑现的关键拼图。

#### 6.4.1 `@schema(Name={...})` —— 根级 schema 装饰器糖

把 schema 声明从根 dict 体内挪到根装饰器栈里，与 `@input(...)` 并
排，纯粹是**布局糖**——语义等价于在根 dict 里写一个
`@private @schema Name: { ... }` 字段。

```relon
// 老写法：schema 在 dict 体内，被 @input 从 dict 外引用
@input(req=Req)
{
    @schema Req: { String name: *, Int retries: * },
    greeting: f"hello ${input.req.name}"
}

// 新写法：schema 与 @input 同处装饰器栈，视觉上同一处
@schema(Req={ String name: *, Int retries: * })
@input(req=Req)
{
    greeting: f"hello ${input.req.name}"
}
```

可以一次声明多个：

```relon
@schema(User={ String name: * })
@schema(Cart={ Int total: * })
@input(user=User)
@input(cart=Cart)
{
    summary: f"${input.user.name} - ${input.cart.total}"
}
```

**规则**：

1. `@schema(...)` 只在**根级**装饰器栈里有此语义；嵌套 dict 上写
   `@schema(Name=...)` 不会触发糖。
2. 每个参数必须是 `Name=Body`：
   - 缺 name（位置参数）→ `Analyze` 错误 `RootSchemaDecoratorMissingName`。
   - `Body` 必须是 dict 字面量 `{ ... }` 或 `Enum<...>` 类型表达式；
     其它形态 → `RootSchemaInvalidValue`。
   - 同一文件内重复声明同名 schema → `DuplicateRootSchemaName`。
   - 同名 schema 同时以根装饰器形与字段形（`@schema X: ...`）声明 →
     `RootSchemaCollidesWithField`（必须二选一）。
   - 完全没参数（`@schema()`）→ `RootSchemaDecoratorEmpty`。
3. 注册的 schema 同时对**根 dict 体内**与 `@input(...)` 引用可见，
   解析路径与字段形 `@schema` 完全一致。
4. 这是纯布局糖，不引入新语义；任何 conformant runtime 必须把它视作
   `@private @schema Name: Body` 字段的等价处理，否则会偏离 §1.2 的
   跨 runtime 一致性。

## 7. Host 可注册扩展的边界

Host 可以通过 `register_fn` / `register_fn_with_caps` /
`register_decorator` 注入：

* 原生函数（数据进、数据出）
* 装饰器插件（`@expect`、自定义 `@brand` 行为等）

但 **conformant 规范不要求其它 runtime 提供同名扩展**——脚本如果
依赖了 host 注入的名字，它就脱离了「跨 runtime 可移植」的承诺，
仅在该 host 上保证行为。

最佳实践：

* 业务库写在 `.relon` 中（用 `@library` 标记），通过 `@import`
  分发；这样的库自动跨 runtime 可移植。
* 只在「必须用宿主能力」的场景注册原生函数（FS、数据库、HTTP），
  并用 `register_fn_with_caps` 标记所需 `NativeFnGate`。

## 8. 版本化

* 本文档对应 **spec v1**。
* std 模块按 semver 演进：函数语义变更必须升 major；新增函数升
  minor。
* `@import("std/<name>")` 默认绑定到 runtime 实现的最新兼容版本。
  未来可能引入 `@import("std/<name>", version="1.x")` 显式绑定。
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
