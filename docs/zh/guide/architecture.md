# 架构概览

> 本页面向**贡献者**和**深度集成 host**：解释代码组织、关键数据结构、扩展点、设计取舍。
> 仅作为使用者阅读其它章节，不需要这一页。

## 三层架构

```
relon-parser  ──→  relon-analyzer  ──→  relon-evaluator
   (AST)            (side-tables)         (tree-walk)
                          │
                          ▼
                     relon-lsp
                  (IDE 诊断 / 跳转 / 补全)

facade crate: relon  ——  对宿主暴露 evaluate_source / json_from_*
```

每一层都是独立 crate，下游单向依赖上游。

| Crate | 职责 | 主要导出 |
| --- | --- | --- |
| `relon-parser` | 词法 + 语法 → AST。每个 `Node` 携带 process-wide `NodeId` 用于跨层 side-table | `Node`, `Expr`, `TypeNode`, `Decorator`, `NodeId`, `parse_document` |
| `relon-analyzer` | 4 个 pass（schema / resolve / modules / typecheck）输出 `AnalyzedTree` 侧表 | `AnalyzedTree`, `SchemaDef`, `ResolvedRef`, `Diagnostic`, `analyze` |
| `relon-evaluator` | 树遍历求值，承载 `Context` / `Capabilities` / `Value` / 内建装饰器 / stdlib | `Context`, `Capabilities`, `Value`, `Evaluator`, `RuntimeError` |
| `relon` (facade) | 拼装 parse → analyze → eval 全链路；`Projector` 控制 JSON 输出形态 | `evaluate_source`, `value_from_str`, `json_from_str`, `Error` |
| `relon-lsp` | 同步 lsp-server，复用 analyzer 的 `Diagnostic` 与 side-tables | 二进制 `relon-lsp` |

## 数据流

```
source string
     │
     ▼ parse_document
AST: Node { id: NodeId, expr, decorators, type_hint, range }
     │
     ▼ analyze
AnalyzedTree {
    schemas:    HashMap<NodeId, SchemaDef>,
    references: HashMap<NodeId, ResolvedRef>,
    node_index: HashMap<NodeId, Arc<Node>>,
    imports:    Vec<ModuleImport>,
    diagnostics: Vec<Diagnostic>,
    is_library: bool,
}
     │
     ▼ Context::with_root(...).with_analyzed(...)
求值（tree-walk）
     │
     ▼ Projector
plain JSON
```

`AnalyzedTree` 是**只读的侧表**，不修改 AST。求值器需要哪一项就查哪一项；如果某一项缺失，evaluator 走 fallback（例如 schema 没预降级时按需调用 `lower_schema_pure`）。

## Analyzer 的四个 pass

执行顺序固定，下一 pass 可读上一 pass 的产物：

1. **`schema`**：识别 `#schema Name { ... }` / `#schema Name: { ... }` / `#schema Name Enum<...>`，降级为 `SchemaDef`。tagged-enum sum type 的 variant 列表也在这里抽取。
2. **`resolve`**：把 `Reference` / `Variable` 节点绑定到目标字段的 `NodeId`。保守策略：闭包参数和 dict spread 标记 frame 为 dynamic，引用不强行报错。
3. **`modules`**：扫描 `#import ... from "..."` 顶层指令，收集 import 边。
4. **`main_sig`**：识别根级 `#main(Type name, ...) [-> ReturnType]` 指令，构建 `MainSignature`。
5. **`typecheck`**：聚合诊断 —— `UnresolvedReference`、`StaticTypeMismatch`、`NonExhaustiveMatch`、`UnknownVariant`（带 did-you-mean）、`DuplicateMatchArm`、`HeterogeneousEnum`、`SchemaBodyNotDict` 等。

诊断分两级：`Severity::Error` 阻断求值，`Severity::Warning` 仅提示，evaluator 仍会执行。

## 关键不变量

- **`NodeId` 进程内唯一**：`AtomicU32::fetch_add` 分配，作为 side-table 的 key。AST clone 不重新分配 id（`Node::PartialEq` 跳过 id 比较）。
- **`Value::Dict` 携带 `brand: Option<String>`**：通过 `#schema` 类型检查后会盖上品牌。重新合并时再次校验，业务侧无法绕过 schema。
- **`Value::Dict` 携带 `variant_of: Option<String>`**：仅 sum-type variant 携带，标记父 enum 名。`Projector` 据此决定是否按 externally-tagged 形态输出。
- **JSON 输出闭环**：默认 `JsonProjector` 在 Dict 内对 closure / Schema / EnumSchema / Type / Wildcard 这类运行时-only 的 Value **静默丢弃**；`#private` 字段更进一步——它们根本不进入 `Value::Dict::map`，所以 projector 见不到它们。但在 **List** 或文档**顶层**遇到 closure 会触发 `UnsupportedClosure` 错误而非静默：列表是「数据序列」语义，悄悄丢一个元素会让索引和长度撒谎。`#private` / closure-filtering / `UnsupportedClosure` 是三道层叠防线：前两个把「不该出现的东西」按位置悄悄藏起来，最后一个在「悄悄藏会改变结构」时显式报错。

