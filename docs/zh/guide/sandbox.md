# 沙箱与权限

Relon 的承重定位是 **Logic as Data**——业务逻辑像 JSON 一样可存可
传，由嵌入式运行时确定地求值。这意味着脚本**不可以**依赖任何宿主侧
的隐式信任：FS 访问、网络、宿主原生函数、求值预算，全部由宿主**显
式授权**。

`Capabilities` 就是这个授权通道。规范明确禁止「trusted 模式」式的
旁路构造器（详见 [语言规范](./spec.md) §4.2）——脚本必须显式声明它
需要的能力，宿主显式决定授予哪些。

## 沙箱要防什么

不可信脚本能干什么坏事？

- **读文件**：`#import x from "/etc/passwd"` 把宿主进程能读的文件吸
  出来。
- **求值爆炸**：写一个无限递归 / 指数递归的 closure 把进程吃光。
- **超大 value**：构造一个百万元素的 list / tuple / dict 把内存吃光。
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

// 1. 给一个文件读 root（如果脚本需要 #import 别的 .relon）
ctx.capabilities.reads_fs = true;
ctx.prepend_module_resolver(Arc::new(
    FilesystemModuleResolver::with_root_dir("/var/relon-userscripts"),
));

// 2. 设步数预算（防止递归 / 推导式爆炸）
ctx.capabilities.max_steps = Some(1_000_000);

// 3. 设 value 元素水位（防止巨型 list/tuple/dict）
ctx.capabilities.max_value_elements = Some(10_000);

// 4. 给确实需要的 bit 授权（按需开放，例如读时钟）
ctx.capabilities.reads_clock = true;
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
「FS 全开 + 六个能力 bit 全开（任何 gate 都过）+ 无步数预算」——哪
行不想要就删哪行。

## `Capabilities` 字段

完整结构（`crates/relon-evaluator/src/eval.rs::Capabilities`）：

```rust
#[non_exhaustive]
pub struct Capabilities {
    pub reads_fs: bool,
    pub writes_fs: bool,
    pub network: bool,
    pub reads_clock: bool,
    pub reads_env: bool,
    pub uses_rng: bool,
    pub max_steps: Option<u64>,
    pub max_value_elements: Option<usize>,
}
```

`#[non_exhaustive]` 意味着以后加新的 capability bit 不算 breaking
change——宿主侧别用穷举式 struct literal 构造，统一通过
`Capabilities::default()` / `Capabilities::all_granted()` 拿到一个
基线再赋值，或者在 struct 字面量里用 `..Capabilities::default()` 收
尾。`NativeFnGate` 同理。

逐个解释：

### 六个能力 bit：`reads_fs` / `writes_fs` / `network` / `reads_clock` / `reads_env` / `uses_rng`

每一个 bit 都是「宿主是否授予这一类副作用」的总开关。`false`（默
认）表示禁止，`true` 表示放行。它们的语义对照：

| bit | 语义 | 典型副作用源 |
| --- | --- | --- |
| `reads_fs` | 文件读 | `#import "./local.relon"`、宿主注册的 `fs.read`、`std::fs::read*` |
| `writes_fs` | 文件写 | 宿主注册的 `fs.write`、`std::fs::write*` / `OpenOptions::write` / `create_dir*` / `remove_*` |
| `network` | 网络 | 宿主注册的 `http.get`、socket、HTTP client、DNS |
| `reads_clock` | 时钟读 | `SystemTime::now`、`Instant::now` 之类的非确定时间源 |
| `reads_env` | 进程环境读 | `std::env::var`、`std::env::args` |
| `uses_rng` | 非确定随机源 | 宿主注册的 `rand.*`、任何调用 `OsRng` / `thread_rng` 的函数 |

每一个 bit 同时出现在两个地方：

- **`Capabilities`**（宿主授予）：`ctx.capabilities.network = true` 表
  示「这个 context 允许做网络」；
- **`NativeFnGate`**（函数声明）：注册原生函数时声明「我需要 network
  这一位才能跑」。

求值时每次原生函数调用都走同一条 gate 检查：函数声明的所有 bit 都
必须在 `Capabilities` 里被授予。任何一个缺失就抛
`RuntimeError::CapabilityDenied`，错误 `reason` 是
``"function declared `<bit>` but caller did not grant it"``——`<bit>`
是缺失的第一个能力名。analyzer 静态可达性检查会更激进，对每一个
缺失的 bit 各发一条 `Diagnostic::CapabilityRequired`（一个需要
`reads_fs + network` 但两个都没授的函数会产生两条诊断）。

`reads_fs` 还有一层执行机制在 `FilesystemModuleResolver`（见下
文）——bit 是策略开关，resolver 是实际执行点。其它 bit 都没有内置
的 resolver 类比物：是不是「真的」会读时钟 / 发网络包，由宿主写
的原生函数自己决定，capability 层只负责对照声明放行或拒绝。

`std/...` 虚拟模块**不**消耗 `reads_fs`——它们走 `StdModuleResolver`
而非文件系统，是规范的一部分。

### 语言没有 effectful builtin

