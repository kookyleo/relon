# Schema-Rooted Implementation Log

> 实施过程的决策日志。Phase A.1 / B / C / D / E 实施时遇到的、未在
> [`schema-rooted-model-2026-05-11.md`](./schema-rooted-model-2026-05-11.md)
> 决策清单里覆盖的细节，在这里登记。每条记录形态：上下文 + 选择 + 理由。

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

