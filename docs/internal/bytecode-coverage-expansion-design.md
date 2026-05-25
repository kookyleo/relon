# Bytecode 覆盖扩张 — Design

**Status**: 待立项（等 W4 IV-overflow-elim 完工后启动）
**前置依赖**: `iv-overflow-elim-design.md` 完工 + push
**核心定位变更**: bytecode 不再是"独立 tier 跟 trace_jit 竞争"，而是 **trace_jit 的 deopt landing pad**

## 背景与定位

### 当前现状

| Workload | bytecode 状态 |
|---|---|
| W1 | ✓ 编译 + 运行 (但 80× 慢于 LuaJIT) |
| W12 | ✓ 编译 + 运行 (1.16× LuaJIT) |
| W2-W11 | ✗ `analyzer rejected source: N error(s)` — 全 n/a |

analyzer 当前只接受 **M2-A scalar envelope** —— closure / dict / list / 大部分 stdlib 都拒。

### 关键洞察

bytecode 的真正价值不在"跟 LuaJIT 比单 workload"，在于：

```
                          guard fail
trace_jit (hot path)  ──────────────▶  ???
                                       └──▶ tree_walk (~100-1000× 慢) — 不可接受
                                       └──▶ bytecode (~5-50× 慢) — 可接受
                                       └──▶ AOT (不可，需要 recompile) — 不可接受
```

这是 **LuaJIT canonical design**：trace exit → interp landing → interp 收集新观察 → 重 record。

测试已经验证 deopt 协议工作:
- `bytecode_trace_deopt_handoff_e2e.rs`
- `bytecode_deopt_integration.rs`

但只覆盖 M2-A 的 source。production workload 包含 closure/dict 的 deopt **会回退到 tree_walk**, trace_jit 价值崩塌。

## 目标

**Coverage 契约**: bytecode VM 必须能执行 **trace_jit 能 record 的全部 source surface**。

具体含义：
- 每个 TraceOp 变体在 bytecode VM 都有对应实现
- analyzer 接受所有 trace_jit 可 record 的 source

## 当前 trace_jit 可录 surface (用作 bytecode 覆盖参考)

从 cmp_lua W2-W12 工作负载提取（已验证 trace_jit 能 record）：

| 类别 | 操作 |
|---|---|
| 整数算术 | Add/Sub/Mul/Div/Mod/Cmp(Eq/Ne/Lt/Le/Gt/Ge)/Neg |
| 浮点 | F64 Add/Sub/Mul/Div + 取值/存值（W2 hot path） |
| String stdlib | concat / contains / find / substring / glob_match |
| List stdlib | ListGet / iteration (range/map/filter/len) |
| Dict | DictLookup (字符串键 + 数值键) / DictShapeGuard |
| Control flow | Block / Loop / Br / BrIf / Return |
| 闭包 | 简单 lambda inline (W4/W5 .filter/.map shape) |

## 分阶段实施

### Phase B-1: TraceOp ↔ BcOp 对齐 (1 周)

每个 `TraceOp::*` 变体在 `BcOp::*` 都有 1:1 对应：

```text
TraceOp::Add → BcOp::AddI64
TraceOp::StrContains → BcOp::StrContains (call helper, 同 trace_jit fallback shim)
TraceOp::DictLookup → BcOp::DictLookup
TraceOp::ListGet → BcOp::ListGet
TraceOp::Cmp → BcOp::CmpI64 (各 CmpKind)
TraceOp::Guard → BcOp::Guard (deopt 到 tree_walk fallback；或 abort)
```

实现：BcOp 已有大部分基础 op；查 `crates/relon-bytecode/src/op.rs` 看 gap。

### Phase B-2: analyzer 扩 surface (1 周)

analyzer 拒绝 W2-W11 的具体原因要先查（每条 error 对应一个限制）。常见预期：
- closure 接受（至少 inline lambda 形式）
- list/dict 字面量
- stdlib 调用 (`contains`, `find`, `map`, `filter`, `range`, `sum`, ...)
- iteration protocol

每一项展开都要 typecheck + bytecode lowering + 测试。

### Phase B-3: deopt 集成测试扩张 (3-5 天)

`bytecode_trace_deopt_handoff_e2e.rs` 当前只测 `x + y` shape。新加 e2e 测试：
- W3-shape (string concat) deopt → bytecode resume
- W5-shape (dict lookup) deopt → bytecode resume
- 每个 production-shape workload 至少 1 个 deopt scenario

### Phase B-4: bench panel 增 deopt 场景 (2-3 天)

cmp_lua 增加 `deopt_recovery` workload group:
- 强制 trace_jit deopt（用稀有 input shape 让 guard fire）
- 测 bytecode resume 的 steady-state ratio (vs LuaJIT)

新增 panel rows：
- `relon_deopt_to_tree_walk` (current 行为 baseline)
- `relon_deopt_to_bytecode` (期望 5-50× 比 tree_walk 快)

## Naming alignment — 采用 Dart 二分法 + 子环节统一前缀

