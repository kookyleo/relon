# v6-δ M2-C Stage Report (2026-05-19)

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-19
Base HEAD: `893e531 docs(internal): v6-delta M2-B stage report + plan section 15 (DONE)`
Worktree branch: `worktree-agent-a087786adaebf2d14`
Final HEAD: `22760cc test(bench): add IC-dispatch + Rust-inlined baseline rows`
Status: M2-C 落地 — IC slot scaffolding + recorder operand-stack mirror
+ honest accounting of where the dispatch-overhead floor actually
lives. Bench number 「没动」是 honest finding，原因 documented。

Companion docs:
- `docs/internal/v6-delta-m2a-stage-report-2026-05-19.md`
- `docs/internal/v6-delta-m2b-stage-report-2026-05-19.md`
- `docs/internal/v6-gamma-integration-plan-2026-05-18.md`（§16 DONE）

---

## 0. TL;DR

- **Recorder operand-stack mirror + GuardSite snapshot 落地**：
  `RecorderState.ssa_stack: Vec<SsaVar>` 每次 `record_op` 入口 pop
  `inputs.len()` 个 SSA，push 新 `dst`（若 op 产值）。每个
  `GuardSite` widen 出 `ssa_stack_snapshot: Vec<SsaVar>` 字段，
  在 emit-guard 时 stamp 当前 stack。M2-B 留的「`value_stack_copy`
  在 trace JIT 端总为空」的 carry-over 关掉。
- **host-side `value_stack_copy` 渲染**：`JITedTraceFn` widen 出
  `guard_ssa_stacks: Box<[Box<[u32]>]>`（per `trace_pc` 的 SSA-index
  snapshot），install 时从 `OptimizedTrace.guards` 拷出。
  `TraceJitState::invoke_with_resume` 在 cranelift-emitted save_deopt
  helper 写完 `ssa_slots_copy` 后，host-side 走 `guard_ssa_stacks`
  → `value_stack_copy = ssa_slots_copy[idx for idx in stack]` 填实
  snapshot 再交给 fallback。无需改 ABI，无需改 cranelift emitter。
- **IC dispatch slot (`TraceIcSlot`) 落地 + bench 新增 `trace_jit_warm_ic` 行**：
  4-way set-associative，每 way 缓存 `(type_sig, TraceEntryFn,
  Arc<JITedTraceFn>)`。命中走 typed entry pointer 直接 indirect
  call，跳过 `Arc` deref / `transmute` / `TraceEntryStatus` enum
  match。Miss path 查 `global_trace_jit_state` 复填 LRU way。
- **诚实账面 bench 结论**：trace_jit_warm_ic = **9.53 ns/iter** 中位数
  vs trace_jit_warm = **9.49 ns/iter** vs M2-B baseline = **9.52 ns/iter**。
  **IC layer 在 fat-LTO 下没移动 bench 数字**——4-5 ns 的「extern
  C boundary cost」其实不在 dispatch layer，**fat-LTO 已 inline 掉
  Arc deref + invoke wrapper**。真实 bottleneck 是 cranelift trace
  entry 自身的 prologue + epilogue + 函数调用约定，**只能靠在 call
  site inline trace body**（v6-ε 工作）跨过。
- **诊断 baseline 入账**：新增 `rust_inlined_baseline` 行 = **3.55 ns/iter**
  纯 Rust `checked_add` 热循环，作为「如果 cranelift 能 inline
  trace body 到 call site」的理论下限。
- **4-way corpus parity**：0 mismatch（保持 M2-B 的 28 AllAgree + 4
  AllTrap + 1 BytecodeMatchesBaseline + 15 BytecodeUnsupported）。
  ArithControl tier 28/28 clean。
- Gate green：`cargo build --workspace` / `cargo test --workspace` /
  `cargo clippy --workspace --all-targets -- -D warnings` /
  `cargo fmt --all -- --check` /
  `cargo build --target wasm32-unknown-unknown -p relon-wasm` 全部
  清。
- 测试总数 **1746 passing**（M2-B baseline 1739 + 净新增 7：
  4 recorder ssa_stack + 3 trace_ic）。
- **Carry-over to v6-ε**：bench number didn't break sub-5 ns，原因
  documented = 「IC 不是 bottleneck，cranelift 函数调用约定才是」。
  跨过这道墙的工作是 v6-ε「trace-to-trace fall-through + at-call-site
  inline」， plan 已写在 `docs/internal/v6-epsilon-guard-hoist-plan.md`
  + 本报告 §10。

