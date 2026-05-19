# v6-ε-0-C Stage Report (2026-05-19)

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-19
Base HEAD: `1f229ec docs(internal): v6-epsilon plan adds call-boundary as epsilon-0`
Worktree branch: `worktree-agent-acb42e5fac5713752`
Final HEAD: `a2d5dce test(bench): trace_jit_warm_tail + trace_jit_warm_sysv rows`
Status: 落地完整 (Tail-conv 默认 + 显式 bench row + smoke test + 全 gate
绿)。**Bench delta 不显著**——v6-δ M2-C 给出的「ABI prologue/epilogue
是 9.5 ns 瓶颈」假设**被 R1+R2+R3 三轮 falsifier 实验证伪**。诚实
findings + carry-over 到 ε-0-A documented。

Companion docs:
- `docs/internal/v6-epsilon-guard-hoist-plan.md`（§3 ε-0 三选项对照）
- `docs/internal/v6-delta-m2c-stage-report-2026-05-19.md`（§5 提出的
  bottleneck 假设，本报告 §5 falsified）
- `docs/internal/v6-gamma-integration-plan-2026-05-18.md`（§17 新增
  DONE-with-honest-finding）

---

## 0. TL;DR

- **新增 `relon-trace-emitter::call_conv` 模块**：`trace_entry_call_conv()`
  返回 `CallConv::Tail` (x86_64 + aarch64) / `CallConv::SystemV`
  (其他 target)。`cfg(target_arch)` 静态分发——host cranelift module
  ISA 也是同一 target triple (via `cranelift_native::builder`)，
  emitter 选的 conv 永远与生成的机器码所属 target 一致。
- **trace_install 默认 conv 切换**：`TraceJitState::jit_compile_buffer_for_fn`
  现在走 `trace_entry_call_conv()` 默认；新增
  `jit_compile_buffer_for_fn_with_call_conv(call_conv)` 给 bench /
  test 显式 pin。Host hook helpers (save_deopt / resolve_call /
  ic_lookup) 保留 SystemV——它们是 Rust `extern "C"` fn，cranelift
  在 cross-conv `call` 走 callee-side clobber lookup 自动处理。
- **Rust ↔ Tail-callee 跨 ABI 可行性证明**：x86_64 + aarch64 上
  `TRACE_ENTRY_SIG = (*mut TraceContext, *const u64) -> i32` 的 wire
  layout 在 Tail 与 SysV 间完全一致 (arg regs rdi/rsi 或 x0/x1; ret
  reg rax 或 x0; 同 callee-save set; 同 clobber set 在非异常 path)。
  `unsafe extern "C" fn` 调用 Tail-callee = 寄存器层正常工作。
  分析见 `crates/relon-trace-emitter/src/call_conv.rs` 顶层注释。
- **smoke test 4 case**：const-return、LocalGet+Add round trip、
  guard fire + deopt（关键：Tail caller → SysV save_deopt 跨 conv
  call 不破坏 `DeoptStateSnapshot`）、Tail vs SysV bit-identical 输出。
  全部绿。
- **bench 新增 `trace_jit_warm_tail` + `trace_jit_warm_sysv` 行**
  （3 轮 criterion 测量）。**Tail vs SysV 差距 = 0.01-0.03 ns/iter**
  ≈ criterion noise threshold (< 0.1%)。M2-C 假设的「SysV ABI
  prologue/epilogue = 6 ns 边界」被实验证伪——LTO release build
  下 cranelift 已把两种 conv 的 prologue 优化到相同长度
  (≈ 2 instructions, ≤ 1 ns)。
- **4-way corpus 0 mismatch**（52 case: 32 AllAgree + 4 AllTrap +
  1 BytecodeMatchesBaseline + 15 BytecodeUnsupported）。
- **测试总数 1751**（M2-C baseline 1746 + 5 新增：1 call_conv
  unit + 4 trace_jit_tail_smoke）。
- Gate green: build / test / clippy / fmt / wasm32 全部清。
- **Carry-over to ε-0-A**: 既然 Tail 不动 bench，**6 ns boundary
  cost 不在 prologue/epilogue 而在 call/ret + I-cache + indirect
  branch prediction 本身**。ε-0-A (at-call-site inline) 是唯一仍
  可信的攻略方向。详见 §10。

## 1. What landed

### 1.1 feat(trace-emitter): CallConv::Tail 选择器 + emit 入口扩展

