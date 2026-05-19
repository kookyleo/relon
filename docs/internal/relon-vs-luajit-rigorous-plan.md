# Relon vs LuaJIT 严谨对照 — 落地规划

**Status**：In flight（ε-M0 agent `aca8119df8d5aaf3a` 正在跑 recorder loop 录制；后续 phase 按本文档执行）
**前置基线**：main HEAD `dcf353e`（trace_jit_loop 手搭路径 1.185 ns/iter，未经 recorder 录制）

---

## 0. 为什么需要这份文档

bench-rewrite 教训：用错 bench 方法论 → 6 周走错方向。再开 LuaJIT 对照 phase 前**必须把方法论钉死**，否则花 2 周做出来的对照数字一样不可信。本文档定义：

1. 「LuaJIT 同级」的精确含义（8 维度）
2. bench 方法论的 6 个陷阱及如何避免
3. workload 选择的 adversarial 原则
4. phase 顺序、停止条件、决策矩阵
5. 机器状态要求（thermal、freq、isolcpus）

---

## 1. 「LuaJIT 同级」的 8 维度定义

LuaJIT 2.1 是 SOTA dynamic language runtime。其优秀不止单一指标。要诚实声明 Relon 达级别需要测**所有 8 个维度**：

| 维度 | LuaJIT 典型水平 | Relon 测过没 |
|---|---|---|
| D1 hot-loop throughput（紧凑 arith） | 1-3 ns/iter | ✓ 1.185 ns (手搭路径) |
| D2 cold-start latency（从 source 到首次执行） | ~100-500 μs | ✓ 339 μs cached |
| D3 memory footprint per fn | ~1-2 KB | ✗ 未测 |
| D4 steady-state mixed workload | varies | ✗ 未测 |
| D5 p99/p99.9 tail latency（含 deopt） | trace exit ~50 ns | ✗ 未测 |
| D6 polymorphic call site degradation | ~3-5× monomorphic | ✗ 未测 |
| D7 string hot path（concat/find/format） | rope-based, near-C | ✗ 未测 |
| D8 hash table 操作（含碰撞） | linear-probe, near-C | ✗ 未测 |

**达级别 = ≥ 6 / 8 维度处于 LuaJIT × 2 内**。低于此阈值不能声称同级。

---

## 2. bench 方法论 6 大陷阱

每个陷阱都有 v6 phase 之前或之中亲眼见过的事故：

### 陷阱 A — 编译器消除（compiler elimination）
**症状**：`rust_native_loop = 2.48 ns/iter` 太快。rustc 看出 `acc += i` 是闭式表达式，optimize 成常数。
**避免**：bench 输入必须 `black_box`，输出必须 `black_box(result)`。所有 SSA chain 末端读写都加 fence。

### 陷阱 B — Warm-up vs steady-state 混淆
**症状**：trace JIT 需要 ≥ 10k iter 才热（HotCounter 阈值 + IC fill）。criterion 默认 sample size 不一定够。
**避免**：bench `iter_batched_ref` 或显式 `for _ in 0..100_000 { invoke() }` 预热，再 criterion 测。每次 sample 单独预热。

### 陷阱 C — 调用方开销污染（caller-side overhead）
**症状**：v6-δ M2-C / v6-ε-0-C / v6-ε-0-A 测的是 Rust→JIT 边界，不是 hot loop。
**避免**：bench body 内部循环大于 1M iter，让 caller 开销 < 1% 总时间。mlua→Lua 边界同理。

### 陷阱 D — Cache 冷热不一致
**症状**：criterion 多次 sample 之间数据被 evict，每次 cold cache。
**避免**：bench setup 阶段把数据预读到 L1/L2；测量前 dry-run 一次填 cache。

### 陷阱 E — GC vs no-GC 不对等
**症状**：Lua 跑 1M iter 触发 GC pause；Relon 当前无 GC，0 pause。直接对照不公平。
**避免**：选择 zero-allocation workload（纯算术、in-place 数组改）。涉及 allocation 的 workload 单独标记"含 GC bias"。

### 陷阱 F — 分位数掩盖（distribution hiding）
**症状**：criterion 默认 median + IQR。但 trace JIT 的 deopt 是 tail 事件，median 看不到。
**避免**：报告 p50 / p90 / p99 / max。每个 workload 看完整分布，不只是中位数。

---

## 3. Workload taxonomy（10 个 adversarial 选）

选择原则：**adversarial against Relon**。挑 LuaJIT 著名强项 + Relon 可能弱项，逼出真 ceiling。