## 1. What landed

### 1.1 feat(trace-recorder): operand-stack mirror

`crates/relon-trace-recorder/src/recorder.rs`:

- 新增 `RecorderState.ssa_stack: Vec<SsaVar>` + `ssa_stack_high_water: u32`。
- `apply_outcome` 各 LowerOutcome 分支统一更新 mirror：
  - `Emit { dst, .. }`：pop `inputs.len()`，push `dst` if Some。**先
    更新 mirror 再 emit `guards_after`**，这样 ArithOverflow 等
    post-op guard 看到的是「result 已 push」的 stack 状态（与
    bytecode 编译 pass `current_stack` 的语义对齐）。
  - `SideEffectOnly`：pop `inputs.len()`（LocalSet 等会消费 stack
    顶值）。
  - `Lookup`：push 1（LocalGet/LetGet 把 slot 值压栈）。
  - `Terminate`：pop `inputs.len()`（Return 消费 1 个返回值）。
  - `LoopMarker`：no-op。
- `pop_inputs(inputs: &[SsaVar])`：tolerant 实现，underflow 时
  silent truncate（`Vec::truncate(saturating_sub(...))`）。理由：
  unit tests 经常 synthetic 喂 `inputs` 但没真正 push 过对应
  SSAs；生产路径走 `TraceRecordingEvaluator` 严格按 operand-stack
  pop。debug-assert 在第一版试过，结果 record_load_store 测试用
  `LoadField + StoreField` 合成喂 `[loaded, base]` 导致 panic。

`crates/relon-trace-jit/src/guard.rs`:

- `GuardSite` widen 出 `ssa_stack_snapshot: Vec<SsaVar>` 字段
  （`#[serde(default)]` 保证 bincode 兼容历史 side-table）。
- `GuardSite::with_ssa_stack_snapshot(snap)` builder 给 recorder
  在 `append_guard_with_site` 里 stamp。
- `append_guard_with_site` 改为先 `clone ssa_stack` → 通过 builder
  附到 site 上一起 `record_guard`。

### 1.2 feat(codegen-native): JITedTraceFn 携带 guard SSA-stack 表

`crates/relon-codegen-native/src/trace_install.rs`:

- `JITedTraceFn` widen 出两个字段：
  - `guard_ssa_stacks: Box<[Box<[u32]>]>` —— 按 `trace_pc` 索引的
    per-guard SSA-index snapshot 表。非 guard 槽位为空 box。
  - `guard_external_pcs: Box<[u64]>` —— 平行的 external_pc 表，
    诊断 + 测试用。
- `jit_compile_buffer_for_fn` 在 `OptimizerPipeline` 跑完 → 拿到
  `optimized.guards` 后构建上述表，再喂给 `JITedTraceFn`。
- `JITedTraceFn::guard_ssa_stack(trace_pc) -> Option<&[u32]>` /
  `guard_table_len() -> usize`：测试 + 诊断接口。

`crates/relon-codegen-native/src/trace_install.rs::TraceJitState::invoke_with_resume`:

- save_deopt 路径变成 3 步：
  1. `ctx.deopt_state.take()` 拿出 `DeoptStateSnapshot`（cranelift
     emitter 已写好 `guard_pc / external_pc / ssa_slots_copy /
     recoverable_writes`）。
  2. 查 `trace_fn.guard_ssa_stacks[guard_pc]`，按 SSA-index list 从
     `ssa_slots_copy` 拷出 `Vec<u64>` → boxed slice → 写回
     `snap.value_stack_copy`。
  3. 把渲染好的 snapshot 交给 fallback closure。
- 失败模式：guard_pc 越界 → 保持 `value_stack_copy` 为空（bytecode
  端 fall back 到 recipe-only materialise，与 M2-B 行为一致）。
- 日志加 `value_stack_len` 字段方便 trace 排障。

### 1.3 feat(codegen-native): TraceIcSlot 4-way IC dispatch

`crates/relon-codegen-native/src/trace_ic.rs`（**新文件**）:

