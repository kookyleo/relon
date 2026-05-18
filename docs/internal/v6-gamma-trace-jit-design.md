# v6-γ trace JIT 设计稿（LuaJIT-style hot loop tracing）

> 状态：**设计稿**（2026-05-18）。撰写时项目刚开 v5-β-1，距 v6-γ 落地至少 6+ 个月。本文档**提前**把架构 + 数据结构 + algorithm + deopt 协议落定，目的是让 β-1 / β-2 / γ 实施时给 trace JIT 留正确的 hook（最关键：IR Op 的 `is_idempotent` 标注必须在 β-1 时就齐备，否则 v6-γ trace recorder 无法决策可 trace 范围）。
>
> 上游：[`wasm-aot-v4-roadmap-sandbox-safe.md`](./wasm-aot-v4-roadmap-sandbox-safe.md) §v6-γ。
>
> 性能目标：hot loop op **sub-ns/op**（10⁵ 条 ETL 1.2 s → ~10 ms）。
>
> **最大风险**：guard 失败时 side effect 还原。本文档 §3 是设计的最关键部分。

## Section 1：总体架构

### 1.1 三层执行模型

```
┌────────────────────────────────────────────────────────────────┐
│  Layer C: trace-specialized code (γ 引入后)                    │
│     - 单 trace per hot path                                    │
│     - 类型 specialized, inlined stdlib                         │
│     - guard 失败 → deopt 跳到 Layer B 同 IR pc                 │
├────────────────────────────────────────────────────────────────┤
│  Layer B: cranelift-AOT generic code (β-1 / β-2 / γ all)       │
│     - 编译 IR → native code 一次                               │
│     - 每个 fn entry / loop back-edge emit hot counter          │
│     - counter 阈值触发 → 切到 Layer A 进入 recording           │
├────────────────────────────────────────────────────────────────┤
│  Layer A: tree-walk interpreter (始终在)                       │
│     - debugger / fallback / trace recording host               │
│     - trace recorder 借此通道驱动 IR op + 记录 trace           │
└────────────────────────────────────────────────────────────────┘
```

### 1.2 Hot detection（嵌入 generic code）

每个 fn entry + 每个 loop back-edge 处 cranelift codegen 时 emit：

```asm
; thread-local counter increment
lock incl %fs:relon_hotcounter_<site>@TPOFF(%rip)
; compare against threshold
cmpl $RELON_TRACE_THRESHOLD, %fs:relon_hotcounter_<site>@TPOFF(%rip)
jge relon_trace_trigger@PLT
; fall through to generic code
```

设计点：

- `relon_hotcounter_<site>` 是 thread-local u32，每 site 一个（site = fn entry id + back-edge id 全集）。
- 阈值 = 10（LuaJIT default，可调）。
- 触发后 jump 到 trampoline `relon_trace_trigger`：
  - 保存当前 register state 到 spill area
  - 调 Rust 端 `enter_trace_recording(site, args, spill)`
  - Rust 端切到 Layer A 接管执行
- counter overhead：单条 `lock incl + cmp + jge`，~3 ns/iteration（非 lock 时 ~0.5 ns，但多线程 false-sharing 风险）。
- **TODO（待 host 决策）：`lock incl` vs non-atomic increment？counter 不必精确（差几次没事），但 multi-thread 同时跑同一 fn 时 counter 增长更快不是坏事。建议 non-atomic（性能 priority），可能轻微提前触发，可接受。**

阈值触发后**立即停**继续增（写哨兵值 `u32::MAX` 表示"已 trace / 不再 trace"）。

### 1.3 Trace recording 流程

