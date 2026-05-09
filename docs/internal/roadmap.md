# Relon Roadmap (internal)

> 内部路线图，不进 vitepress sidebar。维护者用。
>
> 用户可见的语言规范见 [`docs/zh/guide/spec.md`](../zh/guide/spec.md)；
> 业务定位与场景见 [`docs/zh/guide/use-cases.md`](../zh/guide/use-cases.md)。

## 项目状态

Relon 2.0 核心特性已落地。系统已从单体评估器进化为具备静态分析能力
的工业级架构。

## ✅ 已达成里程碑 (V2.0 Core Achievements)

### 1. 高级契约特性 (Advanced Contracts)

- **身份守卫 (Identity Guard)**：带 `brand` 的字典在 `+` /
  `dict.merge` 修改后会自动重新校验。
- **Schema 组合 (Composition)**：完整支持 `Schema + Schema` 与
  `Schema + Dict` 派生新 Schema。
- **泛型支持 (Generics)**：支持 `List<T>`、`Dict<K, V>` 以及自定义
  泛型 `Foo<T>` 的解析与校验。
- **和类型 (Sum Types / Enums)**：支持 `@schema Enum<A, B>` 语法，
  提供标签化联合体支持。

### 2. 架构与工程化 (Architecture & Engineering)

- **静态分析层 (relon-analyzer)**：独立分析阶段，支持错误聚合
  （Diagnostic Aggregation），在求值前识别结构化问题。
- **沙盒与能力管控 (Sandboxing)**：`Capabilities` 模型，对原生函数
  调用、执行步数及内存占用做精细管控。
- **宿主扩展体系**：`DecoratorPlugin`、`ModuleResolver` 与
  `Projector` 接口已标准化。
- **性能优化**：`Value` 采用 `Arc` + CoW (Copy-on-Write)。

### 3. 工具链集成 (Tooling)

- **LSP 服务 (relon-lsp)**：悬停（Hover）、定义跳转（Definition）
  与基础补全。
- **格式化工具 (relon-fmt)**：基于规则的代码自动格式化。
- **CLI 诊断增强**：集成 `miette` 提供美观的结构化错误报告。

## 🚀 当前与后续任务

### 阶段 F — 分析器增强 (Analyzer Refinement)

- [x] **语义校验增强**：将更多运行时的 `TypeMismatch` 提升到分析阶
      段（`relon-analyzer`）。
- [x] **文档提取 (Doc Extraction)**：从 `@schema` 和字段注释中自动
      提取元数据用于 LSP 悬停。
- [x] **引用追踪 (Usage Tracking)**：实现跨文件的引用查找。

### 阶段 G — 性能与可观测性 (Performance & Observability)

- [ ] **JIT/字节码预研**：评估引入中间表示 (IR) 以加速大规模推导。
- [ ] **评估轨迹 (Trace)**：提供执行路径的可视化，方便调试复杂的
      `&sibling` 引用链。

### 阶段 H — 语言特性扩展 (Language Extensions)

- [x] **参数化 Schema**：深化泛型 Schema 支持，如
      `@schema Page<T>: { List<T> items: * }`。
- [x] **品牌注册表 (BrandRegistry)**：正式化名义类型的运行时注册与
      查找机制。

### 阶段 I — strict 完备化 (v1.4 – v1.8)

> 主线目标：「能从 source + schemas 静态推得的，全部静态校验」。每个
> 子阶段都伴随 fixture 驱动的回归测试集。

- [x] **v1.4** 严格完备性：path-tail walking、严格 silent-fallback
      诊断、typed-spread 源扩展（51 新测试）。
- [x] **v1.5** 长尾收口：comprehension / where / spread 推断、
      closure / `#main` 严格类型、head-unresolved 升级、多段 FnCall
      路径、list / dict strict 全扫（50 新测试）。
- [x] **v1.6** 退役 `Any`：用户面禁用 `Any`，stdlib 签名改写为
      unbound 泛型（36 新测试）。
- [x] **v1.7** Tuple + ban bare 泛型：引入 `(T1, T2, ...)` 真正的
      tuple 类型；`List` / `Dict` / `Closure` / `Fn` / `Enum` 不带
      泛型参数即报错（26 新测试）。
- [x] **v1.8** Enum / Result 一等公民 + host fn audit + 跨模块 +
      tuple 位置访问：`Enum<...>` slot 改为按替代项检查；
      `Result<T, E>` / `Option<T>` 在分析阶段做泛型替换；host fn
      签名也走 ban-`Any` / ban-bare 走查；`pkg.SchemaName` 跨模块
      slot 通过 `WorkspaceImportIndex` 折叠为单段 `Schema(name)`;
      `walk_path` 新增 Index 段 + Tuple/List 位置访问（35 新测试）。

### 阶段 J — 后续候选

- [ ] **trait bound 系统**：给 stdlib `len<T>` 加 `T: Stringifiable`
      之类的约束。这是 v2.0 范畴。
- [ ] **多段 path 中段 schema 类型纯度**：目前 `pkg.X.field` 这种
      三段及以上的路径仍然是 alias-or-nothing；下一段（field 名）
      静态推断仍要落 Any 兜底。

## 测试基线

- **当前测试总数**：753 项（全绿，2026-05-09）
- **覆盖范围**：parser、evaluator、analyzer、lsp、fmt、facade、cli
- **代码规范**：`clippy -D warnings` 干净，`rustfmt` 已对齐，全工程
  零 `unsafe`