能力位只门控**宿主注册的 `#native` fn**，绝不门控语言 builtin：Relon
**没有**任何 effectful builtin（`clock()`、`random()`、`read_file()`、
`read_dir()`、`stat()` 都不存在）。语言是一个纯函数 —— effectful 的值
由宿主取好、作为 input 喂进来。详见
[ADR](https://github.com/kookyleo/relon/blob/main/docs/internal/adr-effectful-io-builtins-2026-06-04.md) 与
[标准库 → 语言没有 effectful builtin](./stdlib.md)。

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

### `max_value_elements: Option<usize>`

字段名带 `_bytes` 是为了未来扩展，**当前的实测维度是「Value 的元素
数」**——一个 list / tuple 的元素个数，或一个 dict 的键值对数。检查点覆盖
所有「语言层面」产生 list / tuple / dict 的入口：

- 字面量构造（`[...]` / `(...)` / `{ k: v }`）；
- 字典 `+` 合并；
- 推导式 `[for x in xs: ...]`；
- 标准库内置算子（`range`、`string.split`、`list.map` / `filter` /
  `reduce`、`dict.merge` 方法形式、`dict.keys` / `values`、`iter()`
  家族等）。这些都在 `call_function` / `try_call_native_method` 的
  公共出口被检查，无论是自由函数还是 `xs.method(...)` 派发。
- `range` 还会**在分配前**预检：`range(0, 10_000_000_000)` 不会先
  分配一个 10G 的 `Vec` 再触发检查——它会先比较 `end - start` 和
  cap，立刻拒绝。

```rust
ctx.capabilities.max_value_elements = Some(3);
// `[1, 2, 3, 4, 5]` 触发 ValueTooLarge { limit: 3, actual: 5 }
// `range(0, 1_000_000)` 同样在 stdlib 入口被拦截
```

注意范围：检查只看「最外层」容器的元素数。`List<List<T>>` 外层
长度小、内层巨大的情况会绕过——递归大小检查是后续的独立决策，
现在不在 cap 的语义里。

宿主自己通过 `register_fn` 注册的原生函数返回 list/tuple/dict 时也会被
同一个 cap 拦截——之前文档说「那是宿主自己的领域」是 spec 之前的
状态，现在统一按「runtime 出 list/tuple/dict ⇒ 走 check」处理；宿主想
完全不受限请把 `max_value_elements = None`。

注：通过 `register_pure_fn` 注册的纯函数（`len`、`range`、`string.*`、
`math.*` 等 stdlib intrinsics 都走这条路）声明的是空 gate
（`NativeFnGate::default()`），任何 `Capabilities` 都能平凡满足，所
以即便所有 bit 都没授权也能跑。spec §4.3 把它们划入「规范内容」、
不属于宿主信任决策。

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
2. 每次 `#import` 进来：
   - 把目标路径 join 到当前 scope 的 `current_dir`，再
     canonicalize 一次（同样会消解 symlink）。
   - 检查 canonical target 是否以 root 为前缀；不是则返回
     `RuntimeError::CapabilityDenied { reason: "path escapes filesystem root ..." }`。

这意味着两类常见攻击都会被挡：

- **`../` 路径逃逸**：`#import x from "../../etc/passwd"` 的 canonical
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

    // 文件读关掉（用户脚本不允许 #import 文件，只能用 std/）
    ctx.module_resolvers = vec![Arc::new(StdModuleResolver)];

    // 求值预算
    ctx.capabilities.max_steps = Some(100_000);
    ctx.capabilities.max_value_elements = Some(10_000);

    // 暴露一个只读、无副作用的函数（纯函数走 register_pure_fn，
    // 声明的是空 gate，沙箱默认 Capabilities 就能调，无需授予任何
    // bit）
    ctx.register_pure_fn(
        "user.current_id",
        Arc::new(CurrentUserId(current_user.to_string())),
    );

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
- ❌ 不能 `#import` 文件
- ❌ 不能调任何声明了 capability bit（`reads_fs` / `network` / …）
  而宿主又没授予对应 bit 的原生函数 → `CapabilityDenied`
- ❌ 跑超 10 万步 → `StepLimitExceeded`
- ❌ 构造超 10000 元素的 list/tuple/dict → `ValueTooLarge`

## 错误清单

沙箱触发的运行时错误（`RuntimeError` 中的相关变体）：

| 错误 | 触发条件 |
| --- | --- |
| `CapabilityDenied { name, reason, range }` | `#import` 走到 default-reject 的 resolver；或 `#import` 路径逃出 root；或调一个声明了未授予 bit 的原生函数（`reason` 形如 ``function declared `<bit>` but caller did not grant it``） |
| `StepLimitExceeded { limit, range }` | `eval_internal` 调用次数超过 `max_steps` |
| `ValueTooLarge { limit, actual, range }` | 单个 list/tuple/dict 元素数超过 `max_value_elements` |

每一种都带 `TokenRange`——可以直接喂给 miette 拿到带源码上下文的可
读输出。

## 不在沙箱设计内的事

为了不让大家高估它，列一下 Relon 沙箱**不**承担的职责：

- ❌ **CPU 挂钟时间限制**：Relon 没有内建 wall-clock budget。如果你
  需要「这个脚本最多跑 100ms」的语义，请在宿主侧用
  `tokio::time::timeout` / 单独线程 + 超时通道实现。
- ❌ **堆字节精确计量**：`max_value_elements` 只查 list/tuple/dict 的元素
  数，不算 String 的字节数、不算闭包捕获的引用计数。如果你想要严
  格的进程内存上限，加 OS 层 `setrlimit` / cgroup。
- ❌ **跨进程隔离**：Relon 跑在你的进程里，挂了会带翻你的进程（不
  过 Rust 没有 segfault 风险——RuntimeError 是干净抛出的）。强隔离
  需求请把 Relon 跑在子进程 / wasm 沙箱里。
- ❌ **网络 / IPC 隔离**：Relon 自身没有网络原语，所以默认天然没有
  网络。但是！如果你**注册了**一个会发网络请求的原生函数，这是宿主
  层的事——记得用 `register_fn` 时把 `NativeFnGate` 的 `network` 位
  打上，并默认不授予 `Capabilities::network`，让 capability 层替你
  把这个能力门关上。

## 接下来

- 完整的能力模型和实现契约见 [语言规范](./spec.md)。
- 宿主集成完整流程见 [嵌入宿主](./host-integration.md)。
