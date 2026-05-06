# Relon 2.0 Todo List

## 项目状态 (Project Status)
Relon 2.0 核心特性已完成落地。系统已从单体评估器进化为具备静态分析能力的工业级架构。

## ✅ 已达成里程碑 (V2.0 Core Achievements)

### 1. 高级契约特性 (Advanced Contracts)
- **身份守卫 (Identity Guard)**：带有 `brand` 的字典在 `+` / `dict.merge` 修改后会自动重新校验。
- **Schema 组合 (Composition)**：完整支持 `Schema + Schema` 与 `Schema + Dict` 派生新 Schema。
- **泛型支持 (Generics)**：支持 `List<T>`、`Dict<K, V>` 以及自定义泛型 `Foo<T>` 的解析与校验。
- **和类型 (Sum Types / Enums)**：支持 `@schema Enum<A, B>` 语法，提供标签化联合体支持。

### 2. 架构与工程化 (Architecture & Engineering)
- **静态分析层 (relon-analyzer)**：新增独立分析阶段，支持错误聚合（Diagnostic Aggregation），在求值前识别结构化问题。
- **沙盒与能力管控 (Sandboxing)**：引入 `Capabilities` 模型，支持对原生函数调用、执行步数及内存占用的精细管控。
- **宿主扩展体系**：`DecoratorPlugin`、`ModuleResolver` 与 `Projector` 接口已标准化，方便宿主深度集成。
- **性能优化**：`Value` 采用 `Arc` + CoW (Copy-on-Write) 优化，大幅降低大型数据结构的克隆开销。

### 3. 工具链集成 (Tooling)
- **LSP 服务 (relon-lsp)**：支持悬停（Hover）、定义跳转（Definition）与基础补全。
- **格式化工具 (relon-fmt)**：支持基于规则的代码自动格式化。
- **CLI 诊断增强**：集成 `miette` 提供美观的结构化错误报告。

## 🚀 当前与后续任务 (Active & Next Steps)

### 阶段 F — 分析器增强 (Analyzer Refinement)
- [ ] **语义校验增强**：将更多运行时的 `TypeMismatch` 提升到分析阶段（`relon-analyzer`）。
- [ ] **文档提取 (Doc Extraction)**：从 `@schema` 和字段注释中自动提取元数据用于 LSP 悬停。
- [ ] **引用追踪 (Usage Tracking)**：实现跨文件的引用查找。

### 阶段 G — 性能与可观测性 (Performance & Observability)
- [ ] **JIT/字节码预研**：评估引入中间表示 (IR) 以加速大规模推导。
- [ ] **评估轨迹 (Trace)**：提供执行路径的可视化，方便调试复杂的 `&sibling` 引用链。

### 阶段 H — 语言特性扩展 (Language Extensions)
- [ ] **参数化 Schema (Parameterized Schemas)**：深化泛型 Schema 支持，如 `@schema Page<T>: { List<T> items: * }`。
- [ ] **品牌注册表 (BrandRegistry)**：正式化名义类型的运行时注册与查找机制。

## 测试基线 (Test Baseline)
- **当前测试总数**：227 项 (全绿)
- **覆盖范围**：parser (63+)、evaluator (51+)、analyzer (40+)、lsp/fmt/facade (70+)
- **代码规范**：`clippy` 无警告，`rustfmt` 已对齐。
