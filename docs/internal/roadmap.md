# Relon Roadmap (internal)

> 内部路线图，不进 vitepress sidebar。维护者用。
>
> 用户可见的语言规范见 [`docs/zh/guide/spec.md`](../zh/guide/spec.md)；
> 业务定位与场景见 [`docs/zh/guide/use-cases.md`](../zh/guide/use-cases.md)。

## 项目状态

Relon 2.0 核心特性已落地。系统已从单体评估器进化为具备静态分析能力
的工业级架构。

## 近期优先级校准（2026-05-10）

一次批判性审视见
[`relon-self-consistency-review-2026-05-10.md`](./relon-self-consistency-review-2026-05-10.md)。
结论：当前最该优先补强的不是新语言特性或性能，而是把
**host integration 的 capability 契约做硬**。

- [x] **P0：Capability model hardening**（落地于 capability hardening
      批次，2026-05-10）：`NativeFnGate` 与 `Capabilities` 从单 bit
      `reads_fs` 扩展到 6 bit（`reads_fs` / `writes_fs` / `network` /
      `reads_clock` / `reads_env` / `uses_rng`），两侧都加了
      `#[non_exhaustive]`；`Capabilities::all_granted()` 同步翻全 6 位。
      runtime `check_native_fn_capability` 与 analyzer
      `capability_check` 都改成 `NativeFnGate::missing_bits` 表驱动 ——
      runtime 报第一位失败，analyzer 每位失败报一条
      `CapabilityRequired`。
- [x] **P1：stdlib 纯度守门**（同批次落地）：`stdlib.rs` 末尾加了一条
      `#[cfg(test)]` lint 测试，扫源码禁止 `std::fs` / `std::env` /
      `std::net` / `std::process` / `SystemTime` / `Instant::now` /
      `rand::` / `chrono::` / `tokio::fs` / `tokio::net` / `reqwest`
      关键字。任何 ambient 能力（如 `std/time`）必须走 gated
      host-facing module + `register_fn(name, gate, fn)`，不进 stdlib
      intrinsic。
- [x] **P1：注册 API 语义收口**（同批次落地）：原 `register_fn(name,
      fn)` 与 `register_fn_with_caps(name, gate, fn)` 两条入口合并。
      新 API 唯一注册路径是 `register_fn(name, gate, fn)`；`register_pure_fn(name, fn)`
      作为 `gate = NativeFnGate::default()` 的语法糖。内部
      `gated: bool` 旁路彻底删除，纯 fn 通过空 gate 自动满足检查。

## ✅ 已达成里程碑 (V2.0 Core Achievements)

### 1. 高级契约特性 (Advanced Contracts)

- **身份守卫 (Identity Guard)**：带 `brand` 的字典在 `+` /
  `dict.merge` 修改后会自动重新校验。
- **Schema 组合 (Composition)**：完整支持 `Schema + Schema` 与
  `Schema + Dict` 派生新 Schema。
- **泛型支持 (Generics)**：支持 `List<T>`、`Dict<K, V>` 以及自定义
  泛型 `Foo<T>` 的解析与校验。
- **和类型 (Sum Types / Enums)**：支持 `#schema Enum<A, B>` 语法，
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
- [x] **文档提取 (Doc Extraction)**：从 `#schema` 和字段注释中自动
      提取元数据用于 LSP 悬停。
- [x] **引用追踪 (Usage Tracking)**：实现跨文件的引用查找。

### 阶段 G — 性能与可观测性 (Performance & Observability)

- [ ] **JIT/字节码预研**：评估引入中间表示 (IR) 以加速大规模推导。
- [ ] **评估轨迹 (Trace)**：提供执行路径的可视化，方便调试复杂的
      `&sibling` 引用链。

### 阶段 H — 语言特性扩展 (Language Extensions)

- [x] **参数化 Schema**：深化泛型 Schema 支持，如
      `#schema Page<T>: { List<T> items: * }`。
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

### 阶段 J — schema-rooted dispatch（trait-bound 系统的实施形态）

设计文档：[`schema-rooted-model-2026-05-11.md`](./schema-rooted-model-2026-05-11.md)（20 条决策）。
实施记录：[`schema-rooted-implementation-log.md`](./schema-rooted-implementation-log.md)。

- [x] **Phase A.1**：parser 层 body-less `#schema` + `#extend`
      directive。`with { ... }` 块支持 `#derive` / `#no_auto_derive` /
      `#native` / `#private` 等 method-level pragma；body 字段保留
      `Box<Node>`（合成空 dict 占位，不破坏 destructure 站点）。
