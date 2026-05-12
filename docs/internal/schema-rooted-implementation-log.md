# Schema-Rooted Implementation Log

> 实施过程的决策日志。Phase A.1 / B / C / D / E 实施时遇到的、未在
> [`schema-rooted-model-2026-05-11.md`](./schema-rooted-model-2026-05-11.md)
> 决策清单里覆盖的细节，在这里登记。每条记录形态：上下文 + 选择 + 理由。

## Reading guide / Status snapshot

- 本 log 是 **append-only** 时间序日志，记录每一节实现细节；**不是
  现状文档**。要看当前实施进度请去 [`roadmap.md §J`](./roadmap.md)；
  要看设计基线请去 [`schema-rooted-model-2026-05-11.md`](./schema-rooted-model-2026-05-11.md)。
- 章节走向：
  - **A.1**：parser 层 body-less `#schema` + `#extend` directive；
  - **B**：analyzer schema-rooted dispatch + evaluator method call 端到端；
  - **C.1 .. C.13**：constraint lowering（Equatable / Comparable /
    Iterable / Indexable / Addable 等 witness）、cross-module method
    propagation、generic K/V unification、Iter cursor 等子项；
  - **D**：host-side API（`register_method` / `register_pure_method`）+
    stdlib mirror；
  - **Phase D 收尾后**：review-driven follow-up（sandbox 实测语义、
    proptest harness、cross-module value-path 推断等）也按时间序追加
    到对应字母后面。
- 找具体决策回流（哪条折叠回设计文档）时，按 "决策 X" 字样 grep。

## 体例

每个条目：

```text
### N. 主题
日期 / 阶段 / 涉及文件 / 备注

**问题**：实施中冒出来的具体细节
**选择**：定下来的方案
**理由**：基于过往 Q&A / 倾向的推导
**回流**：是否要把它折叠回主设计文档（默认是；记录原因）
```

---

## A.1 阶段（parser body-less + #extend）

### A.1.1：body-less 时 body 字段如何表示
2026-05-11 / Phase A.1 / `crates/relon-parser/src/directive.rs`

**问题**：`#schema String with { ... }` 没有 body 表达式。
`DirectiveBody::NameBody.body` 类型是 `Box<Node>`（必须存在）。要么
改成 `Option<Box<Node>>`（破坏性，几个 destructure 站点要更新），
要么让 parser 在 body-less 场景合成一个空 Node。

**选择**：parser 合成一个 `Expr::Dict(vec![])` 节点占位；body 字段类
型保持 `Box<Node>` 不变。

**理由**：
- 决策 10 表面语法是「省 body」，但 AST 是 parser 自己的形状，让
  AST 层面始终有 body 不影响表层语义
- destructure 站点（lsp/cursor.rs / eval.rs / 其它）继续按
  `body, ..` 形态读，不需要做 `Option` 解包
- 「empty dict body = no fields」与「body absent」在分析阶段语义一
  致（schema 没有字段集），合成不引入歧义
- 反之改 `Option` 是 N+1 文件的破坏性改动，杠杆低

**回流**：是。已记录到 schema-rooted 主文档「实施细节」附录（待添加）。

### A.1.2：`#extend` 的 directive shape
2026-05-11 / Phase A.1 / `crates/relon-parser/src/directive.rs`

**问题**：`#extend X with { ... }` 表面看是「name + with-block」，
跟 `#schema X with { ... }` 形状完全一致 —— 是注册成 `NameBody` 复
用 `parse_name_body`，还是单独搞个 `DirectiveShape::Extend`？

**选择**：复用 `DirectiveShape::NameBody`。`#extend` 与 `#schema` 在
parser 层走同一条路径，AST 都是 `DirectiveBody::NameBody`，analyzer
通过 `dir.name == "schema" | "extend"` 区分语义。

**理由**：
- A.1.1 的合成空 body 让两者语法形态完全统一
- 无新 `DirectiveShape` 变体 = 无 N+1 处 match arm 调整
- analyzer 端按名字 dispatch 是已有惯用法（`SCHEMA` / `IMPORT` / `MAIN`
  常量已经作为名字字符串）

**回流**：否，是 parser 内部实现选择。

---

## B 阶段（analyzer schema-rooted dispatch + evaluator method calls）

### B.1：method 表的 key 选择
2026-05-11 / Phase B / `crates/relon-analyzer/src/tree.rs`

**问题**：`schema_methods` 应该按什么 key 索引？候选：
1. `(SchemaName, MethodName)` 扁平 map
2. `SchemaName -> Vec<SchemaMethodInfo>` 嵌套
3. `SchemaName -> HashMap<MethodName, SchemaMethodInfo>` 嵌套

**选择**：方案 2（外层 `HashMap<String, Vec<...>>`），同时另起
`method_signatures: HashMap<(String, String), FnSignature>` 给签名查找用。

**理由**：
- 决策 8 要求方法在同一 schema 上不重名，但方法的源序很重要
  （`#schema` 自带的方法 + 多个 `#extend` 块需要按出现顺序累积）。
  `Vec` 保留这个顺序；`HashMap<MethodName, _>` 会丢失。
- 跨模块合并时，conflict diagnostic 需要 first / second 范围 ——
  线性扫描 `Vec` 已经够用，用嵌套 map 就要专门查 `entry.or_insert`
  来跟踪 first range，反而绕。
- 签名查找走 `(schema, method)` 扁平 key 匹配 `lookup_signature`
  的现有 cross-module aliased_closures 形态，避免再多一层间接。

**回流**：否，纯实施层数据结构选型。

### B.2：method 调用 dispatch 入口在 typecheck.rs 的位置
2026-05-11 / Phase B / `crates/relon-analyzer/src/typecheck.rs`

**问题**：`Expr::FnCall` 已经走 `check_unresolved_fn_call` /
`check_fn_call` / `check_strict_fn_call` 三步。schema-rooted dispatch
在哪一步插入？

**选择**：扩展 `resolve_call_signature`（让 `check_fn_call` 走签名校验
路径直接命中）+ 新增 `check_method_dispatch` 专门处理 UnknownMethod /
PrivateMethodViolation。

**理由**：
- 决策 12 是统一 dispatch 模型；签名校验、参数校验、返回类型推断都
  走 FnSignature 现有机器是最经济的——不需要给方法搞独立校验路径。
- UnknownMethod / private 是新诊断类型，混进现有 check_fn_call 的
  arity / type 校验会把这块代码搞乱，所以拆成 check_method_dispatch。
- check_method_dispatch 跑在 unresolved / fn_call / strict 之后是
  因为前三步已经过滤了同名 sibling closure / aliased import 等
  非方法形态，方法 dispatch 拿到的 path 是「真的指向 schema 方法」
  的 path。

**回流**：否，是 typecheck 内部 walker 顺序选择。

### B.3：runtime 自动 brand
2026-05-11 / Phase B / `crates/relon-evaluator/src/schema.rs`

