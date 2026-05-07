# 沙箱与权限

Relon 的承重定位是 **Logic as Portable Data**——同一份脚本在任何
conformant 运行时上拿到同一个输入都给出相同结果。这意味着脚本**不可
以**依赖任何宿主侧的隐式信任：FS 访问、网络、宿主原生函数、求值预
算，全部由宿主**显式授权**。

`Capabilities` 就是这个授权通道。规范明确禁止「trusted 模式」式的
旁路构造器（详见 [语言规范](./spec.md) §4.2）——脚本必须能在任何
runtime 上观察到宿主授予了什么、没授予什么。

## 沙箱要防什么

不可信脚本能干什么坏事？

- **读文件**：`@import("/etc/passwd", as="...")` 把宿主进程能读的
  文件吸出来。
- **求值爆炸**：写一个无限递归 / 指数递归的 closure 把进程吃光。
- **超大 value**：构造一个百万元素的 list / dict 把内存吃光。
- **调宿主注册的危险函数**：宿主注册了 `secret.read`、`db.query`，
  但不希望任意脚本都能调。

Relon 对每一类都给了 capability 旋钮——**默认全关**，宿主显式打开
后才生效。

## 唯一构造器：`Context::sandboxed()`

`Context` 只有一个面向真实使用的构造器——`Context::sandboxed()`：
默认零特权，宿主必须显式授权才能放行。

```rust
use relon_evaluator::module::FilesystemModuleResolver;
use relon_evaluator::{Capabilities, Context};
use std::sync::Arc;

let mut ctx = Context::sandboxed();

// 1. 给一个文件读 root（如果脚本需要 @import 别的 .relon）
ctx.capabilities.reads_fs = true;
ctx.prepend_module_resolver(Arc::new(
    FilesystemModuleResolver::with_root_dir("/var/relon-userscripts"),
));

// 2. 设步数预算（防止递归 / 推导式爆炸）
ctx.capabilities.max_steps = Some(1_000_000);

// 3. 设 value 元素水位（防止巨型 list/dict）
ctx.capabilities.max_value_bytes = Some(10_000);

// 4. 把允许调用的「门控」原生函数加白名单（如果有）
ctx.capabilities.allow_native_fn.insert("currency.format".to_string());
```

### 「我就是想全开」——`Capabilities::all_granted()`

宿主自己写的脚本（CLI、构建期、host-owned 配置文件）当然有权访问
所有能力。规范要求**这个授权必须显式可见**，不能藏在一个名为
`trusted()` 的构造器后面。所以代码里写出来：

```rust
let mut ctx = Context::sandboxed();
ctx.capabilities = Capabilities::all_granted();
ctx.prepend_module_resolver(Arc::new(FilesystemModuleResolver::trusted()));
```

这三行 = 旧的 `Context::trusted()`。区别是 code review 看一眼就知道
「FS 全开 + 所有门控函数全放 + 无步数预算」——哪行不想要就删哪行。

## `Capabilities` 五个字段

完整结构（`crates/relon-evaluator/src/eval.rs::Capabilities`）：

```rust
pub struct Capabilities {
    pub allow_all_native_fn: bool,
    pub allow_native_fn: HashSet<String>,
    pub reads_fs: bool,
    pub max_steps: Option<u64>,
    pub max_value_bytes: Option<usize>,
}
```

逐个解释：

### `reads_fs: bool`

宏观开关：脚本是否允许通过 `@import("./local.relon")`、
`@import("/abs/path/x.relon")` 触发文件读。**只是开关，不是路径限
制**——具体允许哪些路径由 `FilesystemModuleResolver` 的 root 决定
（见下文）。

`std/...` 虚拟模块**不**消耗 `reads_fs`——它们走 `StdModuleResolver`
而非文件系统，是规范的一部分。

### `max_steps: Option<u64>`

求值器内部有一个步数计数器——每次 `eval_internal` 进来就 `+1`。
`max_steps = Some(N)` 表示最多走 N 步，超了直接抛
`RuntimeError::StepLimitExceeded`。

```rust
ctx.capabilities.max_steps = Some(100);
// 之后跑 `loop(): loop()` 这种无限递归，会在第 101 步被截住
```