```
recorder 入口：
1. 切到 Layer A，从 Layer B 触发点对应 IR pc 开始解释执行
2. 创建空 TraceBuffer
3. 每解释一个 IR op：
   - 记录 op kind / inputs / output / 类型
   - 如果是 conditional branch，记 guard，记取 taken 分支
   - 如果是 stdlib call，记入参 / 出参 + 类型；如果是 hot stdlib 且 inline-able，递归展开
   - 如果遇到不可 trace 的 op（io, idempotent=no），ABORT trace
4. trace 终止条件：
   (a) 回到起始 IR pc（loop trace 闭合）
   (b) fn return（function trace 闭合）
   (c) buffer 长度超 max（默认 1024 op），ABORT
   (d) 异常 / trap，ABORT
5. ABORT 时丢 buffer，把 counter 设为 u32::MAX/2（避免立即重 trace，给些回退余地）
6. 成功 termination → 进入 Trace optimizer
```

### 1.4 Trace optimizer pass 列表

按顺序：

1. **Type specialization**：把 op 的"abstract type"（如 `Value`）换成已观察 concrete type（如 `Int`）；guard 失败处插类型 check。
2. **Constant folding**：trace 内 const fold（recorder 期间已收集 const values）。
3. **Load forwarding / store-to-load propagation**：跟踪同一 memory location 读写，跳过冗余 load。
4. **Dead store elimination**：guard 失败时 deopt state 需要的 store 不能删；其它纯局部 store 可删。
5. **Loop invariant code motion**（仅 loop trace）：把不依赖 loop var 的 op 提到 loop preheader。
6. **Strength reduction**：如 `mul %x, 8` → `shl %x, 3`，但 cranelift mid-end 已做，本 pass 多余 → 可跳。
7. **Register allocation hint**：给 cranelift 一组 register binding 偏好（hot var 优先 register）。

### 1.5 Trace compilation + install

优化后 trace IR → cranelift IR → native code（同 β-1 codegen 路径，但小函数粒度）→ install。

install 方式：

- 在 generic code 的 fn entry 处 patch 一条 `jmp trace_entry`，绕过 hot counter 路径。
- patch 用 atomic store 8 字节（确保 race-safe；x86_64 8-byte aligned store 是 atomic）。
- 进入 trace 后，guard 失败处 deopt 见 §3。

### 1.6 Trace 入口替换 hot fn dispatch table

如果 fn entry 是 trace 起点，简单 patch jmp。如果起点是 loop back-edge（loop-only trace），patch loop back-edge 的 cond jmp 跳到 trace entry。

更复杂的：side trace（trace 内某 guard 失败次数超阈值 → 在 guard 处启动 sub-trace）。**γ M5 之后才考虑**，M1-M4 仅做 root trace。

## Section 2：核心数据结构

```rust
/// Trace buffer 累积期间的 IR 流
pub struct TraceBuffer {
    /// Linear sequence of recorded ops (one entry per executed Relon IR op)
    pub ops: Vec<TraceOp>,

    /// Each guard site (type check, branch fork, bounds check)
    pub guards: Vec<GuardSite>,

    /// SSA var (u32 dense id) -> observed concrete type
    pub type_info: HashMap<u32, IrType>,

    /// Constants encountered during recording, keyed by SSA var id
    pub consts: HashMap<u32, TraceConst>,

    /// Entry IR pc (where the trace starts); used for loop closure detection
    pub entry_pc: IrPc,

    /// Loop detection: if any op's ir_pc == entry_pc, mark as loop trace
    pub kind: TraceKind,

    /// Max ssa id allocated so far
    pub next_ssa: u32,
}

pub enum TraceKind {
    Function,  // 始于 fn entry，终于 fn return
    Loop,      // 始于 loop back-edge，回到自己
    Side,      // γ M5+：从 guard 失败处起，回到主 trace
}

pub struct TraceOp {
    pub kind: TraceOpKind,
    /// Inputs (refer to other SSA ids in this trace; max 3 since most ops are unary/binary/ternary)
    pub inputs: SmallVec<[u32; 3]>,
    /// Output SSA id (if any; some ops are void)
    pub output: Option<u32>,
    /// Source IR pc this op was lifted from (used for deopt)
    pub ir_pc: IrPc,
    /// Side-effect classification (must match IR op's is_idempotent attr)
    pub effect: EffectClass,
}

pub enum EffectClass {
    /// 纯函数 op，无 side effect，可任意 reorder / dedup（abs, add, load, ...）
    Pure,
    /// 读 mutable state，不写（loop counter read, env load）
    ReadOnly,
    /// 写 state，但可由 deopt_state 完全还原（local var write, scratch arena write）
    RecoverableWrite,
    /// 写 state 且不可还原（IO, host fn with state, network）—— trace 必须 ABORT
    UnrecoverableEffect,
}

pub struct GuardSite {
    /// trace 内的 pc（ops index）
    pub trace_pc: u32,

    /// failure 时跳回的 generic code IR pc
    pub deopt_pc: IrPc,

    /// 失败时需要还原的 SSA values 列表
    pub deopt_state: DeoptState,

    /// guard 类型：类型 check / cond branch fork / bounds check / etc.
    pub kind: GuardKind,
}

pub struct DeoptState {
    /// 把 trace SSA id 映射到 generic code 的 IR-level Value slot
    pub bindings: SmallVec<[(u32 /* ssa */, IrSlot), 8]>,

    /// 此 guard 之前已经被 trace 折优化掉但 deopt 时必须重建的 IR ops
    /// (例如 trace 内 dead-store-eliminated 的 write，guard 失败时仍需要 re-execute 给 generic code 看)
    pub side_effects_to_replay: SmallVec<[ReplayOp; 2]>,
}

pub enum GuardKind {
    TypeCheck { expected: IrType },
    BranchTaken { cond_value: bool },
    BoundsCheck { len: u32 },
    OverflowCheck,
    DeadlineCheck,  // 复用 sandbox deadline guard，trace 失败 → deopt 而不是 trap
}
```