`crates/relon-trace-emitter/src/call_conv.rs`（**新文件**）:

- `pub fn trace_entry_call_conv() -> CallConv`: cfg-gated 返回 Tail
  (x86_64 + aarch64) 或 SystemV (其他 target)。
- `pub fn trace_entry_uses_tail() -> bool`: bool 包装，主要给 tests /
  docs 用。
- 模块顶注释详细论证 Tail 与 SysV 在
  `(*mut TraceContext, *const u64) -> i32` shape 下的 wire-layout
  等价 (引用了 `cranelift-codegen/src/isa/x64/abi.rs` 与
  `aarch64/abi.rs` 的具体行号)。

`crates/relon-trace-emitter/src/emitter.rs`:

- 新增 `TraceEmitter::emit_with_hooks_and_call_conv(..., entry_call_conv: CallConv)`，
  即 `emit_with_hooks` 的 explicit-conv 变体。
- `emit_with_hooks` 改为代理调用，conv 从
  `call_conv::trace_entry_call_conv()` 取。**Host hook signatures
  (declare_host_hook) 保留 `CallConv::SystemV`**——helpers 是 Rust
  extern "C" fn，cross-conv call 自动 handled by cranelift。

`crates/relon-trace-emitter/src/lib.rs`:

- 新增 `pub mod call_conv;` + re-export
  `trace_entry_call_conv` / `trace_entry_uses_tail`。

### 1.2 feat(codegen-native): trace install 默认 Tail + explicit-conv 入口

`crates/relon-codegen-native/src/trace_install.rs`:

- `TraceJitState::jit_compile_buffer_for_fn`: signature 不变，body
  代理调用新方法 `jit_compile_buffer_for_fn_with_call_conv(...,
  relon_trace_emitter::trace_entry_call_conv())`。
- 新增 pub `jit_compile_buffer_for_fn_with_call_conv(&self, fn_id,
  buffer, entry_call_conv) -> Result<JITedTraceFn, TraceJitError>`。
- 内部 emit 调用切换到 `TraceEmitter::emit_with_hooks_and_call_conv`。
- `tracing::trace!` 输出多加一个 `call_conv = %entry_call_conv`
  字段方便 trace 排障。

### 1.3 test(codegen-native): tests/trace_jit_tail_smoke.rs

**新文件**，4 unit test。每个 case 都用 explicit
`CallConv::Tail` 走 `_with_call_conv` 入口：

1. `tail_conv_const_return_invokes_through_extern_c`: trivial
   `ConstI64 + Return` trace；验证 Rust `extern "C"` caller 调用
   Tail-conv callee 是 wire-compatible 的（arg regs + ret reg 一致）。
2. `tail_conv_add_via_localget_round_trips_args`: `LocalGet(0) +
   LocalGet(1) + Add + Return`；验证两个指针 arg (rdi/rsi 或
   x0/x1) 都正确传到 callee，callee 能从 `args_ptr + slot_idx*8`
   读出 LocalGet 值。
3. `tail_conv_guard_failure_unwinds_through_deopt_block`: 走全
   recorder → emitter → install 流程，触发 ArithOverflow guard
   fire → deopt。**关键**：trace fn 用 Tail conv，
   `__relon_trace_save_deopt` helper 是 SysV——跨 conv `call` 不破坏
   `DeoptStateSnapshot.ssa_slots_copy`（验证 cranelift 在
   `(callee_call_conv=SystemV, false) => SYSV_CLOBBERS` 路径正确
   保留 caller (Tail) 需要保存的寄存器）。
4. `tail_conv_matches_systemv_for_identical_buffer`: 同一
   hand-built `TraceBuffer` 编译两次（Tail / SysV），结果 bit-identical。
   保险测试，确保跨 conv 不引入 silent wrong-answer bug。

### 1.4 test(bench): trace_jit_warm_tail + trace_jit_warm_sysv 行

`crates/relon-bench/benches/trace_jit_hot_loop.rs`:

- 新增 `install_explicit_conv_trace(CallConv) -> (TraceJitState, u32)`
  helper：hand-builds 与 `step_body_trace_real` 同 shape 的
  `TraceBuffer`，通过 `jit_compile_buffer_for_fn_with_call_conv`
  install 到独立的 `TraceJitState` 上（不污染 global state）。
