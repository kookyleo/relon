# 嵌入宿主

Relon 不是「装好就跑」的独立程序——它是一个 **Rust 可嵌入的 toolkit**。这一页讲怎么把它接进你自己的进程：解析、求值、注册原生函数、定制模块解析、控制 JSON 输出形态。

> 想要不可信脚本的安全策略？看完这页之后跳到 [沙箱与权限](./sandbox.md)。

## 推荐范式：Push-by-default

在动手集成之前，先确定一件**架构决策**：外部数据怎么进 Relon？

Relon 推荐的范式是 **push**——宿主在求值**之前**完成所有 I/O，把数
据净化成 `Value` 注入 `Evaluator::run_main(scope, args)`；脚本通过
`#main(...)` 签名声明它**期望**的形状；整体保持纯函数
`(source, args) → output`：

```rust
// ✅ 推荐：push-style，#main 入口程序
use std::collections::HashMap;
use std::sync::Arc;
use relon_evaluator::{Context, Evaluator, Scope, Value};

let user_data = http_client.get(&format!("/api/user/{user_id}")).await?;
let posts_data = db.query_user_posts(user_id).await?;

// 将 host-side 数据净化成 Value
let user_value: Value = serde_json::from_value(user_data)?;
let posts_value: Value = serde_json::from_value(posts_data)?;

let analyzed = relon_analyzer::analyze(&parsed_node);
let mut ctx = Context::sandboxed().with_root(parsed_node);
ctx.analyzed = Some(Arc::new(analyzed));

let mut args = HashMap::new();
args.insert("user".to_string(), user_value);
args.insert("posts".to_string(), posts_value);

let result = Evaluator::new(Arc::new(ctx))
    .run_main(&Arc::new(Scope::default()), args)?;
```

脚本端配上一个 `#main(...)` 签名，描述 host 必须推进来的形状：

```relon
#main(User user, PostList posts)
{
    #schema User { String name: *, String tier: * },
    #schema Post { String title: * },
    #schema PostList List<Post>,
    summary: f"${user.name} has ${len(posts)} posts",
    eligible: len(posts) > 10 && user.tier == "gold"
}
```

`#main(Type name, ...) [-> ReturnType]` 是文件的**入口签名**，每个参数声明一个
host-pushed slot：

- 参数名是脚本里直接可见的根级绑定（注意：**不是** `input.user`，
  就是 `user`）；
- 参数类型必须是已声明的 `#schema` 或基础类型；
- runtime 在跑 body 之前会校验 `args` 与签名：缺字段 →
  `MissingMainArg`；多字段 → `UnexpectedMainArg`；类型不匹配 →
  `MainArgTypeMismatch`。

> **编译后端 — 结构化入参。** 编译执行器（cranelift-native /
> llvm-native / 编译版 wasm）的 buffer 协议现已支持结构化 `#main`
> 入参，而不仅是标量。以下形态都与 tree-walk oracle 逐字节一致地流入：
>
> - 标量叶子（`Int` / `Float` / `Bool` / `Null`）；
> - **`String`** 入参（如 host 读好、喂进来的文件内容）；
> - **`List<scalar>`**、**`List<String>`**、**`List<Schema>`** 与嵌套
>   **`List<List<scalar>>`** 入参（经 `.length()` 或同级标量字段读出消费；
>   元素的内层记录 —— schema 子记录 / 内层 list 记录 —— 会物化进 buffer
>   tail 区，并重定位进父 buffer 坐标系）；
> - **用户 `#schema` 结构体入参**，其字段为标量、`String`、
>   `List<scalar>`、`List<String>`、`List<Schema>` 或 `List<List<scalar>>`
>   —— 即整包结构化 config 记录，含字符串、列表、record 列表与嵌套列表字段。
>
> 编译后端仍 **暂不支持**（前置阶段以明确的 `unsupported type in
> #main` / `layout v1 does not yet support list element` 报错，绝不静默
> 回退；这类形状请改用 tree-walk 解释器）：
>
> - `Dict<_, _>` 入参（analyzer 无法给 `d["x"]` 下标定类型；结构化
>   config 请改用 `#schema` 结构体）；
> - 内层为指针数组元素的嵌套 List（`List<List<String>>` /
>   `List<List<Schema>>`）—— 递归逐元素重定位尚未建模；
> - 从 `#main` **返回** `List<Schema>` / `List<List<…>>`（入参解码已支持，
>   返回方向的写出尚未支持）；
> - 多段嵌套 schema 字段链（`o.inner.x`）。

### 入口边界 Result 与 Relon 值层 Result