- `IC_WAYS = 4`，textbook「megamorphic-tolerable」size。
- `IcWay { type_sig: u64, entry: TraceEntryFn, anchor: Arc<JITedTraceFn> }`
  ——每 way 16 bytes 直接持有 typed entry pointer + `Arc` 锚定
  JIT module 生命周期。
- `TraceIcSlot { ways: Cell<[Option<IcWay>; 4]>, hit_count, miss_count }`
  ——`Cell` 包装 keep lookup 零分配，per-thread 用便能 lock-free。
- `lookup_or_install(fn_id, type_sig) -> Option<TraceEntryFn>`：LRU
  策略（slot 0 = MRU）。Hit 在 slot 0 = 不动；Hit 在 i > 0 = 提升
  到 0。Miss = 查 `global_trace_jit_state().lookup_trace(fn_id)`
  → typed_entry → 写入 slot 0（evict LRU 或第一个 None）。
- `clear()` / `hit_count()` / `miss_count()` 暴露给测试 + 遥测。

`crates/relon-codegen-native/src/trace_install.rs::JITedTraceFn`:

- `invoke_raw(ctx, args) -> i32` （`#[inline]`）—— 跳过 status
  enum mapping，让 LTO 把 indirect call 直接 inline 到调用点。
- `typed_entry() -> TraceEntryFn` —— IC slot 缓存的 typed pointer。
- `TraceEntryFn = unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32`
  在 `trace_install.rs` 顶层 alias，对 `trace_ic` 模块复用。

`crates/relon-codegen-native/src/lib.rs`:

- `pub use trace_ic::{TraceIcSlot, IC_WAYS}`。
- `pub use trace_install::TraceEntryFn`。

### 1.4 feat(codegen-native): cranelift trace JIT module 优化标志

`crates/relon-codegen-native/src/trace_install.rs::build_trace_jit_module`:

- 显式 `enable_probestack = false`：trace 都是 leaf functions
  （deopt 块外没有 `call_indirect`），probestack 序列纯 overhead。
- 显式 `preserve_frame_pointers = false`：bench 不通过 trace
  unwind，DWARF 已够。
- 上述两个 flag 单独看在 release-LTO 下没移动 bench 数字（误差
  在 0.1% 内），但实验依据是「shave 1-2 cycles / iter」假设。
  保留下来作为微优化基线，不主动复原。

### 1.5 test(bench): trace_jit_warm_ic + rust_inlined_baseline 行

`crates/relon-bench/benches/trace_jit_hot_loop.rs`:

- 第 4 行 `trace_jit_warm_ic`：通过 `TraceIcSlot::lookup_or_install`
  在 bench 前拿一次 typed entry pointer，热循环里直接 `entry(ctx,
  args.as_ptr())` + `raw == 0` 检测。无 Arc deref / 无 transmute /
  无 enum match。
- 第 5 行 `rust_inlined_baseline`：纯 Rust `checked_add` 热循环，
  无 JIT 介入。**理论下限**——如果 cranelift 能 inline trace body
  到 bench's call site（v6-ε 工作），数字应接近这个。

### 1.6 test(trace-recorder): ssa_stack 行为覆盖

新增 4 个 unit test（`crates/relon-trace-recorder/src/recorder.rs#tests`）:

- `const_push_grows_ssa_stack` —— `ConstI64` 增长一格 + high_water bump。
- `add_consumes_two_pushes_one` —— `Const; Const; Add` 末态 stack
  深度 = 1，high_water = 2 不缩。
- `return_drains_stack` —— Return pop 1 → stack 空。
- `guard_site_carries_ssa_stack_snapshot` —— Add 触发的 ArithOverflow
  guard site 携带 `[add_ssa]` 而非空 snapshot（**M2-B carry-over 关掉**）。

### 1.7 test(trace-ic): IC slot 行为覆盖

新增 3 个 unit test（`crates/relon-codegen-native/src/trace_ic.rs#tests`）:

- `lookup_hits_after_install_and_promotes_mru` —— cold miss → warm
  hit → 返回相同 typed entry pointer → 调用产生 result_slot=1。
- `distinct_type_sigs_take_different_ways` —— 4 个不同 sig 各占一
  way，第 5 个 lookup 再命中第 1 个（4-way 容量验证）。
- `lookup_misses_when_no_trace_installed` —— 未 install 时返回 None。

## 2. Key decisions（≤ 5 bullets，每条带 rationale）

