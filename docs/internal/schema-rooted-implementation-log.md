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