**问题**：`#main(Money m)` 把外部传入的 `Value::dict(...)` 当 Money
校验后，dict 自身没有 `brand`，导致 `m.cents_value()` 找不到 receiver
schema。原本 brand 只有显式 `#brand X` 或内置 type_hint 才会写入。

**选择**：schema 校验成功后，如果 dict 没有 brand，自动 brand 为该
schema 名。`#brand X` 显式标注的不动。

**理由**：
- decision 1 的 schema-rooted dispatch 假设 value 自带 schema 标识。
  没有 brand 就退化到只能用静态 `Schema.method(...)` 形式，丢失了
  receiver dispatch。
- 自动 brand 与「`{X x: ...}` 之后 x 是 X 的实例」的直觉一致。
- 选择「保留显式 #brand」是因为决策 13 强调用户对 brand 的主动控制
  权——如果用户主动写 `#brand Y` 把 Money 标成别的 brand，schema 校
  验不该回退它。

**回流**：是。是语言级语义变更，应折叠回 schema-rooted 主文档「runtime
brand 行为」一节。

### B.4：`#extend` 跨模块时机 vs schema_known
2026-05-11 / Phase B / `crates/relon-analyzer/src/extend.rs`

**问题**：单文件 analyze 时，lib_extend.relon 里 `#extend User with`
找不到 `User`（User 来自 `#import`），会触发 `ExtendUnknownSchema`。
但 workspace pass 才能拿到全图。两阶段 validate 怎么协调？

**选择**：单文件 collect_extends 时若 root 有任何 `#import`，松开
`schema_known` 检查（始终记录方法）。无 import 的纯单文件依然走严
格检查。workspace post-pass 持有更完整的视图后做最终 conflict
detection。

**理由**：
- 决策 9 per-import-chain 视角下，单模块视图本来就不完整，把诊断
  放在「不知道全图就不要乱报」更稳妥。
- 用 `has_imports` 作开关是因为：根本没 import 的模块，不可能通过
  cross-module 拿到 User —— 这种场景下严格报 typo 是正确的。
- 替代方案「单文件总是松开」会让 typo（`#extend Usre with`）只在
  workspace 模式才报，单文件模式静默 —— 退化太严重。

**回流**：否，分层 validate 是实施细节。

---

## C 阶段（约束模型 + auto-derive + operator lowering）

### C.1：operator lowering 与原 built-in 共存
2026-05-11 / Phase C / `crates/relon-evaluator/src/arithmetic.rs`

**问题**：`==` / `<` 决策是「下沉到 schema-rooted witness」。但
现存代码全靠 `Value PartialEq` 与 `eval_numeric_comparison` 跑——
要把所有 primitives 也强制走 method dispatch 吗？

**选择**：分层。`try_compare_op_method` 作为 fast-path：仅当
receiver 是 *branded* 的 dict 且 schema 上确实声明了 `eq`/`lt`，
才走 method 路径；否则 fallback 到原 built-in。

**理由**：
- 决策 18 主要解决「用户类型上的语义」——i32 / String 这些
  primitives 的相等/比较语义是固定的，没必要绕 method dispatch
  增加调用成本。
- 不强制 primitives 走 method 也避免了「需要给所有内置类型
  hardcode `eq`/`lt` 注册」的工程债，这本属于 Phase D stdlib 迁移。
- `>` 拆成 `rhs.lt(lhs)` 而非「同时支持 lt 与 gt witness」是
  决策 18 末尾「单 lt 覆盖两向」的直接落实，少一个 witness slot。

**回流**：是。属于语言级 evaluation 语义，应折叠回主文档「operator
lowering」一节。

### C.2：scope 收口 —— Phase C 仅做 lowering
2026-05-11 / Phase C / N/A（scope 决策）

**问题**：Phase C 原计划包含 (a) operator lowering、(b) constraint
模型登记、(c) auto-derive、(d) `#derive` witness 检查。彻底实施需要
新建 constraint registry + 在 analyzer 验证 method 满足 witness
形状（`eq(other: Self) -> Bool` 等）。是一气呵成还是分次落地？

**选择**：本批仅落地 (a) operator lowering。constraint 模型
登记、auto-derive、`#derive` witness 形状校验留给后续 PR。

**理由**：
- 整个 Phase C 工作量超过单批可控范围；先把 (a) 做齐让用户层
  立刻能用 schema-rooted `==` / `<`，降低尾部风险。
- (b)~(d) 之间有相互依赖：constraint 注册表是 (c)/(d) 的前提，
  但 (a) 不需要它——operator lowering 通过 method 名（`eq`/`lt`）
  驱动而非通过 trait bound，正是决策 17 选「nominal trait」时
  避开的复杂度。
- 无 (b)~(d) 时用户依然要手写 `eq` / `lt`；折损是没自动 derive
  能力，而非缺失语义。这与「先稳，再扩」节奏一致。

**回流**：否，路线图层面调度选择。已记录到 roadmap.md §J。

### C.3：constraint registry 与 witness shape 校验位置
2026-05-11 / Phase C / `crates/relon-analyzer/src/constraints.rs`

**问题**：`#derive Constraint` 标记的 witness method 形状必须与 constraint
定义一致（`Equatable` 需要 `eq(other: Self) -> Bool` 等）。注册表
和形状校验放哪？候选：
1. 直接挂在 `extend.rs` 末尾，跟 method table 走同一个文件。
2. 拆独立模块 `constraints.rs`，把「constraint 元数据 + auto-derive
   + witness 校验」三块耦合内容放一起。

**选择**：方案 2（独立模块）。

**理由**：
- `extend.rs` 已经承担 method 收集、conflict 检测、signature
  table 构建三件事；再加 constraint 元数据 + auto-derive 注入 +
  shape 校验，文件会膨胀到难审计。
- 决策 17 的「nominal trait」语义把 constraint 模型本质上变成「名字
  + 期望签名形状」的查表，是一个独立闭包，不依赖 `#extend` /
  `with` 的源代码采集逻辑——分模块更对位。
- 未来添加新 constraint（Iterable / Indexable / Callable / Number）
  时只需扩 `CONSTRAINTS` 数组 + 注入对应 lowering hooks；现在的
  布局让这件事不会污染 extend pass。

**回流**：否，纯实现层模块拆分。

### C.4：auto-derive 通过合成 `is_native = true` 占位 method 实现
2026-05-11 / Phase C / `crates/relon-analyzer/src/constraints.rs`、
`crates/relon-evaluator/src/arithmetic.rs`

**问题**：决策 15 / 19 要求 `Equatable` 和 `JsonProjectable` 默认
ON，但 evaluator 已经把 `==` 的 fallback 路径定为
`Value::PartialEq`，把 JSON 序列化交给 serde_json。怎么让 analyzer
端的 auto-derive 与 evaluator 端的 fallback 无缝衔接，又不引入新的
能力位 / 错误类型？