`u32` SSA ids 而非 `Box<Trace>` 引用：

- cache friendly（紧凑数组遍历）。
- 优化 pass 改写 inputs 时不用动指针。
- LuaJIT 也用 32-bit ref。

## Section 3：Deopt 协议（**正确性死穴**）

这是 v6-γ 设计最关键的一节。任何 bug → trace 出错误结果 / 进程 panic / 状态错乱。

### 3.1 Deopt 触发时机

trace 内 guard 失败时（任一）：

1. 类型 guard 失败（trace 假设 `x: Int`，但实际 `x: Float`）
2. 分支 guard 失败（trace 录的是 taken，运行时 not taken）
3. bounds guard 失败（trace 假设 `idx < len`，运行时不成立）
4. overflow guard 失败（trace 假设 `+ 不溢`，运行时溢）
5. deadline guard 失败（trace 内 deadline exceeded）

注意：**bounds / overflow / deadline 即便在 generic code 里也是 trap**。trace 的特别处是：trace 把这些 trap 转成 deopt 给 generic code，generic code 再走 trap 路径。

### 3.2 Deopt trampoline 流程

```asm
; trace code 内 guard fail：
guard_fail_block:
    ; 保存 trace 内 live SSA registers 到 spill area
    movq %rbx, deopt_spill+0(%rip)
    movq %r12, deopt_spill+8(%rip)
    ; ... 视实际 register binding
    ; 调 Rust 端 deopt_dispatch(trace_id, guard_id, spill_ptr)
    leaq trace_id_<n>(%rip), %rdi
    movl $<guard_id>, %esi
    leaq deopt_spill(%rip), %rdx
    call relon_deopt_dispatch@PLT
    ; deopt_dispatch 不返回 trace（longjmp 走 generic code）
```

Rust 端 `relon_deopt_dispatch`：

```rust
pub extern "C" fn relon_deopt_dispatch(
    trace_id: TraceId,
    guard_id: u32,
    spill: *const u8,
) -> ! {
    let trace = TRACE_TABLE.get(trace_id);
    let guard = &trace.guards[guard_id as usize];

    // 1. 从 spill area 读 trace SSA values
    let ssa_values = read_ssa_from_spill(spill, &trace.live_at_guard[guard_id as usize]);

    // 2. Replay side effects that were optimized away
    for replay in &guard.deopt_state.side_effects_to_replay {
        execute_replay(replay, &ssa_values);
    }

    // 3. 把 SSA values 映射回 generic code 的 IR slot
    let mut generic_state = current_generic_state();
    for (ssa_id, ir_slot) in &guard.deopt_state.bindings {
        generic_state.write_slot(ir_slot, ssa_values[ssa_id]);
    }

    // 4. siglongjmp 到 generic code 同 ir_pc 入口
    relon_runtime::resume_generic_at(guard.deopt_pc, generic_state);
    // 不返回
}
```

