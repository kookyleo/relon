# Relon 目标与实现自洽性批判记录（2026-05-10）

## 结论

Relon 的目标和实现**大体自洽**：它不是一个单纯的配置语言，也不是通用脚本语言，而是一个面向宿主嵌入的「Logic as Data」DSL 基座。业务逻辑以 JSON-like 文档存在，经 Rust runtime 沙箱求值，默认产出 JSON / `Value`，并通过 analyzer 尽量把可静态判断的问题前移到求值前。

当前实现已经支撑住这条主线：parser / analyzer / evaluator 三层分离，workspace analyzer、schema、泛型、sum type、`#main`、strict mode、sandbox、CLI、LSP、fmt、golden fixtures 都已经落地。`cargo test` 本地通过，说明工程基线是健康的。

但项目最大的短板也很明确：**嵌入与能力边界还没被产品级地收口**。文档和定位已经在讲完整 capability model，但代码里的能力位仍然偏窄，host integration 的安全语义还没有完全闭环。

## 项目目标

Relon 的真实目标可以概括为：

> Rust 原生、能力受控、可把业务逻辑当数据发布的嵌入式 DSL 基座。

它的核心使用模型是两层作者模型：

- 平台 / 框架团队：用 Rust 注册 native fn、decorator、resolver，并用 `.relon` 写共享 schema / 领域库，形成业务词表。
- 业务 / 产品团队：写较薄的 `.relon` 入口文件，像写 JSON 一样组合数据和规则，由 schema、analyzer、sandbox 兜底。
- 宿主：在求值前 push 输入，通过 `#main(...)` 声明参数契约，求值后拿 JSON / `Value` 给下游系统消费。

这个定位和 CUE、Jsonnet、Dhall、Lua、CEL 的差异是清楚的。最接近的竞品是 Pkl，但 Relon 的差异点应该落在 Rust 原生、轻量嵌入、decorator 扩展、一等沙箱能力上。

## 实现自洽的地方

### 1. 架构方向正确

当前代码组织和目标一致：

- `relon-parser`：只负责语法到 AST。
- `relon-analyzer`：承接 schema lowering、name resolution、workspace import graph、typecheck、strict mode、capability reachability。
- `relon-evaluator`：树遍历求值，处理 `Context`、`Capabilities`、stdlib、native fn、runtime `Value`。
- `relon` facade：拼装 parse -> analyze -> eval -> projector。
- `relon-cli` / `relon-lsp` / `relon-fmt`：工具链开始成型。

这说明项目不是靠 evaluator 单点硬跑，而是在向“可嵌入、可诊断、可工具化”的方向推进。

### 2. 静态分析优先原则基本落地

规范写的是：

> 凡是只依赖 source / module graph / schema / stdlib signature 的信息，错误必须在 analyzer 阶段报。

实现上已经有大量证据支撑这点：

- workspace analyzer 能先于 evaluator 捕获 import cycle / missing module / cross-module schema 问题。
- v1.4-v1.8 已经把 path-tail、closure、comprehension、where、tuple、Result / Option、cross-module schema slot 等大量问题前移到 analyzer。
- `#strict` 传播到 import 链，避免 strict entry 被非 strict lib 偷渡动态类型。
- host fn signature 也被纳入 `Any` / bare generic 审计。

这和项目“业务逻辑可审计”的目标一致。

### 3. 沙箱默认姿态已经改对

facade 和 CLI 默认 sandboxed，`--trust` / `*_trusted` 是显式 opt-in。这比早期“默认全开”的模型更符合 Logic as Data 的承诺。

当前已经具备：

- 文件 import 默认拒绝，只允许 `std/*` 虚拟模块。
- `FilesystemModuleResolver::with_root_dir` 做 canonical root 限制。
- `max_steps` 和 `max_value_elements` 控制计算爆炸和大 value。
- `register_fn_with_caps` 能让 host fn 进入 allowlist gate。

这条线是对的。

## 最大短板