## 扩展点

宿主可在 `Context` 上注册四类对象，构成 relon 的 plugin surface：

| 接口 | 用途 | trait / 类型 |
| --- | --- | --- |
| **Native fn** | 让 .relon 调宿主侧函数 | `RelonFunction` + `Context::register_fn` / `register_pure_fn` |
| **Decorator plugin** | 编写新装饰器，参与 `pre_eval` / `wrap` / `schema_field_meta` 三个钩子 | `DecoratorPlugin` + `Context::register_decorator` |
| **Module resolver** | 控制 `#import ... from "..."` 的解析（沙箱、虚拟文件系统、注册表） | `ModuleResolver` + `Context::prepend_module_resolver` |
| **Projector** | 调整 JSON 输出形态（默认 `JsonProjector`） | `Projector` trait |

详见[嵌入宿主](./host-integration.md)。

## 沙箱模型

`Capabilities` 决定 evaluator 的边界。结构是 `#[non_exhaustive]`，主要分两类：

- **能力 bit（6 个）**：`reads_fs` / `writes_fs` / `network` / `reads_clock` / `reads_env` / `uses_rng`，对应 `NativeFnGate` 上同名的 6 个 bit。一个 host fn 声明哪些 bit，host 必须在 `Capabilities` 上同步授予才能在沙箱下被调到。授权路径只有这一条——没有按名白名单、没有总开关短路。
- **预算**：`max_steps`（超阈值 → `StepLimitExceeded`）、`max_value_elements`（list/dict 构造点）。

文件系统的真正执行点在 resolver：`FilesystemModuleResolver` 默认拒绝；`with_root_dir` 限定根目录；`trusted()` 全开。`reads_fs` / `writes_fs` 只是 capability 层的 bit。

`Context::sandboxed()` 默认拒绝文件系统与有 bit 声明的原生函数；需要全开时显式设置 `Capabilities::all_granted()`（一次翻全 6 bit），并安装 `FilesystemModuleResolver::trusted()`。`Context::new()` 是轻量基础构造器：只挂载虚拟 std 模块与内置纯函数，不代表全开信任模式。

详见[沙箱与权限](./sandbox.md)。

## 设计取舍记录

- **三层而非单 crate**：拆分代价是若干跨 crate 引用，回报是 LSP / 求值器可独立消费 analyzer 侧表，不必拖整个 evaluator。
- **保守 reference 解析**：closure 参数 / dict spread 出现时不强行报错，避免对动态特性误报。代价是部分错误推迟到 runtime。
- **Match 穷尽性是错误而非警告**：sum type 上漏 variant 直接 `NonExhaustiveMatch` Error，错误尽早抛。
- **JSON externally tagged 而非 internally tagged**：内存中 dict 仍是扁平 + brand，序列化时由 projector 包外层。这样业务作者写 `notification.address` 直接能用，不需要 `notification.Email.address`。
- **入口 / 库的二分以 `#main(...)` 标记**：带 `#main` 的文件是入口程序，必须由宿主 `run_main(args)` 推参数；不带 `#main` 的文件可以被宿主直接 `eval_root` 求值，也可以被其它文件 `#import`。两种用法不会互相串扰——把库文件当 entry 跑会立即报 `NoMainSignature`。

## 当前还在演进的部分

- **跨语言 host**：v1 路线图是 C ABI cdylib（JSON 进 JSON 出），v2 加 native-fn callback；v3 看需求做 PyO3 / napi-rs 封装。**不**做跨语言类型/装饰器注册。
- **stdlib 厚度**：当前是 6 类约 30 个函数；`time` / `regex` / `path` / `base64` 在 roadmap 上。
- **Analyzer 类型推导深度**：当前 typecheck 的「matched expr 类型推导」只覆盖 Reference 链，更复杂的表达式跳过。
- **性能层**：bytecode IR + cranelift JIT 是「等正确性和生态稳定后才碰」的远期目标。