用户视角：**JIT 模式** vs **AOT 模式**（Dart-style binary）。内部 tier 用 `jit_` 前缀分组：

### 用户面向类型

```rust
// 当前 (4 个并列 Evaluator)
TreeWalkEvaluator / BytecodeEvaluator / CraneliftAotEvaluator
+ trace_jit 通过 dispatcher hook

// 目标 (2 个顶层 + 内部 tier)
JitEvaluator       // 管理 tree_walk → bytecode → trace 自动 tier 迁移
AotEvaluator       // AOT 编译产物
```

`JitEvaluator` 内部按 hot-counter 阈值自动 tier 迁移 —— 跟 Dart VM / LuaJIT canonical pattern 一致。用户不选 tier。

### 内部 tier 命名

| 当前 | 重组后 | 角色 |
|---|---|---|
| `TreeWalkEvaluator` | `JitTier::TreeWalk` (or `tier::tree_walk`) | 初始解释 + fallback |
| `BytecodeEvaluator` | `JitTier::Bytecode` | trace deopt landing |
| trace_jit dispatcher | `JitTier::Trace` | hot path JIT |
| `CraneliftAotEvaluator` | `AotEvaluator` (drop `Cranelift`, `Native` 冗余) | 独立 AOT 编译路径 |

### Bench 标签

```
当前:                 重组后:
relon_tree_walk    → relon_jit:tree_walk    (engineer-facing tier breakdown)
relon_bytecode     → relon_jit:bytecode
relon_trace_jit    → relon_jit:trace
(none)             → relon_jit              ← **新增** 集成 panel (auto-tier，对标 LuaJIT)
relon_aot          → relon_aot              ← 新增 (cmp_lua 当前没注册)
```

新增 `relon_jit` row 是默认 mode 端到端跟 LuaJIT 比，省去用户头脑里 tier 选择 —— 跟 LuaJIT 单点比较一致。tier breakdown 给 engineer。

### 落地工作量

| 项 | 量 |
|---|---|
| Type rename (`CraneliftAotEvaluator` → `AotEvaluator` 等) | 30-60 callsites, 0.5 天 |
| 顶层 `JitEvaluator` wrapper | 1 天 |
| `relon_jit` 集成 bench row (auto-tier) | 0.5 天 |
| `relon_aot` 集成 bench row | 0.5 天 |
| Crate 名重组（`relon-codegen-native` → `relon-aot`, etc.） | **延后到下个 season**（高成本 + 低 ROI） |

合计 ~2.5 天，跟 bytecode 扩覆盖项目同 PR 做。

### 不动的部分

- crate 名 `relon-codegen-native` 暂保留（rename 牵涉太多 import）
- 已有 `relon_tree_walk` / `relon_bytecode` / `relon_trace_jit` bench label 保留**别名兼容** 1-2 season 防破坏历史 baseline 比较

## 验收标准

1. `cargo test --workspace` 全过
2. `cargo clippy --workspace --all-targets -- -D warnings` 干净
3. cmp_lua W2-W12 都至少有 `relon_bytecode` row（不是 n/a）
4. 新增 deopt panel 验证 bytecode resume 比 tree_walk fallback 快 ≥ 5×
5. `CraneliftAotEvaluator` 重命名落地，public API 用 `AotEvaluator`

## 工作量预估

- Phase B-1: 1 周
- Phase B-2: 1 周（最大风险，依赖 analyzer 现状）
- Phase B-3: 3-5 天
- Phase B-4: 2-3 天
- Naming alignment: 0.5 天

**总计**: ~3 周（一名 subagent 长跑 + 主 worktree 阶段性 sync）

## Open Questions

1. analyzer 的 M2-A envelope 是设计上的限制还是 implementation 拖后？需要 owner 确认。
2. bytecode VM 当前 dispatch 性能是否瓶颈？W12 已经 1.16× LuaJIT，说明 dispatch 不是大头。新增 op 应该按现有 dispatch 模式实现即可。
3. deopt 后 bytecode 重启动的 entry point 怎么定？目前 `resume_from_snapshot` 用 external_pc，扩 closure / nested call 后路由会变复杂。

## 启动条件

**两层 gate**：

1. W4 IV-overflow-elim subagent 完工 + push + bench 验证 W4 < 1.0× LuaJIT （现状：subagent commits 已 cherry-pick，s90 panel 跑中）
2. **命名 / 代码结构 refactor 全量落地** (`JitEvaluator` 顶层 wrapper + tier rename + `AotEvaluator` rename + bench `relon_jit` / `relon_aot` row) ← 2026-05-25 用户指令"立即将代码结构、命名方面的调整全面完成。再继续后续工作"

Bytecode 扩覆盖项目 **在 gate 2 完成后**才启动。原 design 是 gate 1+2 同 PR，现在拆开：先纯 refactor 一个 PR，再 bytecode coverage 一个 PR。这样 refactor diff 可读、bytecode 扩张工作能站在干净命名基线上。