**选择**：analyzer 合成一条 `SchemaMethodInfo { name: "eq", is_native:
true, body_node: None, derives: ["Equatable"], ... }`（`to_json`
同理）追加到 `schema_methods`。evaluator 在 dispatch 时检测到「有
method entry 但既没 body 也没在 native_methods 表里注册」就走兜底
路径：`eq` 用 `Value::PartialEq`、`to_json` 用 `serde_json::to_string`。

**理由**：
- 用同一个数据结构（`schema_methods`）表达「用户写的」「`#native`
  host 注册的」「auto-derive 合成的」三类 method，让 dispatch 路径
  保持单一查表，不需要在 evaluator 里加 capability bit 或专门标记。
- 决策 17 的 nominal trait 语义已经允许 method 通过名字命中，合成
  路径正好沿用这套机制——不需要另写一层 trait resolution。
- evaluator 端的 fallback 是「兜底」而非「错误恢复」：auto-derived
  `eq` 永远命中 PartialEq、`to_json` 永远命中 serde；不需要新增
  `RuntimeError` variant，符合任务约束。
- `#no_auto_derive Constraint` 通过 schema 级 `schema_no_auto_derives`
  阻断 analyzer 的合成；阻断后 evaluator 的 `try_compare_op_method`
  根本拿不到 method entry，照样落回顶层的 `Value::PartialEq`，与
  「没合成 = 没影响」的直觉一致。

**回流**：是。属于跨 analyzer / evaluator 的语言级语义——「内置
constraint 的 evaluator 兜底」应折叠回主文档「auto-derive」一节。

### C.5：`<=` / `>=` 全 lowering vs 全 fallback 的二选一
2026-05-11 / Phase C / `crates/relon-evaluator/src/arithmetic.rs`

**问题**：`<=` 设计为 `a.lt(b) || a.eq(b)`、`>=` 为 `b.lt(a) ||
a.eq(b)`。当 `lt` 命中但 `eq` 没命中（或反之），怎么处理？候选：
1. 一边命中就走 method 路径，缺失的一半用结构等值 / 数值默认补齐。
2. 全有才用 method 路径，缺一个就整体 fallback 到 `eval_numeric_comparison`。
3. 让 `eq` 缺失时落 `Value::PartialEq`，`lt` 缺失时整体 fallback。

**选择**：方案 3（不对称兜底）。`lt` 是 strict-order 判别器，没有
合理 fallback——一旦缺失就放弃 method 路径整体落数值默认；`eq`
缺失时（即 `#no_auto_derive Equatable` 阻断了 auto-derive 合成）
走结构 `Value::PartialEq`，因为决策 15 把结构等值定为 fallback
合同。

**理由**：
- 方案 1 会让「同一个 `<=` 表达式在不同 schema 上有截然不同
  语义」（一半 method、一半数值），违反 Logic-as-Data 的「可
  审计」原则。
- 方案 2 太严，把 `#no_auto_derive Equatable` 当成「禁止 `<=`」
  的开关——但 `#no_auto_derive` 的本意只是「不合成 method
  entry」，没要求 evaluator 也丢掉 fallback。
- 方案 3 与决策 15 + C.4 一致：`eq` 永远有 fallback，`lt` 没有；
  非对称恰好对应「Equatable 默认 ON、Comparable 默认 OFF」的非
  对称设计。

**回流**：是。属于语言级 evaluation 语义，应折叠回主文档
「operator lowering」一节（紧跟 C.1 后面）。

---

## D 阶段（register_method + stdlib 迁移）

### D.1：register_method key 形状
2026-05-11 / Phase D / `crates/relon-evaluator/src/eval.rs`

**问题**：host 注册 `Money::formatted` 时，存哪？候选：
1. 复用 `functions: HashMap<String, GatedNativeFn>`，key 为
   `"Money.formatted"` 字符串。
2. 新建 `native_methods: HashMap<(String, String), GatedNativeFn>`。

**选择**：方案 2（新建表）。

**理由**：
- 决策 12 把 method 与 free fn 视为正交两套调度。复用同一表
  会让 free fn `Money` 与方法 `Money.formatted` 撞一个 key
  分隔符（'.' 还是没分隔符？），增加误解风险。
- `(String, String)` key 直接对应 `tree.method_signatures` 的
  分析层 key，命中查找逻辑形态一致，方便后续添加 method-only
  能力（per-schema 计数、per-schema 反射）。
- 性能：双 HashMap 查找比单 HashMap + 字符串拼接还快（无堆分配）。

**回流**：否，runtime 内部表结构选型。

### D.2：stdlib 迁移留作后续
2026-05-11 / Phase D / N/A（scope 决策）

**问题**：原计划把 36 条 stdlib intrinsic（`len`、`string.*`、
`math.*`、`list.*` 等）从 `register_pure_fn` 全部迁到
`register_pure_method` + 新 `std_relon/<type>.relon` schema 声明
载体。是本批次做完吗？

**选择**：仅落地 `register_method` API；stdlib 迁移留 follow-up PR。

**理由**：
- 36 条迁移本质机械重复，但每条都涉及 stdlib_signatures.rs
  里的签名条目、analyzer-side built-in 名集、用户文档示例。
  风险来自分布式细节（哪些当 method 哪些保留 free-fn），不
  适合塞进同一批改动。
- `register_fn` / `register_pure_fn` 保持兼容，现存 host 与
  全部 test corpus 不受影响——可以平滑滚动迁移。
- 决策 14 强调「method 是模型的中心」，但没要求 stdlib 必须
  立刻全转过去；现有 `len(x)` 风格仍合法的「polymorphic free
  fn」语义（决策 14 只反对 *用户定义* 的 free fn）。

**回流**：是。属于 Phase 范围调整，已在 roadmap.md §J 标记
「Phase D 收尾」未完成。

### C.6：补 4 个 constraint 的 witness 形状（lowering 未挂）
2026-05-11 / Phase C / `crates/relon-analyzer/src/constraints.rs`

**问题**：之前 constraint 注册表只登 Equatable / Comparable /
JsonProjectable 三项，Iterable / Indexable / Callable / Number
留注释占位。后续问答中决定逐条 design out（结果：选 iter() 真
迭代器、Optional 索引、删 Callable、拆细 Number 为 5 个独立
constraint）。现在补哪些到注册表？

**选择**：把 Iterable、Indexable、Addable、Subtractable、
Multiplicable、Divisible、Modable 加入 `CONSTRAINTS` 数组（共 7
条新条目），witness 形状定义齐全；**但 evaluator 端的 operator
lowering 钩子（for / `a[i]` / arithmetic）暂不实施**，留待单独
follow-up。Callable 按决策 23 从 spec 移除，不进注册表。

**理由**：
- 注册表条目让 `#derive Addable add(other: Self) -> Self`
  立刻享受 `ConstraintWitnessShapeMismatch` 静态检查；用户哪怕
  现在不能 `u + v` 命中 method，至少 `#derive` 的形状错能被
  analyzer 立刻指出来。