宿主调用 `run_main` 拿到的是 Rust 一侧的
`Result<Value, RuntimeError>`：成功时 `Ok(json_value)`，失败时
`Err(...)`（schema 校验未过、运行时溢出、capability 拒绝等）。这条
**边界 Result 由 Rust 端承担**——脚本作者不感知。

`#main(...) -> ReturnType` 中的 `ReturnType` 描述的是 **body 产生
的 Json 形态**（一个原子值、dict 或 list），不是 Result 包装。
Relon 内置的 `Result<T, E>` / `Option<T>` 是**值层**概念（建模数据
里某个字段「可能没有」/「可能失败」），不该出现在入口签名的返回
位置。

```relon
// 正确：ReturnType 描述 body 产生的 Json
#main(Order order) -> Order
{ id: order.id, total: order.total * 1.1 }

// 应避免：在入口边界写 Result —— 与 Rust 侧的 Result 重复记账
#main(Order order) -> Result<Order, String>
...
```

宿主代码侧：

```rust
match evaluator.run_main(&scope, args) {
    Ok(value) => /* value 是 ReturnType 描述的 Json */,
    Err(e)    => /* 校验/求值/能力错误 */,
}
```

这样写有几个一致好处：
- 「外部数据契约」写在 .relon 文件里，由 `#schema` 静态校验
- host 推数据缺字段 / 类型不匹配 → 求值开始前就报错
- 多个 schema 自然组合成入口签名（每个 slot 命名空间隔离）

对应的反面（**不推荐**作为默认）：

```rust
// ⚠️ pull-style：把 I/O 搬进求值过程
ctx.register_fn("http.get",
    NativeFnGate { network: true, ..Default::default() },
    Arc::new(HttpGet),
);
```

```relon
// 脚本内主动拉数据
{
    user: http.get("/api/user/" + user_id),
    posts: db.query("SELECT * FROM posts WHERE author = " + user.id)
}
```

### 为什么 push 优先

| 维度 | push | pull |
|---|---|---|
| 「同源 + 同输入 → 字节级一致」可兑现？ | ✅ args 是显式 `Value` 树，可重放 / diff / hash | ❌ args 隐式包含 `http.get` 当时的网络状态 |
| 测试 | 构造 args 即可 | 要 mock http / db client |
| 缓存 / 预编译 / fuzz | 真·纯函数，可 memoize | 任何缓存都跟时间和外部状态绑死 |
| 审计「这段逻辑会读到什么」 | 看一眼 `#main(...)` 签名 | 要 trace 所有 host fn reachability |
| 求值确定性（spec §1） | ✅ 只要 args 一致，结果一致 | ❌ 网络 / 外部状态随时间变化，结果无法重放 |
| 心智分工 | host 负责跨界 I/O，脚本负责数据组合，边界清晰 | 两者交织 |

### pull 不是禁，是「主动放弃求值确定性」

下面这些场景里 pull 仍合理：

- **延迟加载**：数据集大到全 push 不现实（「从 1M 用户里 filter」）
- **动态查询**：query 条件依赖脚本中间计算结果
- **副作用动作**：规则引擎判断后触发邮件 / 日志 / webhook —— 本来就要 side effect
- **观察性**：调试用 `@log("...")` 装饰器，不影响结果