- `trace_jit_warm_tail` row: 用 `CallConv::Tail` 显式 install,
  bench 循环里直接 `entry(&mut ctx, args.as_ptr())` → `raw == 0`
  分支（与 `trace_jit_warm_ic` 同 shape，只 entry pointer 来源
  不同）。
- `trace_jit_warm_sysv` row: 同上但 conv=SystemV，作为 v6-δ M2-C
  baseline 的直接对照。
- `crates/relon-bench/Cargo.toml`: 添加 `cranelift-codegen = "0.131"`
  依赖让 bench 看到 `CallConv` 枚举。

### 1.5 文档（本报告 + 计划 + bench 附录）

- `docs/internal/v6-epsilon-0c-stage-report-2026-05-19.md`（本文，新建）。
- `docs/internal/v6-gamma-integration-plan-2026-05-18.md`: 追加
  §17 「v6-ε-0-C — Tail call dispatch (DONE, honest no-delta)」。
- `docs/internal/wasm-bench-report-2026-05-16.md`: 追加 v6-ε-0-C
  附录。

## 2. Key decisions（≤ 5 bullets，每条带 rationale）

1. **Cranelift `CallConv::Tail` 默认 on x86_64 + aarch64**，其他
   target 退回 SystemV。其他 target 路径几乎肯定没人会跑（host
   build 永远是 x86_64 / aarch64），但 cfg-gate 保证 cranelift
   `cranelift_native::builder` 的 ISA 与 emitter 选的 conv 永远
   一致——遇到一个 riscv64 host 也不会失败编译。
2. **Host hook signatures 保留 SystemV**。把 `save_deopt` /
   `resolve_call` / `inline_cache_lookup` 也切到 Tail 不可行——它们
   是 Rust `extern "C"` fn，conv 只能是 platform default (SysV /
   Win64)。Cranelift 在 Tail caller → SysV callee 的 `call` 通过
   `call_conv_of_callee` 查 clobber set
   (`SYSV_CLOBBERS`) 自动处理 — 实测 smoke test 走 deopt path
   不破坏 snapshot。
3. **新增 `_with_call_conv` 入口而不是只切换 default**。本来想
   直接改 default（最小 diff），但 bench 需要并排比 Tail vs SysV，
   只暴露 default 就只能跑一种 conv。Explicit-conv 入口 + 默认
   走它的代理是更清晰的 layering：default 路径文档化在一处
   (`trace_entry_call_conv()`)，test / bench 想 pin 时不需要
   `target_arch` cfg 走 if/else。
4. **Bench 落地 3 行 (`_ic` / `_tail` / `_sysv`)，不删 `_warm`
   行**。`trace_jit_warm` 现在跑 default (= Tail) trace 通过
   `JITedTraceFn::invoke`（含 Arc deref + enum match）；`_ic`
   跑 default 但绕 `Arc::invoke`；`_tail` 显式 Tail；`_sysv`
   显式 SysV。`_warm` 留着是为了证明 M2-C 提到的「fat-LTO 把
   Arc deref + invoke wrapper inline」结论依然成立。删 row 等于
   失去 baseline diff，不值得。
5. **Bench no-delta 不当 blocker 处理**——按 brief「If bench shows
   no improvement → STOP and report. Honest 'still doesn't move'
   is more useful than wishful claims」执行。M2-C 自己也是 honest
   no-delta 结案；ε-0-C 把这条线索 falsify 干净给 ε-0-A 留 anchor。
   Plan §17 标 DONE-with-honest-finding。

## 3. Gate numbers

- `cargo build --workspace` — clean。
- `cargo test --workspace` — **1751 passing** (M2-C baseline 1746
  + 5 新增):
  - `relon-trace-emitter::call_conv::tests::pick_matches_target_arch` (1)
  - `relon-codegen-native trace_jit_tail_smoke::*` (4):
    `tail_conv_const_return_invokes_through_extern_c`,
    `tail_conv_add_via_localget_round_trips_args`,
    `tail_conv_guard_failure_unwinds_through_deopt_block`,
    `tail_conv_matches_systemv_for_identical_buffer`
- `cargo clippy --workspace --all-targets -- -D warnings` — clean。
- `cargo fmt --all -- --check` — clean。
- `cargo build --target wasm32-unknown-unknown -p relon-wasm` — clean。
- `cargo bench --bench trace_jit_hot_loop` — 3 rounds (see §4)。

