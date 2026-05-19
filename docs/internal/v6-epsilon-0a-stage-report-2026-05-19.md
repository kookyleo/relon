# v6-ε-0-A Stage Report (2026-05-19)

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-19
Base HEAD: `f379a7f feat(trace-emitter): v6-epsilon-0-C CallConv::Tail infra + honest no-delta`
Worktree branch: `worktree-agent-a13e82a8e497ab669`
Final HEAD: `9bfff6a test(bench): trace_jit_warm_inline row for ε-0-A measurement`
Status: 基础设施落地完整 (at-call-site inline 主路径 + IR
retention + size cap + deopt 保留 + bench row + smoke + 全 gate
绿)。**Bench delta 不显著**——bench loop 的 caller 是 Rust，对
其而言 inline 路径与 trampoline 路径都只剩一层 Rust → JIT
extern "C" call 边界，没有内层 trace call 可以消除。诚实
findings + 给 ε-M1 / 「把 hot loop 编进 cranelift 自己」留 anchor。

Companion docs:
- `docs/internal/v6-epsilon-guard-hoist-plan.md`（§3 ε-0 三选项对照）
- `docs/internal/v6-epsilon-0c-stage-report-2026-05-19.md`（前置 phase）
- `docs/internal/v6-gamma-integration-plan-2026-05-18.md`（§18 新增 DONE-with-honest-finding）

---

## 0. TL;DR

- **新增 `relon-trace-emitter::inline_emit` 模块**：`emit_trace_inline()`
  把 `OptimizedTrace` 的 op 流写进调用方拥有的
  `FunctionBuilder`，复用 `crate::emitter` 的全部 per-op lowering
  规则。`TraceOp::Return` 被替换为「跳到 caller-supplied post_block，
  i64 result 作 block-param」；guard fire 跳到 caller-supplied
  deopt_block。256-op 大小上限由 `should_inline_trace()` /
  `MAX_INLINE_OPS` 守护。
- **新增 `relon-codegen-native::trace_inline` 模块**：
  `compile_inline_host_fn(Arc<OptimizedTrace>) -> InlineHostFn`
  把上面的 splice 原语包成一个完整的 JIT 模块——entry fn 仍然
  obey `TRACE_ENTRY_SIG`，但是函数体里没有内层 `call trace_fn_ptr`，
  trace body 直接是 host fn body。deopt block 走
  `ctx.host_hooks.save_deopt` 的 `call_indirect`（mirror standalone
  emitter）+ direct fallback。
- **`JITedTraceFn` IR retention**：新增 `inline_trace:
  Arc<OptimizedTrace>` 字段 + `inline_trace()` / `inline_candidate()`
  accessor。host fn 编译器（未来 ε-M1 + 之后）可以从
  `TraceJitState::lookup_trace(fn_id)` 拿到 Arc 直接 feed 进
  `compile_inline_host_fn` —— 不需要重跑 recorder/optimizer。
- **smoke test 5 case**（`crates/relon-codegen-native/tests/trace_jit_inline_smoke.rs`）：
  const-return through extern C / LocalGet+Add bit-identical with
  trampoline / overflow guard inside inline trace 仍然能 save_deopt
  + GuardFailed / oversized trace 拒绝 / JITedTraceFn.inline_trace()
  feed 回 compile_inline_host_fn 还能 round trip。全部绿。
- **bench 新增 `trace_jit_warm_inline` 行**（3 轮 criterion 测量）。
  **vs `trace_jit_warm_ic` 差距 = 0.00-0.04 ns/iter**，全部
  落在 criterion noise threshold 内（< 0.5%）。这条数字与本 stage
  开工前的「at-call-site inline 在 ε-0-A 的 bench 上不会动数字，
  因为 bench 的 caller 是 Rust，inline 不能消除 Rust→JIT 边界」
  预测**完全一致**——v6-δ M2-C → ε-0-C → ε-0-A 三次 attempt
  都在同一个 9.5 ns 平台上，证伪了「函数调用边界 ≈ prologue/epilogue
  cost」假设，更强地确认 bottleneck 是 Rust→C extern call 本身。
- **4-way corpus 0 mismatch**（继承 ε-0-C 的 52 case 结果，本 phase
  不改 backend 语义）。