这些场景下 host fn 用 [`register_fn`](#受-capability-门控的注册)
注册，按需声明 `NativeFnGate { reads_clock: true, network: true, ..Default::default() }`。
**这是有意识的取舍**：脚本作者主动放弃了「同一份 args 跑两次结果
必然一致」的承诺，换取了「能动态拉数据」。spec §1 的求值确定性只覆
盖 push 形态。

> **一句话总结**：能 push 就 push。只在 push 实在不可行时（数据量、
> 动态性、副作用）才用 pull，并且清楚知道这部分逻辑不再可重放。

## 入口程序 vs 库

是否声明 `#main(...)` 决定了文件**怎么用**：

| 声明 | 用法 | 入口求值 |
| --- | --- | --- |
| `#main(...)` | 入口程序 | `Evaluator::run_main(scope, args)`；缺签名时直接 `eval_root` 会报 `NoMainSignature` |
| 无 `#main` | 纯数据库 / 共享 schema 库 | `Evaluator::eval_root(scope)` 直接求值；同时也可被 `#import` |

库文件被 `#import` 时不需要 `#main`——`#import` 只取它的导出。这条
设计的好处：
- 库与入口的边界清晰，宿主不会把库文件当 entry 跑（拒之于门外）；
- 入口程序的 args 契约写在源码里，宿主无须额外约定。

## 最小例子

最常见的需求是「读一个 `.relon` 文件，拿一个 JSON 出来」。三行：

```rust
use relon;

let json = relon::json_from_file("config/app.relon")?;
println!("{}", serde_json::to_string_pretty(&json)?);
```

如果 source 已经在内存里：

```rust
let json = relon::json_from_str(r#"{ host: "localhost", port: 8080 }"#)?;
```

> 顶层 `relon::*` API 走的是「无 `#main` 库 / 数据文件」的快路径
> （内部调 `eval_root`）。要跑带 `#main(...)` 的入口程序，请直接用
> `Evaluator::run_main(...)`。

想直接拿到一个反序列化好的强类型结构？走 serde：

```rust
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct ServerConfig {
    host: String,
    port: u16,
}

let cfg: ServerConfig = relon::from_file("config/app.relon")?;
```

`relon::from_str` / `from_file` 内部就是 `json_from_*` + `serde_json::from_value`。

## 顶层 API 一览

| 函数 | 行为 |
| --- | --- |
| `value_from_str(src) -> Value` | 解析 + 求值，返回 Relon 内存值（含 closure / schema 等不能直接 JSON 化的形态） |
| `value_from_file(path) -> Value` | 同上，从文件读 |
| `json_from_str(src) -> serde_json::Value` | 求值 + 走默认 `JsonProjector` 投影到 JSON |
| `json_from_file(path) -> serde_json::Value` | 同上，从文件读 |
| `from_str::<T>(src) -> T` | 求值 + 投影 + serde 反序列化到自定义类型 |
| `from_file::<T>(path) -> T` | 同上，从文件读 |
| `analyze_from_str(src) -> AnalyzedTree` | **只**跑 parser + analyzer，不求值——用来给 LSP / CI 拿静态诊断 |
| `project_with(&projector, &value) -> P::Output` | 用自定义 `Projector` 处理已经求值的 `Value` |
| `project_from_str(src, &projector) -> P::Output` | parse + eval + 投影一气呵成 |

## `Context` 是什么

走 `relon::*` 顶层 API 时，`Context` 在内部被构造好。如果你需要注册原生函数、装饰器、自定义模块解析或 capability，就要直接构造 `Context`：

```rust
use relon_evaluator::{Context, Evaluator, Scope};
use relon_parser::parse_document;
use std::sync::Arc;

let node = parse_document(source).unwrap();
let mut ctx = Context::sandboxed().with_root(node);

// （在这里注册函数 / 装饰器 / 替换 module resolver）

let value = Evaluator::new(Arc::new(ctx)).eval_root(&Arc::new(Scope::default()))?;
```

`Context` 持有：

- **`functions`** — 通过 `register_fn` 注册的原生函数表（纯函数走便捷封装 `register_pure_fn`）。
- **`decorators`** — 通过 `register_decorator` 注册的装饰器插件。
- **`module_resolvers`** — `#import` 走的解析器链；`Context::sandboxed()` 默认是 `[StdModuleResolver, FilesystemModuleResolver::default()]`。
- **`capabilities`** — 沙箱 / 资源预算（[沙箱与权限](./sandbox.md) 详解）。
- **`root_node`** + **`analyzed`** — 根 AST 与 analyzer side-table（含 `#main` 签名）。
- **多份 cache**（path / module / loading）——避免重复求值。

> 历史说明：早期版本提供 `Context.input: Option<Value>` 和
> `with_input(value)` 作为 push 入口，已**移除**——push 现在统一走
> `Evaluator::run_main(scope, args)`。再之前的 `Context.globals:
> HashMap<String, Value>` 通用注入点也已移除：多种语义混在一个 map
> 里会让破壳点散布；现在是单一入口 + `#main` 契约。

构造方式有两条主线：

| 构造器 | 默认安全等级 |
| --- | --- |
| `Context::sandboxed()` | 完全沙箱：filesystem 默认拒绝、capability 全空、只剩 `std/...` 虚拟模块 |
| `Context::new()` | 轻量基础构造器：只挂载虚拟 std 模块与内置纯函数；需要真实 workloads 时优先用 `Context::sandboxed()` 并显式授权 |
| `Capabilities::all_granted()` + `FilesystemModuleResolver::trusted()` | 宿主自有脚本的显式全开形态：filesystem 全开、门控 native fn 全放、无步数 / 大小预算 |

## 注册一个原生函数

最常见的需求：暴露一个由 Rust 算的常量或纯函数给 `.relon` 用。

```rust
use relon_evaluator::{Context, NativeArgs, RelonFunction, Value, RuntimeError};
use relon_parser::TokenRange;
use std::sync::Arc;

struct AppVersion;

impl RelonFunction for AppVersion {
    fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
        Ok(Value::String(env!("CARGO_PKG_VERSION").to_string()))
    }
}

let mut ctx = Context::new();
ctx.register_pure_fn("app_version", Arc::new(AppVersion));
```

之后在 `.relon` 里：

```relon
{
    version: app_version()
}
```

要点：

- `register_pure_fn` 是 `register_fn(name, NativeFnGate::default(), fn)`
  的便捷封装：声明一个空 gate，任何 `Capabilities` 都能平凡满足，所
  以纯函数在沙箱下也能直接调。
- `NativeArgs` 同时拆好了 positional 和 named 参数：`args.get(0)` 拿位置参数，`args.get_named("name")` 拿命名参数。
- 函数返回 `Value`——Relon 的内存值类型；想构造 dict / list 用 `Value::Dict` / `Value::List`。

## 受 capability 门控的注册

读文件、调网络、读环境这类**有副作用**的函数，用 `register_fn` 注
册时把对应的 `NativeFnGate` bit 标上：

```rust
use relon_evaluator::{Context, NativeFnGate, NativeArgs, RelonFunction, Value, RuntimeError};
use relon_parser::TokenRange;
use std::sync::Arc;

struct ReadSecret;

impl RelonFunction for ReadSecret {
    fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
        let secret = std::fs::read_to_string("/etc/myapp/secret").unwrap_or_default();
        Ok(Value::String(secret))
    }
}

let mut ctx = Context::sandboxed();
ctx.register_fn(
    "secret.read",
    NativeFnGate { reads_fs: true, ..Default::default() },
    Arc::new(ReadSecret),
);

// 沙箱下放行的方式：授予 gate 声明的每一个 bit
ctx.capabilities.reads_fs = true;
```

每个原生函数都走同一条 gate 检查：函数声明的所有 bit 都必须在
`Capabilities` 里被授予，否则 `CapabilityDenied`。`register_pure_fn`
注册的纯函数声明的是空 gate，零 bit 缺失，所以不需要 capability 授
权也能跑；`register_fn(name, gate, fn)` 在 `gate` 含任何置位的 bit
时就需要宿主显式授予对应能力。`Capabilities::all_granted()` 一次把
六个 bit 全部打开。详见
[沙箱与权限](./sandbox.md)。

## 模块解析（Module Resolvers）

`#import <bindspec> from "path"` 不是直接读文件——它问 `Context::module_resolvers` 链上的每个 resolver 「你能解析这个路径吗？」第一个返回 `Some(ModuleSource)` 的赢，错误（`Err`）会立刻中断。

默认链：

1. **`StdModuleResolver`** — 解析 `std/list`、`std/string` 这些虚拟模块（嵌在 binary 里，零 IO）。
2. **`FilesystemModuleResolver`** — 从文件系统读：
   - host-owned 脚本可显式安装 `FilesystemModuleResolver::trusted()`，无 root 限制；
   - `Context::sandboxed()` 下使用 `FilesystemModuleResolver::default()`，**默认拒绝一切**——必须替换或追加一个 `with_root_dir(...)` 实例。

替换示例：

```rust
use relon_evaluator::{Context, FilesystemModuleResolver, StdModuleResolver};
use std::sync::Arc;

let mut ctx = Context::sandboxed();
ctx.module_resolvers = vec![
    Arc::new(StdModuleResolver),
    Arc::new(FilesystemModuleResolver::with_root_dir("/var/relon-configs")),
];
```

`with_root_dir` 会把 root 路径 canonicalize，并在每次 import 时确认目标路径在 root 下面（包括防止符号链接逃逸）——细节见 [沙箱与权限](./sandbox.md#filesystemmoduleresolver-的行为)。

要插入自定义 resolver（比如「从内存读」「从 OCI registry 读」），实现 `ModuleResolver` trait 然后：

```rust
ctx.prepend_module_resolver(Arc::new(MyResolver)); // 走最前
// 想做 fallback：直接 push 到 ctx.module_resolvers 末尾即可
ctx.module_resolvers.push(Arc::new(FallbackResolver));
```

## 装饰器插件

**`@name(...)` 装饰器**只用于值变换，区别于结构 / 元数据用的
`#name ...` 指令（详见 [基础语法](./syntax.md)）：

- 内置：`@value(...)` 是唯一一个由 runtime 提供的装饰器名字；
- 用户定义：`@my_fn(arg)` 等价于把下方值传入 `my_fn` 的最后一个位置
  参数。`my_fn` 可以是同 dict 内的闭包、`#import` 进来的函数，乃至
  host 注册的 native fn——任何可调用的绑定都行；
- 宿主注册：实现 `DecoratorPlugin` trait 之后注册一个名字。

```rust
use relon_evaluator::{Context, DecoratorPlugin};
// 实现 trait 的细节略——3 个钩子全是 default no-op
ctx.register_decorator("my_org.audit", Arc::new(MyAuditPlugin));
```

`DecoratorPlugin` 提供三个钩子，全部默认 no-op，按需要 override：

| 钩子 | 触发时机 | 典型用途 |
| --- | --- | --- |
| `pre_eval` | 在被装饰节点求值**之前** | 注入 scope / 直接覆盖结果 |
| `wrap` | 在被装饰节点求值**之后** | 校验、转换（如 `@ensure.int`） |
| `schema_field_meta` | 从 schema 字典提取字段时 | 给字段挂元数据 |

trait 完整签名见 `crates/relon-evaluator/src/decorator.rs`，这里不抄一遍——大多数宿主只需要 `wrap`。

## `Projector`：定制 JSON 输出形态

默认的 `JsonProjector` 把 `Value` 投影成 `serde_json::Value`，处理细节：

- 闭包、schema、type、wildcard 在 dict 里**静默丢弃**（保留运行时元素，不污染 JSON）；
- 出现在顶层时**报错**（没法投影成 JSON）；
- 非有限浮点（`Infinity`/`NaN`）报错；
- sum-type 变体输出**外部标签**形式：`{ "Email": { ... } }`；
- 普通 branded dict 保持**扁平**——`#schema User` 标过的 dict 不会被包一层。

想换一种输出形态——比如 sum-type 用 `{ "type": "Email", "address": "..." }` 内部标签风格，或者直接 BSON、Protobuf——实现 `Projector` trait：

```rust
use relon::Projector;
use relon_evaluator::Value;

struct InternallyTaggedJson;

#[derive(Debug, thiserror::Error)]
#[error("projection failed: {0}")]
struct ProjErr(String);

impl Projector for InternallyTaggedJson {
    type Output = serde_json::Value;
    type Error = ProjErr;

    fn project(&self, value: &Value) -> Result<Self::Output, Self::Error> {
        // 自己控制遍历，对 Value::Dict 看 brand/variant_of 改写形状……
        todo!()
    }
}

let json = relon::project_from_str(source, &InternallyTaggedJson)?;
```

> **注意范围**：`Projector` 是「JSON 形状的微调旋钮」，不是「跳出 JSON 的逃生通道」。Relon 的输出永远要落到 JSON 上——这是它的硬约束。如果你想生成 YAML/TOML/XML，那是另一种工具的领域（比如 Pkl）。

## 错误类型

`relon::Error` 是 facade crate 的统一错误：

| 变体 | 来源 |
| --- | --- |
| `Error::Parse(String)` | 词法 / 语法错误 |
| `Error::Analyze(Vec<Diagnostic>)` | analyzer 错误**批量**返回（4 个 pass 一起跑完） |
| `Error::Eval(RuntimeError)` | 求值期错误：类型不匹配、未定义引用、capability 拒绝、step 超限等 |
| `Error::Io { path, source }` | 读文件失败 |
| `Error::Deserialize(serde_json::Error)` | `from_str::<T>` 类 API 反序列化失败 |
| `Error::NonFiniteFloat(f64)` | JSON 投影时遇到 `Infinity` / `NaN` |
| `Error::UnsupportedClosure` / `UnsupportedSchema` | 顶层就是一个 closure 或 schema，没法投影 |

`RuntimeError` 在沙箱模式下还会出现 `CapabilityDenied`、
`StepLimitExceeded`、`ValueTooLarge`——这些归属
[沙箱与权限](./sandbox.md)。入口程序还会出现 `NoMainSignature`（库
文件被当 entry 跑）、`MissingMainArg`/`UnexpectedMainArg`/
`MainArgTypeMismatch`（args 不匹配 `#main` 签名）。

## 接下来

- 不可信脚本的安全策略：[沙箱与权限](./sandbox.md)
- 让 `.relon` 端能用上你注册的函数：在 schema / library 里包装它们，参考 [类型与契约](./types.md)
- 错误的 miette 友好格式：直接把 `RuntimeError` / `Diagnostic` 喂给 `miette::Report`