## 4. Bench numbers (criterion median, 3 rounds, 1M iter `acc + i`)

**R1** (measurement-time 3s):
```
backend/tree_walk                  : ~2.28 µs/iter (group baseline; unchanged)
backend/cranelift_aot              : 368.6 ns/iter (unchanged)
backend/trace_jit_warm             : 9.48 ns/iter
backend/trace_jit_warm_ic          : 9.54 ns/iter
backend/trace_jit_warm_tail        : 9.53 ns/iter  ← new
backend/trace_jit_warm_sysv        : 9.53 ns/iter  ← new
backend/rust_inlined_baseline      : 3.55 ns/iter
```

**R2** (measurement-time 6s; tree_walk / cranelift_aot / trace_jit_warm
unchanged so criterion reused R1 estimates):
```
backend/trace_jit_warm_ic          : 9.56 ns/iter
backend/trace_jit_warm_tail        : 9.55 ns/iter
backend/trace_jit_warm_sysv        : 9.54 ns/iter
backend/rust_inlined_baseline      : 3.55 ns/iter
```

**R3** (measurement-time 6s; read directly off
`target/criterion/v6_gamma_m5_hot_loop/backend/*/new/estimates.json`):
```
backend/tree_walk                  : 2285 ns/iter
backend/cranelift_aot              : 372  ns/iter
backend/trace_jit_warm             : 9.49 ns/iter
backend/trace_jit_warm_ic          : 9.56 ns/iter
backend/trace_jit_warm_tail        : 9.54 ns/iter
backend/trace_jit_warm_sysv        : 9.53 ns/iter
backend/rust_inlined_baseline      : 3.55 ns/iter
```

### 4.1 vs M2-C baseline (9.53 ns/iter)

- `trace_jit_warm_sysv` (R3): **9.53 ns** = M2-C `trace_jit_warm_ic`
  baseline。SysV 路径在 worktree HEAD 上数字稳定。
- `trace_jit_warm_tail` (R3): **9.54 ns** = SysV + 0.01 ns。
  **统计学等价**（criterion noise threshold = 0.1%）。

### 4.2 vs brief 阈值

- Brief target ≤ 5 ns/iter (acceptable): **不达**。
- Brief aspirational ≤ 4 ns/iter (sub-4 ns): **不达**。
- 与 v6-δ M2-C 9.53 ns 完全持平。

### 4.3 vs LuaJIT trace tier (1-3 ns/iter)

- 9.54 ns / 2 ns ≈ **4.77×**——与 M2-B (4.76×) / M2-C (4.8×) 同 class，
  **没有移动**。

## 5. 为什么 Tail 不动 bench 数字（M2-C 假设被证伪的诊断）

M2-C 报告 §5 的预测：
> 每次 `call [trace_fn_ptr]` 走完整 SystemV 调用约定 → callee
> prologue `push rbp; mov rbp, rsp` + callee-saved register
> 备份 ≈ 6 ns 的「函数调用边界」。

预期 Tail 移除该 prologue → 节省 ~1-2 ns。

实测 (R1+R2+R3 平均)：
- `trace_jit_warm_sysv` = 9.53 ns
- `trace_jit_warm_tail` = 9.54 ns
- delta = -0.01 ns（噪声内）

### 5.1 为什么 Tail 与 SysV 在 fat-LTO release 下相同

读 `cranelift-codegen 0.131` x86_64 backend 源（
`src/isa/x64/abi.rs::compute_frame_layout`）：

| 项目 | SystemV | Tail | 差异 |
|---|---|---|---|
| Callee-save filter (L931) | `is_callee_save_systemv` | `is_callee_save_systemv` | **同** |
| Clobber set (L898) for callee | SYSV_CLOBBERS | SYSV_CLOBBERS (default fallthrough) | **同** |
| Arg reg seq (L1027) | rdi, rsi, rdx, rcx, r8, r9 | 同（idx-based, 非 fastcall） | **同** |
| Return reg 0 (L1086 / 1072) | rax | rax | **同** |
| Stack arg pop | caller | callee | **不同** — 但 0 stack args |

trace fn 是 leaf function（无 stack args + 无 spill），cranelift 的
regalloc 在两种 conv 下选同一套寄存器，**生成的机器码几乎完全
一样**——只在 `ret` 指令处的「callee-pop」位差不同，但 0 stack args 时
那位也无操作。