### 3.3 Side effect 还原（最大风险）

trace optimizer 会**消除 / reorder** trace 内某些 op。如果这些 op 有 side effect，guard 失败时必须**恢复**对应 state 到"guard 失败前应该的样子"。

Relon 的优势：**绝大多数 IR op 是纯函数式**。但需要 deopt 还原的 op 类别：

1. **`StoreField`**（schema instance 字段写）—— 写后 reorder 到 guard 之后？**不允许**。trace optimizer 的 store-to-load forwarding 必须保证 store 不跨 guard 移动。
2. **`Call(stdlib_with_state)`** —— 如 list_append 修改 list 长度，map 写新数据到 arena cursor。Arena cursor 是关键 state。
3. **Scratch arena cursor mutation** —— 每次 `concat` / `substring` / `upper` 把 cursor 推进。Trace 内可能 batch 多次推进，guard 失败时若 trace 已 batch 推进但 generic code 期望逐步推进 → 错位。

**Mitigation 设计原则**（**β-1 / β-2 / γ 期间 IR 设计就执行**）：

#### 红线 1：每个 IR Op 必须标 `is_idempotent`

`crates/relon-ir/src/ir.rs` 中每个 `Op` variant 必须有 doc-comment 标：

```rust
pub enum Op {
    /// is_idempotent: yes
    /// effect: Pure
    Add(Value, Value),

    /// is_idempotent: no（推进 arena cursor）
    /// effect: RecoverableWrite (arena cursor 可由 deopt_state 中 saved cursor 还原)
    Concat(Value, Value),

    /// is_idempotent: no
    /// effect: UnrecoverableEffect（用户 host fn 调用，外部 state）
    HostCall(HostFnId, Vec<Value>),

    // ...
}
```

#### 红线 2：trace recorder 只接受 `Pure` + `ReadOnly` + `RecoverableWrite` 的 op chain

任一 `UnrecoverableEffect` 出现 → 立刻 ABORT trace。等同 LuaJIT 的 NYI（"not yet implemented" trace abort）机制。

#### 红线 3：`RecoverableWrite` op 必须把所需 deopt state 收集进 `GuardSite::deopt_state.side_effects_to_replay`

例如 `Concat`：

- recorder 期间记录 arena cursor 的 before 值进 deopt_state。
- guard 失败时把 cursor 还原到 before 值。
- generic code 重新 execute 该 IR op 时，会重新从 before 推进 cursor。

#### 红线 4：trace optimizer 跨 guard reorder/eliminate 必须 conservative

- **不允许**消除 `RecoverableWrite` op（即便其后 store 覆盖）：因为 guard 在中间失败时，generic code 期望该 store 已发生。
- **不允许**把 `RecoverableWrite` 移到 guard 之前（速度上诱人，但 deopt 时 generic code 重 execute 会重复 effect）。
- 安全 reorder：纯 op 任意；ReadOnly op 可跨 guard 移动（但不可跨 store 移动）。

### 3.4 Deopt 测试矩阵

γ M4 关键里程碑（"guard 失败 deopt 路径打通"）必须覆盖：

1. 类型 guard：trace 假设 `Int`，运行时给 `Float`。
2. 分支 guard：trace 录 `if x > 0` taken 分支，运行时给负值。
3. Loop trace exit：trace 假设 loop 回起点，运行时提前 break。
4. Concat 中途 deopt：concat 已写半串到 arena，guard 失败时 cursor 还原，generic code 重 execute concat。
5. List map 中途 deopt：map 第 K 元素 closure 失败 type guard，已 map 的 K-1 个元素丢弃（输出 list 在 arena 中尚未 commit），cursor 还原，generic code 从第 0 元素重做。
6. Deadline guard：trace 内 deadline exceeded，deopt 后 generic code 走 trap → host RuntimeError。
7. Nested call deopt：trace 内 inline 一个 stdlib fn，stdlib 内 guard 失败 → 还原到调用 site 的 generic code。

