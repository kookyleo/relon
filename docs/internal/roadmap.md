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
- [~] **引用追踪 (Usage Tracking)**：当前实现仅覆盖 in-file
      `textDocument/references`（基于 `AnalyzedTree.references`
      forward 表的反向遍历），支持 `&sibling / &root / &uncle /
      Variable` 四类静态可解析引用。跨文件引用查找延后：
      `WorkspaceTree` 现有索引为 forward-only 且 `target: NodeId` 仅
      在 module 内部唯一，需要先在 analyzer 层补一张 cross-module
      `(canonical_id, NodeId) → usages` 反向表。

### 阶段 G — 性能与可观测性（deferred，待重新立项）

**当前不在迭代窗口内**，列在此处只为占位、避免被遗忘。

原 roadmap 写的是 "JIT/字节码预研"，技术倾向过强；真实需求是
**性能强化 + 多部署目标（桌面 / wasm）覆盖**。具体路径
（IR / AOT / 解释器优化 / 编译到 wasm-bytecode / ...）目前既没有
ADR 也没有 benchmark baseline，先不预设方案。下一轮立项时应先：

1. 跑 `relon-bench` 拿到 baseline 数字；
2. 评估桌面与 wasm 两个部署 profile 的实际 hot path 与瓶颈维度；
3. 再决定是否引入 IR / 字节码 / 其它方案。

可观测性（评估轨迹 Trace）属于同一象限的独立子项目，立项时一并讨论。

- [ ] **重新立项 §G**：为 "性能强化" 建立 ADR + benchmark baseline，
      明确桌面 / wasm 两个 profile 的 KPI 与 hot path。
- [ ] **可观测性子项目**：评估轨迹 / `&sibling` 引用链可视化作为独立
      子项目立项，依赖上一条的 baseline 与 ADR。

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
- [x] **stdlib type schema 载体**：`crates/relon-analyzer/src/core/{string,list,dict,iter}.relon`
      用 `include_str!` 内嵌的 schema-rooted 载体，analyzer 启动时
      通过 `core_schemas::inject_core_schemas` 注入 `tree.schema_methods`。
      用户写 `s.upper()` / `lst.map(f)` / `d.keys()` 直接命中，无需
      `#extend String with { #native upper() ... }` boilerplate。详见
      schema-rooted-implementation-log §C.8。
- [x] **constraint lowering 全部接通**：Iterable（`iter() -> Iter<T>`
      + Comprehension 走 `materialize_iterable`）、Indexable
      （`a[i]` / `?[i]` → `a.index(i)`，Optional 解包后返回 value /
      Null / VariableNotFound 三态）、Addable / Subtractable /
      Multiplicable / Divisible / Modable（5 个 `+ - * / %` 算子
      witness dispatch）全部落地。Callable 按决策 23 不在列表。
      详见 §C.7 / C.9 / C.13。
- [x] **多段 method dispatch**：`obj.field.method()` 已经在 analyzer
      `check_method_dispatch` / `resolve_call_signature` 走
      `infer::walk_path(path[..-1])` 推 prefix 到 `Schema(name)`、
      在 evaluator `try_call_schema_method` 走
      `resolve_variable(path[..-1])` 拿 receiver value，对任意长度
      `>= 2` 的 path 都生效。详见 §C.10。
- [x] **方法级 generics**：parser 接受
      `map<U>(f: Closure<T, U>) -> List<U>` 语法，`SchemaMethod` /
      `SchemaMethodInfo` / `FnSignature` 三层都带上
      `generics: Vec<String>`；`core/list.relon` 升级回真泛型签名，
      `stdlib_signatures.rs` 的 double-source 心智模型清零。详见 §C.12。
- [x] **user-callable `Iter.next()`**：`xs.iter().next()` 返回
      Option 风格的 variant_dict；cursor 状态由 stdlib-local
      `OnceLock<Mutex<HashMap<u64, usize>>>` 承载（id stamp 在 iter dict
      的 `_id` 字段）；comprehension fast path 不读 cursor，两条路径
      独立。详见 §C.11。

剩余未决项（独立 follow-up，不阻塞 schema-rooted 核心）：