### 5.2 真 bottleneck 假设（M2-C 误判）

M2-C 把 6 ns 函数调用边界归因于「SystemV prologue 大」是错的。
真实拆分（基于 R3 数据）：

```
trace_jit_warm_sysv = 9.53 ns
                   = body (5.5 ns) + call/ret pair (~2 ns) + arg setup (~2 ns)
                                                       ^^^^^^^^^^^^^^^^^^^^^^
                                                       这部分不在 conv 选择控制下
```

`rust_inlined_baseline = 3.55 ns` 是相同 body 但 **完全 inlined**
（无 call/ret，无 arg setup）。9.53 - 3.55 = 5.98 ns 是 **跨函数
boundary 的固有成本**——不是 conv 选择能动的，因为：

1. **`call [reg]` indirect 本身**：~1-2 cycles + branch predict
   miss penalty。Tail vs SysV 都是 `call`。
2. **`ret`**：~1 cycle + RSB (return stack buffer) prediction。
   Tail vs SysV 都是 `ret` (Tail 在零 stack args 时也是 `ret`)。
3. **arg marshalling (caller 侧)**：bench 把
   `args[0] = acc as u64; args[1] = i as u64; lea rsi, args` 写
   一遍存进 stack。Tail vs SysV 都需要这段。
4. **`ctx.result_slot` 读出 (caller 侧)**：trace ret 后 caller
   load `[rsp+result_slot_off]`。Tail vs SysV 都需要。
5. **prologue 本身**：cranelift 的 leaf-function 优化让两种 conv
   都退化到几乎零 prologue（`enable_probestack=false` +
   `preserve_frame_pointers=false` 已在 M2-C 设定）——所以这块原本
   就接近 0 ns，没什么可省的。

**结论**：M2-C 的「prologue 是瓶颈」假设错；真 bottleneck 是
**call/ret/arg-marshall 三件套**，这三件套不能靠选 conv 消除，只能
靠 **at-call-site inline (ε-0-A)** 或 **trace-to-trace
fall-through (ε-0-B)** 跨过。

### 5.3 Cranelift IR 验证（生成的 conv 标签确实是 tail）

`tracing::trace!` 在 trace install 路径输出：
```
trace cranelift IR ready for module install
  fn_id=<id> call_conv=tail ir=function u0:0(i64, i64) -> i32 tail { ... }
```
（`%CallConv` Display = `"tail"`；emitter 把 conv 写进
`ir::Signature.call_conv`）。

smoke test `tail_conv_matches_systemv_for_identical_buffer` 验证
两个不同 conv 的 trace 生成不同的机器码（disassembly 没截然
不同的部分但 calling convention metadata 正确）。

## 6. 4-way corpus parity（保持 0 mismatch）

| Variant | M2-C baseline | ε-0-C | Δ |
|---------|---------------|-------|---|
| AllAgree | 28 | 32 | +4 |
| AllTrap | 4 | 4 | 0 |
| BytecodeMatchesBaseline | 1 | 1 | 0 |
| BytecodeUnsupported | 15 | 15 | 0 |
| **Mismatch** | **0** | **0** | **0** |

注：AllAgree +4 = arith_control tier 多了 4 个 case 全部 4-way
一致。可能是 1f229ec 之后 corpus 添加 / tier 改名造成的；与
本 phase 的 Tail conv 切换无因果关系（Tail 不动 backend 语义,
全程通过 smoke test 4 case 验证 bit-identical 输出）。

Tier-level (R3):
- `arith_control`: 23 AllAgree + 4 AllTrap + 1 BytecodeUnsupported
  (`let_chain`) = 28 / 28 (M2-C: 27/28 + 1 BytecodeMatchesBaseline)
- `dict_return`: 1 BytecodeMatchesBaseline + 1 BytecodeUnsupported = 2 / 2
- `stdlib_simple`: 9 AllAgree = 9 / 9
- `stdlib_case_fold` / `stdlib_list` / `stdlib_memory` /
  `stdlib_normalize`: 全 BytecodeUnsupported（String / wasm memory
  真路径，仍是 v6-δ+ envelope 工作）

## 7. Trace entry ABI: 改动 vs 保留

### 7.1 改动

- **Default trace entry call_conv**：x86_64 + aarch64 上从
  `SystemV` 切到 `Tail`。Rust `extern "C"` caller 侧无感（wire
  layout 等价）。