| # | Workload | 维度 | 为什么 adversarial |
|---|---|---|---|
| W1 | tight i64 sum loop `for i in 0..N { acc += i }` | D1 | LuaJIT trace tier baseline |
| W2 | tight f64 dot product `for i { sum += a[i] * b[i] }` | D1 + 数组访问 | bounds check + 2 reads，ε-M1 bounds hoist 实力测试 |
| W3 | string concat `for i { s = s + chars[i] }` | D7 | 经典 quadratic trap，LuaJIT 不一定快但 Relon 必须能跑 |
| W4 | string find/match `for s in strs { if contains(s, "x") }` | D7 | KMP / 朴素 search 实力 |
| W5 | dict 字符串 key lookup 1M `arr.foldl((m,k) => m + dict[k])` | D8 | hash + string hashing + IC |
| W6 | dict 数字 key dense `for i { sum += arr[i] }` 但 arr 是 dict | D8 | LuaJIT array-part 优化的拿手戏 |
| W7 | recursive `fib(30)` | D1 + call overhead | call ABI + recursion stack |
| W8 | polymorphic call site `f(any_of_4_types)` × 1M | D6 | IC 4-way set-assoc 够不够 |
| W9 | nested loop matrix transpose `for i { for j { b[j][i] = a[i][j] }}` | D1 + D2 + cache | branch prediction + cache pattern |
| W10 | 真实 Relon config eval（10-rule access control） | D4 综合 | 模拟生产 workload |

每个 W 必须：
- Relon 版（写到 `crates/relon-bench/benches/cmp_lua/relon_w*.rs`）
- Lua 版（写到 `crates/relon-bench/benches/cmp_lua/lua_w*.lua`）
- **逐字节同语义**：同输入 → 同输出（哈希、求和、字符串）。一致性测试在 bench setup 内 assert，否则视作 broken。

---

## 4. Phase 顺序 + 决策矩阵

**前置（in-flight）**：ε-M0 recorder loop 录制 — 必须先完成，否则后续 bench 都用手搭路径，对照失意义

**新顺序**：

```
ε-M0 (in flight)
    │
    ▼
λ-0 bench 方法论硬化（1 天）
    │  ‒ 6 陷阱全部硬化进 bench harness
    │  ‒ p50/p90/p99 出，不只 median
    │  ‒ pre-warm + cache-fill + black_box 强制
    ▼
λ-机器 quiescence 配置（0.5 天）
    │  ‒ CPU freq 锁定（performance governor）
    │  ‒ taskset / isolcpus 分核
    │  ‒ thermal 监控（多 sample 之间不许超 5°C 漂移）
    │  ‒ 跑前/跑后 baseline noise 测，超阈值 fail
    ▼
λ-1 LuaJIT install + smoke + mlua boundary calibrate（0.5-1 天）
    │  ‒ LuaJIT 2.1 fixed version
    │  ‒ mlua feature=luajit 接入 bench crate
    │  ‒ 测 mlua→Lua 边界本身的 cost，记入 baseline 减除
    ▼
λ-2 10 workload 实现 + 一致性测试（2-3 天）
    │  ‒ W1-W10 Relon/Lua 各一份
    │  ‒ 一致性 assert：相同输入 → 相同输出
    │  ‒ 4-way Relon backend × Lua 共 50 measurement
    ▼
λ-3 跑全 50 + 分析（1-2 天）
    │  ‒ 中位数 + p90 + p99 + max 表
    │  ‒ ratio Relon-best / LuaJIT
    │  ‒ 每 > 2× 的 W 归因到具体瓶颈
    ▼
λ-4 决策点（看表）：
    │
    ├── ≥ 7/10 ≤ LuaJIT × 2 → λ-5 写综合报告 + 停 loop
    │
    ├── 4-6 / 10 ≤ LuaJIT × 2 → 派 targeted fix phase
    │   ‒ 优先级：ratio 最大的 W 先修
    │   ‒ 每 fix phase 1-2 周，单一 workload
    │   ‒ 改完跑 W 全集（不只是被修的 W）
    │   ‒ 不达 7/10 不停
    │
    ├── < 4/10 ≤ LuaJIT × 2 → 诚实汇报 ceiling，说明哪些维度
    │   是结构性 gap（如 sandbox tax 无法消除），停 loop
    │
    └── 任何 W > 10× → STOP 整个 loop，先派 RCA agent 排查
        是否是 bench 本身 broken（再次方法论 trap）
```

---