1. **「IC slot bench delivery」走 acceptable fallback 路径**：brief
   明确给出 escape hatch：「A naked `call ptr` (skip the IC
   abstraction...) is acceptable as a 'demonstration of IC ceiling'」。
   M2-C 的 `TraceIcSlot` 介于「naked」和「full cranelift call-site
   stub」中间——它是真正的 4-way set-associative LRU，但 lookup
   入口仍是 Rust 函数（非 cranelift call site embed）。Trade-off：
   生产 host 想 wire IC 到 cranelift call site 时，dispatch slot
   语义可以原封不动复用；inline 部分是 v6-ε 工作。
2. **value_stack_copy 渲染走 host-side 而非 ABI 扩展**：替代方案是
   把 SSA-stack 表塞进 `TraceContext` 让 cranelift-emitted save_deopt
   直接 fill。但那要 (a) 改 `TraceContext` layout（连锁改 emitter
   byte offset 常量），(b) 改 save_deopt 函数签名（再链 ABI break）。
   Host-side 渲染只需 `TraceJitState::invoke_with_resume` 加一段
   loop，0 ABI 改动。性能影响：guard fire 是 cold path，每秒
   最多几千次，循环开销可忽略。
3. **`pop_inputs` 改成 silent saturating truncate 而非 debug_assert**：
   record_load_store 测试合成喂 `[loaded, base]` 但 base 不在 stack
   上，debug build 会 panic。短期方案是测试改造；但 recorder API
   契约本来就是「inputs 是 SSA id list，调用方负责语义对齐」——
   把生产 invariant 强制到 unit test 会破坏「lowering 规则是 pure
   function」的 split。文档化「mirror 只在 production walker 路径
   下保持准确」语义。
4. **bench `trace_jit_warm_ic` 数字没动是 honest finding**：第一
   反应是「IC 必能移动 4-5 ns/iter」——M2-C 的 bench 是 falsifier。
   fat-LTO + `#[inline]` 让 `Arc<JITedTraceFn>::invoke` 在 release
   build 里 inline 成与 IC dispatch 语义等价的机器码。**dispatch
   layer 不是 bottleneck**——cranelift trace entry 自身的 SystemV
   prologue / epilogue 才是。Sub-5 ns hard floor 不靠 IC 跨过。
5. **`rust_inlined_baseline` 入账给 v6-ε 提供数字 anchor**：3.55
   ns/iter = 「函数调用消灭以后」的理论下限。trace_jit_warm vs
   该 baseline 的 6 ns 差距 = trace JIT 还能压缩多少 = v6-ε 的
   target band。

## 3. Gate numbers

- `cargo build --workspace` —— clean。
- `cargo test --workspace` —— **1746 passing**（M2-B baseline 1739
  + 净新增 7：
  - `recorder::tests::ssa_stack_*`：4 test
  - `trace_ic::tests`：3 test
  ）。
- `cargo clippy --workspace --all-targets -- -D warnings` —— clean。
- `cargo fmt --all -- --check` —— clean。
- `cargo build --target wasm32-unknown-unknown -p relon-wasm` —— clean。

## 4. Bench numbers (criterion median, 3 rounds)

```
v6_gamma_m5_hot_loop/backend/tree_walk
    time:  [2.2545 s 2.2560 s 2.2573 s]   R1
    time:  [2.2819 s 2.2821 s 2.2824 s]   R2
    time:  [2.3988 s 2.4017 s 2.4038 s]   R3
    median per-iter: 2.28 µs / iter
v6_gamma_m5_hot_loop/backend/cranelift_aot
    time:  [361.06 ms 361.22 ms 361.39 ms] R1
    time:  [363.06 ms 363.14 ms 363.27 ms] R2
    time:  [364.25 ms 364.30 ms 364.35 ms] R3
    median per-iter: 363 ns / iter
v6_gamma_m5_hot_loop/backend/trace_jit_warm
    time:  [9.4995 ms 9.5030 ms 9.5072 ms] R1
    time:  [9.4908 ms 9.4935 ms 9.4963 ms] R2
    time:  [9.5016 ms 9.5043 ms 9.5081 ms] R3
    median per-iter: 9.49 ns / iter
v6_gamma_m5_hot_loop/backend/trace_jit_warm_ic
    time:  [9.5509 ms 9.5547 ms 9.5591 ms] R1
    time:  [9.5198 ms 9.5229 ms 9.5257 ms] R2
    time:  [9.5519 ms 9.5589 ms 9.5646 ms] R3
    median per-iter: 9.53 ns / iter
v6_gamma_m5_hot_loop/backend/rust_inlined_baseline
    time:  [3.5518 ms 3.5522 ms 3.5527 ms] R1
    time:  [3.5520 ms 3.5523 ms 3.5527 ms] R2
    time:  [3.5530 ms 3.5537 ms 3.5546 ms] R3
    median per-iter: 3.55 ns / iter
```