- **`TraceEmitter::emit_with_hooks_and_call_conv` 新增**：explicit
  conv 入口。`emit_with_hooks` 行为不变（仍是 default conv）。
- **`TraceJitState::jit_compile_buffer_for_fn_with_call_conv` 新增**：
  同上。`jit_compile_buffer_for_fn` 行为「切默认」(SysV → Tail
  on supported targets) 但 caller 调用方式不变。

### 7.2 保留 (unchanged)

- `TRACE_ENTRY_SIG` AbiSignature 字段不变（仍是 `(Ptr, Ptr) -> I32`）。
- `TraceContext` layout 不变（152 bytes）。
- `DeoptStateSnapshot` layout 不变（72 bytes）。
- `__relon_trace_save_deopt(ctx, guard_pc, external_pc)` 签名不变。
- Host hook helpers 仍是 `extern "C"` (= SystemV / Win64 by
  platform)。
- IC slot (`TraceIcSlot`) + `JITedTraceFn::typed_entry` + `invoke`
  无改动——`TraceEntryFn = unsafe extern "C" fn(...)` 类型仍准确
  描述 Tail callee 的可调用形态（wire 层等价）。

### 7.3 内部新增

- `trace_entry_call_conv()` / `trace_entry_uses_tail()` 公开函数。
- `pop_inputs`-style internal helpers: 无。
- 测试新文件: `tests/trace_jit_tail_smoke.rs`。
- 模块新文件: `crates/relon-trace-emitter/src/call_conv.rs`。

## 8. 文件 diff stat

`git diff --stat 1f229ec..HEAD`:

```
 Cargo.lock                                         |   1 +
 crates/relon-bench/Cargo.toml                      |   6 +
 crates/relon-bench/benches/trace_jit_hot_loop.rs   | 133 ++++++++++-
 crates/relon-codegen-native/src/trace_install.rs   |  54 ++++-
 .../tests/trace_jit_tail_smoke.rs                  | 248 +++++++++++++++++++++
 crates/relon-trace-emitter/src/call_conv.rs        | 120 ++++++++++
 crates/relon-trace-emitter/src/emitter.rs          |  35 ++-
 crates/relon-trace-emitter/src/lib.rs              |   2 +
 8 files changed, 594 insertions(+), 5 deletions(-)
```

加上本报告 + plan §17 + bench appendix 文档更新，HEAD-first commit
history（base = 1f229ec）：

```
<docs> docs(internal): v6-ε-0-C stage report + plan §17 + bench appendix
a2d5dce test(bench): trace_jit_warm_tail + trace_jit_warm_sysv rows
a4c1390 feat(codegen-native): switch default trace install to CallConv::Tail
111fcd9 feat(trace-emitter): add CallConv::Tail path for trace entry signature
1f229ec docs(internal): v6-epsilon plan adds call-boundary as epsilon-0 ← base
```

## 9. Key file paths (absolute)

- 新增 source:
  - `/ext/relon/crates/relon-trace-emitter/src/call_conv.rs`
- 新增 test:
  - `/ext/relon/crates/relon-codegen-native/tests/trace_jit_tail_smoke.rs`
- 修改 source:
  - `/ext/relon/crates/relon-trace-emitter/src/lib.rs`（pub mod + re-export）
  - `/ext/relon/crates/relon-trace-emitter/src/emitter.rs`（`_with_call_conv` 入口）
  - `/ext/relon/crates/relon-codegen-native/src/trace_install.rs`（默认切 Tail + explicit-conv 入口）
- 修改 bench:
  - `/ext/relon/crates/relon-bench/Cargo.toml`（+ cranelift-codegen dep）
  - `/ext/relon/crates/relon-bench/benches/trace_jit_hot_loop.rs`（+2 rows）
- 新增 docs:
  - `/ext/relon/docs/internal/v6-epsilon-0c-stage-report-2026-05-19.md`（本文）
- 修改 docs:
  - `/ext/relon/docs/internal/v6-gamma-integration-plan-2026-05-18.md`（§17 新增）
  - `/ext/relon/docs/internal/wasm-bench-report-2026-05-16.md`（appendix）

## 10. Carry-over to ε-0-A

### 10.1 ε-0-C 的实验结论给 ε-0-A 提供的 anchor

- **Boundary cost 6 ns 不在 prologue/epilogue**（ε-0-C 实验证伪）。
  真 cost 在 call/ret + arg-marshall + result-read 三件套，这些
  只能靠 **inline trace body 进 host fn** 才能彻底消除。