- Iterable / Indexable / Number 的 witness 涉及泛型
  (`Iter<T>` / `Optional<V>` / `Self`)，但当前 `ExpectedParam`
  只编码单段类型名 —— Iterable 的 return type 用 `"Iter"`
  head-name match，Indexable 的 param type 暂用 `"Any"` 占位
  注释中标记。完整泛型 unification 等 lowering 时再补；现在
  存的是「shape 检查能用 + 文档上说得清」的最小集合。
- 拆细 5 个 Number constraint 而非合并：决策 24 的「按需 derive」
  落到注册表是 5 条独立条目，与「一个 constraint = 一个 method」
  形状对齐，方便诊断信息精准定位（错 sub 不会说成「Number 的第
  2 项」）。
- Callable 不登（决策 23 删除）：避免用户写 `#derive Callable
  call(...)` 期待 `f(args)` 直接命中，事后失望。注册表是「能 derive
  的 constraint 集合」语义，不该列出无 lowering 的项。

**回流**：是。schema-rooted 主文档新增「4 个剩余 constraint 的
lowering（决策 21-24）」章节，注册表 + 主文档同步。lowering
钩子留 roadmap §J 单独追踪。

### C.8：core.relon 载体落在 analyzer crate 而非 evaluator crate
2026-05-11 / Phase C-D / `crates/relon-analyzer/src/core/*.relon`、
`crates/relon-analyzer/src/core_schemas.rs`

**问题**：决策 21' 要求把 `#schema String with { #native upper() -> String,
... }` 等内置 method 声明做成 always-on 编译期内嵌的 .relon 载体，
让用户写 `s.upper()` 无需 `#extend` boilerplate。.relon 文件物理上
该放在 evaluator 还是 analyzer crate？

**选择**：放在 `crates/relon-analyzer/src/core/*.relon`，新增
`crates/relon-analyzer/src/core_schemas.rs` 模块用 `include_str!` 嵌入
+ `inject_core_schemas(&mut tree)` 入口，从 `analyze_with_options`
最前面调用（位于 `collect_schemas` 之前）。host 实现仍在
evaluator 的 `stdlib::register_to`，跟 analyzer 隔开。

**理由**：
- 「这些 schema 存在 + 它们的 method 表是什么形状」是 analyzer 的
  权威知识；evaluator 只是消费方（依靠 `register_pure_method` 提供
  native 实现）。把声明放在 analyzer crate 就避免了 analyzer 反向
  依赖 evaluator —— 这与现有 crate 依赖方向（evaluator -> analyzer
  -> parser）一致。
- `include_str!` 与 `parse_document` + `collect_root_schemas` 复用
  现有 lowering 路径，跟用户写 `#schema Foo with { ... }` 走完全
  同一条 pipeline。没有「内置 schema 专用」的代码分支，意味着 schema-
  rooted dispatch 的 method 表对内置与用户类型保持同一形态。
- 把 .relon 文件「内嵌进 analyzer」而非「内嵌进 evaluator」还
  让 LSP / cli / fmt 这些只引 analyzer 的 crate 自动获得内置
  schema 视图 —— 不需要 host 提前 wire evaluator 才能 hover 出
  `upper` 的签名。

**实施细节注脚**：
- 把 `inject_core_schemas` 放在 `analyze_with_options` 的最前面
  （早于 `collect_schemas`）。这样后续所有 collector / checker
  看到的 `tree.schema_methods` 已经包含内置项，
  `check_method_uniqueness` / `check_derive_witnesses` / `auto_derive_schemas`
  全部对内置与用户 method 一视同仁。
- `merge_core_into` 不复制 `schemas` / `root_schemas` 条目，只
  搬 `schema_methods`。原因：把内置 schema 也算入用户视野的
  declared-schemas 集合会触发奇怪的 cross-module collision 检测
  （比如「String 已在内置中声明，用户 import 后又看见」）。
- carrier .relon 文件以 `{}` 结尾：parser 要求 root 能解出一个
  `Expr`，body-less `#schema X with { ... }` 后必须接表达式才能
  parse 通过。用空 dict 占位最经济。
- `core/list.relon` 写 `map(f: Closure)` 而非
  `map<U>(f: Closure<(T) -> U>)`：parser 不支持 method-level
  generics 与 tuple-arrow 类型；真正的多态签名仍由
  `relon-analyzer/src/stdlib_signatures.rs::_list_map` 承载。
  carrier 只是 method-name 注册 shell，与 `register_pure_method`
  的角色对称。

**回流**：是。schema-rooted 主文档「21' core.relon 载体」章节加
最后一段说明 carrier 的落点 + 注脚原因。

### C.9：`Iter<T>` 形态 + Iterable lowering 的 Comprehension 路径
2026-05-11 / Phase C-D / `crates/relon-analyzer/src/core/iter.relon`、
`crates/relon-evaluator/src/stdlib.rs`、`crates/relon-evaluator/src/eval.rs`

**问题**：决策 21 要求把 `for x in c` / `[x for x in c]` Comprehension
lowering 到 `c.iter()` + 反复 `next()`。但 Relon Value 是不可变的
（`Arc` 共享，无 interior mutability），用户写的 `next() -> Optional<T>`
witness 没法在原地推进 cursor —— 这与「不可变值」的语言约束冲突。
怎么定 Iter 的运行时形态？

**选择**：
1. `Iter<T>` 的 analyzer-side 声明保留主文档原样：
   `#schema Iter<T> with { #native next() -> Option<T> }`。
   `Option` 对齐现有 prelude 里 `Option<T>` 的命名（主文档写
   `Optional<T>` 是 typo —— prelude 实装是 `Option`）。
2. 运行时 `Iter<T>` 实装为 brand `"Iter"` 的 Dict，字段
   `_kind: String` (one of `"list"` / `"string"` / `"dict_entries"`)、
   `_source: Value`。`List.iter()` / `String.iter()` / `Dict.iter()`
   就是构造这种 Dict 的 thin wrapper。
3. Comprehension evaluator (`Expr::Comprehension` arm) 不调
   `next()`，而是直接 dispatch on `_kind` 走 list / string / dict
   迭代驱动。即「内置 Iter 是 inert 容器，迭代逻辑由 evaluator
   loop 持有」。
4. `Iter.next()` host 实现是 stub（返回
   `RuntimeError::UnsupportedOperator`），保留 witness slot 给
   未来真正的 user-callable next 协议；用户当前调 `it.next()`
   得到明确错误信息。

**理由**：
- 拒绝 mutable cursor on Value：会破坏 `Arc::make_mut` 的 lazy-clone
  契约（next() 推进会让所有共享别名跟着动），与 Logic-as-Data 的
  「值即快照」直觉冲突。