`None`（默认）= 不限。注意：步数对应的是「dispatch 次数」，不是
CPU 时间——一次 dispatch 内部如果调了一个慢的内置算子（比如大列
表的 `string.join`），步数只 +1 但实际耗时可能高。如果你需要更严
格的 CPU 控制，请在宿主侧加 wall-clock timer（详见
[不在沙箱设计内的事](#不在沙箱设计内的事)）。

### `max_value_bytes: Option<usize>`

字段名带 `_bytes` 是为了未来扩展，**当前的实测维度是「Value 的元素
数」**——一个 list 的元素个数，或一个 dict 的键值对数。检查点在求
值器构造 list/dict 的边界（字面量、`+` 合并、推导式）。

```rust
ctx.capabilities.max_value_bytes = Some(3);
// `[1, 2, 3, 4, 5]` 触发 ValueTooLarge { limit: 3, actual: 5 }
```

注意：通过 `register_fn` 自己写的函数返回的 list/dict 不会被自动
检查——那是宿主自己的领域（你在自己的函数里想多大就多大）。

### `allow_native_fn: HashSet<String>`

通过 `register_fn_with_caps` 注册的「门控」函数，沙箱模式下**只有
名字在这个集合里**才能调。详见
[嵌入宿主：受 capability 门控的注册](./host-integration.md#受-capability-门控的注册)。

```rust
ctx.register_fn_with_caps(
    "fs.read",
    NativeFnGate { reads_fs: true },
    Arc::new(MyReader),
);
ctx.capabilities.allow_native_fn.insert("fs.read".to_string());
// 沙箱下 `.relon` 里调 fs.read() 会过；没加白名单则 CapabilityDenied
```

`register_fn`（不带 `_with_caps`）注册的函数**不**走这条路——它们
被视作完全可信，在 sandbox 下也直接放行。stdlib（`len`、`range`、
`type`、`ensure.*`、std 模块的 `_*` intrinsics）也是用
`register_fn` 注册的：spec §4.3 把它们划入「规范内容」、不属于宿
主信任决策。

### `allow_all_native_fn: bool`

总开关：`true` 时所有 `register_fn_with_caps` 注册的函数全放，等于
忽略 `allow_native_fn`。`Capabilities::all_granted()` 帮你把这个
flag 也设成 `true`。

## `FilesystemModuleResolver` 的行为

文件读限制的**真正执行点**在 resolver，不在 `Capabilities`。三个常
用变体：

| 构造 | 行为 |
|---|---|
| `FilesystemModuleResolver::default()` | 拒绝**所有**真实路径（`std/...` 走 `StdModuleResolver`，不受影响） |
| `FilesystemModuleResolver::with_root_dir("/path")` | 只允许 `/path` 内（含递归子目录）的文件；canonical 后必须以 root 为前缀，自动挡 `../` 和 symlink 逃逸 |
| `FilesystemModuleResolver::trusted()` | 允许任意路径——**仅用于 host-owned 脚本**（CLI、构建期） |

`with_root_dir` 的具体安全语义：

1. 构造时，把 root 路径走 `std::fs::canonicalize` 解析掉所有 `..`
   和 symlink，得到一个干净的绝对路径。
2. 每次 `@import` 进来：
   - 把目标路径 join 到当前 scope 的 `current_dir`，再
     canonicalize 一次（同样会消解 symlink）。
   - 检查 canonical target 是否以 root 为前缀；不是则返回
     `RuntimeError::CapabilityDenied { reason: "path escapes filesystem root ..." }`。

这意味着两类常见攻击都会被挡：

- **`../` 路径逃逸**：`@import("../../etc/passwd")` 的 canonical
  形态显然不在 root 下面。
- **symlink 逃逸**：在 root 里放一个 symlink 指向外面，canonicalize
  也会解析掉，仍然检查得到。

## Recipe：跑用户脚本 + 暴露一个 readonly 函数

把上面所有积木拼起来——典型场景：用户提交一段 `.relon` 当作
feature-flag 规则，宿主提供一个「读当前用户 ID」的函数，其它一律
不准动。

```rust
use relon_evaluator::module::StdModuleResolver;
use relon_evaluator::{
    Context, NativeArgs, NativeFnGate, RelonFunction, RuntimeError, Value,
};
use relon_parser::{parse_document, TokenRange};
use std::sync::Arc;

struct CurrentUserId(String);

impl RelonFunction for CurrentUserId {
    fn call(&self, _args: NativeArgs, _range: TokenRange) -> Result<Value, RuntimeError> {
        Ok(Value::String(self.0.clone()))
    }
}

fn run_user_rule(
    rule_src: &str,
    current_user: &str,
) -> Result<serde_json::Value, RuntimeError> {
    let mut ctx = Context::sandboxed();

    // 文件读关掉（用户脚本不允许 @import 文件，只能用 std/）
    ctx.module_resolvers = vec![Arc::new(StdModuleResolver)];

    // 求值预算
    ctx.capabilities.max_steps = Some(100_000);
    ctx.capabilities.max_value_bytes = Some(10_000);

    // 暴露一个门控的、只读的函数
    ctx.register_fn_with_caps(
        "user.current_id",
        NativeFnGate::default(),
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

每一种都带 `TokenRange`——可以直接喂给 miette 拿到带源码上下文的可
读输出。

## 不在沙箱设计内的事

为了不让大家高估它，列一下 Relon 沙箱**不**承担的职责：

- ❌ **CPU 挂钟时间限制**：Relon 没有内建 wall-clock budget。如果你
  需要「这个脚本最多跑 100ms」的语义，请在宿主侧用
  `tokio::time::timeout` / 单独线程 + 超时通道实现。
- ❌ **堆字节精确计量**：`max_value_bytes` 只查 list/dict 的元素
  数，不算 String 的字节数、不算闭包捕获的引用计数。如果你想要严
  格的进程内存上限，加 OS 层 `setrlimit` / cgroup。
- ❌ **跨进程隔离**：Relon 跑在你的进程里，挂了会带翻你的进程（不
  过 Rust 没有 segfault 风险——RuntimeError 是干净抛出的）。强隔离
  需求请把 Relon 跑在子进程 / wasm 沙箱里。
- ❌ **网络 / IPC 隔离**：Relon 自身没有网络原语，所以默认天然没有
  网络。但是！如果你**注册了**一个会发网络请求的原生函数，这是宿主
  层的事，Relon capability 层管不到——记得用 `register_fn_with_caps`
  标记相关 `NativeFnGate`，并把名字默认放在白名单外。

## 接下来

- 完整的能力模型和 conformant runtime 契约见 [语言规范](./spec.md)。
- 宿主集成完整流程见 [嵌入宿主](./host-integration.md)。
