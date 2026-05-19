# v6-ε — Guard hoisting：通往 LuaJIT 1-3 ns / iter 同级 hot loop

**Status**：Planning（待 v6-δ M2-A/B/C 全部落地后启动）
**Owner**：TBD
**预估总工期**：6-10 周（4 子 phase sequential）
**前置依赖**：v6-δ M2-C IC dispatch + sub-3 ns/iter（baseline ~5 ns/iter）

---

## 1. 目标

v6-δ M2-C 后预期 trace-tier hot loop ~5 ns/iter，仍 1.7-5× 慢于 LuaJIT 最佳 1-3 ns/iter。差距全部来自 **4-prong sandbox 在 hot loop 内逐 op 收税**：

| Prong | 每 op 开销 | hot loop 累计（per iter，假设 3 ops） |
|---|---|---|
| bounds check（数组访问） | ~0.3-0.5 ns | ~1 ns |
| trap（overflow/div-zero） | ~0.5-0.8 ns | ~1.5-2 ns |
| capability gate | ~0.4 ns（间接调用） | ~0.4 ns（如有 cap 调用） |
| resource limit tick | ~0.3 ns（原子或非原子 inc） | ~0.9 ns |
| **总计** | | **~3-4 ns**（与 LuaJIT gap 完全吻合） |

v6-ε 任务：**保 4-prong sandbox 语义 (correctness 非协商)，但把 hoistable 的检查提到 trace 外**，靠 deopt cookie + invalidation 兜底语义。

**最终预期**：hot loop **1.5-2 ns / iter**，进 LuaJIT 同级（1-3 ns / iter 区间）。

---

## 2. 设计原则

1. **Hoist 不等于消除**。所有 4 prong 的语义在 deopt 路径仍 100% 保留（bytecode VM 重入承担）。
2. **每个 hoist 必须可证明**（loop-invariant、单调、被更宽 dominator 覆盖等），且配 **invalidation cookie**：trace entry 验一次，运行时若依赖值变化（数组 resize、cap 撤销、类型变窄）→ 写 cookie → 下次 trace entry 验失败 → deopt 重 record。
3. **差分测试不可少**。Hoist 前后 1697+ 个测试 + 52-case 4-way corpus + adversarial corpus（专门触发 hoisted-guard 失败）必须 0 mismatch。任何分歧 = sandbox bypass = STOP。
4. **每个 phase 独立可发布**。即使 ε 链没全跑完，每个子 phase 落地也带来可观察 perf 提升。

---

## 3. 4 子 phase 分解

### v6-ε M1：Bounds-check hoisting + LICM 扩展（预估 2 周）

**目标**：把数组访问的逐 iter bounds check 提到 trace 入口。

**Hoist 条件**：
- 索引 `i` 是 loop induction variable，已知步长 + 已知上下界
- 数组长度 `len(arr)` 在 loop 内不变（LICM invariant）
- 静态证明：`0 ≤ i_min` ∧ `i_max < len(arr)`（覆盖整个 loop 范围）

**Hoisted check**（trace entry 处一次性）：
```
assert i_max < len(arr); else deopt
```

**Invalidation cookie**：
- 每个数组带 `generation: u32`（resize/clear 时 ++）
- Hoisted check 同时记录 `arr.generation` 到 trace metadata
- Trace 每次入口验：`current_arr.generation == recorded_generation`，否则 deopt + 重 record

**Loop 内**：bounds 检查 **完全删除**（依赖已 hoist 的范围证明）

**新增 LICM passes**（`crates/relon-trace-jit/src/optimizer/`）：
- `bounds_hoist.rs`：识别 `IndexLoad(arr, i)` ops，证明 i 范围 ⊆ [0, len)
- `licm_extended.rs`：把已证明 invariant 的 `len(arr)` load 提到 trace 入口

**测试**：
- 经典 case：`for i in 0..n { sum += arr[i] }` — bounds check 完全消失
- 边界：`for i in 0..arr.len() { ... }` — 隐含范围
- Adversarial：trace 跑到一半 `arr.push(x)` → generation 改变 → 下次入口 deopt

**预期 bench delta**：5 ns → ~3.5 ns（数组 hot loop）

---

### v6-ε M2：Overflow / Trap hoisting via 类型范围跟踪（预估 2-3 周）

**目标**：把 `i64 + i64` 的 overflow guard 在已证明范围内消除。

**Hoist 条件**：
- SSA 值的运行时范围已知（来自 const fold、type spec、loop induction analysis）
- 加法 `a + b`：若 `|a| + |b| < i64::MAX`，则不溢出
- 乘法 `a * b`：若 `|a| * |b| < i64::MAX`
- 减法 / 模数 / 除法类似

**Hoisted check**（trace entry 处）：
- 对 loop induction：`assert i_max + step ≤ i64::MAX`
- 对外部参数：trace entry 验输入范围（IC entry guard）

**Invalidation**：
- Loop induction 安全（范围由 trace 自身控制）
- 外部参数变化 → IC mismatch → deopt（已有机制，零额外成本）