- 拒绝 `next() -> Optional<Tuple<T, Self>>` witness 形状：与主文档
  公开签名 `next() -> Optional<T>` 不一致；改 witness 形状要求所有
  user `#derive Iterable` 同步改 —— 影响面大，PR 内不消化。
- 选 「evaluator 持有迭代状态 + Iter 是 inert 容器」：
  - 与现有 List comprehension 已落 `for item in items.iter()` 的
    Rust-侧 driver 形态对齐（这条 PR 之前的代码本就是 host-loop 模型）。
  - 用户 `iter()` 现在只能返回内置 Iter 形状（一般做法是
    `self.items.iter()` 委托），无法真正自定义 lazy 迭代逻辑 —— 这
    与决策 21 「lazy 表达力」的 spirit 部分让步，但保留了 witness
    形状的稳定性，留出 next-PR 升级路径。
- shape 校验放松：constraints.rs 的 `return_type_matches` 对 `Iter`/
  `Option`/`Optional` 三个 generic head 改成「head 名相等即认」。
  否则 `iter() -> Iter<Int>` 被注册表的 `return_type: "Iter"` 拒掉。
  这是「witness 形状 = head 名」的最小放松，对其他 primitive
  return type 仍是严格匹配。
- multi-hop receiver 限制：`self.items.iter()` 调用形式（path 3 段）
  当前 `try_call_schema_method` 不命中（它只看 path.len() == 2）。
  user-schema Iterable 因此 short-term 只能用 sibling-binding /
  let-style workaround 才能 end-to-end 跑通；本批次的
  `user_schema_iterable_shape_accepted_by_analyzer` 测试退化为
  「analyzer 接受声明」的契约校验，运行时端 user iterable 留待
  chained-dispatch follow-up。

**回流**：是。schema-rooted 主文档「21 Iterable」章节末尾加
"runtime Iter 表达 + Comprehension lowering" 注脚；Iter witness
对齐 `Option<T>`（不是 `Optional<T>`）。multi-hop receiver 限制
作为已知问题记 roadmap.md §J。

### C.7：5 个算术 operator 的 evaluator lowering
2026-05-11 / Phase C / `crates/relon-evaluator/src/arithmetic.rs`

**问题**：决策 24 把 Number 拆细为 Addable / Subtractable /
Multiplicable / Divisible / Modable 5 个独立 constraint，C.6 已
把 witness shape 登入 `CONSTRAINTS`，但 evaluator 还没把 `+ - * / %`
接到对应 method。怎么挂？候选：
1. 复刻 `try_compare_op_method` 5 份，每个 operator 一份独立函数。
2. 抽 `try_arith_op_method(receiver, other, method_name, ...)` 单个
   helper，5 个 arm 共享，operator → method 名通过 `arith_method_for`
   小函数映射。
3. 把 compare + arith 合成一个超级 helper，参数化方法名 + 返回类型
   形态。

**选择**：方案 2。

**理由**：
- compare witness 返回 Bool，arith witness 返回 Self（或 Int/Float
  退化），调度后语义不同——合在一个 helper 里会让两个语义点纠缠，
  违反「一个函数一件事」。
- 方案 1 复制 5 份，每份只差一个字符串，违反 DRY。
- 方案 2 抽出共享形态（branded receiver + schema_methods 查表 +
  body / native fallback），与 `try_compare_op_method` 镜像但保持
  独立。5 个 arm 通过 `arith_method_for(op)` 拿到方法名，主分发体
  只多 3 行。

**Dict + Dict 合并的次序权衡**：原 `(Operator::Add, Dict, Dict)`
arm 立刻 `deep_merge` 并按 brand 重 check_type，是 Logic-as-Data 的
「两 dict 结构组合」承诺。但用户写 `add(other: Self) -> Self` 是
明确意图覆盖 merge——直觉是「我定义了加法，不要给我合并」。两种方案：

a. Dict + Dict merge 优先（保留），用户 add 不会命中。
b. Dict + Dict 命中 → 先 `try_arith_op_method`，无 witness 才走
   merge。

选 b。理由：
- 用户写 `#derive Addable` + `add(...)` 是显式行为声明，沉默地被
  merge 替代会让审计语义崩塌。
- merge 与 method 不冲突：无 witness 时行为完全等同 a 方案，向后
  兼容；有 witness 时按用户预期走 method，零意外。
- compare 路径（C.1）已经走的就是「先试 method，再 fallback」次序，
  arith 跟同一个范式才一致。

**测试**：8 个新测试。
- 5 个「branded value + body 方法命中」（Add/Sub/Mul/Div/Mod）。
- 1 个「primitive Int + Int 仍走数值 fallback」回归保护。
- 1 个「branded value 但 schema 没 add witness（仅有 sub）→ 其他算子
  不被串台」。
- 1 个「host-native add 返回带 brand 的 Money」结构化数学示例
  （body 路径返回带 brand 的 dict 涉及 method body 语法的细节，
  用 #native + register_pure_method 更直接，且 body 路径已被前 5
  个测试覆盖）。

**回流**：是。属于跨 analyzer / evaluator 的语言级语义——5 个算术
constraint 从「shape-check only」推进到「lowering 完成」，主文档
「operator lowering」节应同步列入 +、-、*、/、% 的 method 名。

### C.10：多段 receiver dispatch（`o.customer.greet()`）
2026-05-11 / Phase C / `crates/relon-analyzer/src/typecheck.rs` +
`crates/relon-evaluator/src/eval.rs`

**问题**：决策 12 起的 schema-rooted dispatch 在 analyzer 端
`check_method_dispatch` / `resolve_call_signature` 末尾的 schema-
method 分支、evaluator 端 `try_call_schema_method`，都硬性写
`path.len() == 2`，只识别 `value.method(...)` / `Schema.method(...)`
两种 2-segment 形态。3 段及以上的 `o.customer.greet()` 走不到
schema_methods 查表——analyzer 直接 `return`，evaluator 也直接
`Ok(None)` → 上层 `FunctionNotFound`。roadmap.md §J 把它显式记为
「3-segment 需要 path-tail 推断与 receiver 解耦」。怎么破？候选：

1. 在 `check_method_dispatch` 里特化 path.len() == 3 / 4 / ...
   每个长度分别处理。
2. 用 `infer::walk_path(path[..-1], scope)` 一把推 prefix 到
   `InferredType::Schema(name)`，最后段做 method 查表。evaluator
   端镜像形态：`resolve_variable(path[..-1])` 拿 value，
   `value_schema_tag(value)` 拿 brand，最后段做 method dispatch。
3. 把 dispatch 抽成 Expr 上的 method-call variant（让 parser 产生
   独立 AST 节点），analyzer / evaluator 都不用走 path 分支。

**选择**：方案 2。

**理由**：
- 方案 1 长度爆炸，且每个 case 与 2-seg 的逻辑只差「prefix 怎么
  推到 schema」一处——重复成本远超共享成本。