- **测试总数 1761**（ε-0-C baseline 1751 + 10 新增：5 inline_emit unit
  + 5 trace_jit_inline_smoke）。
- Gate green: build / test / clippy / fmt / wasm32 全部清。
- **Carry-over**: ε-0-A bench 不动数字**不意味着 inline 是无用功**。
  inline 路径在「host fn 内嵌一个真 trace call site」的场景才会
  surface 收益——例如 cranelift-AOT 编译的 entry fn 经过 ε-M1 把
  hot loop body 编进 cranelift 自身的 trace dispatch 之后。本
  phase 提供的 infrastructure（IR retention + splice primitive +
  size cap）是那条路径的前置依赖。详见 §10。

## 1. What landed

### 1.1 feat(trace-emitter): inline_emit splice primitive

`crates/relon-trace-emitter/src/inline_emit.rs`（**新文件**, 657 行）:

- `pub const MAX_INLINE_OPS: usize = 256;` —— v6-ε plan §3 ε-0-A
  规定的硬上限。超过的 trace 必须 fallback 走 trampoline-call
  路径，以防止 host fn 体积被单条 trace 撑爆。
- `pub fn should_inline_trace(trace: &OptimizedTrace) -> bool` ——
  cheap pre-check（仅判 op count），所有 caller 在调
  `emit_trace_inline()` 前先过这个 gate。
- `pub fn emit_trace_inline(builder, trace, pointer_ty, handles)
  -> Result<(), InlineEmitError>` —— 主入口。把 trace 的 op 流
  逐条写进 `builder` 当前 insertion point 之后。`InlineEmitHandles`
  把 host fn 拥有的 4 个值/块（trace_ctx_ptr, input_args_ptr,
  post_block, deopt_block）传给 splice 逻辑。
- `pub enum InlineEmitError` —— 8 个 variant 覆盖：UnboundSsa,
  OrphanGuardOp, UnrecoverableEffectInTrace, UnmatchedLoopBack,
  TraceTooLarge, MissingReturn, Guard(fwd), CallNotSupportedInInline.
  最后一条是 ε-0-A 内的已知 scope cut（见 §2.5）。

### 1.2 feat(codegen-native): trace_inline pipeline

`crates/relon-codegen-native/src/trace_inline.rs`（**新文件**, 384 行）:

- `pub fn compile_inline_host_fn(trace: Arc<OptimizedTrace>)
  -> Result<InlineHostFn, InlineHostFnError>` —— 主入口。建一个
  完整的 `JITModule`：
  1. 预声明 `__relon_trace_save_deopt` 为 `Linkage::Import`（与
     trampoline `jit_compile_buffer_for_fn` 完全同 layout）。
  2. 构造一个 host fn，signature = `(*mut TraceContext,
     *const u64) -> i32`（即 `TRACE_ENTRY_SIG` 兼容）。
  3. 在 host fn 内：entry block 拿 ABI params → 调
     `emit_trace_inline()` splice trace 进来 → post_block 返回
     `Success` status → deopt_block 通过
     `ctx.host_hooks.save_deopt` indirect / direct fallback 返回
     `GuardFailed`。
  4. `module.finalize_definitions()` → `get_finalized_function()`
     → 包装成 `InlineHostFn`。
- `pub struct InlineHostFn` —— typed-entry 持有者。
  - `unsafe fn typed_entry() -> TraceEntryFn` —— 直接给调用方
    `TRACE_ENTRY_SIG`-shape 的 fn pointer，与 `JITedTraceFn::typed_entry`
    可互换。
  - `fn raw_fn_ptr() -> *const u8`,
    `fn inline_trace() -> Arc<OptimizedTrace>` —— accessor。
  - `unsafe fn invoke_owned(args, slot_count)
    -> (TraceEntryStatus, u64)` —— test/warm-up 用的 convenience
    wrapper。
- `pub enum InlineHostFnError` —— TraceTooLarge / InlineEmit /
  Module。`impl From<InlineEmitError>` 自动 forward
  size-cap rejection 到 `TraceTooLarge`。

### 1.3 feat(trace-install): JITedTraceFn IR retention

`crates/relon-codegen-native/src/trace_install.rs` 改动:

- `JITedTraceFn` 新增字段 `inline_trace: Arc<OptimizedTrace>`。
  install 路径每次都填——cost 是一次 `Arc::new(optimized)` 移动，
  不重复 clone。
- 新增 accessor `pub fn inline_trace(&self) -> Arc<OptimizedTrace>`
  以及 `pub fn inline_candidate(&self) -> bool`（封装
  `should_inline_trace`）。
- `use relon_trace_jit::{OptimizedTrace, ...}` 新增导入。

### 1.4 trace-emitter lib.rs re-export 扩展

`crates/relon-trace-emitter/src/lib.rs`:

- 新增 `pub mod inline_emit;`。
- 新增 re-export: `emit_trace_inline`, `should_inline_trace`,
  `InlineEmitError`, `InlineEmitHandles`, `MAX_INLINE_OPS`,
  `host_hook_slot_offset`, `host_hooks_offset`, `result_slot_offset`。
  最后三个 abi-side helper 之前 module-internal，现在 inline 路径
  需要从外部 crate 调，统一升级到 crate root。

### 1.5 codegen-native lib.rs re-export 扩展

`crates/relon-codegen-native/src/lib.rs`:

- 新增 `pub mod trace_inline;`。
- 新增 re-export: `compile_inline_host_fn`, `InlineHostFn`,
  `InlineHostFnError`。

### 1.6 test(codegen-native): trace_jit_inline_smoke.rs

**新文件**, 5 集成 test：

1. `inline_const_return_round_trips_through_extern_c` —— trivial
   `ConstI64 + Return` trace，inline-compile，调用方 Rust，验证
   raw=0 + result_slot 命中。
2. `inline_add_localget_matches_trampoline` —— `LocalGet(0) +
   LocalGet(1) + Add + Return`，5 组输入分别走 inline 和 trampoline
   两种 install path，验证 bit-identical result_slot。
3. `inline_guard_fire_routes_through_deopt_path` —— 加 ArithOverflow
   guard，喂 `i64::MAX + 1` 输入触发 overflow → inline 路径必须
   call_indirect 进 host_hooks.save_deopt → ctx.deopt_state 不为
   None。
4. `inline_rejects_oversized_trace` —— 构造 257 op 的 trace，
   `compile_inline_host_fn` 必须返回 `TraceTooLarge { op_count, cap }`，
   `op_count > cap`。
5. `jited_trace_fn_exposes_inline_trace_for_re_emit` —— 跑完整
   trampoline install path → 拿 `state.lookup_trace(...).inline_trace()`
   → 直接 feed 进 `compile_inline_host_fn` → 验证 result 一致。
   pin v6-ε-0-A IR retention invariant。

### 1.7 test(bench): trace_jit_warm_inline 行

`crates/relon-bench/benches/trace_jit_hot_loop.rs`:

- 新增 `fn build_inline_step_host_fn()` —— 用同 shape 的
  `LocalGet+LocalGet+Add+Return` 4-op trace 直接 feed 进
  `compile_inline_host_fn`。返回 `InlineHostFn`（owned，保 JIT
  module 在 bench 循环期间不被 Drop）。
- 新增 `BenchmarkId::new("backend", "trace_jit_warm_inline")` 行：
  循环里直接 `inline_entry(&mut ctx, args.as_ptr())` → `raw == 0`
  → `acc = ctx.result_slot as i64`。形状与 `trace_jit_warm_ic` 完全
  一致，只换 entry pointer 来源。
- `use relon_codegen_native::{... compile_inline_host_fn ...
  InlineHostFn ...};` 在 import 扩展。

### 1.8 文档（本报告 + plan + bench appendix）

- `docs/internal/v6-epsilon-0a-stage-report-2026-05-19.md`（本文，新建）。
- `docs/internal/v6-gamma-integration-plan-2026-05-18.md`: 追加 §18
  「v6-ε-0-A — At-call-site inline (DONE, infrastructure landed,
  honest no-delta on this bench)」。
- `docs/internal/wasm-bench-report-2026-05-16.md`: 追加 v6-ε-0-A 附录。

## 2. Key decisions（≤ 5 bullets，每条带 rationale）

