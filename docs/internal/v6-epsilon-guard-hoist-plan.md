# v6-ε — Call boundary + Guard hoisting：通往 LuaJIT 1-3 ns / iter 同级 hot loop

**Status**：Ready to start（v6-δ M2 全部 3 子 phase 已落地）
**Owner**：TBD
**预估总工期**：8-12 周（5 子 phase sequential）
**前置依赖**：v6-δ M2-C 完成（main HEAD ≥ `137c5ff`，bench baseline 9.5 ns/iter）

---

## 0. v6-δ M2-C 收尾后的新认知（plan 重写起因）

v6-δ M2-C 实测 `trace_jit_warm_ic = 9.53 ns/iter`，**和 IC 上线前 9.49 ns 基本相同**。深入分析发现：

- `fat-LTO` 下 Rust 侧 `Arc::invoke` / IC dispatch 完全 inline，机器码等价
- **真瓶颈 = cranelift trace entry 的 SystemV ABI prologue/epilogue**（保存被调用方寄存器、对齐栈、设置 fp、传 args）
- `rust_inlined_baseline = 3.55 ns/iter` 是无 call boundary 时的理论地板

**结论**：原 plan 把 4 prong sandbox 视为唯一 gap 是错的。实际 gap 由两块组成：

```
9.5 ns  ──[call boundary]──▶  ~4-5 ns  ──[sandbox tax]──▶  ~1.8 ns
   现状                            ε-0 后                       ε-M4 后
                                                              (LuaJIT 同 class)
```

所以新 plan **先打 call boundary（ε-0），再做 4 子 guard-hoisting（ε-M1 至 ε-M4）**。

---

## 1. 目标

把 hot loop 从 **9.5 ns/iter** 砍到 **1.5-2 ns/iter**，进 LuaJIT 1-3 ns 区间，**同时保 4-prong sandbox 语义**。

两块 gap 的成本拆分：

**Call boundary**：
- SystemV ABI prologue/epilogue ~4-5 ns / trace entry（按 1 次 trace = 1 次 ABI 边界算）
- 修复手段：at-call-site inline / trace-to-trace fall-through / `CallConv::Tail`

**Sandbox tax** （每 hot loop iter，假设 3 ops）：

| Prong | 每 op 开销 | 累计 |
|---|---|---|
| bounds check（数组访问） | ~0.3-0.5 ns | ~1 ns |
| trap（overflow/div-zero） | ~0.5-0.8 ns | ~1.5-2 ns |
| capability gate | ~0.4 ns（间接调用） | ~0.4 ns（如有 cap 调用） |
| resource limit tick | ~0.3 ns | ~0.9 ns |
| **总计** | | **~3-4 ns** |

---

## 2. 设计原则

1. **Hoist / inline 不等于消除**。所有 4 prong sandbox 语义在 deopt 路径仍 100% 保留（bytecode VM 重入承担）。
2. **每个 hoist 必须可证明**（loop-invariant、单调、被更宽 dominator 覆盖等），且配 **invalidation cookie**：trace entry 验一次，运行时若依赖值变化 → 写 cookie → 下次 trace entry 验失败 → deopt 重 record。
3. **差分测试不可少**。Hoist 前后 1746+ 个测试 + 52-case 4-way corpus + adversarial corpus（专门触发 hoisted-guard 失败）必须 0 mismatch。任何分歧 = sandbox bypass = STOP。
4. **每个 phase 独立可发布**。即使 ε 链没全跑完，每个子 phase 落地也带来可观察 perf 提升。
5. **Call boundary 优先于 hoist**（新原则）：guard hoisting 把每 op 的 ~0.5 ns 砍到 ~0 ns，但 4 ns ABI overhead 不动等于事倍功半。先把 4 ns 拿掉，guard hoisting 的相对收益才显著。

---

## 3. 5 子 phase 分解

### v6-ε-0：Call-boundary elimination（预估 2-3 周）⭐ NEW

**目标**：去掉 cranelift trace entry 的 SystemV ABI 边界，trace 调用从 ~6 ns 降到 ~1 ns。

**三选项对照**：

| 选项 | 做法 | 实现成本 | 兼容性 | 预期收益 |
|---|---|---|---|---|
| **A. At-call-site inline** | 把整个 trace 机器码 splice 进 host fn 体内的 call site | 高（需要 cranelift `JITModule` 子图复制 + reloc 重定向） | 高（ABI 完全消失） | 6 → ~1 ns |
| **B. Trace-to-trace fall-through** | 多 trace 链式调用：trace A exit 直接跳到 trace B entry，跳过 host ABI | 中（需要 trace metadata 记 successor，编译期生成 fall-through 表） | 高（hot loop 多 trace 时尤其有效） | 6 → ~3 ns |
| **C. `CallConv::Tail`** | 用 cranelift `Tail` 调用约定（精简 reg save、无 fp、stack-arg only） | 低（改 trace emitter 的 `call_conv` 一行） | 中（target 限制：x86_64 + aarch64 only；caller 也要用 Tail） | 6 → ~4 ns |

**推荐路径**：**A 为主，C 为补**
- A 实施前先做 C（低成本试水，~2 周拿到 ~4 ns）
- 然后做 A（追求 ~1 ns，多 1-2 周）
- B 暂缓——多 trace 链式调用是 hot loop 常态但不是单一 trace 的瓶颈

**Sandbox 影响**：零。call boundary 是纯 perf 层；4-prong 检查的位置不变。

**Adversarial 测试**：
- Inline 后的 trace 必须能正常 deopt（保存调用 fn 的寄存器状态到 snapshot）
- Tail call 上下文中的 stack unwind 必须仍能恢复
- 至少 5 个 case：guard 失败 → resume_from_pc → bytecode VM 接住 → 结果与 tree-walker bit-identical