- 方案 3 是「语言层重构」级别的改动，需要 parser / AST / formatter /
  IDE 全栈跟随，跟「填一个 TODO」的 scope 完全不匹配。也不符合主
  agent「不引入新 Expr variant」的边界。
- 方案 2 顺着 v1.4 path-tail walker 已经服役的形态走：
  - analyzer：`walk_path` 已经处理 Schema field → field type 的递
    归下钻、Optional 拨皮、Dict 的 value-type、Tuple positional
    等所有形态，spread / strict-mode 等模块都在用同一个 driver；
    把 method dispatch 接上去就是「再添一个 consumer」。
  - evaluator：`resolve_variable` 已经支持任意长度 path（dict /
    list 下钻、optional 拨皮、动态 key），`try_call_schema_method`
    在 2-seg 形态下已经从 `scope.get_local(head)` 取 value——把
    它升级成 `resolve_variable(prefix)` 是最小代码 delta。
- side-effect：head 是 sibling closure / aliased import 的「让规
  则签名检查兜底」逻辑只对 2-seg 仍有意义（3+ seg 时这些表都没
  有「nested fields」概念），所以保留 `path.len() == 2` 时的早
  退分支，3+ seg 直接走 path-walk authoritative。

**evaluator 端 prefix 解析失败的处理**：`resolve_variable(prefix)`
失败（typo head、缺字段……）时，返回 `Ok(None)` 让上层
`call_function` 走 `FunctionNotFound`，**不** 当场抛 prefix 的
`VariableNotFound`。理由：

- 用户写 `o.typo_field.method()` 时，prefix 推断失败已经在
  analyzer 端通过 `UnresolvedReference` / `UnknownMethod` 兜底报
  过；evaluator 撞到错误时再多一份「method not found」反而对用
  户更友好（一次顶层 path-level 错误比深层 field-walk 错误更易
  排查）。
- 与 2-seg「receiver 不在 scope」时已经 `return Ok(None)` 的回退
  语义一致——同一个分发函数对「找不到 receiver」的两种长度形态
  给同样的契约（让 caller 决定生 `FunctionNotFound`）。

**静态 `Schema.method` 分支限制为 2-seg**：multi-hop 形态下，
`Order.User.greet` 这种 prefix 不会是「找一个静态 schema 名」——
prefix 必然要 resolve 到一个 runtime value 才能拿 brand。所以静
态分支只在 `path.len() == 2` 时进入，这是从语义而非便利出发的硬
约束。

**动态 key 在 prefix 中是否支持**：当前不需要特殊处理。
`resolve_variable` 已经处理 `TokenKey::Dynamic`（运行时求值后当
key 用），但 analyzer 端 `walk_path` 把 Dynamic step 当 Name step
看——这对 `Dict<K, V>` 形态返回 value-type，对 Schema 形态会
unknown-step。语义上「`m[key].method()` 推不到 schema」就静默
fallback 到 `FunctionNotFound`（runtime 端 `resolve_variable` 跑
通，但 `value_schema_tag` 拿不到目标 schema 也 fallback）——和
2-seg 同样的契约，无新行为。

**测试**：4 个新测试。
- 2 个 analyzer fixture + 测试（`multi_hop_dispatch.relon` 正向、
  `multi_hop_unknown_method.relon` UnknownMethod 反向）。
- 2 个 evaluator 端 e2e 测试：
  - `multi_hop_schema_method_dispatches_through_field`：
    `o.customer.greet()` 走 prefix 解析 + brand → User.greet。
  - `multi_hop_schema_method_with_arg`：multi-hop + 非空 arg 列
    表，验证 `invoke_method_body` 的 arg-binding 在 multi-hop 前
    缀下不变。

**回流**：是。roadmap.md §J「多段 method dispatch」从「TODO」推
进到「已落地」；主文档 dispatch 章节末尾应补一句「path n>2 走
walk_path 推 prefix 到 Schema，behavior 与 2-seg 等价」。


### C.11：user-callable `Iter.next()` 的 cursor 承载方案
2026-05-11 / Phase D 收尾 / `crates/relon-evaluator/src/stdlib.rs`

**问题**：C.9 把 `Iter.next()` 留成 stub（返回
`RuntimeError::UnsupportedOperator`），Comprehension 走
`materialize_iterable` 旁路。用户写 `let first = it.next()` 还是
拿不到迭代器协议。本批次的目标是把这条「user-callable next」补
上。难点回到 C.9 已经辨认过的核心矛盾：`Value` 不可变（`Arc`
共享，无 interior mutability），cursor 没法放进 Value 内部；放
外面又得有个状态承载点。

**选择**：把 cursor 表放在 stdlib.rs 的 module-local static
（`OnceLock<Mutex<HashMap<u64, usize>>>`），iter id 由 module-local
`AtomicU64` 分配。`iter()` 构造时 stamp 一个 `_id` 字段到
`Iter`-branded dict 上；`next()` 读 `_id` 查表推 cursor。

**理由**：候选三选项的对比——

1. **(A) Context-side cursor 表 + NativeFnCaps trait 扩展**：
   cleaner——cursor 生命周期跟 Context 走，`eval_root`/`run_main`
   末尾 clear；多 Evaluator 之间互不干扰；测试可观测。但需要：
   - `Context` 加 `iter_cursors: Mutex<HashMap<u64, usize>>` +
     `iter_id_counter: AtomicU64`；
   - `NativeFnCaps` 加两条 default-method（`allocate_iter_id` +
     `iter_cursor_fetch_and_inc`），`EvaluatorCaps` 覆写；
   - `make_iter_value` 拿 `&dyn NativeFnCaps` 参数。
   触动 `native_fn.rs`（trait 公开 API）+ `eval.rs`（Context 字段 +
   EvaluatorCaps impl），活动范围溢出本批次的 stdlib-only 任务边界。

2. **(B) Value 内部带 `Arc<AtomicUsize>` 的特殊变体**：在 `Value`
   enum 加 `IterCursor(Arc<AtomicUsize>)`。但动 `Value` enum 是大
   面积破坏（`PartialEq` / `Serialize` / 各 match 站点全要补），与
   C.9 的「Iter 是 inert 容器」哲学也冲突。

3. **(C) 当前选项：stdlib-local static**：cursor 表与 iter
   builtin 共处一文件；id 从 module-local `AtomicU64` 取；表本身
   是 `OnceLock<Mutex<HashMap<u64, usize>>>`。代价：
   - 进程生命周期内不释放（每个 iter() 留 16B：`(u64, usize)`），
     短脚本可接受，长跑 host 是已知 leak；
   - 跨 Context 共享同一张表（id 唯一性靠全局 `AtomicU64` 保证，
     不撞车，但表本身全局）。
   收益：完全不触 `native_fn.rs` / `eval.rs`，整个 user-callable
   iteration protocol 全部落在 stdlib.rs 一个文件，PR scope 小到
   只剩单文件 + 测试。