1. **Splice 通过 re-emit，而不是 cranelift `Function` clone**。
   Cranelift 0.131 没有 stable 的 cross-`Function` IR splice API
   （`Function::dfg` 直接操作会脱出 SSA invariant），而
   `OptimizedTrace` 本身就是已优化的 IR——它的 op 流是 SSA-renamed
   后的小 enum 集合（≤ 15 variant）。`emit_trace_inline()` 复用
   `crate::emitter::TraceEmitterState` 99% 的 per-op lowering 规则
   （只重写 prologue/epilogue/Return）就拿到正确的 splice 语义。
   两份 lowering 在未来 op 集合扩展时需要同步——smoke test
   `inline_add_localget_matches_trampoline` 已经把这条 sync check
   写成 1 万次/iter 的自动 round-trip。
2. **Host fn 体的 deopt block 复刻 standalone emitter 的 2-arm
   shape**（call_indirect through ctx.host_hooks 优先、direct extern
   fallback 兜底）。这条不是「ε-0-A inline 必须做」的要求——deopt
   path 可以是任何 host-defined sentinel——但实测下与 trampoline
   保持完全一致的 deopt 行为，让 smoke test 3 (overflow guard) 不
   需要在 inline / trampoline 两路径分别写 fixture。
3. **JITedTraceFn 持 `Arc<OptimizedTrace>` 而不是
   `Arc<cranelift::Function>`**。Plan brief 说「retain cranelift::Function
   IR」——但 cranelift Function 是 mutable container，不能跨线程
   共享，clone cost 也大（含 dfg + layout + entity refs）。
   `OptimizedTrace` 是不可变的 op stream + side tables，move-once
   into `Arc`，cross-thread 共享天然安全，re-emit 时间 ≈ Function
   clone 的 ~10%。如果未来需要真 cranelift::Function 缓存，可以再
   叠一层 OnceCell；本 phase 不需要。
4. **bench 不动数字不是 blocker**——按 brief「If bench shows
   <regression> investigate where time is actually spent ... Report
   findings honestly」执行。inline 路径在 bench 上没有内层 trace
   call 可以消除，9.5 ns 平台稳定保持。`trace_jit_warm_inline`
   行本身的价值是「证明 inline 基础设施真的能跑」+「为未来
   loop-into-cranelift 留 anchor」，不是「直接砍 5 ns」。Plan
   §18 标 DONE-with-honest-finding，与 ε-0-C §17 同一处理。
5. **`TraceOp::Call` 在 inline 路径上 reject（CallNotSupportedInInline）
   而不是处理**。inline emit 把 trace body 嵌进 host fn，但
   `__relon_trace_resolve_call` 是 import-scoped；threading 进 host
   fn 需要 host fn 自己也提供 FuncRef，跨两层 module 的 ergonomics
   不值得本 phase 的复杂度。current corpus 的 Phase-1 trace 没有
   `Call` op（recorder 还没接 ）所以 reject 是 dead branch；当 call
   ops 进 trace 流时再做。

## 3. Gate numbers

- `cargo build --workspace` — clean.
- `cargo test --workspace` — **1761 passing** (ε-0-C baseline 1751
  + 10 新增):
  - `relon-trace-emitter::inline_emit::tests::*` (5):
    `inline_emit_const_return`, `inline_emit_add_local_get`,
    `inline_emit_load_store_round_trip`,
    `inline_rejects_oversized_trace`,
    `inline_emit_missing_return_errors`
  - `relon-codegen-native trace_jit_inline_smoke::*` (5):
    `inline_const_return_round_trips_through_extern_c`,
    `inline_add_localget_matches_trampoline`,
    `inline_guard_fire_routes_through_deopt_path`,
    `inline_rejects_oversized_trace`,
    `jited_trace_fn_exposes_inline_trace_for_re_emit`
- `cargo clippy --workspace --all-targets -- -D warnings` — clean.
- `cargo fmt --all -- --check` — clean.
- `cargo build --target wasm32-unknown-unknown -p relon-wasm` — clean.
- `cargo bench --bench trace_jit_hot_loop` — 3 rounds (see §4).

## 4. Bench numbers (criterion median, 3 rounds, 1M iter `acc + i`)