每条 test 验证：deopt 完成后 generic code 输出 == tree-walk 输出。

## Section 4：Differential test harness

**起点：β-2 时建 `crates/relon-test-harness/` 雏形**（不是 γ 才开始，否则 v5 期间 stdlib 移植已无可信验证）。

### 4.1 Crate 结构

```
crates/relon-test-harness/
├── Cargo.toml          # depend on relon-eval (tree-walk), relon-codegen-native, [v6-γ] relon-trace-jit
├── src/
│   ├── lib.rs          # pub diff_test(source, args) -> Result<()>
│   ├── corpus.rs       # 内置 corpus 加载
│   └── backends.rs     # 各 backend 启动 + run_main 包装
└── tests/
    ├── arith.rs
    ├── stdlib_*.rs
    ├── normalization.rs
    ├── closure.rs
    ├── remote_import.rs
    └── trace_jit.rs    # γ M3 后启用
```

### 4.2 `diff_test` 函数签名

```rust
pub fn diff_test(source: &str, args: &[Value]) -> Result<(), DiffTestError> {
    let out_tw = run_tree_walk(source, args)?;
    let out_aot = run_cranelift_aot(source, args)?;
    if out_tw != out_aot {
        return Err(DiffTestError::Mismatch { tw: out_tw, aot: out_aot });
    }

    // γ M3+：
    #[cfg(feature = "trace-jit")]
    {
        let out_trace = run_trace_jit(source, args)?;
        if out_trace != out_tw {
            return Err(DiffTestError::TraceMismatch { tw: out_tw, trace: out_trace });
        }
    }

    Ok(())
}
```

bit-identical 比较：

- `Value::Int(_)` 直接 eq
- `Value::Float(_)` 用 `to_bits()` eq（NaN bit 不变）
- `Value::String(_)` byte eq
- `Value::List(_)` 递归
- `Value::Schema(_)` 字段排序后递归
- 抛 trap：`RuntimeError` kind + 字段（不比 message string，因为 message 可能 backend-specific）

### 4.3 Corpus 规划

Corpus 总目标：v6-γ M5 完工时**至少**覆盖：

| 类别 | 用例数 | 覆盖点 |
|---|---:|---|
| 基础算术 / cmp / 控制流 | 100+ | int add/sub/mul/div、float ops、bool 逻辑、if/else、while loop、break/continue |
| stdlib `length` / `is_empty` / `min` / `max` / `abs` | 30+ | ASCII / UTF-8 / 边界（空串、`i64::MIN`） |
| stdlib `concat` / `substring` / `starts_with` | 40+ | 空串拼、UTF-8 边界 substring、prefix match |
| stdlib `upper` / `lower` / `title` | 80+ | ASCII / Latin / Cyrillic / Greek（含 σ final-sigma）/ CJK / 全角 / Turkish ı / German ß / 含 combining mark |
| stdlib `upper_locale` / `lower_locale` / `title_locale` | 30+ | `tr` / `az` / `en` / `de` / `xx` locale 分支 |
| stdlib list 简单（`list_int_sum` / `list_int_max` / `list_*_length`） | 30+ | 空 list、单元素、大 list（10⁴+） |
| stdlib `list_int_map` / `filter` / `fold` | 40+ | 简单 closure、闭包 capture、嵌套 map、空 list、closure 内 trap |
| Unicode normalization NFD / NFKD / NFC / NFKC | 200+ | W3C 标准 conformance suite + Hangul + 自定义边界 |
| 闭包 / higher-order | 30+ | curry、recursive closure、cross-fn capture、deep nesting |
| 远程 import 跨文件 | 20+ | single import、transitive、versioned import |
| schema 操作（CRUD-like） | 50+ | field load/store、nested schema、list-of-schema |
| **Total** | **650+** | |

