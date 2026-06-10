# 架构概览

> 本页面向**贡献者**和**深度集成 host**：解释代码组织、关键数据结构、扩展点、设计取舍。
> 仅作为使用者阅读其它章节，不需要这一页。

## 三层架构

```
relon-parser  ──→  relon-analyzer  ──┬─→  relon-evaluator (tree-walk)
   (AST)            (side-tables)    │
                          │          └─→  relon-ir ──→ relon-codegen-cranelift
                          ▼                       └──→ relon-codegen-llvm
                     relon-lsp                       (AOT 编译后端)
                  (IDE 诊断 / 跳转 / 补全)

facade crate: relon  ——  对宿主暴露 from_str / json_from_* / EvaluatorBuilder（Backend::Auto 选档）
```

每一层都是独立 crate，下游单向依赖上游。

| Crate | 职责 | 主要导出 |
| --- | --- | --- |
| `relon-parser` | 词法 + 语法 → AST。每个 `Node` 携带 process-wide `NodeId` 用于跨层 side-table | `Node`, `Expr`, `TypeNode`, `Decorator`, `NodeId`, `parse_document` |
| `relon-analyzer` | 多个 pass（schema / extend / main_sig / modules / resolve / typecheck 等）输出 `AnalyzedTree` 侧表 | `AnalyzedTree`, `SchemaDef`, `ResolvedRef`, `Diagnostic`, `analyze` |
| `relon-evaluator` | 树遍历求值，承载 `Context` / `Capabilities` / `Value` / 内建装饰器 / stdlib | `Context`, `Capabilities`, `Value`, `Evaluator`, `RuntimeError` |
| `relon-ir` + `relon-codegen-cranelift` / `relon-codegen-llvm` | AST + 侧表 lowering 为 IR，再 AOT 编译为本机机器码；与 tree-walk 按位一致 | IR module、各编译后端入口 |
| `relon` (facade) | 拼装 parse → analyze → eval 全链路；`EvaluatorBuilder` 选后端（默认 `Backend::Auto`）；`Projector` 控制 JSON 输出形态 | `from_str`, `value_from_str`, `json_from_str`, `EvaluatorBuilder`, `Error` |
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

`AnalyzedTree` 是**只读的侧表**，不修改 AST。求值器需要哪一项就查哪一项；如果某一项缺失，evaluator 走 fallback（例如 schema 没预转换时按需调用 `lower_schema_pure`）。

编译后端走平行管线：同一份 AST + `AnalyzedTree` 经 `relon-ir` lowering 为 IR，再由 cranelift / LLVM 后端编译为本机机器码——三个后端共享沙箱语义，结果按位一致。

## Analyzer 的 pass 流水线

执行顺序固定（入口 `analyze_with_options`），后面的 pass 可读前面的产物。按职责分组：

1. **宿主签名审计**：`audit_host_fn_signatures`——host 注册的原生函数签名同样要过 `Any` / 裸泛型禁令，配错的宿主集成在这里浮现。
2. **内建载体注入**：`inject_core_schemas`——安装内建 `String` / `List<T>` / `Dict<K, V>` / `Iter<T>` 方法表，让 `s.upper()` 与用户自定义方法走同一条分派路径（可经 `AnalyzeOptions::skip_core_schemas` / CLI `--lite` 跳过）。
3. **Schema 收集**：`collect_schemas` + `collect_root_schemas`——识别 `#schema Name { ... }`、`#schema Name: { ... }`、`#enum Name { ... }` 及根级 `#schema A Body` 形态，转换为 `SchemaDef`；tagged enum 的 variant 列表也在这里抽取。
4. **方法与约束**：`collect_extends`（`#extend X with { ... }`）、方法重名 / 泛型遮蔽检查、`#derive` witness 形状检查、Equatable / JsonProjectable 自动派生、方法签名表合成。
5. **入口与模块**：`collect_main`（根级 `#main(Type name, ...) [-> ReturnType]` → `MainSignature`）、`collect_imports`（收集 `#import` 边，供 workspace pass 消费）。
6. **解析与类型检查**：`resolve_references`——把 `Reference` / `Variable` 节点绑定到目标字段的 `NodeId`，保守策略：闭包参数和 dict spread 标记 frame 为 dynamic，引用不强行报错；`typecheck` + `check_main_return`——聚合诊断：`UnresolvedReference`、`StaticTypeMismatch`、`NonExhaustiveMatch`、`UnknownVariant`（带 did-you-mean）、`DuplicateMatchArm`、`SchemaBodyNotDict` 等。调用方可再追加可选的静态权限可达性检查（`capability_check`，编译后端默认打开）。