**新增 passes**：
- `range_analysis.rs`：SSA 值范围跟踪（abstract interpretation，类似 LLVM ScalarEvolution 简化版）
- `overflow_hoist.rs`：基于范围证明删除 inner overflow guard

**Div-by-zero hoisting**：
- 若分母是 loop invariant，验 entry 一次：`divisor != 0` else deopt
- Loop induction step 已知 ≠ 0 时安全
- 用户代码常规 case：`for i in 1..n { result /= i }` — i 从 1 起，恒非零

**Adversarial 测试**：trace entry 接受边界值（接近 i64::MAX），运行中走到溢出点 → 必须 trap 而非静默错误

**预期 bench delta**：3.5 ns → ~2.8 ns（带 i64 arith 的 hot loop）

---

### v6-ε M3：Capability hoisting + invalidation cookie（预估 1-2 周）

**目标**：把 hot loop 内重复 capability lookup 提到入口。

**Hoist 条件**：
- 同一 `cap_id` 在 loop 内多次查
- `Context.capability_vtable` 在 loop 内不改

**Hoisted check**：
- Trace entry：`fn_ptr = cap_lookup(cap_id); else deopt`
- 同时记录 `vtable.generation`
- Loop 内直接 `call fn_ptr`（去掉间接 vtable 查找）

**Invalidation cookie**：
- 每次 capability grant/revoke 时 vtable.generation++
- Trace entry 验 `current_vtable.generation == recorded`，否则 deopt

**Sandbox 语义保留**：
- Hoist 不放松 capability 检查（仍验 cap_id 合法 + cap 已授权）
- 只是把"查表 + 拿 fn_ptr"这步缓存

**预期 bench delta**：2.8 ns → ~2.5 ns（hot loop 含 cap 调用时）

---

### v6-ε M4：Resource limit batching（预估 1-2 周）

**目标**：tick counter 从 per-op 改 per-trace-exit。

**当前**：每个 BcOp dispatch 前 `ctx.tick_instr(); if ctx.exhausted() trap`

**Hoisted**：
- Trace entry：估算 trace body 含 N ops（编译期已知）
- Trace exit：`ctx.tick_instr_by(N)`（一次性）
- Trace 中间 backward branch 处加 poll：`if ctx.exhausted() trap`
- Memory limit 类似：trace entry 估算 max allocation，trace exit 一次性记账

**Safety constraint**：
- N 不可任意大。设 cap `MAX_TRACE_OPS_NO_POLL = 4096` — 超过则 trace 中间强制插 poll
- 兜底：用户 timeout 1 ms，假设 1 ns/op → max 1M ops 不 poll → 远低于 4096 cap

**Sandbox 语义**：
- Resource limit 仍精确：trace exit 记账时若 N > 剩余 quota → 立即 trap
- 略微悲观（可能多跑几个 op 才 trap，但在 cap 内），换大幅 perf 提升

**预期 bench delta**：2.5 ns → ~1.8 ns（所有 hot loop）

---

## 4. 终局 bench 预期

| Phase | hot loop ns/iter | vs LuaJIT 1-3 ns/iter |
|---|---|---|
| v6-γ M5（const-only） | 4.39 | trace tier ✓ |
| v6-δ M1（真 add body） | 9.52 | **3-9× 慢** |
| v6-δ M2-C（IC dispatch） | ~5.0 | 1.7-5× 慢 |
| v6-ε M1（bounds hoist） | ~3.5 | 1.2-3.5× 慢 |
| v6-ε M2（overflow hoist） | ~2.8 | 0.9-2.8× 慢 |
| v6-ε M3（cap hoist） | ~2.5 | **trace tier same class** ✓ |
| v6-ε M4（resource batch） | **~1.8** | **0.6-1.8× — LuaJIT 同 class** ✓ |

---

## 5. 风险登记

| 风险 | 等级 | 缓解 |
|---|---|---|
| Hoist 证明 bug → sandbox bypass | **CRITICAL** | adversarial corpus + 1697 现有 + 52-case 4-way 全 0 mismatch；每个 phase 单独 review |
| Invalidation cookie 漏掉边界 case | HIGH | 每个 cookie 配 ≥ 3 adversarial 测试（修改 → trace 必须 deopt） |
| 范围分析复杂度爆炸（M2） | MEDIUM | 限制 abstract interp 深度；超过 fallback 不 hoist |
| Resource batch 在长 trace 里 timing 跑偏 | MEDIUM | MAX_TRACE_OPS_NO_POLL cap；trace 编译期估 N，超 cap 强插 poll |
| LuaJIT 1 ns 仍达不到（剩余开销在哪？） | LOW | 接受 "trace-tier same class"；1.5-2 ns 已经足够，绝大多数场景看不出差异 |

---

## 6. 启动时点

待 v6-δ M2 全部 3 子 phase 落地（M2-A 已 done，M2-B in flight，M2-C 待派）+ 真 sub-3 ns bench 写完，再启 v6-ε M1。

期间不抢资源，不开早期 PR。
