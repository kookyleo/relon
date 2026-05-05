# 沙箱与权限

Relon 的定位是「**可嵌入宿主**的工具集」——这意味着它经常会被拿去跑**用户提供的脚本**：feature flag 配置、A/B 实验描述、由产品经理改的业务规则。这类脚本不可信，宿主必须能把它关进笼子。

`Capabilities` 就是这个笼子。

## 为什么要做沙箱

不可信脚本能干什么坏事？

- **读文件**：`@import("/etc/passwd", as="...")` 把宿主进程的可读文件吸出来。
- **求值爆炸**：写一个无限递归 / 指数递归的 closure 把进程吃光。
- **超大 value**：构造一个百万元素的 list / dict 把内存吃光。
- **调宿主注册的危险函数**：宿主注册了 `secret.read`、`db.query`，但不希望任意脚本都能调。

Relon 的沙箱针对每一类都有 capability 旋钮——**默认全关**，宿主显式打开后才生效。

## `trusted()` vs `sandboxed()`

`Context` 提供两个语义化构造器：

| | `Context::trusted()` （= `Context::new()`） | `Context::sandboxed()` |
| --- | --- | --- |
| 文件读 | 全开（`FilesystemModuleResolver::trusted()`） | 默认拒绝（`FilesystemModuleResolver::default()`，无 root） |
| 步数预算 | 无限 | 无限（你需要显式设） |
| Value 大小预算 | 无限 | 无限（你需要显式设） |
| `register_fn` 注册的函数 | 全放 | 全放（合法语义：fn 已被宿主认证过） |
| `register_fn_with_caps` 注册的函数 | 全放（`allow_all_native_fn = true`） | 默认拒绝（除非加进 `allow_native_fn` 白名单） |
| 用法场景 | 自己人写的脚本、CLI、构建期 | 用户脚本、SaaS 配置、多租户场景 |

> 关于步数 / Value 大小：`sandboxed()` 不会自动给你套上限——它默认让你**自己**决定预算。这样设计是因为合理预算高度依赖业务场景，没有合适的「安全默认」。

切换到 sandbox 后，多数情况下你需要做这几件事：

```rust
use relon_evaluator::{Context, FilesystemModuleResolver, StdModuleResolver};
use std::sync::Arc;

let mut ctx = Context::sandboxed();

// 1. 给一个文件读 root（如果脚本需要 @import 别的 .relon）
ctx.module_resolvers = vec![
    Arc::new(StdModuleResolver),
    Arc::new(FilesystemModuleResolver::with_root_dir("/var/relon-userscripts")),
];

// 2. 设步数预算（防止递归 / 推导式爆炸）
ctx.capabilities.max_steps = Some(1_000_000);

// 3. 设 value 元素水位（防止巨型 list/dict）
ctx.capabilities.max_value_bytes = Some(10_000);

// 4. 把允许调用的「门控」原生函数加白名单（如果有）
ctx.capabilities.allow_native_fn.insert("currency.format".to_string());
```

## `Capabilities` 四个字段

完整结构（`crates/relon-evaluator/src/eval.rs::Capabilities`）：

```rust
pub struct Capabilities {
    pub max_steps: Option<u64>,
    pub max_value_bytes: Option<usize>,
    pub allow_native_fn: HashSet<String>,
    pub allow_all_native_fn: bool,
}
```

> 文件读策略不在 `Capabilities` 里——它由 `FilesystemModuleResolver::with_root_dir(...)` 在 resolver 层强制。详见下一节。

逐个解释：

### `max_steps: Option<u64>`

求值器内部有一个「步数计数器」——每次 `eval_internal` 进来就 `+1`。`max_steps = Some(N)` 表示最多走 N 步，超了直接抛 `RuntimeError::StepLimitExceeded`。

```rust
ctx.capabilities.max_steps = Some(100);
// 之后跑 `loop(): loop()` 这种无限递归，会在第 101 步被截住
```