选 (C) 作为本批次的实装，把 (A) 列为 §J roadmap entry「`Iter.next()`
cursor table 升级到 Context-bound 生命周期」的未来工作 —— 接口收口
之后回流（届时再补 NativeFnCaps 扩展）。

形态细节：
- `Iter`-branded dict 字段新增 `_id: Value::Int(i64)` —— `u64` 用
  `as i64` 二进制转回（`IterNext` 端再 `as u64` 回去），不动
  `Value::Int(i64)` 既有签名。
- `next()` 返回 `Option.Some { value: T }` / `Option.None {}` 的
  `Value::variant_dict`（prelude `Option<T>` 形状）；exhaustion 后
  幂等返回 `None`，cursor 永不回退。
- Comprehension fast path（`materialize_iterable`）**不读** `_id`、
  **不动** cursor 表 —— 保证 user-side `it.next()` 与
  `[for x in it: ...]` 互相独立，不会因 comprehension 把 cursor
  推过头让后续 `next()` 直接拿到 `None`。
- String / Dict 的每次 `next()` 当前重建 `chars` / sorted keys
  vec —— O(n) per call。优化（缓存解 vec）留给 follow-up；
  comprehension 走 fast path 不受影响。

**测试**：3 个 evaluator test 覆盖三种 kind：
- `iter_next_on_list_walks_through_then_returns_none`：3 个元素
  + 1 次 over-run，断言每次 `Some / None` 形态正确。
- `iter_next_on_string_advances_per_codepoint`：UTF-8 边界 + 末尾
  `None` 幂等。
- `iter_next_on_dict_yields_key_sorted_entries`：dict 乱序 insert
  后 `next()` 仍按 key-sorted 出对，与 `materialize_iterable` 同
  规约。

**回流**：是。roadmap.md §J「`Iter.next()` 升级到 Context-bound
cursor 表」新增 TODO（当前是 stdlib-local static 的过渡实装）；
主文档「21 Iterable」章节末尾「next() 是 stub」一句改为「next()
是 host 实装，cursor 走 stdlib-local static（升级到 Context-bound
是 §J follow-up）」。


### C.12：方法级 generics 解析（载体 vs stdlib_signatures 单源化）
2026-05-11 / Phase C / `crates/relon-parser/src/directive.rs` + `token.rs`
+ `crates/relon-analyzer/src/schema.rs` + `extend.rs` + `core/list.relon`

**问题**：C.8 让 `core/list.relon` 充当 List/Dict/Iter method 的「dispatch
载体」，但 parser 当时不支持方法级 generics（`map<U>(...)`），载体
被迫退化为 `map(f: Closure) -> List<T>`——signature 真相落在
`stdlib_signatures.rs::_list_map<T, U>(...)`。一个 name 注册在 carrier，
一个 generic 化的真签名活在另一处，是双源。要清掉这条 TODO，把
方法级 generics 也接上 parser → analyzer → FnSignature.generics 这条链。

**选择**：parser 接受可选的 `name<T1, T2, ...>(params)` 语法，复用现有
schema-level `parse_generic_param_list` helper；analyzer
`SchemaMethodInfo` 增 `generics: Vec<String>` 字段，
`method_info_from_parser` 复制过来；`extend.rs::synthesize_method_signature`
把它接到 `FnSignature.generics`；carrier `core/list.relon` 升级回真泛型
`map<U>(f: Closure<T, U>) -> List<U>` 等签名。

**理由**：
- 复用 schema-level 已有的 `parse_generic_param_list` 让 parser 改动
  最小（一个 try-parse + 一个新字段），不引入新 grammar 规则。
- `FnSignature.generics` 早就存在（v1.1 通用机制），载体侧填进
  method 的 generics 后，现有 `sig::instantiate` 直接复用——零额外
  inference 逻辑。
- 单一真理源：method-form dispatch (`lst.map(f)`) 现在从 carrier 拿
  签名，free-fn `_list_map(list, f)` 继续从 `stdlib_signatures` 拿——
  两个 dispatch path 各有独立的 signature carrier，互不重叠，不再
  「同一信息写两次」。

**schema-level vs method-level generics 同名怎么办**：当前不查重。
若用户写 `List<T> with { foo<T>(...) ... }`，parser 不报错，analyzer
不报错——`foo` 的 `T` 在 method scope 内 shadow 掉 schema 的 `T`。
设计上方法 generics 是 method body 内可见的占位符；外层 schema 的 `T`
绑到 receiver 类型，方法 generic 的 `T` 是 fresh placeholder。这两者
在 `instantiate` 里是独立 binding key——key 撞了的话，substitution
顺序是先把 receiver schema T 套进 method 的 `params/return_type`
（在 `resolve_call_signature` 里），再把方法 generic 的 T 让
`instantiate` 绑到具体类型。但因为是同名 key，前一步会把 receiver T
当成方法 generic T 一并替换掉，导致语义混乱。短期可接受（这种命名
违反一般编码规范，用户不会真正这么写）；后续 follow-up 可以在
`build_method_signature_table` 处加一个 warning diagnostic
「method generic shadows schema generic」。

**stdlib_signatures.rs 清理**：扫了一遍，没有「method-only」重复的条目
——`_list_map` 这些是 free-fn dispatch 的真签名，与载体 `List.map` 走
**不同**的 lookup path（一个查 stdlib_signatures，一个查 schema_methods）。
都保留，没有冗余可清。

**测试**：3 个新测试 + 1 个 fixture。
- parser 端 `method_with_generics_parses`：fixture
  `with_block/method_with_generics.relon` 包含
  `map<U>(...) -> List<U>` + `reduce<U>(...) -> U` + 单态 `same()`，
  断言 SchemaMethod.generics 字段被正确填充。
- analyzer 端 `core_list_map_carries_method_level_generics`：空
  document，验证 carrier 注入的 List.map.generics 为 `["U"]`。
- analyzer 端 `method_signature_table_propagates_method_generics`：
  验证 FnSignature.generics 被 `synthesize_method_signature` 接住，
  且 schema-level T 没被错误地复制进 method signature 的 generics
  列表。

**回流**：是。主文档决策 4 那块「parser 改动：method generics 当前
不支持，stdlib_signatures 兜底」一句应改为「parser 支持方法级
generics；载体即真签名」。stdlib_signatures.rs 的注释「The carrier
here is just the dispatch shell」也已经在 list.relon 里替换为
「The carrier is the single source of truth」。

### C.13：Indexable lowering（`a[i]` → `a.index(i)`）
2026-05-11 / Phase C / `crates/relon-parser/src/expr.rs`
+ `crates/relon-analyzer/src/constraints.rs`
+ `crates/relon-evaluator/src/eval.rs` + `reference.rs`

**问题**：决策 22 要求 `a[i]` 在 receiver schema 派生 `Indexable` 时
desugar 到 `a.index(i)` 调用，witness 形状 `index(key: K) -> Option<V>`。
constraint 注册表的 shape 校验已经登记，但「bracket 后跟方法分派 +
Optional 透明解包」这条 evaluator 通路一直空缺。