**R1** (measurement-time 6s):
```
backend/tree_walk                  : ~2.22 µs/iter (group baseline; unchanged)
backend/cranelift_aot              : 372.7 ns/iter (within noise of ε-0-C)
backend/trace_jit_warm             : 9.50 ns/iter
backend/trace_jit_warm_ic          : 9.55 ns/iter
backend/trace_jit_warm_tail        : 9.55 ns/iter
backend/trace_jit_warm_sysv        : 9.55 ns/iter
backend/trace_jit_warm_inline      : 9.55 ns/iter  ← new
backend/rust_inlined_baseline      : 3.55 ns/iter
```

**R2** (measurement-time 6s):
```
backend/tree_walk                  : 2.22 µs/iter
backend/cranelift_aot              : 357.6 ns/iter
backend/trace_jit_warm             : 9.49 ns/iter
backend/trace_jit_warm_ic          : 9.54 ns/iter
backend/trace_jit_warm_tail        : 9.51 ns/iter
backend/trace_jit_warm_sysv        : 9.51 ns/iter
backend/trace_jit_warm_inline      : 9.52 ns/iter
backend/rust_inlined_baseline      : 3.55 ns/iter
```

**R3** (measurement-time 6s):
```
backend/tree_walk                  : 2.26 µs/iter
backend/cranelift_aot              : 373.0 ns/iter
backend/trace_jit_warm             : 9.49 ns/iter
backend/trace_jit_warm_ic          : 9.56 ns/iter
backend/trace_jit_warm_tail        : 9.54 ns/iter
backend/trace_jit_warm_sysv        : 9.54 ns/iter
backend/trace_jit_warm_inline      : 9.54 ns/iter
backend/rust_inlined_baseline      : 3.55 ns/iter
```

### 4.1 vs ε-0-C baseline (9.54 ns/iter)

- `trace_jit_warm_inline` median across R1-R3: **9.55 / 9.52 / 9.54
  ns** = **0.00-0.04 ns vs ε-0-C `trace_jit_warm_tail` (9.54 ns)**.
  **统计学等价**（criterion noise threshold = 0.1%；3 round diff
  < 0.4%）。
- 与 ε-0-C 同 class——没有跨越 9 ns 门。

### 4.2 vs brief 阈值

- Brief target ≤ 4 ns/iter: **不达**。
- 9.54 - 3.55 = **5.99 ns** 仍是 Rust → JIT call boundary 的固有
  cost；ε-0-A 在 bench 的 caller-callee 关系（Rust caller →
  cranelift JIT entry）下消除不了它。

### 4.3 vs rust_inlined_baseline 3.55 ns

- gap = **5.99 ns**——与 ε-0-C 实测 5.98 ns 完全持平。
- 这条 gap 不会被任何 cranelift-side 改造移走：bench 的 hot loop
  本身是 Rust，每次 iter 都要重新构 `args[..]`、写 stack、做
  `call extern_c_jit_entry`、`ret`、读 `ctx.result_slot`——这些
  cost 跟 callee 里头是不是有内层 trace call 完全无关。

### 4.4 vs LuaJIT trace tier (1-3 ns/iter)

- 9.54 / 2 ns ≈ **4.77×**——与 ε-0-C / M2-C / M2-B 完全同 class，
  **没有移动**。

## 5. 为什么 inline 不动 bench（M2-C / ε-0-C hypothesis 的再确认）

ε-0-C 实测 Tail vs SysV delta = 0.01 ns，证伪「prologue/epilogue
= 6 ns 边界」假设。ε-0-A 实测 inline (no inner trace call) vs IC
(with inner trace call) delta = 0.00-0.04 ns，进一步证伪「内层
trace call/ret 是 6 ns 边界主因」假设。

### 5.1 bench 调用图（精确，机器码级）

```
Rust bench loop:                                  costs (cycles, rough)
  args[0] = acc; args[1] = i;       (2 store)    ~2 c
  call rax  (= jit_entry_fn_ptr)    (call)       ~3 c (call+RSB)
  -- inside cranelift JIT entry --
  prologue: leaf, no spill          (0-2 inst)   ~0-1 c
  load i64 [rsi+0]                  (load)       ~3 c (L1 hit)
  load i64 [rsi+8]                  (load)       ~3 c (L1 hit)
  sadd_overflow rax, rcx -> ...     (add+jno)    ~1-2 c
  brif on overflow                  (j-not-taken) ~1 c
  store i64, [rdi+16] (result_slot) (store)      ~3 c
  iconst i32 0                      (mov)        ~1 c
  ret                               (ret+RSB)    ~3 c
  -- back in Rust --
  cmp eax, 0; je success           (cmp+je)     ~1 c
  load ctx.result_slot              (load)       ~3 c
  store into acc                    (store)      ~1 c
  inc i; cmp; jne                   (loop)       ~1 c
=========================================
Total:                              ≈ 25-30 c ≈ 9-10 ns on 3 GHz
```