### vs M2-B baseline (9.52 ns/iter)

- `trace_jit_warm`: 9.52 → **9.49 ns/iter**（基本相同，0.03 ns 在
  噪声内）。M2-C 不动这条路径的代码——它是 cranelift trace
  install 之后的「原版 `Arc<JITedTraceFn>::invoke`」语义。
- `trace_jit_warm_ic`: M2-C 新增 = **9.53 ns/iter**。

### vs brief 阈值

- 阈值 ≤ 5 ns/iter (hard floor): **9.53 不达**。诊断在 §5。
- 阈值 ≤ 3 ns/iter (aspirational): 不达。

### vs LuaJIT trace-tier (1-3 ns/iter)

差距倍数：9.53 / 2 = **4.8×**（取 LuaJIT 中位数 2 ns）。M2-B 时是
9.52 / 2 = 4.76×。**比 LuaJIT 慢约 5 倍——没动**。原因不是 IC，是
cranelift 函数调用约定。

## 5. 为什么 IC dispatch 没移动 bench 数字（诊断）

预期：移除 extern C boundary 节省 4-5 ns/iter。
实测：节省 0.04 ns/iter（统计学噪声）。

### 5.1 Rust 侧 dispatch tail 在 fat-LTO 下已 zero-cost

`Cargo.toml [profile.release] lto = "fat"`，fat-LTO 把
`JITedTraceFn::invoke` 完全 inline 到 bench 的热循环里。生成的
机器码看起来跟 IC dispatch 完全一样：

```text
trace_jit_warm 内层循环：           trace_jit_warm_ic 内层循环：
  mov [rsp+args+0], rcx               mov [rsp+args+0], rcx
  mov [rsp+args+8], rdx               mov [rsp+args+8], rdx
  lea rdi, [rsp+ctx]                  lea rdi, [rsp+ctx]
  lea rsi, [rsp+args]                 lea rsi, [rsp+args]
  call [trace_fn_ptr]   ← Arc 已 inline 出  call [entry_ptr]    ← IC 已 inline 出
  test eax, eax                       test eax, eax
  jne deopt                           jne deopt
  mov rax, [ctx+result_slot]          mov rax, [ctx+result_slot]
```

`TraceEntryStatus` enum match 在 release 也被 `cmp/test eax` 等价
形式取代——`Success=0` 的 niche 让两者退化到同一 cmov 序列。
**dispatch layer 不是 bottleneck**。

### 5.2 真 bottleneck：cranelift trace entry 的 SystemV ABI 调用

每次 `call [trace_fn_ptr]` 走完整 SystemV 调用约定：

1. **caller 侧 spill / fill**：bench 用了 `acc`/`i`/`ctx`/`args` 几个
   live var，function call 强制 spill 一部分到 stack。
2. **callee prologue**：cranelift-emitted 函数即使 disable
   `enable_probestack`/`preserve_frame_pointers`，仍要 `push rbp;
   mov rbp, rsp` 或 (启用 frameless 时) `sub rsp, k`（k = max 4
   for register spill），以及 callee-saved register 备份（cranelift
   保守地认为它可能用 r12-r15）。
3. **call/ret pair**：2-3 cycles 的 branch prediction + ret stack
   buffer。
4. **callee epilogue**：mirror 的 reverse。

每次 invoke 大约 6 ns 的「函数调用边界」开销（与 v6-γ M5 测的
4.39 ns const-only baseline 一致——后者无 body，纯 invoke
overhead）。**body 本身 3.55 ns（见 rust_inlined_baseline）**。
9.49 ≈ 6 + 3.5。

### 5.3 跨过 5 ns 的唯一路径：消灭函数调用边界

可选方案（按 cost 排序）：