**IR 选择**：保留现有 `Expr::Variable(Vec<TokenKey>)` + `TokenKey::Dynamic(expr, opt)` 形态，**不**引入新的 `Expr::IndexAccess` variant。
理由：
- parser 早就把 `a[i]` / `a?[i]` 解析成 `Variable(vec![..., Dynamic])`，
  evaluator 端只需要在 dispatch 时增加一层 schema-method 兜底；新建
  variant 会让 `resolve_variable` / `lookup_value_path` / dict
  reference 路径都要各自学认两种节点形态，回报甚低。
- 「a[i] 是带 dynamic key 的路径访问」的语义本来就一致，分支只在
  「访问目标是 branded value + schema 有 index 方法」这一点收窄。
- 同样的好处：未来如果想给 `a?[i]` 的 `?` 走 path-tail Optional 通路
  统一解包，IR 不需要再拆一次。

**dispatch 通路**：新增 `Evaluator::try_index_method(receiver,
key_value, is_optional, display, scope, range)`，封装：
1. 用 `value_schema_tag` 抽 receiver 的 schema 标签。
2. 查 `analyzed.schema_methods[&schema][index]`，或
   `context.native_methods[(schema, "index")]`。两表都空 →
   `Ok(None)`，由 caller 落回内建 Dict/List 的结构性查找。
3. 命中后调 method body（`invoke_method_body`，receiver 进 `self`）或
   native impl（`try_call_native_method`）。
4. 通过 free 函数 `unwrap_optional_for_index` 统一解 Optional 包。

调用点三处：
- `reference.rs::resolve_variable`：`a[i]` 在 root 是 local 变量的场景；
- `reference.rs::lookup_value_path`：`&this.bag[i]` / `&prev.x[i]` /
  函数返回值后接 `[i]` 的场景；
- 注：`eval_reference_path_from` 的 `Expr::Dict` / `Expr::List` 两条
  AST-walk 分支不引入分派——它们处理的是**字面量** AST，brand 信息
  要等 thunk 物化后才存在；物化后的值最终都会经过
  `lookup_value_path`，那里已经接好。

**Optional 解包协议**：witness 返回 `Option.Some { value: v } |
Option.None {}`（prelude 的 `Option` enum schema 形状，`variant_dict
brand=Some/None, variant_of=Option`）。`unwrap_optional_for_index`
按下列规则解包：
- `Some { value }` → 取 `value` 字段返回；
- `None`：若 caller 段是 `?[i]`（is_optional=true）→ `Value::Null`；
  否则 → `VariableNotFound(display, range)`（和内建 dict-miss /
  list-miss 一致的错误形态）；
- 非 Option-shape 的返回 → 原样穿透。analyzer 的 witness shape
  校验已经把源代码侧的 `#derive Indexable` 锁到 `-> Option<...>`，所以
  非 Option 出现意味着 host 注册的 native method 绕开了源代码侧
  形状校验——此时穿透是唯一安全选择。

**和现有 `?` 协议的对接**：路径段的 `?` 在 Relon 里是**前缀**形式
（`?.field` / `?[expr]`），不是后缀（任务描述里写的 `a[i]?` 实为
笔误，确认与 path-tail `?.field` 保持一致 = `a?[i]` 前缀）。
是不变量：`TokenKey::Dynamic(expr, is_optional)` 的 `is_optional`
原本就映射 `?[` 前缀；evaluator dispatch 时直接读这个 bit。

**parser 微调**：`parse_type_expr` 之前对单段标识符 `xs?` 太贪——
`xs?[0]` 整体被吃成 `Type(xs?)` + 残留的 `[0]`，让上层
`parse_var` 永远拿不到。修正：当 `t.is_optional && t.generics.is_empty()
&& t.path.len() == 1` 且后续 char 是 `[` 时，让 `parse_type_expr`
回滚，把控制权交给 `parse_var` 走 `Variable + Dynamic`。改动局限
于一处布尔卫语句，对 `Int?` / `List<T>?` / 复合路径 `a.b?` 这些
合法 type 形态无影响。

**constraint 注册表 shape 校验微调**：注册表本体「不动」（per
任务约束），但 shape 校验的**比对逻辑**做了两处放宽：
- `param_type_matches`：`expected.type_name == "Any"` 视作通配。
  注册表里 Indexable.key 用 `"Any"` 当 stand-in（注释明确说是
  placeholder，generic K 还没参与），不放宽就永远拒掉用户的
  `index(key: Int)` 这种具体类型。
- `return_type_matches`：`"Optional"` 兼容 head `"Optional"` 与
  `"Option"`。prelude 把运行期 enum schema 注册成 `Option`，让
  用户写 `-> Option<V>`（短名，和代码库其它地方一致）也能过
  shape 校验。

**测试**：4 个新测试 + 1 个 fixture。
- analyzer 端 `derive_indexable_with_matching_shape_populates_method_table`：
  fixture `schema_methods/index_dispatch.relon`（`Bag` schema +
  `index(key: Int) -> Option<Int>` 三段式 ternary 体），验证零
  `ConstraintWitnessShapeMismatch` + `schema_methods["Bag"]` 含
  `index` method。
- evaluator 端 `indexable_lowering_dispatches_through_index_method`：
  端到端跑 `bag[0]` / `bag[1]`（命中 Some）/ `bag?[99]`（命中
  None，`?` 解包成 Null）。
- evaluator 端 `indexable_lowering_missing_key_without_question_mark_errors`：
  `bag[99]`（无 `?`）走 None → `RuntimeError::VariableNotFound`。
- evaluator 端 `builtin_dict_and_list_indexing_still_works_without_witness`：
  回归测试，`{ d: {...}, xs: [...], d["b"], xs[2] }` 无 schema-method
  注册时落回内建结构性查找。
- parser 端 `test_optional_bracket`：直接对 `parse_var` 跑 `a?[0]`，
  锁定 `Dynamic(opt=true)` 的解码不再因 `parse_type_expr` 抢跑而退化。

**未决问题**：witness `index(key: K)` 里 generic `K` 的类型推断还没
做——目前 shape 校验把 `K` 等价为「Any」，evaluator dispatch 时
把用户传进来的 key 原样喂给 method body。当用户写 `bag["abc"]` 而
witness 声明 `index(key: Int)` 时，本应在 typecheck 早期报错，但
当前只会在 method body 里运行时崩。这条挂在 follow-up「constraint
generic unification」里，IR 选择没有挡道。

**回流**：是。`constraints.rs` 模块顶注释「Indexable lowering not
yet hooked up」可以划掉。decision 22 在主文档里关于「a[i]? 后缀」的
描述应澄清为「`?[i]` 前缀，与 path-tail `?.field` 一致」——避免再
有 follow-up agent 误读。