另有一条**平凡标量 `#main` 短路流水线**：当源码被判定为平凡标量 `#main` 形状时，除 `collect_main` + `check_main_return` 外其余 pass 可证为空操作，分析器整体跳过它们并产出与全流水线逐字节等价的侧表——这是 `Backend::Auto` 冷启动路径的一部分。

诊断分两级：`Severity::Error` 阻断求值，`Severity::Warning` 仅提示，evaluator 仍会执行。

## 关键不变量

- **`NodeId` 进程内唯一**：`AtomicU32::fetch_add` 分配，作为 side-table 的 key。AST clone 不重新分配 id（`Node::PartialEq` 跳过 id 比较）。
- **`Value::Dict` 携带 `brand: Option<String>`**：通过 `#schema` 类型检查后会盖上品牌。重新合并时再次校验，业务侧无法绕过 schema。
- **`Value::Dict` 携带 `variant_of: Option<String>`**：仅 sum-type variant 携带，标记父 enum 名。`Projector` 据此决定是否按 externally-tagged 形态输出。
- **JSON 输出闭环**：默认 `JsonProjector` 在 Dict 内对 closure / Schema / EnumSchema / Type / Wildcard 这类运行时-only 的 Value **静默丢弃**；`#internal` 字段更进一步——它们根本不进入 `Value::Dict::map`，所以 projector 见不到它们。但在 **List**、**Tuple** 或文档**顶层**遇到 closure 会触发 `UnsupportedClosure` 错误而非静默：list / tuple 都是「数据序列」语义，悄悄丢一个元素会让索引和长度撒谎。`#internal` / closure-filtering / `UnsupportedClosure` 是三道层叠防线：前两个把「不该出现的东西」按位置悄悄藏起来，最后一个在「悄悄藏会改变结构」时显式报错。

## 扩展点

宿主可在 `Context` 上注册六类对象，构成 relon 的 plugin surface：

| 接口 | 用途 | trait / 类型 |
| --- | --- | --- |
| **Native fn** | 让 .relon 调宿主侧函数 | `RelonFunction` + `Context::register_fn` / `register_pure_fn` |
| **Native method** | 给某个 schema 上声明为 `#native` 的方法挂宿主实现，按 brand 分派（`m.cents_value()`） | `RelonFunction` + `Context::register_method` / `register_pure_method` |
| **Host schema** | 把宿主侧构造的 schema 按名注册进上下文，供求值时引用 | `Context::register_schema` |
| **Decorator plugin** | 编写新装饰器，参与 `pre_eval` / `wrap` / `schema_field_meta` 三个钩子 | `DecoratorPlugin` + `Context::register_decorator` |
| **Module resolver** | 控制 `#import ... from "..."` 的解析（沙箱、虚拟文件系统、注册表） | `ModuleResolver` + `Context::prepend_module_resolver` |
| **Projector** | 调整 JSON 输出形态（默认 `JsonProjector`） | `Projector` trait |

详见[嵌入宿主](./host-integration.md)。

## 沙箱模型

`Capabilities` 决定 evaluator 的边界。结构是 `#[non_exhaustive]`，主要分两类：

- **能力 bit（6 个）**：`reads_fs` / `writes_fs` / `network` / `reads_clock` / `reads_env` / `uses_rng`，对应 `NativeFnGate` 上同名的 6 个 bit。一个 host fn 声明哪些 bit，host 必须在 `Capabilities` 上同步授予才能在沙箱下被调到。授权路径只有这一条——没有按名白名单、没有总开关短路。
- **预算**：`max_steps`（超阈值 → `StepLimitExceeded`）、`max_value_elements`（list/tuple/dict 构造点）。

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
- **性能层**：已落地，不再是远期目标——Cranelift AOT 与 LLVM AOT 两个编译后端与 tree-walk 并行存在，`Backend::Auto` 是 SDK 默认选档（平凡标量短路 + 惰性编译 + 不支持形状响亮回退）。仍在演进的是冷启动链路：`.o` 对象缓存已就绪（每次编译写入并回读校验），dlopen 直接执行该对象的路径推迟到后续阶段。详见 [性能与执行后端](./performance.md)。