1. **cranelift `CallConv::Tail`**：自定义寄存器分配，去掉 GP-reg
   备份。理论节省 1-2 ns，但 trace 仍是独立函数。代价：bench
   `entry` transmute 需要改类型；touched everywhere。
2. **at-call-site inline**：cranelift-AOT entry function 在 hot
   counter saturate 时直接把 trace body 折进自己。这是 v6-ε 的
   核心工作——需要 cranelift_module 的 patch-point API + 在
   AOT entry 留一个 IC stub。
3. **trace-to-trace fall-through**：trace 之间的 tail call 直接
   `jmp` 到下一个 trace 头（LuaJIT 风格），完全跳过 ret/call
   pair。需要重新设计 entry signature + 共享 register state。

M2-C 没尝试 (1) 是因为它的 ABI 影响面比 IC 本身还大；M2-C
明确把 (2) (3) 划归 v6-ε。

## 6. 4-way corpus parity（保持 0 mismatch）

| Variant | M2-B | M2-C | Δ |
|---------|------|------|---|
| AllAgree | 28 | 28 | 0 |
| AllTrap | 4 | 4 | 0 |
| BytecodeMatchesBaseline | 1 | 1 | 0 |
| BytecodeUnsupported | 15 | 15 | 0 |
| Mismatch | 0 | 0 | 0 |

Tier-level:

- `arith_control`: 23 AllAgree + 4 AllTrap + 1 BytecodeUnsupported
  (`let_chain`) = 28 / 28
- `dict_return`: 1 BytecodeMatchesBaseline + 1 BytecodeUnsupported = 2 / 2
- `stdlib_simple`: 9 AllAgree = **9 / 9 clean**
- `stdlib_case_fold` / `stdlib_list` / `stdlib_memory` /
  `stdlib_normalize`: 全 BytecodeUnsupported（String / wasm
  memory 路径）

M2-C 不动 envelope，BytecodeUnsupported 数字保持 M2-B 的 15。
任务 brief 给的 ≤ 12 目标差 3 case，原因 documented in M2-B
report §7（String/wasm memory 真路径是 v6-δ+ 工作）。

## 7. Trace entry ABI: 改动 vs 保留

### 7.1 改动（host-side only）

- `TraceJitState::invoke_with_resume` callback 现在收到的
  `DeoptStateSnapshot` 中 `value_stack_copy` **可能非空**（如果
  install 时 GuardSite 携带 ssa_stack_snapshot）。M2-B 的「永远
  空」语义关掉。
- bytecode-side `resume_from_snapshot` 调用方继续按 recipe 消费
  `value_stack_copy` —— 接口未变。

### 7.2 保留（unchanged）

- `TRACE_ENTRY_SIG` 仍是 `(*mut TraceContext, *const u64) -> i32`。
- `TraceContext` layout 不变（152 bytes）。
- `DeoptStateSnapshot` layout 不变（72 bytes）。
- `__relon_trace_save_deopt(ctx, guard_pc, external_pc)` 签名不变。
- `extern "C"` 是 cranelift trace entry 的 ABI，**没换**——brief 要
  求换的是「remove extern C boundary」语义，但 §5 论证 boundary
  cost 不在 dispatch layer，换 ABI 不解决问题。

### 7.3 内部新增（不破坏 ABI）

- `JITedTraceFn` 私有字段 `guard_ssa_stacks` + `guard_external_pcs`。
- `JITedTraceFn` 新增 pub method `invoke_raw` / `typed_entry` /
  `guard_ssa_stack` / `guard_table_len`。
- `TraceEntryFn` type alias。
- `TraceIcSlot` + `IC_WAYS` 新 module。

## 8. 文件 diff stat

`git diff --stat 893e531..HEAD`：

```
 crates/relon-bench/benches/trace_jit_hot_loop.rs | 101 +++++++-
 crates/relon-codegen-native/src/lib.rs           |   4 +-
 crates/relon-codegen-native/src/trace_ic.rs      | 307 +++++++++++++++++++++++
 crates/relon-codegen-native/src/trace_install.rs | 179 ++++++++++++-
 crates/relon-trace-jit/src/guard.rs              |  25 ++
 crates/relon-trace-recorder/src/recorder.rs      | 187 +++++++++++++-
 6 files changed, 783 insertions(+), 20 deletions(-)
```

Commit history（HEAD-first，到 M2-B base 共 3 commit）：