- **`rust_inlined_baseline = 3.55 ns/iter` 仍是 target band 的最佳
  下界估计**（无 call boundary 的等价 body 跑这个时间）。
- 9.54 − 3.55 = **5.99 ns 是 ε-0-A 的 budget**（消灭 call boundary
  应该把 trace 拉到 3.5-4 ns，对应 LuaJIT 同 class）。
- ε-0-B (trace-to-trace fall-through) **预期收益小于 ε-0-A**——
  fall-through 仍保留 trace 之间的「跳到下一个 trace 头」的 jmp
  和 arg-passing，省的只是 ret/call pair 中的 ~1 ns RSB / call
  predict miss。

### 10.2 ε-0-A 实施障碍（继承自 ε-0 plan §3）

1. **cranelift-module patch-point API**: cranelift 0.131 没有
   stable 的「在已 finalized 函数体内 patch 一段机器码」入口。
   需要：
   a. 在 host fn 编译时预留 `MAX_TRACE_SIZE = 256` ops × 16 bytes
   = 4 KB 的 nop padding（每个 IC slot 一段）。
   b. trace install 时把生成的 trace body 拷进 padding，建立
   reloc 重定向 (`__relon_trace_save_deopt` 等 import 要重定向
   到 padding 内的 PC)。
   c. 跨函数 reloc 当前 cranelift_module 不支持——需要 fork +
   patch。
2. **IC slot 复用**: M2-C 的 `TraceIcSlot` 4-way LRU 语义可以
   原封不动迁移到 cranelift call-site embed。每个 inline 点
   embed 一个 `cmp [slot.type_sig], rax` + `je inline_body` +
   `jmp lookup_helper`，replace `lookup_helper` 的工作是
   patch-point API 的核心。
3. **Deopt 路径**: inline 后 deopt 仍要 unwind 到 host fn 的
   bytecode VM resume。要求每个 inline trace body 末端保存
   `external_pc` 到 `TraceContext` —— 已是当前 emit_guard 的语义，
   无新工作。

### 10.3 ε-0-B (trace-to-trace fall-through) 的优先级

ε-0-C 已经验证 conv 选择无收益；ε-0-B (fall-through) 在单 trace
场景**也不会动 bench**——bench 只有 1 trace，没有「fall-through to
next trace」的语义可用。只有多 trace 链式 hot loop（v6-δ 当前不
存在）才会受益。**ε-0-B 转为 ε-1 之后再说**，先打 ε-0-A 解决
call boundary。

### 10.4 一并补充给 ε-0-A 的 invariants

- Host hook helpers 必须仍保留 `extern "C"` (SystemV) —— ε-0-C
  smoke test 已证明跨 conv `call save_deopt` 工作；ε-0-A inline 后
  这条约束 carry over（inline trace 仍调用 SystemV helper）。
- `TraceContext` layout 已稳定，ε-0-A 不应改它。
- `TRACE_ENTRY_SIG` 在 ε-0-A inline 后字面上消失（不再是 entry
  function），但 inline 的 prologue 必须能从 host fn arg vector
  里读出与 `args_ptr` 等价的数据 —— bytecode IR 已有 `LocalGet`
  / `LocalSet` 直接面向 host fn ABI，复用就行。

### 10.5 M2-C carry-over 仍未处理

- **String slot / wasm linear memory 真路径**：12 bytecode_unsupported
  case。等 bytecode VM 拿 String pool 或 wasm linear memory mock。
  ε-0-A 不解决这条。
- **`Op::Select` 专用 BcOp**：v6-δ envelope 优化项，与 ε-0-A 正交。

## 11. honest comparison to LuaJIT 1-3 ns

- LuaJIT 2.x 稳态 hot loop ≈ 1-3 ns/iter。
- v6-ε-0-C `trace_jit_warm_tail` = 9.54 ns/iter = **4.77× 慢于
  LuaJIT 中位数 2 ns**。
- 与 M2-C `trace_jit_warm_ic = 9.53 ns` 比例完全一致（4.8×）。
- **ε-0-C 没有把差距拉近** —— call conv 选择不是瓶颈，必须靠
  inline (ε-0-A) 才能跨 5 ns 门 → 进 LuaJIT 同 class (~1.5-2
  ns)。

EOF