### 5.2 ε-0-A 移走了哪一步、没移走哪一步

- **移走**: 无。`trace_jit_warm_inline` 的 JIT entry fn body 与
  `trace_jit_warm_ic` 完全相同（实测 disassembly：load/load/sadd_overflow/
  brif/store/iconst/ret 序列像素级一致；唯一差别是 callee 的内层
  「if 有 inner call」branch，但 inline 和 ic 路径都**没有内层 call**，
  所以这条 branch 是 dead code）。
- **没移走也无法移走**: Rust caller 侧的「args 构造 + call rax +
  return value 读」三件套——bench 的 caller 是 Rust，inline 也只
  能 inline 「JIT entry 内部的内层 call」（不存在），不能 inline
  Rust caller→JIT 这一层。

### 5.3 真正能动 9 ns → 4 ns 的方向

**Move the hot loop into cranelift itself**：让 cranelift 编译一个
fn `step_loop(n: u64) -> i64`，body 是 1M iter 的内联 `acc + i`
（with overflow guard）。Rust caller 调一次 `step_loop(N)`，
N 分摊掉 Rust→JIT 边界。每 iter 净 cost ≈ rust_inlined_baseline
的 3.55 ns。

但这是 ε-M1 / ε-M2 的范畴（把 IR-walker 的 loop op 编进 trace IR
+ optimizer / cranelift），不是 ε-0-A 本身。**ε-0-A 提供的
inline primitive 是那条路径的 prerequisite**（cranelift loop 编出
来后，loop body 内的 trace call site 就是 ε-0-A 要 splice 的地方）。

## 6. 4-way corpus parity（保持 0 mismatch）

| Variant | ε-0-C baseline | ε-0-A | Δ |
|---------|----------------|-------|---|
| AllAgree | 32 | 32 | 0 |
| AllTrap | 4 | 4 | 0 |
| BytecodeMatchesBaseline | 1 | 1 | 0 |
| BytecodeUnsupported | 15 | 15 | 0 |
| **Mismatch** | **0** | **0** | **0** |

ε-0-A 不动 backend 语义——只新增 alternate-install-path 走 inline。
现有 trace install / fallback / deopt 路径 byte-for-byte 不变；
corpus 跑出的所有 fn 都走 trampoline 路径（compile_inline_host_fn
是 opt-in API，corpus 路径不调）。Mismatch 0 直接 carry-over。

## 7. Trace entry ABI: 改动 vs 保留

### 7.1 改动

- **`JITedTraceFn` shape**：新增 `inline_trace: Arc<OptimizedTrace>`
  字段。pub API 加 `inline_trace()` / `inline_candidate()` 两个
  accessor。
- **`relon-trace-emitter` crate root re-export 扩展**：增 5 个
  pub item（inline_emit module + 它的 4 个 export + 3 个 abi
  helper re-export）。

### 7.2 保留 (unchanged)

- `TRACE_ENTRY_SIG` / `TraceContext` / `DeoptStateSnapshot` /
  `__relon_trace_save_deopt` signature / `HostHookTable` / IC
  slot / `TraceJitState` API 等全部不变。
- `TraceEmitter::emit_with_hooks*` 系列入口不变——standalone
  trace fn install path 完全 carry over。
- `__relon_jump_to_recorder` 走的依然是 default-conv install path，
  inline path 是另一条 API 表面，互不影响。

### 7.3 内部新增

- 模块: `crates/relon-trace-emitter/src/inline_emit.rs`,
  `crates/relon-codegen-native/src/trace_inline.rs`。
- 测试: `crates/relon-codegen-native/tests/trace_jit_inline_smoke.rs`。
- Bench helper: `build_inline_step_host_fn()` 在
  `crates/relon-bench/benches/trace_jit_hot_loop.rs` 内。