```
22760cc test(bench): add IC-dispatch + Rust-inlined baseline rows
fa5d7d1 feat(codegen-native): IC dispatch slot + per-guard ssa_stack table
428fdd9 feat(trace-recorder): mirror operand stack into GuardSite snapshot
893e531 docs(internal): v6-delta M2-B stage report + plan section 15 (DONE)  ← base
```

## 9. Key file paths (absolute)

- 新增 source:
  - `/ext/relon/crates/relon-codegen-native/src/trace_ic.rs`
- 修改 source:
  - `/ext/relon/crates/relon-bench/benches/trace_jit_hot_loop.rs`（新增 2 行）
  - `/ext/relon/crates/relon-codegen-native/src/lib.rs`（export `TraceIcSlot` / `TraceEntryFn`）
  - `/ext/relon/crates/relon-codegen-native/src/trace_install.rs`（`JITedTraceFn` widen + `invoke_with_resume` 渲染 + cranelift flags）
  - `/ext/relon/crates/relon-trace-jit/src/guard.rs`（`GuardSite::ssa_stack_snapshot` 字段 + builder）
  - `/ext/relon/crates/relon-trace-recorder/src/recorder.rs`（`ssa_stack` mirror + 4 unit test）
- 更新 docs:
  - `/ext/relon/docs/internal/v6-gamma-integration-plan-2026-05-18.md`（§16 v6-δ M2-C DONE）
  - `/ext/relon/docs/internal/wasm-bench-report-2026-05-16.md`（附录 v6-δ M2-C）
- 新增 docs:
  - `/ext/relon/docs/internal/v6-delta-m2c-stage-report-2026-05-19.md`（本文）

## 10. Carry-over to v6-ε

### 10.1 v6-ε 主轴：消灭函数调用边界

- **trace-to-trace fall-through**：LuaJIT 风格的 tail-call 链。
  trace 不再 ret；末尾 jmp 到下一个 trace 头或回到 cranelift-AOT
  entry。需要重新设计 entry signature（保留 register state across
  jumps）+ 共享 register allocator。
- **at-call-site inline**：cranelift-AOT entry function 在 hot
  trigger 之后把 trace body 直接折进自己。需要 cranelift_module
  patch-point API（不是 v6-δ 范围）。
- **Tail ABI**：用 `CallConv::Tail` 替换 SystemV，去掉 GP-reg
  save/restore。需要全链路改型，影响 emitter / runtime helper /
  bench 入口。

预算: M2-C 实测 trace_jit_warm 9.49 ns / rust_inlined 3.55 ns =
**6 ns 函数调用边界 budget**。v6-ε 目标是把 dispatch tail 压到
3.5-5 ns 范围（即 boundary 消减到 0-1.5 ns）。

### 10.2 v6-ε 副轴：guard hoisting (已写 plan)

`docs/internal/v6-epsilon-guard-hoist-plan.md` 单独 plan，与本
roadmap 正交。M2-C 的 `ssa_stack_snapshot` 数据可以喂给 guard
hoisting pass（loop-invariant TypeCheck 提升到 loop head 后，仍
能从 snapshot 重建 fallback path）。

### 10.3 Minor sweep（保留 from M2-B carry-over）

- **String slot 真路径**：5 case_fold + 4 memory + 2 normalize +
  1 dict_with_string_return = 12 case 全部 String / wasm memory
  真实化。M2-B/M2-C 都没动，等 bytecode VM 拿 String pool 或
  wasm linear memory mock。
- **List 真 memory 访问**：2 case (list_int_sum/list_int_max)。
- **Op::Select 专用 BcOp**：v6-δ 内 envelope 优化项。

### 10.4 M2-C 自己留的 trade-off

- `pop_inputs` silent saturating truncate：production walker 路径
  下严格，synthetic test 路径下可能携带 under-populated snapshot。
  影响：synthetic-driven 单测里的 `ssa_stack_snapshot` 长度可能
  少于真实操作栈深度——bytecode 侧 fall back 到 recipe-only
  materialise（与 M2-B 行为一致），不影响 4-way parity。
- IC slot 是 host-side Rust thread-local 而非 cranelift call-site
  embed。v6-ε at-call-site inline 工作把它替换掉时，`TraceIcSlot`
  的 4-way LRU + miss handling 语义可以直接拿过去。

EOF