`None`（默认）= 不限。注意：步数对应的是「dispatch 次数」，不是 CPU 时间——一次 dispatch 内部如果调了一个慢的内置算子（比如大列表的 `string.join`），步数只 +1 但实际耗时可能高。如果你需要更严格的 CPU 控制，请在宿主侧加 wall-clock timer（详见 [不在沙箱设计内的事](#不在沙箱设计内的事)）。

### `max_value_bytes: Option<usize>`

字段名带 `_bytes` 是为了未来扩展，**当前的实测维度是「Value 的元素数」**——一个 list 的元素个数，或一个 dict 的键值对数。检查点在求值器构造 list/dict 的边界（字面量、`+` 合并、推导式）。

```rust
ctx.capabilities.max_value_bytes = Some(3);
// `[1, 2, 3, 4, 5]` 触发 ValueTooLarge { limit: 3, actual: 5 }
```

注意：通过 `register_fn` 自己写的函数返回的 list/dict 不会被自动检查——那是宿主自己的领域（你在自己的函数里想多大就多大）。

### `allow_native_fn: HashSet<String>`

通过 `register_fn_with_caps` 注册的「门控」函数，沙箱模式下**只有名字在这个集合里**才能调。详见 [嵌入宿主：受 capability 门控的注册](./host-integration.md#受-capability-门控的注册)。

```rust
ctx.register_fn_with_caps("fs.read", NativeFnCaps { reads_fs: true }, Arc::new(MyReader));
ctx.capabilities.allow_native_fn.insert("fs.read".to_string());
// 沙箱下 `.relon` 里调 fs.read() 会过；没加白名单则 CapabilityDenied
```

`register_fn`（不带 `_with_caps`）注册的函数**不**走这条路——它们被视作完全可信，在 sandbox 下也直接放行。

### `allow_all_native_fn: bool`

trusted 模式的「旁路开关」——`Context::trusted()` 把它设成 `true`，让所有 `register_fn_with_caps` 注册的函数也能跑。`Context::sandboxed()` 默认 `false`。一般不要自己改这个字段，用对应的构造器即可。

## `FilesystemModuleResolver::with_root_dir` 的行为

这是文件读限制的**真正执行点**：

1. 构造时，把 root 路径走 `std::fs::canonicalize` 解析掉所有 `..` 和 symlink，得到一个干净的绝对路径。
2. 每次 `@import` 进来：
   - 把目标路径 join 到当前 scope 的 `current_dir`，再 canonicalize 一次（同样会消解 symlink）。
   - 检查 canonical target 是否以 root 为前缀；不是则返回 `RuntimeError::CapabilityDenied { reason: "path escapes filesystem root ..." }`。

这意味着两类常见攻击都会被挡：

- **`../` 路径逃逸**：`@import("../../etc/passwd")` 的 canonical 形态显然不在 root 下面。
- **symlink 逃逸**：在 root 里放一个 symlink 指向外面，canonicalize 也会解析掉，仍然检查得到。

```rust
let mut ctx = Context::sandboxed();
ctx.module_resolvers = vec![
    Arc::new(StdModuleResolver),
    Arc::new(FilesystemModuleResolver::with_root_dir("/var/relon-userscripts")),
];

// 用户脚本里写 @import("../../etc/passwd", as="x")
// → CapabilityDenied { reason: "path escapes filesystem root /var/relon-userscripts" }
```

Default-constructed (`FilesystemModuleResolver::default()`) 的 resolver **任何**路径都拒——这是 `Context::sandboxed()` 给你的默认。

## Recipe：跑用户脚本 + 暴露一个 readonly 函数

把上面所有积木拼起来——典型场景：用户提交一段 `.relon` 当作 feature-flag 规则，宿主提供一个「读当前用户 ID」的函数，其它一律不准动。

```rust
use relon_evaluator::{
    Capabilities, Context, FilesystemModuleResolver, NativeArgs, NativeFnCaps,
    RelonFunction, StdModuleResolver, Value, RuntimeError,
};
use relon_parser::{TokenRange, parse_document};
use std::sync::Arc;

struct CurrentUserId(String);

impl RelonFunction for CurrentUserId {
    fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
        Ok(Value::String(self.0.clone()))
    }
}

fn run_user_rule(rule_src: &str, current_user: &str) -> Result<serde_json::Value, RuntimeError> {
    let mut ctx = Context::sandboxed();

    // 文件读关掉（用户脚本不允许 @import 文件，只能用 std/）
    ctx.module_resolvers = vec![Arc::new(StdModuleResolver)];

    // 求值预算
    ctx.capabilities.max_steps = Some(100_000);
    ctx.capabilities.max_value_bytes = Some(10_000);

    // 暴露一个门控的、只读的函数
    ctx.register_fn_with_caps(
        "user.current_id",
        NativeFnCaps::default(),
        Arc::new(CurrentUserId(current_user.to_string())),
    );
    ctx.capabilities
        .allow_native_fn
        .insert("user.current_id".to_string());

    // 求值
    let node = parse_document(rule_src)
        .map_err(|e| RuntimeError::IoError(e.to_string()))?;
    let ctx = ctx.with_root(node);
    let scope = Arc::new(relon_evaluator::Scope::default());
    let value = relon_evaluator::Evaluator::new(&ctx).eval_root(&scope)?;

    // 投影成 JSON（细节略，参考 host-integration）
    Ok(relon::JsonProjector.project(&value).expect("…"))
}
```

这套设置下，用户脚本：

- ✅ 能用 `std/list` / `std/string` / `std/dict` 这些纯计算模块
- ✅ 能调 `user.current_id()` 拿当前用户 ID
- ❌ 不能 `@import` 文件
- ❌ 不能调 `currency.format` 这种宿主**没**加进白名单的函数
- ❌ 跑超 10 万步 → `StepLimitExceeded`
- ❌ 构造超 10000 元素的 list/dict → `ValueTooLarge`

## 错误清单

沙箱触发的运行时错误（`RuntimeError` 中的相关变体）：

| 错误 | 触发条件 |
| --- | --- |
| `CapabilityDenied { name, reason, range }` | `@import` 走到 default-reject 的 resolver；或 `@import` 路径逃出 root；或调一个未在 `allow_native_fn` 里的门控函数 |
| `StepLimitExceeded { limit, range }` | `eval_internal` 调用次数超过 `max_steps` |
| `ValueTooLarge { limit, actual, range }` | 单个 list/dict 元素数超过 `max_value_bytes` |

每一种都带 `TokenRange`——可以直接喂给 miette 拿到带源码上下文的可读输出。

## 不在沙箱设计内的事

为了不让大家高估它，列一下 Relon 沙箱**不**承担的职责：

- ❌ **CPU 挂钟时间限制**：Relon 没有内建 wall-clock budget。如果你需要「这个脚本最多跑 100ms」的语义，请在宿主侧用 `tokio::time::timeout` / 单独线程 + 超时通道实现。
- ❌ **堆字节精确计量**：`max_value_bytes` 只查 list/dict 的元素数，不算 String 的字节数、不算闭包捕获的引用计数。如果你想要严格的进程内存上限，加 OS 层 `setrlimit` / cgroup。
- ❌ **跨进程隔离**：Relon 跑在你的进程里，挂了会带翻你的进程（不过 Rust 没有 segfault 风险——RuntimeError 是干净抛出的）。强隔离需求请把 Relon 跑在子进程 / wasm 沙箱里。
- ❌ **网络 / IPC 隔离**：Relon 自身没有网络原语，所以默认天然没有网络。但是！如果你**注册了**一个会发网络请求的原生函数，这是宿主层的事，Relon capability 层管不到——记得用 `register_fn_with_caps` 标 `reads_fs` 之外（未来）的相关 cap，并把名字默认放在白名单外。

## 接下来

- 怎么注册函数 / 装饰器 / module resolver：[嵌入宿主](./host-integration.md)
- 用 `@library` 把可信库和不可信入口分开：[库与入口](./library-vs-entry.md)
- schema / `@expect` 是另一道防线：[类型与契约](./types.md)