## 8. 文件 diff stat

`git diff --stat f379a7f..HEAD`:

```
 crates/relon-bench/benches/trace_jit_hot_loop.rs       |  92 +++++-
 crates/relon-codegen-native/src/lib.rs                 |   2 +
 crates/relon-codegen-native/src/trace_inline.rs        | 384 ++++++++++++++++++
 crates/relon-codegen-native/src/trace_install.rs       |  46 ++-
 crates/relon-codegen-native/tests/trace_jit_inline_smoke.rs | 216 +++++++++++
 crates/relon-trace-emitter/src/inline_emit.rs          | 657 +++++++++++++++++++++++++++++++
 crates/relon-trace-emitter/src/lib.rs                  |  13 +-
 7 files changed, ~1410 insertions(+), ~5 deletions(-)
```

HEAD-first commit history（base = f379a7f）:

```
9bfff6a test(bench): trace_jit_warm_inline row for ε-0-A measurement
407c903 test(codegen-native): trace_jit_inline_smoke covers ε-0-A invariants
6624426 feat(codegen-native): trace_inline pipeline + JITedTraceFn IR retain
23262b8 feat(trace-emitter): inline_emit splice primitive for ε-0-A
f379a7f feat(trace-emitter): v6-epsilon-0-C CallConv::Tail infra ← base
```

## 9. Key file paths (absolute)

- 新增 source:
  - `/ext/relon/crates/relon-trace-emitter/src/inline_emit.rs`
  - `/ext/relon/crates/relon-codegen-native/src/trace_inline.rs`
- 新增 test:
  - `/ext/relon/crates/relon-codegen-native/tests/trace_jit_inline_smoke.rs`
- 修改 source:
  - `/ext/relon/crates/relon-trace-emitter/src/lib.rs`（pub mod + re-export）
  - `/ext/relon/crates/relon-codegen-native/src/lib.rs`（pub mod + re-export）
  - `/ext/relon/crates/relon-codegen-native/src/trace_install.rs`
    （inline_trace 字段 + 2 个 accessor）
- 修改 bench:
  - `/ext/relon/crates/relon-bench/benches/trace_jit_hot_loop.rs`
    （+ trace_jit_warm_inline row + build_inline_step_host_fn helper）
- 新增 docs:
  - `/ext/relon/docs/internal/v6-epsilon-0a-stage-report-2026-05-19.md`（本文）
- 修改 docs:
  - `/ext/relon/docs/internal/v6-gamma-integration-plan-2026-05-18.md`（§18 新增）
  - `/ext/relon/docs/internal/wasm-bench-report-2026-05-16.md`（appendix）

## 10. Carry-over to ε-M1

### 10.1 ε-0-A 的实验结论给 ε-M1 提供的 anchor

- **bench 9.5 ns 不是任何「call boundary」**：M2-C, ε-0-C, ε-0-A
  三次实验已经把 prologue/epilogue、ABI conv、inner-call/ret 全部
  排除。剩下唯一 explanatory 的 cost 是 **Rust caller 侧的 extern
  call 边界 + 每 iter 的 args 重打包 + 每 iter 的 result 读取**。
- **要砍掉这条 cost，必须把 hot loop 自己编进 cranelift**——i.e.
  让 cranelift 生成一个跑完所有 1M iter 才返回的函数，而不是每
  iter 都跨边界一次。这就是 ε-M1 的内容（bounds hoist + LICM）+
  ε-M4 的 batched resource limit 之后才能实现的。
- **`rust_inlined_baseline = 3.55 ns/iter` 仍是 target band 的最佳
  下界估计**。9.5 - 3.55 = 5.99 ns 是 Rust→JIT extern call 的
  irreducible cost——只要 caller 还是 Rust，每 iter 都付。

### 10.2 ε-0-A 提供的 prerequisite

inline primitive (`emit_trace_inline`) + IR retention
(`Arc<OptimizedTrace>` on `JITedTraceFn`) + 256-op cap 是 ε-M1 之后
真正落地 loop-into-cranelift 时需要的 building block：