- [x] **多段 path 中段 schema 类型纯度**：~~`pkg.X.field` 这种三段及以
      上的路径中段静态推断仍落 Any 兜底（与 method dispatch 的 prefix
      推断不是同一条 path —— prefix 推断借助 schema_methods 表，中段
      字段访问仍走原 walk_path 的 PathTailOutcome）。~~ 已落地：
      `WorkspaceImportIndex` 新增 `aliased_values: alias → field →
      TypeNode`，导入 module 的 root-level 值字段（typed binding，
      非 closure）按字段名 + alias 索引。`infer::walk_path` 在 head
      命中 alias 时优先消耗 `[alias, field]` 两段，把后续 tail 接到
      字段声明的 schema 上（schema 名按 `alias.Name` 限定）；strict
      模式现在能对 `lib.alice.region` 走出 `String`，跨 module 的
      `MainReturnTypeMismatch` / `StaticTypeMismatch` 因此重新生效。
- [x] **method generics 的运行时 unification**：~~`Indexable.index(key: K)
      -> Option<V>` 的 K/V 当前在 shape-check 时 K 走 wildcard、V 用
      head-name 比对；用户传错类型的 key（`bag["abc"]` 对 `index(key:
      Int)`）只在 method body 运行时才挂。constraint-generic typecheck
      pass 是自然落点，无须 IR 改动。详见 §C.13 末尾。~~ 已落地：
      `typecheck.rs::check_index_dispatch` 扫 `Variable(path)` 中的
      `TokenKey::Dynamic` 段，prefix 走 `walk_path` 取 receiver schema，
      再去 `method_signatures[(schema, "index")]` 拿 witness 的 key
      param 类型；只要 param 类型不是当前 method/schema 名内尚未绑定
      的 generic 占位符，就把 dynamic key 的 inferred 类型与 param
      类型 subsume。错配抬升为 `MethodGenericArgMismatch` (Error)。
- [x] **method generic 与 schema generic 同名 warning**：~~`#schema
      List<T> with { foo<T>(...) }` 当前不报错但 substitution 顺序
      可能产生混乱。后续可加 `MethodGenericShadowsSchemaGeneric` warning
      diagnostic。详见 §C.12 末尾。~~ 已落地：
      `extend::check_method_generic_shadowing` 走每个带 generics 的
      `SchemaDef`，比对 method-level 与 schema-level 名字列表，命中
      同名抬升为 `MethodGenericShadowsSchemaGeneric` (Warning)。
      不同名 / 没有方法 generics（`eq(other: Self) -> Bool`）保持静默。
- [x] **Iter cursor leak**：~~stdlib-local cursor 表在进程内永不释放，
      每个 `iter()` 留 16 B。长跑 host 是已知 leak；升级到
      Context-bound cursor 表（NativeFnCaps trait 扩展）是 §C.11 注的
      follow-up。~~ 已落地：cursor 表与 id counter 挂到 `Context`，
      `eval_root` / `run_main` 入口清空 cursor，`NativeFnCaps` 暴露
      `next_iter_id` / `iter_cursor_fetch_and_inc` 给 intrinsics；
      跨 Context iter 默认按 exhausted（`None`）处理，无新错误类型。

## 测试基线

- **当前测试总数**：887 项（`cargo test`，2026-05-11，schema-rooted
  Phase A.1 / B / C / D 全部落地，所有 §J follow-up 关闭；同日完成
  第二轮自洽审视全部 P0/P1 收口——sandbox `max_value_elements` stdlib
  enforcement、`Iter` cursor per-Context 隔离、`Capabilities` 收敛到
  6 bit + 2 budget、`max_steps` 进 stdlib 内循环 tick、`core/*.relon`
  ↔ `register_pure_method` 双源对照测试；§J 三条语义补强 follow-up：
  跨 module value-path 推断 + index witness arg-type unification +
  method/schema generic shadow warning；review 后第三波：seed
  proptest harness（5 条 property test）、cross-module 泛型 schema
  中段类型推断（substitution context 跨 module 传递）、`#main`-style
  example golden snapshot、feature_flag example 转 host-integration
  demo、advisory pre-commit hook）
- **最近全量测试**：`cargo test` 全绿（2026-05-11）
- **覆盖范围**：parser、evaluator、analyzer、lsp、fmt、facade、cli
- **代码规范目标**：保持 `clippy -D warnings` 干净，`rustfmt` 对齐，
  全工程零 `unsafe`