每用例: 一组 `(source, args, expected_output_or_trap)`。

### 4.4 CI integration

- main 上每次 PR 跑 corpus 100% pass。
- nightly 跑扩展 stress corpus（同一 source 用 random args 1000 次重 run，验稳定性 + 性能 regression）。
- v6-γ trace JIT phase：每个 commit 必须跑 corpus**带 trace JIT**通过；任一 mismatch block merge。

## Section 5：启动 milestone（v6-γ 8-12 周）

### M1（γ 第 1-2 周）：Hot detection + recorder shell

任务：

1. cranelift codegen emit hot counter at fn entry + back-edge。
2. 阈值触发 trampoline → 调 Rust `enter_trace_recording`，**recorder 不记任何东西**，只验机制通：log 一行 "trace started at site=N"，立刻 ABORT。
3. counter overhead bench：与 v5-β-2 同 program 跑同 corpus，cranelift native 性能下降 ≤ 5%（counter 增量预算）。

完工：

- 跑 corpus 100% pass（trace JIT 还没真用，所以应没 regression）。
- log 输出能看到 hot 点（可视化辅助 debug）。

### M2（γ 第 3-4 周）：Trace recorder 记简单 arith chain

任务：

1. 实现 `TraceBuffer` + `TraceOp::{Add,Sub,Mul,Div,Const,Load,Store,Branch,Return}`（其它 op 全走 ABORT）。
2. recorder 跑 arith-only hot 程序：记 op chain 到 buffer，遇 unsupported op ABORT。
3. **不做** optimize，**不做** codegen，**不 install** trace。仅验证 trace shape 正确（用 unit test 比对 expected trace 文本表示）。

完工：

- arith corpus 50+ 用例 trace 出来跟 expected match。
- buffer 长度 / guard 数量等统计正确。

### M3（γ 第 5-6 周）：Optimizer + cranelift IR 输出 + install

任务：

1. 实现 type specialization + const fold + load forwarding pass。
2. trace IR → cranelift IR（沿用 v5-β-1 lowering 基础）。
3. install trace（atomic patch fn entry jmp）。
4. **guard 失败暂时全 abort 进程**（M4 才正确实现 deopt）—— 仅用 corpus 中保证 type 稳定的用例测试 trace 进入 + 退出走 trace return。
5. 跑 arith corpus 50+：trace 跑出的结果 == tree-walk 结果。

完工：

- 跑 simple loop bench：trace 命中后 hot loop op 时间下降 ≥ 30%（不到 sub-ns/op，因为还没有 LICM + 完整 type spec）。

### M4（γ 第 7-8 周）：Deopt 路径打通（**最关键里程碑**）

任务：

1. 实现 `relon_deopt_dispatch` + spill area + register restore。
2. 类型 guard / 分支 guard / bounds guard 三类先打通；overflow / deadline 跟上。
3. Side effect 还原：实现 `RecoverableWrite` op 的 deopt_state 收集 + replay。
4. Deopt 测试矩阵 §3.4 全 7 项通过。
5. 跑 corpus 100% 通过（带 trace JIT），任一 mismatch block。

完工：

- corpus 100% pass。
- deopt 频次 / trace stability 等统计可观测。
- 此 milestone 之前 trace JIT 仍可视为"实验性"，M4 之后才有正确性保证。

### M5（γ 第 9-12 周）：扩 trace 范围

任务：

1. 支持 trace 内 loop（非 fn-entry trace）。
2. 支持 stdlib call inlining：trace 内调用 `upper` / `length` 等纯 stdlib 时 inline 展开（小函数）。
3. 支持 closure call inline（`list_int_map` 内的 closure body 直接展开进 trace）。
4. Loop invariant code motion pass。
5. Side trace（M5 末尾，optional）。
6. bench 数据出炉：hot loop op 时间，目标 ~1-5 ns/op（对齐 LuaJIT trace tier）。

完工：

- ETL stream bench：1M 条记录 transform 端到端 ≤ 100 ms。
- 公布 [`v6-trace-jit-bench-report-<date>.md`](./)。