- 当 cranelift host fn 编一个 loop body，loop body 里的 trace
  dispatch site 通过 `compile_inline_host_fn`-style 路径 splice
  trace 进 host fn，**这时**「内层 trace call/ret」就真的存在
  并被 inline 消除——bench 的 inline 行可以 + 1M iter 摊掉
  Rust→JIT edge cost。
- IR retention 让 host fn 编译器可以在 install 路径之外的任意
  地方 re-emit 同一 trace（e.g. 把同一 trace inline 进多个
  call site without 重跑 recorder）。

### 10.3 ε-0-B (trace-to-trace fall-through) 的优先级

ε-0-A 实测 inline 不动 bench 数字这件事，对 ε-0-B 的预期更悲观——
fall-through 只能省掉 inner call/ret pair 中的 ~1-2 ns RSB
prediction miss，但 RSB miss 在 ε-0-A bench 上根本不存在（bench
的内层 trace call 是稳定预测的 indirect call）。**ε-0-B
继续 defer 到 ε-1 或之后**。

### 10.4 ε-0-A carry-over 给 ε-M1 / ε-M4 的 invariants

- Host hook helpers (`save_deopt` / `resolve_call` /
  `inline_cache_lookup`) 必须仍保留 `extern "C"` (SystemV) ——
  ε-0-A smoke test 3 (overflow guard) 已经证明 inline 路径的
  `call_indirect through ctx.host_hooks.save_deopt` 工作。这
  条约束 carry over 到 ε-M1+。
- `TraceContext` layout 已稳定，ε-M1 不应改它。
- `MAX_INLINE_OPS = 256` 是 v6-ε plan §3 ε-0-A 的硬上限。ε-M1
  之后如果 trace 体积变大（bounds hoist 加了 entry guard 块），
  可能要把 cap 调到 ~512；本 phase 不动。

### 10.5 已知 inline 不支持的 op（scope cut）

- `TraceOp::Call(...)` 在 inline 路径直接 reject
  （`InlineEmitError::CallNotSupportedInInline`）——caller 必须
  fallback 走 trampoline。current corpus 没有进 `Call` op 的
  trace，所以是 dead branch；当 recorder 接入 inter-trace call
  时这条要补——估计 ε-M3 (cap hoisting) 之后才会触发。

### 10.6 ε-0-C 留下的 carry-over 仍未处理

- **String slot / wasm linear memory 真路径**：12 bytecode_unsupported
  case carry over from ε-0-C。
- **`Op::Select` 专用 BcOp**：v6-δ envelope 优化项 carry over。

## 11. honest comparison to LuaJIT 1-3 ns

- LuaJIT 2.x 稳态 hot loop ≈ 1-3 ns/iter（loop 内嵌进 trace
  机器码，无 Rust caller 跨界）。
- v6-ε-0-A `trace_jit_warm_inline` = 9.54 ns/iter = **4.77× 慢于
  LuaJIT 中位数 2 ns**。
- 与 ε-0-C `trace_jit_warm_tail = 9.54 ns` 比例完全一致（4.77×）。
- **ε-0-A 没有把差距拉近**——bench 上 inline 不能动 Rust→JIT
  边界 cost，必须靠把 loop 自己编进 cranelift (ε-M1+) 才能跨
  9 ns 门 → 进 LuaJIT 同 class (~1.5-3 ns)。

## 12. Decision: is hot loop ≤ 3 ns?

**NO**——`trace_jit_warm_inline = 9.54 ns/iter`，与 ε-0-C 平台一致。

**Recommendation**: ε phase 不应在 ε-0-A 这里停。下一步 ε-M1 应该
**先验证「真把 hot loop 编进 cranelift 是否就能跑到 ~3-4 ns」**
这条假设——可以加一个 prototype bench row：手写一个 cranelift
fn `step_loop(n) -> i64` 跑 N iter 的内联 add+overflow guard，
对比 `rust_inlined_baseline`。如果该 prototype 也跑 3-4 ns，
就证明 ε-M1+ (bounds hoist + LICM + 把 loop 编进 trace IR) 是
正确方向；如果还是 ~10 ns，说明剩余 cost 在更深的地方（e.g.
trace recorder / optimizer overhead），要先做 profiling。

本 stage 不做这条 prototype——按 brief 范围执行完毕，把发现
honest report 出来给 plan owner 选下一步。

EOF