**新增/修改文件**：
- `crates/relon-trace-emitter/src/call_conv.rs`（新，C 选项）
- `crates/relon-codegen-native/src/trace_inline.rs`（新，A 选项）
- `crates/relon-codegen-native/src/codegen.rs`（修改：在 Call site 试 inline trace）

**预期 bench delta**：9.5 ns → **~4-5 ns**（C 部分实施）→ **~1.5-2 ns**（A 完整实施，与 `rust_inlined_baseline 3.55 ns` 持平，因为 trace 优化器可以把 host context 寄存器分配也参与全局优化）

---

### v6-ε-M1：Bounds-check hoisting + LICM 扩展（预估 2 周）

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

**预期 bench delta**：~4 ns → ~3 ns（数组 hot loop）

---

### v6-ε-M2：Overflow / Trap hoisting via 类型范围跟踪（预估 2-3 周）

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

**预期 bench delta**：~3 ns → ~2.5 ns（带 i64 arith 的 hot loop）

---

### v6-ε-M3：Capability hoisting + invalidation cookie（预估 1-2 周）

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

**预期 bench delta**：~2.5 ns → ~2.2 ns（hot loop 含 cap 调用时）

---

### v6-ε-M4：Resource limit batching（预估 1-2 周）

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

**预期 bench delta**：~2.2 ns → ~1.7-1.8 ns（所有 hot loop）

---

## 4. 终局 bench 预期

| Phase | hot loop ns/iter | vs LuaJIT 1-3 ns/iter |
|---|---|---|
| v6-γ M5（const-only） | 4.39 | trace tier ✓ |
| v6-δ M1（真 add body） | 9.52 | **3-9× 慢** |
| v6-δ M2-C（IC dispatch — bench 未动） | 9.53 | 3-9× 慢 |
| **v6-ε-0（call boundary，C 选项）** | **~4-5** | 1.5-5× 慢 |
| **v6-ε-0（call boundary，A 完整）** | **~1.5-2** | **LuaJIT 同 class** ✓ |
| v6-ε-M1（bounds hoist） | ~3 | 1-3× 慢 |
| v6-ε-M2（overflow hoist） | ~2.5 | 0.8-2.5× 慢 |
| v6-ε-M3（cap hoist） | ~2.2 | trace tier same class ✓ |
| v6-ε-M4（resource batch） | **~1.7-1.8** | **0.6-1.8× — LuaJIT 同 class** ✓ |

**关键观察**：ε-0 选项 A 可能**单独就把 hot loop 拉到 LuaJIT 同 class**（~1.5-2 ns），因为：
- M2-C 已经把 trace body 优化得很彻底（`rust_inlined_baseline = 3.55 ns` 含全部 4-prong sandbox 税）
- A 选项让 trace body 完全 inline 进 host fn，host fn 是 cranelift-AOT 编译的极优代码
- 4-prong sandbox 检查实际在 hot loop 内已被 cranelift 后端 scheduler 优化（pipelined with arith ops）
- ε-M1 至 ε-M4 的边际收益（hoist 重复检查）在 inline 之后更小

**如果 ε-0-A 单独达标，ε-M1 至 ε-M4 可以转为低优先级（保留架构余量）**。

---

## 5. 风险登记

| 风险 | 等级 | 缓解 |
|---|---|---|
| ε-0-A code bloat（每个 call site inline trace 整体） | **HIGH** | 限制 trace size cap（≤ 256 ops）；超过 fallback 走 ε-0-C tail call |
| Hoist 证明 bug → sandbox bypass | **CRITICAL** | adversarial corpus + 1746 现有 + 52-case 4-way 全 0 mismatch；每个 phase 单独 review |
| Invalidation cookie 漏掉边界 case | HIGH | 每个 cookie 配 ≥ 3 adversarial 测试（修改 → trace 必须 deopt） |
| ε-0-A inline 破坏 deopt（resume_from_pc 找不到入口） | HIGH | inline trace 仍记录 IR-PC 范围，deopt 走 host fn 的 bytecode VM 路径 |
| Cranelift `CallConv::Tail` target 限制 | MEDIUM | x86_64 / aarch64 之外的 target 自动 fallback 走 standard call conv |
| 范围分析复杂度爆炸（ε-M2） | MEDIUM | 限制 abstract interp 深度；超过 fallback 不 hoist |
| Resource batch 在长 trace 里 timing 跑偏 | MEDIUM | MAX_TRACE_OPS_NO_POLL cap；trace 编译期估 N，超 cap 强插 poll |
| LuaJIT 1 ns 仍达不到（剩余开销在哪？） | LOW | 接受 "trace-tier same class"；1.5-2 ns 已经足够，绝大多数场景看不出差异 |

---

## 6. 启动时点 & 推进策略

**前置已满足**：v6-δ M2-A/B/C 全部落地（main HEAD `137c5ff`，1746 tests，0 parity 偏差）。

**推进策略**：
1. **先 ε-0-C（tail call，1-2 周）**——低成本试水，先把 6 ns → 4 ns 拿到
2. **再 ε-0-A（at-call-site inline，2-3 周）**——主力 phase，目标 4 ns → 1.5-2 ns
3. **ε-0 完成后实测 bench**——如果已达 ~1.8 ns 进 LuaJIT 同 class，ε-M1 至 ε-M4 转为低优先级（按用户/场景需求决定）
4. **如果 ε-0 后仍 >2.5 ns**——按 ε-M1 → M4 顺序推进，每 phase 实测 bench，达成即停

**期间不抢资源，不开早期 PR**。每个 phase 完成必做：merge worktree、cargo test --workspace 全绿、clippy/fmt/wasm32、清理 worktree、更新 stage report + plan + bench 附录。