最大短板不是 parser、schema 或性能，而是：

> **Capability / host integration 目前还不够完整，文档承诺领先于代码能力。**

具体表现：

1. `NativeFnGate` 目前只有 `reads_fs`。

   文档和示例已经在讲 `network`、`reads_clock` 这类能力，但 evaluator / analyzer 的 mirror type 还没有对应字段。这会造成读者以为 capability model 已经覆盖所有 ambient state，实际只覆盖了文件读这一类。

2. `register_fn` 仍然是默认绕过 gate 的信任入口。

   这是为纯函数和 stdlib 保留的务实通道，但它也意味着 host 只要误用 `register_fn`，就能把有副作用函数放进 sandbox。当前靠文档约定区分 `register_fn` / `register_fn_with_caps`，还不是强约束。

3. stdlib 的纯度靠约定维护。

   现有 stdlib 实际是纯的，但没有机器守门。以后如果有人把 `time.now()`、env、RNG、网络类功能直接放进 ungated stdlib intrinsic，沙箱模型会被悄悄破坏。

4. 静态 capability reachability 已经有了，但能力维度太窄。

   analyzer 能检查 `reads_fs` gate 是否会在 runtime 被拒绝，这是好基础；问题是 gate 维度还不足以描述真实 host 集成里的风险面。

## 最值得优先改进的一点

下一步最值得做的不是继续加语言特性，而是统一扩展 capability model：

> 扩展并统一 `NativeFnGate` / `Capabilities`，加入 `network`、`reads_clock`、`reads_env`、`uses_rng`、`writes_fs` 等明确能力位，并让文档、analyzer 静态检查、evaluator runtime 检查、示例代码完全一致。

这件事的 ROI 高，因为它直接补强 Relon 最核心的差异化：可嵌入、可审计、确定性沙箱。

## 建议拆分

### P0：能力位扩展

- evaluator 的 `NativeFnGate` 增加：
  - `reads_fs`
  - `writes_fs`
  - `network`
  - `reads_clock`
  - `reads_env`
  - `uses_rng`
- evaluator 的 `Capabilities` 增加对应 grant。
- `Capabilities::all_granted()` 同步填满这些 grant。
- analyzer 的 `cap.rs` mirror type 同步字段。
- `capability_check` 对每个 gate bit 生成对应 `CapabilityRequired` 诊断。
- runtime `check_native_fn_capability` 对每个 bit 产生一致错误。

### P1：注册 API 收口

保留向后兼容，但文档和代码导向更清晰：

- `register_fn` 明确命名 / 注释为 trusted pure fn 通道。
- 新增 `register_pure_fn` 或 `PureRelonFunction` marker，至少让 stdlib 和语言 builtins 的“纯”是显式声明。
- `register_fn_with_caps` 作为所有 ambient / side-effect host fn 的默认推荐入口。

### P1：stdlib 纯度守门

短期可用轻量 CI / 单元测试守门：

- 扫描 `stdlib.rs` 禁用 `std::fs`、`std::env`、`std::net`、`SystemTime`、`Instant::now`、RNG、process 等 ambient API。
- 如果未来需要 `std/time` 或 `std/env`，必须作为 gated host-facing module，而不是纯 std intrinsic。

### P2：文档修正

- README / host-integration / sandbox / spec 中所有 `network`、`reads_clock` 示例要么等代码落地后保留，要么暂时标明“roadmap”。
- “没有 trusted 模式”要表达为“脚本不能自提权；host 可以显式全开”，避免和 `--trust` / `*_trusted` API 产生语义冲突。
- 英文文档目前明显弱于中文文档。如果项目要面对外部开发者，至少 host integration / sandbox 应补齐英文版。

## 一句话定性

Relon 的语言设计和实现主线已经相当成熟；当前最需要补的不是表达力，而是把 **host integration 的安全契约做硬**。只有 capability model 从文档口号变成完整的 analyzer + evaluator + API 闭环，Relon 的“Logic as Data”定位才真正站稳。