## Section 6：风险清单（design-level）

1. **side effect 还原 bug** —— §3 详述。Mitigation：IR Op 必须有 `is_idempotent` 标注，trace 只录 idempotent chain，optimizer 不消除 RecoverableWrite。
2. **deopt state 大小爆炸** —— deopt_state 含所有 guard 处 live SSA。Long trace 数百个 guard，每个 8 个 binding，总几 KB / trace。Mitigation：限制 trace 长度 1024 op；只在 type / branch guard 处保 deopt state，bounds / overflow guard 复用之前已保的（共享 nearest preceding guard 的 state）。
3. **patch race** —— install trace 时 atomic patch fn entry，多线程同时跑该 fn 时 race。Mitigation：用 atomic 8-byte store（x86_64 / aarch64 都支持）；race window 内某线程跑老 generic code 也无害（只是少加速）。
4. **trace explosion** —— polymorphic call site 每次类型变化 → trace abort → 重 trace → 再 abort。Mitigation：单 site abort 计数到 N（如 5）后停 trace（永久标记 cold）。
5. **OOM in trace buffer** —— 复杂程序 1024 op 不够。Mitigation：超长 trace ABORT，回 generic code 走完。
6. **interaction with γ object cache** —— trace 是 per-process 内存中生成，**不写** native cache（trace 依赖 type profile，跨 process 不复用）。但 generic code 走 cache。需保证 cache 加载的 generic code 内 hot counter 仍工作（cache 文件包含 counter slot 但 zero-init）。
7. **interaction with sandbox** —— trace code 仍跑在 sandbox 内（bounds check / trap / capability vtable / deadline），所有 sandbox 指令在 trace codegen 时一律 emit（不可裁剪）。
8. **debug 困难** —— trace 失败时 PC 在 trace code 中，与 source IR pc 关联弱。Mitigation：trace 内每 op emit source IR pc 元信息进 srcmap section（β-2 应已建好 cranelift srcmap framework）。

## Section 7：与 v5-β / γ 的接口要求

### 7.1 v5-β-1 必须给的 hook

- IR Op 全集每个 variant 有 `is_idempotent: yes/no` 标注（doc-comment 即可，**β-1 时就要齐**）。
- cranelift codegen 暴露"hot counter slot emit" 接口（β-1 可先 stub，γ 启用）。
- thread-local spill area + jump buffer infrastructure（β-1 sandbox 已经有 jump buffer，γ 扩展）。

### 7.2 v5-β-2 必须给的 hook

- `crates/relon-test-harness/` 起架（§4），corpus 起头至少 100+ 用例。
- IR Op effect class（Pure / ReadOnly / RecoverableWrite / Unrecoverable）正式 enum，不再是 doc-comment（β-2 升级）。
- 每个 stdlib body 标 effect class（β-2 stdlib re-lower 时一并加）。

### 7.3 v5-γ 必须给的 hook

- `cranelift-object` patch 路径需支持 install trace 时 atomic patch jmp 到 in-memory trace code（trace code 是 JIT mmap，不走 object cache，但 generic code 走 cache）。
- cache 元数据加 `trace_jit_compatible: bool`（γ 输出 = true；不支持 trace 的旧版本 = false）。

## 附录 A：术语对照（LuaJIT vs Relon）

| LuaJIT | Relon v6-γ |
|---|---|
| trace | trace |
| recorder | trace recorder |
| IR (LuaJIT IR) | trace IR（数据结构 §2） |
| optimizer (FOLD / NARROW / CSE / DSE / LOOP) | trace optimizer (§1.4 list) |
| mcode (machine code) | cranelift native code |
| snapshot | DeoptState |
| exit / side exit | guard fail → deopt |
| NYI bytecode | UnrecoverableEffect op → ABORT |
| GG_State / global_State | TraceTable + per-thread state |
| trace linking | M5 side trace |
| hotcount | hot counter (§1.2) |

---

**作者**：Relon perf 直路并行 prep 设计稿撰稿 agent
**日期**：2026-05-18
**License**：Apache-2