- [x] **Phase B**：analyzer schema-rooted dispatch 与 evaluator method
      call 端到端：`SchemaDef.methods` / `tree.schema_methods`、
      `tree.method_signatures` 表，`resolve_call_signature` 扩展，
      `check_method_dispatch` 报 `UnknownMethod` /
      `PrivateMethodViolation`，evaluator `try_call_schema_method` 绑
      定 `self`、走 `invoke_method_body`。schema 校验后自动 brand 让
      `value.method(...)` 命中。workspace pass `propagate_schema_methods_across_imports`
      实现 per-import-chain visibility（决策 9）。
- [x] **Phase C**：comparison operator 下沉到 schema-rooted witness ——
      `==` / `!=` 走 `eq`，`<` / `>` 走 `lt`（`>` 等价 `rhs.lt(lhs)`）。
      `<=` / `>=` 合成为 `lt ∨ eq` / 反向。constraint registry
      `constraints.rs` 登记 Equatable / Comparable / JsonProjectable
      witness 形状（Iterable / Indexable / Callable / Number 注释占位
      待 lowering 钩子）；`check_derive_witnesses` 校验 `#derive C`
      method 的形状，不匹配报 `ConstraintWitnessShapeMismatch`。
      auto-derive Equatable / JsonProjectable（合成 `is_native`
      占位 method，evaluator 走 `Value::PartialEq` / serde 兜底），
      `#no_auto_derive` 关闭对应合成；Comparable 默认 opt-in。
- [x] **Phase D（API）**：`Context::register_method(schema, name, gate, func)`
      与 `register_pure_method` 注册 native method；evaluator 在
      `try_call_schema_method` 内先查 host-registered 表再 fallback
      到 source-side body。
- [x] **Phase D 收尾（stdlib mirror）**：`stdlib.rs::register_to` 给
      17 条 String / List / Dict 类型方法做 `register_pure_method`
      镜像注册（与 `register_pure_fn("_xxx_yyy", ...)` 并存，向后
      兼容）。`math.*` / `range` / `type` / `ensure.*` 按决策 14
      保留 free-fn 形式。
- [ ] **stdlib type schema 载体（follow-up）**：让用户写 `s.upper()`
      时不需要先 `#extend String with { #native upper() ... }` 声明
      —— 把 `crates/relon-evaluator/src/std_relon/*.relon` 改写为
      schema-rooted 载体并把内置 schema 注入 analyzer 的
      `schema_methods` 表。当前已通过 `BUILTIN_TYPE_NAMES` 让
      `#extend` 接受 String/Int 等名字，缺的是「开箱即用」的内置
      method 声明。
- [ ] **constraint lowering 扩展（钩子未挂，witness 形状已定）**：
      Iterable（`iter() -> Iter<T>`）、Indexable（`index(key) ->
      Optional<V>`）、Addable / Subtractable / Multiplicable /
      Divisible / Modable（5 个独立约束，对应 `+ - * / %`）已经写进
      `constraints.rs::CONSTRAINTS`，`#derive` 形状检查可用；evaluator
      端的 operator lowering（`for x in c` desugar、`a[i]` →
      `a.index(i)?`、`u + v` → `u.add(v)` 等）尚未挂上。Callable
      按决策 23 从 spec 删除，不在 lowering 列表里。详见
      schema-rooted-model §「4 个剩余 constraint 的 lowering（决
      策 21-24）」。
- [ ] **多段 path 中段 schema 类型纯度**：目前 `pkg.X.field` 这种
      三段及以上的路径仍然是 alias-or-nothing；下一段（field 名）
      静态推断仍要落 Any 兜底。
- [x] **多段 method dispatch**：`obj.field.method()` 已经在 analyzer
      `check_method_dispatch` / `resolve_call_signature` 走
      `infer::walk_path(path[..-1])` 推 prefix 到 `Schema(name)`、
      在 evaluator `try_call_schema_method` 走
      `resolve_variable(path[..-1])` 拿 receiver value，对任意长度
      `>= 2` 的 path 都生效。详见
      schema-rooted-implementation-log §C.10。

## 测试基线

- **当前测试总数**：820 项（`cargo test`，2026-05-11，schema-rooted
  Phase A.1 / B / C / D 全部落地后）
- **最近全量测试**：`cargo test` 全绿（2026-05-11）
- **覆盖范围**：parser、evaluator、analyzer、lsp、fmt、facade、cli
- **代码规范目标**：保持 `clippy -D warnings` 干净，`rustfmt` 对齐，
  全工程零 `unsafe`