## 5. 通过/失败/迭代决策矩阵

| ratio Relon-best / LuaJIT | 数量 | 动作 |
|---|---|---|
| ≤ 2× | ≥ 7 / 10 | **PASS** 写综合报告 + 停 loop |
| ≤ 2× | 4-6 / 10 | **ITERATE** 派 targeted fix（按最大 ratio 排序） |
| ≤ 2× | < 4 / 10 | **HONEST FAIL** 写 ceiling 报告 + 停 loop |
| any ≥ 10× | ≥ 1 | **HALT** 派 RCA 排查 bench 是否 broken |
| any ≥ 100× | ≥ 1 | **HALT** workload 不应该跑这么慢，肯定是 bug |

**ratio 计算**：取每 workload 上 Relon 最快 backend（tree-walk/bytecode/cranelift-AOT/trace-JIT 中最小值）/ LuaJIT 时间。Relon 不规定必须 trace JIT 赢 —— 某 W 上 cranelift-AOT 比 trace-JIT 快也算 Relon best。

---

## 6. 机器 quiescence 要求（硬性）

bench 前必跑：

```bash
# CPU freq 锁定 performance governor
sudo cpupower frequency-set -g performance

# 关掉 turbo boost（避免不同 sample 跑不同 freq）
echo 1 | sudo tee /sys/devices/system/cpu/intel_pstate/no_turbo

# 隔离 bench 跑的 cpu (假设 cpu 4-7)
sudo systemctl set-property --runtime user.slice AllowedCPUs=0-3
taskset -c 4-7 cargo bench ...

# 跑前监控 5 秒 baseline noise
perf stat -a sleep 5  # context-switches 应 < 100 / sec / core
```

**或者**：用 s15 / s16（用户提过的隔壁 192.168.213.15/16 机器）作专用 bench 机，本地只编译，ssh 跑 bench。**这个方案需要用户确认 s15/s16 当前是否空闲**。

---

## 7. 报告输出

**最终交付**：`docs/internal/relon-vs-luajit-final-report-2026-05-19.md`

结构：
1. Executive summary：ratio 表 + PASS/ITERATE/FAIL 判定
2. 各 W 详细分布：p50/p90/p99/max（4-way Relon + Lua）
3. 8 维度逐项：Relon vs LuaJIT 哪些维度 PASS，哪些 ITERATE，哪些 FAIL
4. 归因分析：每 > 2× 的 W 拆到具体瓶颈（sandbox prong / IR op 缺失 / 优化器未触发 / GC 偏置等）
5. Carry-over：每个 FAIL 的具体 fix phase plan
6. 复现指引：完整 bench 命令、机器配置、LuaJIT 版本

并存档：
- 完整 criterion 输出 (HTML + JSON)
- 各 W 的 perf record 火焰图
- 机器状态采样（cpufreq、temperature trace）

---

## 8. 风险登记

| 风险 | 等级 | 缓解 |
|---|---|---|
| 又陷入 bench 方法论 trap（陷阱 G？） | **CRITICAL** | λ-0 把已知 6 陷阱硬化进 harness；任何 ratio < 0.5（Relon "比 LuaJIT 还快") 必须重审 |
| LuaJIT 2.0 vs 2.1 选错 | HIGH | 固定 2.1.0-beta3 或当前 stable head；记入报告 |
| Workload 写得不"同语义" | HIGH | bench setup 内强制 assert 输出一致性 |
| 机器 thermal throttling | HIGH | sect 6 硬化 |
| Workload 选错（偏 Relon 强项） | HIGH | sect 3 强制 adversarial 10 个 |
| Targeted fix phase 无限循环 | MEDIUM | 每 fix phase 最多 2 周 budget；超期直接进 FAIL 路径 |
| Fix 一个 W 时回归另一个 W | MEDIUM | 每 fix 跑完所有 W，不只是被修的 |

---

## 9. 已确认 vs 待确认

**已确认**：
- ε-M0 agent in flight（aca8119df8d5aaf3a）
- bench-rewrite 已落 main = dcf353e
- ε-0-C / ε-0-A 基础设施保留在 trunk

**待确认（需用户决策）**：
- bench 机器：本机 vs s15/s16？需要后者的话需要 ssh 通且确认空闲
- LuaJIT 版本：2.1.0-beta3 vs head？我推荐 2.1.0-stable
- 用户对 8 维度的优先级排序：D1+D7+D8 必过？还是全 8 都要？
- 超时上限：本 phase chain 估 3-4 周，超期是接受还是切换策略？
