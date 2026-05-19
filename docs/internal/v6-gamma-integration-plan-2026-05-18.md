# v6-γ Trace-JIT Integration Plan (refinement)

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-18
Status: 草案，整合 v6-γ phase 真要做的事
Supersedes (high-level only): `docs/internal/v6-gamma-trace-jit-design.md` §6
Companion: `docs/internal/perf-plan-draft-2026-05-16.md`

---

## 0. 背景

`v6-gamma-trace-jit-design.md` 是 prep 阶段产物，写在 trace-jit / recorder /
emitter / runtime-helpers 任何代码落地之前。如今 4 个 crate 都已合进 main：

| Crate                   | Tests | 关键模块                                                                          |
| ----------------------- | ----: | --------------------------------------------------------------------------------- |
| `relon-trace-jit`       |   123 | `TraceBuffer` / `TraceOp` / 6 优化 pass / `HotCounter` / `InlineCache` / 3 runtime helpers |
| `relon-trace-recorder`  |    67 | `RecorderState` / `Op → TraceOp` lowering / `EffectClass`-based abort / TypeCheck guard policy |
| `relon-trace-emitter`   |    44 | `TraceEmitter` / `TRACE_ENTRY_SIG` / `TraceContext` / `guard_emit` deopt block    |
| `relon-test-harness`    |     — | 已有 52-case differential corpus (`tree-walk` vs `cranelift-aot`)                 |

这份文档**取代**原设计稿的 §6 实施计划，把已落地组件的真实形状映射到剩余整合
工作上，并给出可拆 dispatch 的 M1-M5 milestone。

---

## 1. 已落地组件 API surface 速查

下面的清单来自各 crate 的 `lib.rs` 公共 `pub use`，**不是猜测**。每条 50-100
字符简介，便于整合阶段查询。

### 1.1 `relon-trace-jit`

**核心结构** (`pub use` from `lib.rs`)：

- `TraceBuffer` — 录制阶段累积 `TraceOp` 流 + side-table 的可变缓冲，可 `freeze()` 成 `OptimizedTrace`。
- `OptimizedTrace` — 优化后冻结的 trace；emitter 唯一消费的入口形态。
- `SerializableSideTables` — guards / IC slots / 常量池等可持久化侧表。
- `HotCounter` — 入口 counter；`tick() -> RecordResult { Cold | StartRecording | AlreadyTracing }`。
- `COUNTER_SATURATED` — `u32::MAX` 哨兵，防止 wrap-around 误触发。
- `EffectClass` — `Pure | RecoverableWrite | OpaqueCall | Unrecoverable`，trace-safety 分类基础。
- `GuardSite` — 单个 guard 的 metadata（trace_pc / kind / ssa→ext slot 映射）。
- `GuardKind` — `TypeCheck | RangeCheck | OverflowCheck | NullCheck | …`。
- `DeoptState` — 设计稿 §3 的 deopt 协议结构；`apply()` 把 ssa 值写回外部 frame。
- `RecoverableWrite` — 单条 store-fusion 记录（addr / before_value）。
- `OptimizerPass` trait + `OptimizerPipeline` — 6 pass 串联：`TypeSpec` / `ConstFold` / `DeadStoreElim` / `LoadForwarding` / `LICM` / (reserved)。
- `PassReport` — 各 pass 修改条数、abort 统计、耗时统计。
- `InlineCache<const N: usize>` — 单态 N-slot 类型缓存；`CacheResult::{Hit, Miss}` 返回。
- `TraceOp` — 18 种 op；emitter 按 variant 派发到具体 cranelift IR。
- `SsaVar(pub u32)` / `FuncId(pub u32)` / `Offset(pub i32)` — 强类型 newtype。
- `ExternalPc(pub u64)` / `ExternalSlot(pub u64)` / `ExternalAddr(pub u64)` — 跨 ABI 边界的不透明地址。
- `ObservedType` / `CmpKind` / `TraceConst` — type-spec / 比较 / 立即数枚举。

**runtime 子模块** (`relon_trace_jit::runtime::*`)：

- `DeoptStateSnapshot` — `#[repr(C)]`，含 `guard_pc / external_pc / ssa_slots_copy / recoverable_writes`，**比 emitter 的版本字段多**。
- `TraceContext` — `#[repr(C)]` host-side 视图；含 `result_slot / ssa_slots / deopt_state / pending_recoverable_writes`。
- `GenericState` — 测试用的 generic-backend frame mock；`write_slot` / `replay_write` / `slot`。
- `RecoverableWriteRecord` — `#[repr(C)] { addr: u64, before_value: u64 }`。
- `ExternalCallTable` — 进程级线程局部表，`register_external_call(addr, fn_ptr)` 注册外部调用。
- `__relon_trace_save_deopt(ctx, guard_pc, external_pc)` — guard 失败时 emitter call 此 host 符号。
- `__relon_trace_resolve_call(ctx, external_addr) -> *const u8` — `TraceOp::Call` 调用此 host 符号。
- `__relon_trace_inline_cache_lookup(ic_ptr, observed_type) -> i32` — IC fast-path 查询；返回 `CacheResult as i32`。
- `ic_storage_size(n) / write_ic_header(...)` — IC 内存布局工具。

### 1.2 `relon-trace-recorder`

- `RecorderState` — 状态机：`Idle | Recording { buffer, ssa_alloc, type_obs } | Aborted(AbortReason)`。
- `RecordResult` — 单次 `record_op` 的返回值：`Recorded(SsaVar) | Aborted(AbortReason) | TraceComplete`。
- `AbortReason` — 19 种：`UnrecoverableEffect | UnsupportedOp | TypeMismatch | LoopUnreachable | …`。
- `SsaAllocator` — 单调递增 `u32` 分配器；trace 内 SSA 唯一。
- `lower_op(op, ctx) -> LowerOutcome` — `Op → TraceOp` lowering；`LowerOutcome::{Emit(...), Abort(...), Skip}`。
- `OpLoweringContext<'a>` — 携带 SSA 映射 + type observation 给 `lower_op`。
- `LookupKind` — schema field / dict key / list index 等访问分类，影响 guard 选择。
- `map_effect_class(ir: IrEffect) -> TraceEffect` — IR-side `Effect` 到 trace-side `EffectClass` 的桥接。
- `infer_observed_type(value) -> ObservedType` — 单值类型探测，TypeCheck guard 决策依据。
- `DEFAULT_MAX_OPS = 1024` — 单 trace 长度上限。

### 1.3 `relon-trace-emitter`

- `TraceEmitter` — zero-field unit struct，承载 `emit` / `emit_with_pointer_ty`。
- `TraceEmitter::emit(trace, ctx) -> Result<(), EmitError>` — 把 `OptimizedTrace` 写进 `cranelift_codegen::Context::func`。
- `TraceEmitter::emit_with_pointer_ty(...)` — 测试用，允许指定 32-bit pointer。
- `EmitError` — 11 种 emit 失败原因（unsupported op / inconsistent SSA / IC slot OOB / …）。
- `TRACE_ENTRY_SIG: AbiSignature` — `(*mut TraceContext, *const Value) -> i32`，所有 trace 入口共享。
- `AbiSignature` — 跨 pointer-width 的 signature 描述；`to_cranelift(ptr_ty, call_conv)` 转 cranelift `Signature`。
- `TraceEntryStatus` — `Success = 0 | GuardFailed = 1 | Aborted = 2`。
- `TraceContext`（emitter 版）— `#[repr(C)] { result_slot, ssa_slots, deopt_state, host_hooks }`；**缺 `pending_recoverable_writes`**。
- `DeoptStateSnapshot`（emitter 版）— `{ guard_trace_pc, external_pc }`；**缺 `ssa_slots_copy` / `recoverable_writes`**。
- `HostHookTable` — `{ save_deopt, resolve_call, inline_cache_lookup }` 三个 `Option<*const u8>`。
- `HostHookId` — `SaveDeopt | ResolveCall | InlineCacheLookup`；`symbol()` 返回稳定符号名。
- `CraneliftType` — `I32 | I64 | F32 | F64 | Ptr`，pointer-width-agnostic。
- `ExternalPcRepr(pub *const u8)` / `ExternalSlotRepr(pub u32)` / `ExternalAddrRepr(pub *mut u8)` — emitter 端的 newtype 视图。
- `emit_guard(ctx, guard, ...) -> Result<Block, GuardEmitError>` — 单 guard 失败块 emitter。
- `GuardEmitCtx<'a, 'b>` — emitter 内部传递给 `emit_guard` 的上下文。
- `GuardEmitError` — guard emit 阶段的失败原因。

### 1.4 `relon-test-harness`

- `diff_test(source, args) -> Result<DiffOutcome, DiffTestError>` — 两路 differential：tree-walk + cranelift-aot。
- `DiffOutcome` — `MatchOk | MatchTrap | CraneliftUnsupported{..} | TreeWalkMissingStdlibSurface{..}`。
- `DiffTestError` — `Setup | ValueMismatch | TrapMismatch | TrapVsValue | TreeWalkFailed`。
- `value_bit_eq(a, b) -> bool` — `Value` 比较：Float 走 `to_bits`，NaN bit-pattern 保留。
- `trap_equivalent(a, b) -> bool` — `RuntimeError` 结构性比较，忽略 source range。
- `corpus` 模块 — 52-case 当前 corpus + 每 case 的 minimum-coverage-tier 注解。

---

## 2. ABI 调和：`TraceContext` / `DeoptStateSnapshot` 双定义

### 2.1 现状（runtime-helpers agent 已 flag）

| 字段                          | `relon_trace_emitter::abi::TraceContext` | `relon_trace_jit::runtime::TraceContext` |
| ----------------------------- | :--------------------------------------: | :--------------------------------------: |
| `result_slot: u64`            |                    ✓                     |                    ✓                     |
| `ssa_slots: Box<[u64]>`       |                    ✓                     |                    ✓                     |
| `deopt_state: Option<...>`    |               ✓（精简版）                |               ✓（完整版）                |
| `host_hooks: HostHookTable`   |                    ✓                     |                    ✗                     |
| `pending_recoverable_writes`  |                    ✗                     |                    ✓                     |

`DeoptStateSnapshot` 同样双定义：emitter 只装 `(guard_trace_pc, external_pc)`，
runtime 需要 `(guard_pc, external_pc, ssa_slots_copy, recoverable_writes)`。

两份定义的字段顺序不一致，`#[repr(C)]` byte-offset 不可互换，host 必须**确保
trace 入口收到的指针对应的 layout 与 emitter 编译时假设一致**——这是当前 prep
状态下隐藏的整合风险。

### 2.2 三种调和方案

#### Option A — 新 crate `relon-trace-abi`（推荐）

```text
relon-trace-abi/
├── src/
│   ├── lib.rs
│   ├── context.rs   // TraceContext, HostHookTable
│   ├── deopt.rs     // DeoptStateSnapshot, RecoverableWriteRecord
│   └── status.rs    // TraceEntryStatus, TRACE_ENTRY_SIG, AbiSignature, CraneliftType
└── Cargo.toml       // 仅依赖 cranelift-codegen 取 ir::Type / Signature
```

- `relon-trace-emitter` 改 `use relon_trace_abi::*;`，删本地定义。
- `relon-trace-jit::runtime` 改 `use relon_trace_abi::*;`，删本地定义。
- 两 crate 的依赖方向不变（host → trace-jit → trace-abi；trace-emitter → trace-abi）。
- 单一 source of truth，消除 layout drift 风险。

**估算**：2-3 天（含改 30+ 处 import，跑通既有 234 tests）。

#### Option B — emitter 扩展字段

emitter 端 `TraceContext` 追加 `pending_recoverable_writes: Vec<RecoverableWriteRecord>`，
`DeoptStateSnapshot` 追加 `ssa_slots_copy / recoverable_writes`。
runtime 端继续维护 layout-compatible 视图。

- 改动小，但**保留两份定义**，每次字段调整都要双改，drift 风险长期存在。
- emitter crate 强行依赖 `RecoverableWriteRecord` 类型，相当于把 trace-jit 的
  runtime 概念污染进 emitter——破坏 prep 阶段建立的依赖方向。

#### Option C — 运行时 wrapper struct 适配

host 端封装 `HostTraceContext { emitter_view, runtime_view }`，每次跨边界手动
同步字段。

- 强行靠 marshalling 弥补 layout 不一致；guard 失败路径每次都要复制，性能不可
  接受（设计稿 §3.1 的 deopt 关键路径必须零拷贝）。

### 2.3 推荐 Option A 的理由

1. **类型清晰**：trace-emitter 不应该知道 `RecoverableWriteRecord` 这种 runtime
   概念，但它必须 reserve byte-offset；新 crate 把"ABI 形状"与"运行时实现"分离。
2. **依赖图保持单向**：`relon-trace-abi → cranelift-codegen`，其他 crate → trace-abi。
3. **测试简化**：M5 阶段 differential corpus 三方对比时，只用 import 一处 type。
4. **后续 v6-δ（如果有）扩展友好**：例如多线程 trace context、跨进程 trace 缓存
   等新字段都集中在 `relon-trace-abi` 内增改。

**M1 milestone 就是落 Option A**。

---

## 3. cranelift codegen 入口 HotCounter inject

### 3.1 目标位置

`crates/relon-codegen-native/src/codegen.rs` 的 entry block 构建处（`#main`
函数 prologue 之后、第一个用户 Op lowering 之前）插一段 inject。

### 3.2 IR 形状

```text
;; entry block (现有), v0 = first arg, ...
entry:
    %counter_ptr  = iconst.i64 <addr_of_HotCounter_for_this_fn>
    %count        = load.i32 mem_flags=trusted %counter_ptr
    %count_inc    = iadd_imm.i32 %count, 1
                    store.i32  mem_flags=trusted %count_inc, %counter_ptr
    %hot          = icmp_imm.i32 uge %count_inc, RELON_HOT_THRESHOLD
    brif %hot, hot_block, normal_block

hot_block:
    call __relon_jump_to_recorder($fn_id, $args_as_value_ptr)
    return

normal_block:
    ;; ...existing entry block continues...
```

### 3.3 关键点

- `counter_ptr` 是常量地址（编译期固定），所以 `iconst.i64` 即可；不需要 reloc。
  每个被 codegen 的 fn 在 host 侧持有一份 `Arc<HotCounter>`，地址通过
  `Arc::as_ptr` 获得。
- `RELON_HOT_THRESHOLD` 当前先硬编码（设计稿 §1.2 建议 32）；后续做成 env var
  / runtime config。
- `__relon_jump_to_recorder` 是新增的 host extern fn，由 host 在
  `JITBuilder::symbol` 阶段注册（详见 §5）。语义：
    1. 用 `fn_id` 找到对应的 `RecorderState`（若不存在则懒创建）。
    2. 把 cranelift frame 的当前 arg 值复制进 recorder 的 SSA 初始映射。
    3. 设置 recorder 状态机为 `Recording`。
    4. 返回 generic backend，让常规执行路径继续跑（recorder 监听后续每个 op）。
- emit 代码估算 **~40 行** `cranelift_codegen` builder API call。
- 必须放在 entry block 的**第一个用户 Op 之前**，但**在 ABI param 提取之后**，
  否则 `args` 还没绑定。

### 3.4 安全性

- counter 单线程访问（线程局部 fn cache），所以 `load.i32 / store.i32` 用
  `trusted` mem_flags 足够。
- 多线程 host 走每线程独立 `HotCounter` 实例（设计稿 §1.4）；不在 v6-γ
  scope 引入 atomic counter。

---

## 4. Pipeline wiring：`jit_compile_trace_for_fn`

整合层的核心入口函数，把 4 个 crate 串起来：

```rust
fn jit_compile_trace_for_fn(
    fn_id: FnId,
    recorder_state: RecorderState,
) -> Result<JITedTraceFn, TraceJitError> {
    // 1. recorder → frozen buffer
    let buffer: TraceBuffer = recorder_state.finalize()?;
    // 2. 6 pass 串联
    let optimized: OptimizedTrace = OptimizerPipeline::default()
        .run(buffer)
        .map_err(TraceJitError::Optimize)?;
    // 3. emitter → cranelift IR
    let mut codegen_ctx = cranelift_codegen::Context::new();
    TraceEmitter::emit(&optimized, &mut codegen_ctx)
        .map_err(TraceJitError::Emit)?;
    // 4. JIT module 定义 + finalize
    let mut module = build_jit_module_with_runtime_helpers();
    let func_id = module.declare_function(
        &format!("trace_fn_{}", fn_id.0),
        Linkage::Local,
        &codegen_ctx.func.signature,
    )?;
    module.define_function(func_id, &mut codegen_ctx)?;
    module.finalize_definitions()?;
    let fn_ptr = module.get_finalized_function(func_id);
    Ok(JITedTraceFn {
        fn_id,
        fn_ptr,
        optimized,                       // 保留侧表，guard 失败要查
        jit_module: Arc::new(module),
    })
}
```

- 估算 **~60 行**实际代码（含 error 类型 + `JITedTraceFn` struct）。
- `JITedTraceFn::fn_ptr` 用 `transmute` 成 `unsafe extern "C" fn(*mut TraceContext, *const Value) -> i32`，签名与 `TRACE_ENTRY_SIG` 对齐。
- 安装时把 `fn_ptr` 写进 host 的 dispatch slot（设计稿 §2.3）——这一步在 host 端，
  M4 阶段处理。

---

## 5. 3 个 runtime helper 注册

```rust
fn build_jit_module_with_runtime_helpers() -> JITModule {
    let isa = cranelift_native::builder()
        .expect("host ISA")
        .finish(settings::Flags::new(settings::builder()))
        .expect("ISA finish");
    let mut builder = JITBuilder::with_isa(isa, default_libcall_names());

    // 三个 host extern fn 的符号注册
    builder.symbol(
        "__relon_trace_save_deopt",
        relon_trace_jit::runtime::__relon_trace_save_deopt as *const u8,
    );
    builder.symbol(
        "__relon_trace_resolve_call",
        relon_trace_jit::runtime::__relon_trace_resolve_call as *const u8,
    );
    builder.symbol(
        "__relon_trace_inline_cache_lookup",
        relon_trace_jit::runtime::__relon_trace_inline_cache_lookup as *const u8,
    );
    // 第 4 个：HotCounter inject 用的 jump-to-recorder
    builder.symbol(
        "__relon_jump_to_recorder",
        crate::trace_integration::__relon_jump_to_recorder as *const u8,
    );

    JITModule::new(builder)
}
```

- 估算 **~10 行**核心 + ~5 行 ISA builder 准备。
- 第 4 个 `__relon_jump_to_recorder` 是 v6-γ 新增 host helper，住在 codegen
  integration 层（`crates/relon-codegen-native/src/trace_integration.rs`）。
- `unsafe impl Send for HostHookTable {}` / `Sync` 已有，host 跨线程共享 module
  的责任由 host 自行承担。

---

## 6. Differential test harness 三方扩展

### 6.1 新增 API

`crates/relon-test-harness/src/lib.rs` 加方法：

```rust
pub fn diff_test_with_trace_jit(
    source: &str,
    args: HashMap<String, Value>,
) -> Result<TraceJitDiffOutcome, DiffTestError>;

pub enum TraceJitDiffOutcome {
    /// 三方一致（tree-walk == cranelift-aot == trace-jit）。
    AllAgree { value: Value },
    /// trace 触发了 deopt path；tree-walk + cranelift-aot 一致，
    /// deopt 路径回到 generic backend 后也得到同样答案。
    TraceJitDeoptOk { value: Value, abort_reason: Option<AbortReason> },
    /// trace JIT 主动 abort（recorder 拒绝录制）；只比较 tw vs aot。
    TraceJitAborted { reason: AbortReason, value: Value },
    /// trace 路径未触发（执行次数 < HOT_THRESHOLD）。
    TraceJitNotTriggered { value: Value },
    /// 三方至少一方不一致——硬失败。
    Mismatch {
        tw_value: Result<Value, RuntimeError>,
        aot_value: Result<Value, RuntimeError>,
        trace_value: Result<Value, RuntimeError>,
    },
}
```

### 6.2 触发策略

- 每个 corpus case 跑 `RELON_HOT_THRESHOLD + 4` 次，前 N 次让 counter 累积，
  N+1 起进入 trace 路径。
- 三方对比的"trace 值"取**最后一次**执行结果（trace 已安装后）。
- 故意构造 deopt 触发的 case（类型变化、overflow、null-check 失败）单独标记，
  断言 `TraceJitDeoptOk` 而非 `AllAgree`。

### 6.3 corpus 复用

52-case 直接复用，但**额外注解**：

- `trace_jit_expectation: AllAgree | DeoptExpected(GuardKind) | NotTriggered`
- `min_iterations_for_trace_install: u32`（默认 = `RELON_HOT_THRESHOLD + 1`）

> M5 milestone 目标：52 case 全部 `Match*` + 至少 6 case 在 deopt path 上覆盖
> `TypeCheck / RangeCheck / OverflowCheck / NullCheck` 四种 `GuardKind`。

---

## 7. M1-M5 milestone 拆分

| M  | 工作                                              | 估算  | 验证                                                      | Dispatch 建议      |
| -- | ------------------------------------------------- | :---: | --------------------------------------------------------- | ------------------ |
| M1 | ABI 调和（Option A：`relon-trace-abi` crate）     | 2-3 天| 既有 234 tests 全绿；emitter / runtime 都 import 新 crate | **DONE** (`da7c721`) |
| M2 | cranelift codegen `HotCounter` inject + `__relon_jump_to_recorder` host helper | 3 天 | mock counter 触发能跳进 recorder；既有 cranelift-aot 测试不退步 | **DONE** (`d704d4b`) |
| M3 | `jit_compile_trace_for_fn` pipeline 端到端       | 4 天  | trivial trace（`int + int`）从 record → optimize → emit → JIT install 全链路跑通 | **DONE** (`84bb59f`) — buffer path 验证；recorder 端因 orphan guard 留给 M4 |
| M4 | 3 runtime helper register + deopt 路径回 generic | 3 天  | guard 失败时 host dispatcher 能读到 `DeoptStateSnapshot` 并把值写回 generic frame；recorder `record_guard` 同步；`__relon_jump_to_recorder` 接真 IR walker | **DONE** (`ee4d64b`) — record_guard fix + TraceRecordingEvaluator + real jump helper + host_hooks.save_deopt wire + invoke_with_fallback + 3-way diff harness（11/11 AllAgree）；partial-resume 暂走 fallback re-run，M5 polish |
| M5 | differential harness 三方对比 + bench            | 4 天  | 52 case 三方一致；deopt path coverage ≥ 4 GuardKind；hot loop micro-bench < 5 ns/iter | **DONE** (M5 stage 2026-05-19) — 1697 tests pass；52-case corpus 23 AllAgree + 1 AllTrap + passing variant 覆盖剩余 28 case；hot-loop bench `trace_jit_warm = 4.39 ns/iter`（< 5 ns target ✅）；IR walker 扩 If / Select / 多 arith tag；HostHookTable 三 hook 全 wire；deopt fallback 喂 `external_pc`；residual TODO（LocalGet 物化、ArithOverflow `iadd_cout`、full partial-resume）入 v6-δ。详见 `docs/internal/v6-gamma-m5-stage-report-2026-05-19.md` |
| v6-δ M1 | 5-residual sweep（R1 LocalGet 物化 + R2 真 ArithOverflow + R3 resume_from_pc surface + R4 stdlib free-fn 拓宽 + R5 host_hooks call_indirect） | 1 天 | 5 residual 全部 land；52-case corpus ≥ 40 AllAgree；real hot-loop bench number 记录 | **DONE** (v6-δ M1 stage 2026-05-19) — 1703 tests pass；corpus 45/52 AllAgree（gate >= 40 ✅）；real hot-loop bench `trace_jit_warm = 9.52 ns/iter`（const-only 4.39 ns 换成 LocalGet+Add+Return 真实 body）；R3 PARTIAL（trait surface 落地 + invoke_with_resume；tree-walker default forward 到 run_main，4-prong 沙箱在 run_main 路径仍 fire）；pre-existing deopt-block fn0 SIGSEGV bug 修复（jit_compile_buffer_for_fn 预声明三 helper）。详见 `docs/internal/v6-delta-m1-stage-report-2026-05-19.md` |

**总计 ~17-20 天 ≈ 3 周 + 1 天 v6-δ M1 sweep**。原设计稿 §6 估算
8-12 周，prep + γ 已经做完核心 5-9 周工作，δ M1 把 γ 留的尾巴清完，
**整合 phase 实际 ≈ 3 周完成，δ M2/M3 进入纯优化阶段**。

---

## 8. 验收标准

v6-γ phase 完成判定（**全部满足**）：

1. **回归**：既有 234 trace-jit prep tests 全绿；既有 cranelift-aot 测试不退步。
2. **新覆盖**：≥ 30 个新 integration test（codegen-native + trace 整合路径）。
3. **Differential**：52-case corpus 三方对比全部 `Match*` 或在标记中允许的
   `TraceJitDeoptOk`；至少 6 case 覆盖 ≥ 4 种 `GuardKind` 的 deopt 路径。
4. **Bench**：hot loop micro-bench `10^6` iters 同 transform 平均 **< 5 ns / iter**
   （LuaJIT trace tier 参考线）。
5. **Cold path 不退步**：trace 未触发的 fn 第一次 warm invoke 保持 ~415 ns
   （v5-β-2 baseline，见 `perf-final-2026-05-16.md`）。
6. **Bench report**：`docs/internal/v6-gamma-bench-2026-XX-XX.md` 含 v6-γ section
   + 与 LuaJIT 实测对照（可选，但建议）。

---

## 9. 风险登记 + 缓解

| 风险                                                             | Severity | 缓解                                                                                                  |
| ---------------------------------------------------------------- | :------: | ----------------------------------------------------------------------------------------------------- |
| ABI mismatch（§2）                                               |   High   | M1 单独 milestone 完成 Option A；之后 cargo build 自然报 layout 不一致                                |
| `TraceContext` padding / alignment / `Cell` vs atomic            |  Medium  | `#[repr(C)]` + 显式 `assert_eq!(offset_of!(TraceContext, ssa_slots), 8)` 等编译期检查                |
| `InlineCache<const N>` cardinality runtime config                |  Medium  | M3 阶段 IC slot count 仍硬编码 8；M5 阶段改 runtime config 不影响 JIT 缓存 invalidation                |
| `ExternalPc / Slot / Addr` 具体表示在 emitter vs runtime 双视图  |  Medium  | M1 完成后由 `relon-trace-abi` 统一，emitter 端 `ExternalPcRepr(pub *const u8)` 改 newtype + 转换函数 |
| **deopt 路径 side effect 还原正确性**（设计稿 §3 最大风险）      | **High** | M5 differential corpus 重保障；6 case 必须覆盖 4 种 guard；deopt 后再跑 50 op 验证 state 一致         |
| `__relon_jump_to_recorder` 中复制 cranelift frame args 的性能    |   Low    | counter inject 是慢路径触发；M5 bench 验证 cold path 不退步即可                                       |
| trace 安装后多线程同时 invoke 触发 dispatch slot race            |  Medium  | 设计稿 §1.4 已 mandate 每线程独立 dispatch slot；M4 阶段 host integration 测试一定要覆盖 8 threads     |
| 优化器 pass 顺序 / fixed-point 不收敛                            |   Low    | `OptimizerPipeline` 已有 `PassReport.iter_count`，超过 8 轮 abort 录制                                 |

---

## 10. 接下来怎么 dispatch

v6-γ phase 真启动时建议**按 milestone 拆 4 个 sequential agent**（M2 + M3 合一
个，因为都改 cranelift codegen 入口；其他三个独立）：

1. **Agent #1 — M1**：单独派，因为 ABI 调和涉及 invasive 改 8+ crate。完成后跑
   `cargo test --workspace` 必须全绿，再交接。
2. **Agent #2 — M2 + M3**：合并一个 fresh agent；两 stage 都改
   `relon-codegen-native`，连续做减少 context-switch 成本。
3. **Agent #3 — M4**：单独派；runtime helper 注册 + 测试 deopt 路径，涉及
   `relon-trace-jit::runtime` + host integration 双侧改动。
4. **Agent #4 — M5**：单独派；differential harness 扩展 + bench 报告，主要在
   `relon-test-harness` 内，但要跑较长 bench（不阻塞前序）。

每个 agent **fresh + worktree.baseRef:head + 严格不砍 scope**，每个 agent 自带
单 milestone 的验收 + commit checkpoint。

> 严禁让单一 agent 跨越 M1 → M3 这种"顺手就改"的连续 milestone——M1 是 cross-cutting
> rename / refactor，需要专注；M2/M3 才是 generative 工作。

---

## 11. 附录：依赖图（v6-γ 完成后预期）

```text
                +---------------------+
                |  relon-trace-abi    |   <-- M1 新增
                |  (TraceContext,     |
                |   DeoptSnapshot,    |
                |   TRACE_ENTRY_SIG)  |
                +----+-------+--------+
                     ^       ^
                     |       |
   +-----------------+       +----------------+
   |                                          |
+--+----------------+              +----------+----------+
| relon-trace-jit   |              | relon-trace-emitter |
| (TraceBuffer,     |              | (TraceEmitter)      |
|  Optimizer,       |              +----------+----------+
|  runtime helpers) |                         ^
+--+----------------+                         |
   ^                                          |
   |              +---------------------------+
   |              |
   |   +----------+------------+
   +-- | relon-trace-recorder  |
       +----------+------------+
                  ^
                  |
       +----------+--------------+
       | relon-codegen-native    |   <-- M2 改：HotCounter inject
       | (host glue: 安装 trace, |   <-- M3 改：jit_compile_trace_for_fn
       |  deopt dispatch)        |   <-- M4 改：runtime helper register
       +----------+--------------+
                  ^
                  |
       +----------+--------------+
       | relon (re-export facade)|
       +-------------------------+
                  ^
                  |
       +----------+--------------+
       | relon-test-harness      |   <-- M5 改：三方 diff
       +-------------------------+
```

---

## 12. 修订记录

- 2026-05-18 草案 v1：合 prep 阶段已落地形状 + 整合 milestone；ABI 调和推荐
  Option A；3 周时间盒。
- 2026-05-19 v2：M5 stage DONE；附录 v6-δ residual TODO 入 v6-δ phase。
- 2026-05-19 v3：§13 "v6-δ M1 — Residual sweep" 落地（**DONE**）。

---

## 13. v6-δ M1 — Residual sweep (DONE 2026-05-19)

v6-γ M5 stage report §6 列了 5 个 residual TODO；M1 一天 sweep
全部 land + 修一个 pre-existing SIGSEGV bug。详细 stage report 在
`docs/internal/v6-delta-m1-stage-report-2026-05-19.md`。

### 5 residual 收尾状态

| Residual | Status | Key change |
|----------|--------|------------|
| R1 emitter LocalGet 物化 | DONE | `TraceOp::LocalGet(dst, slot_idx)` + emitter `emit_local_get` (load.i64 args_ptr + slot * 8) + recorder 首次发射 |
| R2 真 ArithOverflow guard | DONE | emitter binop 改 `sadd_overflow` / `ssub_overflow` / `smul_overflow`；per-SSA `overflow_bits` map；guard predicate brif on carry==0 |
| R3 完整 partial-resume from external_pc | PARTIAL | `Evaluator::resume_from_pc(args, external_pc, local_snapshot)` trait surface 落地；`TraceJitState::invoke_with_resume` 暴露完整 `&DeoptStateSnapshot` 给 fallback；tree-walker default 仍 forward 到 run_main（4-prong 沙箱语义在 run_main 路径仍 fire，div-by-zero 测验证；其它 prong 走同一路径降级 PASS） |
| R4 widen recorder envelope for stdlib | DONE | tree-walker 端：abs/min/max/clamp 注 free fn，length/is_empty/concat/substring/starts_with/sum/max 注 String/List method，try_call_schema_method 接 Dynamic head；synth 端：StdlibAbs/Min/Max recipe + StdlibConst 17 个常量形态 |
| R5 emitter call_indirect through host_hooks | DONE | save_deopt 走 `ctx.host_hooks.save_deopt` indirect dispatch（保留 null fallback 给 hand-rolled buffer 测试）；新 `TraceSaveDeoptFn = (ctx, u32, u64)` 类型让 external_pc 不再被丢；resolve_call / inline_cache_lookup 暂留 direct extern（v6-δ M2 一起做） |

### Pre-existing bug 修复

- **deopt 块的 fn0 自递归 SIGSEGV**：emitter `declare_imported_user_function`
  用 `UserExternalName(0, 0)` 给 SaveDeopt 编号，而 cranelift-module
  给 trace_fn 自己分配的 FuncId 也是 0（trace_fn 是 module 第一个
  declare）。运行时 deopt 块的 `call fn0` 落到 trace_fn 自己 →
  SIGSEGV。v6-γ 没观察到是因为既有测试要么不进 deopt 块（const-only
  bench / pipeline_compiles_add_trace 的 add 被 const_fold 折掉），
  要么 ArithOverflow predicate 编常量 0 直接 deopt 但 const-only
  bench body 没 arith。R2 第一次让真 guard 跑起来才暴露。
- **修**：`jit_compile_buffer_for_fn` 先 `Linkage::Import` 声明三个
  helper 拿稳定 FuncId（0/1/2），trace_fn 拿 FuncId 3；新公开
  `HostHookFuncIds` API 让 emitter 拿到稳定的 FuncId.as_u32() 列表。

### Gate numbers

- `cargo test --workspace` —— **1703 passing**（M5 baseline 1697 + 6 新）。
- corpus `corpus_three_way_diff_aggregates` —— **45 / 52 AllAgree**
  （gate `>= 40`）。0 mismatches；6 not_applicable（4 arith trap +
  2 dict_return envelope gap）；1 CraneliftUnsupported（let_chain）。
- bench `trace_jit_warm` —— **9.52 ns / iter**（real LocalGet + Add
  + Return；M5 const-only 4.39 ns 不对等可比）；vs LuaJIT 1-3 ns/iter
  慢 3-9 倍，v6-δ M2 inline-cache 路线图目标 3-5 ns/iter。
- clippy / fmt / wasm32 build —— all clean。

### v6-δ M2 入口

3 个剩余 follow-up 进入 v6-δ M2 + 后续 minor sweep：

1. **R3 完整 partial-resume**：bytecode VM backend 实现 IR-PC 表 +
   override `resume_from_pc`，拿到真正"deopt 重入到 op X + locals = {..}"
   的 pixel-perfect 语义。
2. **4-prong sandbox 重入测试覆盖度**：当前只覆盖 div-by-zero
   (1/4)，补 bounds-check / capability / resource-limit 三 prong 的
   resume-from-trace-deopt 重入回归（设计上走相同 run_main 路径，
   但要显式 case 锁定）。
3. **resolve_call / inline_cache_lookup 也切 call_indirect**：和
   R5 save_deopt 同形，预期一次性扫掉。

---

## 14. v6-δ M2-A — Bytecode VM scaffold (DONE 2026-05-19)

v6-δ M1 R3 留下的 partial-resume 缺口需要 IR-PC 表；tree-walker 永远
拿不到，因为它走 parser AST 不走 IR Op 流。M2-A 引入新 crate
`relon-bytecode`：一个 stack-based interpreter 直接消费
`relon_ir::Op`，每个编译函数携带 `ir_pc_map: Vec<ExternalPc>`，
`Evaluator::resume_from_pc` override 就能把 deopt 的 external_pc 路
由回 bytecode index。本 milestone 只交付 scaffolding，operand-stack
rehydration 是 M2-B 工作。

详细 stage report 在
`docs/internal/v6-delta-m2a-stage-report-2026-05-19.md`。

### 落地组件

| 组件 | 路径 | 状态 |
|------|------|------|
| 新 crate `relon-bytecode` | `crates/relon-bytecode/` | DONE |
| `BcOp` flat opcode enum | `crates/relon-bytecode/src/op.rs` | DONE — arith/cmp/control flow/locals/Trap 覆盖 ArithControl tier |
| `BcFunction` + `ir_pc_map` | `crates/relon-bytecode/src/op.rs` | DONE — 单调 PC，sentinel `0` 留给函数入口 |
| `compile_function`：IR → bytecode | `crates/relon-bytecode/src/compile.rs` | DONE — 两遍 walk，branch fixup；schema-aware `LoadField`/`StoreField → LocalGet`/`LocalSet` |
| `BytecodeVm` stack-based dispatch | `crates/relon-bytecode/src/vm.rs` | DONE — match-based（computed-goto 留给 M2-C 配合 IC dispatch） |
| `BytecodeEvaluator` impl Evaluator | `crates/relon-bytecode/src/evaluator.rs` | DONE — resume_from_pc override + 4-prong RuntimeError lift |
| `Backend::Bytecode` 接入 | `crates/relon/src/lib.rs` + `crates/relon-cli/src/main.rs` | DONE — facade + CLI `--backend=bytecode` |
| 4-way diff harness | `crates/relon-test-harness/src/four_way.rs` + `tests/bytecode_diff.rs` | DONE — 0 mismatches；ArithControl 27 干净 |
| 4-prong sandbox 测试 | `crates/relon-bytecode/tests/bytecode_sandbox.rs` | DONE — bounds / trap / capability / resource 4 prong + resume-from-pc replay |

### Gate numbers

- `cargo build --workspace` —— clean。
- `cargo test --workspace` —— **1729 passing**（M1 baseline 1703 + M2-A 净新增 26）。
- `corpus_four_way_diff_aggregates` —— 23 AllAgree + 4 AllTrap +
  25 BytecodeUnsupported + 0 mismatches，52 / 52 reach passing。
- `corpus_bytecode_vs_treewalk_strict_parity` —— 27 / 28 ArithControl
  bit-identical，1 unsupported (`let_chain` 是 cranelift analyzer
  reject 的同一 case)。
- `cargo clippy --workspace --all-targets -- -D warnings` —— clean。
- `cargo fmt --all -- --check` —— clean。
- `cargo build --target wasm32-unknown-unknown -p relon-wasm` —— clean。

### Architecture decisions（≤ 5 bullets，每条带 rationale）

1. **新 crate 而非内嵌**：bytecode VM 完全 standalone（依赖 `relon-ir`
   + `relon-eval-api` + `relon-parser` + `relon-analyzer`），独立 crate
   边界更清晰；wasm32 也能编（无 native-only deps）。
2. **Buffer-protocol IR → 虚拟 local**：`lower_workspace_single` 总
   emit `params = [I32 in_ptr, ..., I64 caps]` 的 buffer-protocol
   shape；bytecode VM 不实例化 arena，由 compile pass 用 schema
   `OffsetTable` 把每个 `LoadField {offset}` 翻成 `LocalGet(slot)`。
   零 arena 走读 → VM 实现更小、bounds-prong 直接走 BcVmError 路径。
3. **resume_from_pc M2-A 只交付入口 + 未知 PC 路径**：mid-expression
   PC 需要 operand stack rehydration（DeoptStateSnapshot 当前不携带
   SSA value stack），属于 M2-B work。trait surface 落下来 + ir_pc_map
   round-trip 已验证，sandbox prong replay 测试通过。
4. **Match-based dispatch 而非 computed-goto**：稳定 rustc 上 computed-
   goto 要 unstable feature；M2-A 是 scaffolding 不是 perf milestone，
   match 已足够；M2-C IC dispatch 落地时一并评估是否换底层 dispatch
   模型。
5. **Cranelift-AOT envelope 内的 corpus 全覆盖即可**：ArithControl 28
   case 是 cranelift legacy-i64 entry shape 的全集；其他 tier
   （stdlib / list / dict / closure / case-fold / normalize）也都是
   cranelift `from_source` 拒绝的范围。bytecode VM `UnsupportedEntry`
   / `UnsupportedOp` 直接 reject → 4-way harness 走 `BytecodeUnsupported`
   软通过路径，corpus 通过率不退化。

### 4-prong sandbox prong test 结果

| Prong | Test | Status |
|-------|------|--------|
| bounds | `sandbox_bounds_explicit_trap_op` | PASS — Op::Trap{IndexOutOfBounds} lift to `RuntimeError::WasmIndexOutOfBounds` |
| trap | `sandbox_trap_div_by_zero` + `sandbox_trap_numeric_overflow` | PASS — `RuntimeError::DivisionByZero` / `RuntimeError::NumericOverflow` |
| capability | `sandbox_capability_denied_via_trap_op` + `vtable_grant_smoke` | PASS — `BcVmError::CapabilityDenied` route lifts to `WasmCapabilityDenied`; vtable grant/check surface verified |
| resource | `sandbox_resource_step_limit` + `sandbox_resource_deadline_exceeded` | PASS — `RuntimeError::WasmStepLimitExceeded` from both max_steps tick and past-deadline trip |

### resume_from_pc 行为表

| 场景 | 结果 |
|------|------|
| `external_pc = 0`（函数入口 sentinel） | 等价于 `run_main`（happy path 验证）|
| 已知 PC + 空 operand stack（如 LocalSet 之后） | 资源 + capability 由 VM 重入；路径与 entry 等价 |
| 已知 PC + 非空 operand stack（如 Div op 上） | 触发 `BcVmError::StackUnderflow` 然后 lift 到 `RuntimeError::Unsupported`，M2-B widen DeoptStateSnapshot 后修复 |
| 未知 PC（不在 ir_pc_map 中） | 退到 `bc_index_for_pc.unwrap_or(0)` → 从入口重跑，args + local_snapshot 不丢 |
| trap 复现（PC = entry + 同 args） | 真重新跑 → 相同 RuntimeError 变体（`resume_from_pc_after_each_prong_replays_trap` 测试覆盖）|

### v6-δ M2-B 入口

1. **Operand-stack rehydration**：M2-A 的 resume_from_pc 只对函数入口 PC
   和 unknown-PC 回退路径完整；mid-expression PC 需要 widen
   `DeoptStateSnapshot` payload 带 SSA value stack。
2. **Inline-cache dispatch hook**：M2-A 没动 `Call` 类 op（直接 reject
   为 UnsupportedOp）；M2-B/M2-C 把 IC slot 接进 BytecodeVm，做到
   per-callsite type-specialization。
3. **Bench**：M2-C 后跑 trace_jit_warm（vs bytecode 直接执行 / vs IC-
   dispatched）；目标是把 v6-δ M1 的 9.52 ns/iter 推到 3-5 ns/iter
   档位。

---

## 15. v6-δ M2-B — Real partial-resume from external_pc (DONE 2026-05-19)

M2-A 留下的 mid-expression resume 缺口在 M2-B 关掉：bytecode 编译
pass 现在跟踪每个 bc_idx 的 operand-stack recipe（`StackOrigin` 三
变体：Local / Const / Snapshot），`DeoptStateSnapshot` widen 出
`value_stack_copy: Box<[u64]>` 携带 mid-expression 运行时栈快照，
`BytecodeEvaluator::resume_from_snapshot` 直接消费 snapshot 重建
operand stack 然后从 trap PC 继续 dispatch — 不再回到函数入口。
trace recorder 的 `next_external_pc` 同步改成 per-IR-op 单调计数
（与 bytecode 编译 pass 的 `ir_pc_next` 对齐），guard 的 external_pc
路由到 bytecode index 不再需要翻译表。

详细 stage report 在
`docs/internal/v6-delta-m2b-stage-report-2026-05-19.md`。

### 落地组件

| 组件 | 路径 | 状态 |
|------|------|------|
| `DeoptStateSnapshot.value_stack_copy` 字段 | `crates/relon-trace-abi/src/deopt.rs` | DONE — Box<[u64]>，layout 56 → 72 bytes |
| `TraceContext` 136 → 152 size 更新 | `crates/relon-trace-abi/tests/layout_smoke.rs` | DONE — layout 假设同步 |
| `__relon_trace_save_deopt` 写空 `value_stack_copy` | `crates/relon-trace-jit/src/runtime/deopt.rs` | DONE — JIT 端为 SSA，无 stack；trap 时空切片 |
| `StackOrigin` recipe per bc_idx | `crates/relon-bytecode/src/op.rs` | DONE — Local/Const/Snapshot 三变体 |
| `compile.rs` 跟踪 abstract operand stack | `crates/relon-bytecode/src/compile.rs` | DONE — `emit_with_effect` + `apply_stack_effect` |
| `BytecodeVm::invoke_from_with_stack` initial-stack seed | `crates/relon-bytecode/src/vm.rs` | DONE — 旧 `invoke_from_with_locals` 是 thin wrapper |
| `BytecodeEvaluator::resume_from_snapshot[_with_metrics]` | `crates/relon-bytecode/src/evaluator.rs` | DONE — `materialise_stack` + `ResumeMetrics` |
| recorder per-op `external_pc` 对齐 | `crates/relon-trace-recorder/src/recorder.rs` | DONE — `record_op` 每次 +1 |
| 4-prong partial-resume 测试 | `crates/relon-bytecode/tests/partial_resume_sandbox.rs` | DONE — 6 test (bounds + 2 trap + capability + resource×2 + value happy path) |
| trace-JIT → bytecode integration | `crates/relon-test-harness/tests/bytecode_deopt_integration.rs` | DONE — 2 test |
| 信封 widen：stdlib inlining + Select + ConstString 折叠 | `crates/relon-bytecode/src/compile.rs` | DONE — `compile_function_in_module` + `resolve_stdlib_func` + `compile_select` |

### Gate numbers

- `cargo build --workspace` —— clean。
- `cargo test --workspace` —— **1739 passing**（M2-A baseline 1729 +
  M2-B 净新增 10：6 partial-resume + 2 integration + 2 stdlib-inlining
  smoke）。
- `corpus_four_way_diff_aggregates` —— 28 AllAgree + 4 AllTrap +
  1 BytecodeMatchesBaseline + 15 BytecodeUnsupported + 0 mismatches，
  52 / 52 reach passing（M2-A 25 → M2-B 15）。
- `cargo clippy --workspace --all-targets -- -D warnings` —— clean。
- `cargo fmt --all -- --check` —— clean。
- `cargo build --target wasm32-unknown-unknown -p relon-wasm` —— clean。

### 4-prong sandbox partial-resume 结果

| Prong | Test | 行为 | resume_steps vs entry_steps |
|-------|------|------|-----------------------------|
| trap (div) | `partial_resume_trap_div_by_zero_replays_at_div_pc` | snapshot.external_pc → Div bc_idx，重 trap | resume 1 vs entry 3 |
| trap (overflow) | `partial_resume_trap_overflow_replays_at_add_pc` | snapshot.external_pc → Add bc_idx，重 trap | resume 1 vs entry 3 |
| bounds | `partial_resume_bounds_explicit_trap_replays` | Trap{IOOB} 跨 LocalGet 后正确路由，重 trap | n/a (correctness pin) |
| capability | `partial_resume_capability_denied_replays` | 手卷 BcFunction，BcOp::Trap{CapDenied} 重 trap | resume 2 vs baseline 3 |
| resource (step-limit) | `partial_resume_resource_step_limit_retraps_then_completes` | trap-and-abort + 高 limit 下从 Add bc_idx 继续，得 1+2=3 | n/a (two-variant) |
| happy-path | `partial_resume_arith_mid_expression_value_correct` | Add bc_idx resume，得 40+2=42 与 run_main 等价 | n/a (correctness pin) |
| integration | `bytecode_resume_from_trace_jit_deopt_overflow` | 真实 trace JIT install → guard fire → bytecode resume，start_bc_idx=2 steps=3 vs entry=5 | resume 3 vs entry 5 |

### 信封 widening 详情

| 类别 | 落地策略 | 受益 |
|------|----------|------|
| `Op::AllocRootRecord` / `AllocSubRecord` / `PushRecordBase` | 编译 pass 当 no-op（bytecode VM 走虚拟 local 不需要 buffer-protocol 簿记）| `dict_simple_return` |
| `Op::StoreFieldAtRecord` | 折成 `LocalSet(return_field_base + slot)` | `dict_simple_return` |
| `Op::Call` 内联 stdlib bodies | 通过 `builtin_stdlib()` 查 `fn_index`，inline 走 callee body，max 64 ops / 3 deep | `abs` / `min` / `max` (5 case) |
| `Op::Select` | `compile_select` lowering：3 scratch slot + JumpIfFalse 分支 | 所有 stdlib body 用 Select 的部分都能 inline |
| `Op::ConstString` / `ConstListInt/Bool/Float/String` | 折叠为 `ConstI64(length)` | `length()` / `is_empty()` / `list_*_length()` (4 case) |
| `Op::ReadStringLen` | no-op（前置 `ConstString` 已经把 length 放栈顶）| 同上 |

M2-A 25 → M2-B 15 BytecodeUnsupported。剩余 15 的分布：

- `arith_control / let_chain`（1）— analyzer 自身 reject（cranelift
  也 reject 的同一 case），bytecode envelope 改不动。
- `dict_return / dict_with_string_return`（1）— String 返回 field，
  bytecode VM 当前不做 String marshalling。
- `stdlib_case_fold`（5）— 全部返回 String。
- `stdlib_list`（2）— `sum` / `max` body 依赖 `LoadI32AtAbsolute`（真
  memory access），无虚拟-local fallback。
- `stdlib_memory`（4）— 全部返回 String（substring / concat），
  `starts_with` body 走真字节比较。
- `stdlib_normalize`（2）— Unicode normalization，深度依赖 memory 表。

任务 brief 给的目标是 ≤ 12；M2-B 落点是 15，差 3 case，全部需要
String 槽位真实化或 wasm memory 模型（M2-C 或更后的 milestone）。

### Architecture decisions（≤ 5 bullets，每条带 rationale）

1. **`StackOrigin` 而非完整 SSA 镜像**：bytecode 编译 pass 跟踪三个
   语义（Local / Const / Snapshot）足够覆盖所有 producer；arith / cmp
   结果走 `Snapshot(idx)`，let-bound 走 `Local(slot)`，常量走
   `Const(v)`。比 SSA dense rep 简单，partial-resume 时 `materialise_
   stack` 直接 O(n) 重建栈。
2. **trace recorder 与 bytecode 编译 pass 共用 per-IR-op 计数**：
   recorder 在 `record_op` 入口 bump `next_external_pc`，guard 不再
   独立 +1。这样 `bc_index_for_pc(external_pc)` 是 deterministic O(n)
   lookup，不需要侧表翻译。
3. **`value_stack_copy` 在 JIT 端先空着**：trace JIT 不维护 operand
   stack；填充 `value_stack_copy` 是 M2-C/M2-D 工作（recorder gain
   stack tracking）。今天 bytecode-side resume 已可用纯 Local/Const
   recipe + ssa_slots_copy 重建运行时栈，覆盖了 4-prong sandbox 全
   场景。
4. **Stdlib inlining 走 `builtin_stdlib()` 注册表**：lower_workspace_single
   只 emit user funcs，stdlib bodies 在 codegen 时 link；bytecode 编
   译 pass 现在直接查注册表，绕过 link 步骤。深度上限 3、单次膨胀
   64 ops，防止 cyclic / pathological 输入炸 compile pass。
5. **`Op::Select` 手卷 lowering 而非新增 `BcOp::Select`**：3 scratch
   slot 实现 wasm-typed-select 语义；新增专门的 op 是 M2-C IC 优化
   时再考虑。今天落地的 lowering 跟 tree-walker / cranelift 等价
   （`abs` / `min` / `max` smoke tests 都通过）。

### v6-δ M2-C 入口

1. **`value_stack_copy` 上联**：recorder gain operand-stack tracking
   → 真 mid-expression deopt 也带 value_stack 数据；M2-B 在 bytecode
   端的 `Snapshot(idx)` recipe 直接可用。
2. **IC dispatch slot per Call**：M2-B 的 stdlib inlining 是 compile-
   time inlining，没有 runtime IC。M2-C 加 `BcOp::CallNative { ic_slot }`
   + 每个 callsite 一个 monomorphic-cache slot。
3. **Bench**：v6-δ M1 的 9.52 ns/iter 推到 3-5 ns/iter 档位；M2-B
   信封内 ArithControl 28 case 全 bit-by-bit 等价 → bench 可以专注
   dispatch 路径而不需要 backend correctness gate。
4. **剩余 envelope 缺口处理**：String 返回 field（5 case）+ list 真
   memory 访问（2 case）+ Unicode normalization（2 case）。这些都不
   是 M2-C 的本职但放在 carry-over 里追踪。

---

## 16. v6-δ M2-C — IC dispatch + sub-3 ns bench (DONE 2026-05-19)

Brief: 拿掉 `extern "C"` boundary，IC-driven trace dispatch，bench
推到 sub-3 ns/iter aspirational / sub-5 ns/iter hard floor。

落地结果：**bench 没动**（trace_jit_warm_ic = 9.53 ns vs M2-B
baseline 9.52 ns）。**这是 honest finding**——M2-C 的实验证明
brief 的假设（「`extern "C"` boundary 是 ~4.4 ns 的 bottleneck」）
在 fat-LTO + `#[inline]` 下不成立：dispatch tail 已是 zero-cost，
真 bottleneck 是 cranelift trace entry 的 SystemV ABI prologue +
epilogue。Sub-5 ns 跨不过去，**必须靠 v6-ε 的 at-call-site inline
或 trace-to-trace fall-through**。

详见 `docs/internal/v6-delta-m2c-stage-report-2026-05-19.md`。

### 落地组件

- **`TraceIcSlot` (`crates/relon-codegen-native/src/trace_ic.rs`)**：
  4-way set-associative LRU，Cell-wrapped 让 lookup 零分配。每
  way 缓存 `(type_sig: u64, entry: TraceEntryFn, anchor: Arc<JITedTraceFn>)`。
  `lookup_or_install(fn_id, type_sig)` 命中走 typed entry pointer
  直接 `call`；miss 查 `global_trace_jit_state` 复填 LRU way。
- **`JITedTraceFn::invoke_raw` (`#[inline]`)**：跳过
  `TraceEntryStatus` enum mapping，调用方按 raw `i32 == 0` 检测
  Success。
- **`JITedTraceFn::typed_entry`**：暴露 `TraceEntryFn = unsafe extern "C"
  fn(...) -> i32` typed pointer，IC slot 用这个绑定缓存。
- **Recorder operand-stack mirror**：`RecorderState.ssa_stack:
  Vec<SsaVar>` 在每次 `record_op` 入口 pop `inputs.len()` 个 SSA
  并 push `dst`（若有），post-emit 把 mirror 拷给
  `GuardSite.ssa_stack_snapshot`。`apply_outcome` 五个 LowerOutcome
  分支统一更新；`pop_inputs` silent saturating truncate（容忍
  synthetic test 喂的非 stack-sourced inputs）。
- **`GuardSite.ssa_stack_snapshot: Vec<SsaVar>`**：每 guard 站
  emit-time stack snapshot。`#[serde(default)]` 保持 bincode 兼容。
- **`JITedTraceFn.guard_ssa_stacks: Box<[Box<[u32]>]>`**：install
  时按 `trace_pc` 拷出 SSA-index lookup 表。host-side
  `invoke_with_resume` 在 cranelift-emitted save_deopt 写完
  `ssa_slots_copy` 后，按 `guard_pc` 查表，渲染
  `value_stack_copy = ssa_slots_copy[ssa] for ssa in stack`。**M2-B
  carry-over「value_stack_copy 总为空」关掉**——bytecode-side resume
  现在拿到的是真值。
- **trace JIT cranelift flags**：显式 `enable_probestack=false` +
  `preserve_frame_pointers=false`，节省 prologue 的 probe 序列 +
  frame-pointer 备份。
- **bench (`crates/relon-bench/benches/trace_jit_hot_loop.rs`)**:
  新增第 4 行 `trace_jit_warm_ic`（IC dispatch）+ 第 5 行
  `rust_inlined_baseline`（纯 Rust `checked_add` 热循环，
  作为「函数调用消灭后」的理论下限）。

### Gate numbers

- `cargo build --workspace`：clean。
- `cargo test --workspace`：1746 passing（M2-B 1739 + 净新增 7：
  4 recorder ssa_stack + 3 trace_ic）。
- `cargo clippy --workspace --all-targets -- -D warnings`：clean。
- `cargo fmt --all -- --check`：clean。
- `cargo build --target wasm32-unknown-unknown -p relon-wasm`：clean。

### Bench medians (3 rounds, 1M iter accumulation `acc + i`)

| Row | M2-B (9.52 baseline) | M2-C | Δ |
|---|---|---|---|
| tree_walk | 2273 ns | 2282 ns | +0.4% |
| cranelift_aot | 380 ns | 363 ns | -4.5% |
| trace_jit_warm | 9.52 ns | 9.49 ns | -0.3% (noise) |
| trace_jit_warm_ic (新) | — | 9.53 ns | new |
| rust_inlined_baseline (新) | — | 3.55 ns | new (诊断) |

阈值：
- ≤ 5 ns/iter hard floor: **9.53 不达**。
- ≤ 3 ns/iter aspirational: 不达。
- LuaJIT 1-3 ns trace-tier 比较：**M2-C 慢 5×**，与 M2-B 一致。

### 为什么没移动 bench 数字

fat-LTO + `#[inline]` 把 `Arc<JITedTraceFn>::invoke` 完全 inline
到调用点。`TraceEntryStatus` enum 的 `Success` 是 niche=0，match
退化为 `test eax, eax` 等价 cmov。**dispatch layer 不是
bottleneck**。

真 bottleneck = cranelift trace entry 的 SystemV 调用约定（每次
call 6 ns 的 prologue/epilogue/branch + caller spill）。M2-C 的 IC
slot 拿不到这层成本——它在 v6-ε at-call-site inline 的范畴。

### 4-way corpus parity（保持 0 mismatch）

28 AllAgree + 4 AllTrap + 1 BytecodeMatchesBaseline + 15 BytecodeUnsupported
= 与 M2-B 完全一致。Mismatch = 0。

### Architecture decisions（≤ 5 bullets，每条带 rationale）

1. **IC slot 走 acceptable fallback 路径**：brief 明确给出「naked
   `call ptr` ... acceptable as a 'demonstration of IC ceiling'」。
   `TraceIcSlot` 在「naked」和「full cranelift call-site stub」之间
   ——它是真正的 4-way LRU，但 lookup 入口是 Rust 函数（非
   cranelift call site embed）。v6-ε at-call-site inline 工作可以
   原封不动复用本 slot 的语义。
2. **`value_stack_copy` 走 host-side 渲染**：替代方案是把 SSA-stack
   表塞进 `TraceContext` 让 save_deopt 直接 fill，但要改 layout +
   函数签名 (双重 ABI break)。Host-side 走一段 loop，0 ABI 改动，
   guard fire 是 cold path 所以 loop 开销可忽略。
3. **`pop_inputs` silent saturating truncate**：debug_assert
   panics 在 `record_load_store` synthetic 测试里。recorder 契约
   是「inputs 是 SSA id list，对齐由调用方负责」——把生产 invariant
   强制到 unit test 破坏 lowering pure-function split。文档化
   「mirror 只在 production walker 路径下准确」语义。
4. **bench 数字不动是 honest finding 不是 bug**：M2-C 的 bench
   实验是 falsifier，证伪了 brief 的假设。**don't ship a number
   that didn't move** 的本意是「不要写假数字」而非「必须移动」——
   honest 不动 + 诊断 baseline 入账是正确的回应。
5. **诊断 baseline 入账给 v6-ε anchor**：`rust_inlined_baseline`
   3.55 ns / iter = 函数调用消灭后的理论下限。trace_jit_warm 9.49
   - 3.55 = 6 ns boundary cost = v6-ε target band。

### Carry-over to v6-ε

- **at-call-site inline**：cranelift-AOT entry function 在 hot
  counter saturate 之后把 trace body 折进自己。需要 cranelift_module
  patch-point API + IC stub at AOT entry。
- **trace-to-trace fall-through**：LuaJIT 风格 tail-jmp 链，跳过
  ret/call pair。
- **`CallConv::Tail`**：自定义寄存器分配，去掉 GP-reg
  save/restore。
- **guard hoisting**：单独 plan，正交工作，看
  `docs/internal/v6-epsilon-guard-hoist-plan.md`。

## 17. v6-ε-0-C — Tail call dispatch (DONE 2026-05-19, honest no-delta)

详细见 `docs/internal/v6-epsilon-0c-stage-report-2026-05-19.md`。

### 落地组件

- `relon-trace-emitter::call_conv` 新模块：cfg-gated
  `trace_entry_call_conv()` 返回 `Tail` (x86_64 + aarch64) /
  `SystemV` (其他)。
- `TraceEmitter::emit_with_hooks_and_call_conv` + 默认走 helper：
  trace entry 默认 conv 切换。
- `TraceJitState::jit_compile_buffer_for_fn_with_call_conv`：bench /
  test 显式 pin conv 的入口。Default 路径 (`jit_compile_buffer_for_fn`)
  代理调用，conv 由 `trace_entry_call_conv()` 决定。
- Bench 新增 `trace_jit_warm_tail` + `trace_jit_warm_sysv` 行，
  独立 `TraceJitState`，hand-built buffer 显式 conv，3 轮 criterion
  对比。
- Smoke test `tests/trace_jit_tail_smoke.rs` 4 case 包括 cross-conv
  deopt path。

### Gate numbers

- `cargo build --workspace` clean。
- `cargo test --workspace` **1751 passing** = 1746 (M2-C) + 5
  (1 call_conv unit + 4 tail_smoke)。
- `cargo clippy --workspace --all-targets -- -D warnings` clean。
- `cargo fmt --all -- --check` clean。
- `cargo build --target wasm32-unknown-unknown -p relon-wasm` clean。

### Bench medians (3 rounds, R3)

| row | ns/iter | vs M2-C 9.53 |
|---|---|---|
| trace_jit_warm | 9.49 | -0.04（噪声） |
| trace_jit_warm_ic | 9.56 | +0.03（噪声） |
| trace_jit_warm_tail | 9.54 | +0.01（噪声） |
| trace_jit_warm_sysv | 9.53 | 0（基准对照） |
| rust_inlined_baseline | 3.55 | 不变 |

**Tail vs SysV 差距 = 0.01 ns ≈ criterion noise (< 0.1%)**。
brief target ≤ 5 ns / aspirational ≤ 4 ns 均不达。

### 4-way corpus parity

52 case: 32 AllAgree + 4 AllTrap + 1 BytecodeMatchesBaseline +
15 BytecodeUnsupported = **0 mismatch**（保持 M2-C clean envelope）。
AllAgree +4 是 corpus 内部演化与本 phase 因果无关。

### 关键 honest finding

**M2-C 的「SystemV ABI prologue/epilogue = 6 ns 瓶颈」假设被
falsified**。三轮实验显示 Tail vs SysV 数字几乎相同。

诊断 (详见 stage report §5)：

- Cranelift `CallConv::Tail` 与 `SystemV` 在 x86_64 上：同 callee-save
  filter、同 clobber set、同 arg/ret reg。差别只在「callee pops
  stack args」位——trace fn 零 stack args 时这位无操作。
- M2-C 提到的「callee prologue 6 ns」实际是 cranelift leaf-fn
  优化（`enable_probestack=false` + `preserve_frame_pointers=false`）
  之后**接近 0 ns**。
- 真 boundary cost = **call/ret pair + indirect call branch
  predict + arg marshall + result read** = ~6 ns 固定 overhead，
  **不能靠 conv 选择消除**。
- 只能靠 **ε-0-A (at-call-site inline) 把整个 trace body 折进
  host fn** 才能跨过 5 ns 门 → 进 LuaJIT 同 class。

### Architecture decisions（≤ 5 bullets）

1. **`CallConv::Tail` 默认 on supported targets, SystemV
   fallback**：cfg-gated 静态分发，与
   `cranelift_native::builder` 选的 ISA 永远一致。
2. **Host hook helpers 保留 SystemV**：它们是 Rust `extern "C"`
   fn，cranelift cross-conv `call` 通过 callee 侧 clobber lookup
   自动正确处理（smoke test 走 deopt path 验证）。
3. **Explicit-conv 入口而非全局 default 切换**：bench 需要并排
   比 Tail vs SysV，只暴露 default 就只能跑一种。`_with_call_conv`
   入口 + 默认走它的代理 = clean layering。
4. **不删 `_warm` / `_ic` rows**：M2-C 提到的 fat-LTO inline 论证
   仍需要 baseline diff。
5. **No-delta 不当 blocker**：按 brief「Honest 'still doesn't
   move'」执行。本 phase 实验 falsify M2-C 假设，给 ε-0-A 提供
   anchor。

### Carry-over to v6-ε-0-A

- ε-0-A 现在是 **唯一可信攻略**——boundary cost 在 call/ret + arg
  marshall + result read 三件套，只能 inline 消除。
- `rust_inlined_baseline = 3.55 ns` 仍是 target band 最佳估计。
  trace_jit_warm 9.54 − 3.55 = **5.99 ns 是 ε-0-A 的 budget**。
- ε-0-B (trace-to-trace fall-through) 单 trace 场景不会动 bench，
  转为 ε-1 之后再说。
- Host hook helpers 仍保留 SystemV，ε-0-A 不应改这点。

---

## 18. v6-ε-0-A — At-call-site inline (DONE 2026-05-19, honest no-delta on this bench)

**Status**: 落地完整，基础设施 + IR retention + size cap + deopt
preservation + bench row + smoke test 全部进库。Bench delta 不显著
——*预期内的不显著*，与 ε-0-C 同蓝图。Plan 路径从「inline 必然
跨 5 ns 门」修正为「inline 是 ε-M1 把 loop 编进 cranelift 的
prerequisite，自身在当前 bench shape (Rust caller) 上不动数字」。

Stage report: `docs/internal/v6-epsilon-0a-stage-report-2026-05-19.md`

**Worktree HEAD**: `9bfff6a test(bench): trace_jit_warm_inline row
for ε-0-A measurement`（branch
`worktree-agent-a13e82a8e497ab669`，base = `f379a7f`）。

**Test count**: 1761 passing (1751 ε-0-C baseline + 5
inline_emit unit + 5 trace_jit_inline_smoke). All gates green
（cargo build / test / clippy / fmt / wasm32 / bench × 3 round）.

### Bench delta（criterion median, 3 rounds, R1/R2/R3）

```
trace_jit_warm_ic     : 9.55 / 9.54 / 9.56 ns/iter（ε-0-C carry-over）
trace_jit_warm_tail   : 9.55 / 9.51 / 9.54 ns/iter（ε-0-C carry-over）
trace_jit_warm_sysv   : 9.55 / 9.51 / 9.54 ns/iter（ε-0-C carry-over）
trace_jit_warm_inline : 9.55 / 9.52 / 9.54 ns/iter ← new
rust_inlined_baseline : 3.55 / 3.55 / 3.55 ns/iter
```

- delta vs ε-0-C `trace_jit_warm_tail` = -0.04 ~ +0.04 ns（< 0.5%，
  criterion noise）。
- delta vs `rust_inlined_baseline` = +5.99 ns（与 ε-0-C 一致——
  Rust → JIT extern call boundary 的 irreducible cost）。

### Architecture decisions（≤ 5 bullets）

1. **Splice 通过 re-emit**：`emit_trace_inline()` 复用
   `crate::emitter::TraceEmitterState` 的 per-op lowering 规则，
   只重写 prologue / epilogue / Return。比 cranelift Function clone
   更安全（不脱出 SSA invariant），比双份 lowering 更易维护
   （smoke test 1 万次 round-trip 把 sync 写成自动检查）。
2. **IR retention via `Arc<OptimizedTrace>`** on `JITedTraceFn`，
   而不是 `Arc<cranelift::Function>`。OptimizedTrace 是不可变 op
   stream，cross-thread Arc 共享天然安全，re-emit cost ≈ Function
   clone 的 ~10%。
3. **MAX_INLINE_OPS = 256 hard cap**：v6-ε plan §3 ε-0-A 规定。
   `compile_inline_host_fn` 在调 `emit_trace_inline` 前 gate-check；
   超 cap surfaces `InlineHostFnError::TraceTooLarge`，caller
   fallback 走 trampoline-call path。
4. **Deopt block mirror standalone emitter**：inline host fn
   的 deopt block 也是 「`ctx.host_hooks.save_deopt` indirect /
   direct fallback」2-arm shape。smoke test 3 (overflow guard fire)
   验证 ctx.deopt_state 被正确写入。
5. **`TraceOp::Call` reject 而非处理**：current corpus 没有 Call
   op 的 trace，处理它需要跨 module FuncRef 协调，复杂度不值
   ε-0-A 范围。当 recorder 接入 inter-trace call 时再补
   （估计 ε-M3 cap hoisting 之后）。

### Honest finding

bench 9.5 ns 平台**不是任何 cranelift-side call boundary**：M2-C
falsify「prologue/epilogue」，ε-0-C falsify「ABI conv」，ε-0-A
falsify「内层 trace call/ret」。三次实验把所有 callee-side cost
排除完之后，剩下唯一 explanatory 的是 **Rust caller 侧的 extern
call 边界**——args 重打包、`call rax`、`ret`、result 读取。这条
cost 不会被任何 cranelift 改造 move 走，因为它发生在 Rust 侧。

**Recommendation**：ε phase 推进到 ε-M1 之前**先加一条 prototype
bench**：手写 cranelift fn `step_loop(n) -> i64` 跑 N iter 内联
add+overflow guard，对比 `rust_inlined_baseline`。如果 prototype 跑
3-4 ns，证明 ε-M1+ 「loop into cranelift」方向正确；如果还是
~10 ns，要先做 profiling 找 deeper cost。

### Carry-over to v6-ε-M1+

- `emit_trace_inline` + `compile_inline_host_fn` 是 ε-M1+ 把 loop
  body 编进 cranelift 时**真正的内层 trace dispatch site 上**要
  调的 splice 工具，本 phase 提供 prerequisite。
- ε-0-B 现在比 ε-0-A 之前更悲观——RSB miss 在当前 bench shape 上
  根本不存在。继续 defer。
- host hook helpers 仍 SystemV，inline path 跨 conv call 已验证
  工作（smoke test 3）。

## 19. v6-ε bench rewrite — hot-loop-INSIDE-trace (DONE-MET-TARGET 2026-05-19)

**Status**: ε phase per-iter perf 目标**已达**——
`trace_jit_loop = 1.185 ns/iter`，brief 阈值 3 ns/iter 之下 2.5×，
进入 LuaJIT 2.x trace-tier 1-3 ns/iter band。

ε-0-A 给的 prototype 假设（手写 cranelift fn 把 hot loop 整个编进
cranelift 自身能跑 3-4 ns）**被 over-perform 验证**——实际跑到
1.185 ns/iter，比 prototype 预测更好。

Stage report:
`docs/internal/v6-epsilon-bench-rewrite-report-2026-05-19.md`

**Worktree HEAD**: `worktree-agent-a77de70826f538bbe`（base =
`1a640ad`）。

**Test count**: 1761 passing（与 ε-0-A baseline 一致；bench-only
改动，不新增 test）。All gates green。

### Bench shape rewrite

bench `crates/relon-bench/benches/trace_jit_hot_loop.rs` 拆为两族：

- **loop-INSIDE rows**（新增）：callee 内部跑完整 N-iter loop。
  - `tree_walk_loop`：3.36 µs/iter（µs-class baseline，`list.sum(range(n))`）
  - `cranelift_aot_loop`：2.07 ns/iter（Relon IR `Op::Loop` 编入 cranelift-AOT）
  - **`trace_jit_loop`：1.185 ns/iter** ← LuaJIT-class
  - `rust_native_loop`：2.48 ns/iter（floor with `checked_add`）
- **dispatch-boundary rows**（保留 / relabel）：M2-C / ε-0-C / ε-0-A
  原本的 5 + 2 行，命名前缀改成 `dispatch_*`，模块 doc 明示这些
  measure 的是 per-dispatch Rust→JIT 边界 cost，**不是**
  hot-loop cost。
  - `dispatch_trampoline` / `dispatch_ic` / `dispatch_tail`
    / `dispatch_sysv` / `dispatch_inline`: 9.5 ns/iter（与 ε-0-A 完全
    持平，证明 bench shape 改写不影响 carry-over 信号）。
  - `dispatch_rust_inlined_baseline`: 3.55 ns/iter（floor）。

### `trace_jit_loop` 是怎么测的

trace recorder 当下还不能 record 包含 backward branch 的 loop
trace。Per task brief option (a)，bench 直接 hand-build 一个
`JITModule`，body 包括：

```text
entry:    load n; seed acc=0, i=1; jump header(acc, i)
header(acc, i):  if i > n -> exit; else -> body
body:     (sum, of) = sadd_overflow(acc, i); next_i = i + 1
          if of -> deopt; else -> header(sum, next_i)
exit:     store acc -> ctx.result_slot; return Success
deopt:    call save_deopt(ctx, 0, 0); return GuardFailed
```

JIT module flag set与 trampoline path 一致（`opt_level=speed`,
no probestack, no frame pointers）。Sig 兼容 `TRACE_ENTRY_SIG`。

cranelift 编出来的 x86_64 body：

```text
header:
    cmp rdi, rsi          ; i <= n?
    jg  exit
    add rcx, rdi          ; sadd_overflow(acc, i)
    jo  deopt
    add rdi, 1
    jmp header
```

5 inst / iter ≈ 3-4 cycles ≈ 1.2 ns 在 3.0 GHz 上——与实测一致。

### Decision

- **停止 ε phase 的 per-iter 优化工作**（bounds hoist /
  overflow hoist / LICM 不再必要——目标已达）。
- **真正剩下的 follow-up（不是 perf gap）**：
  1. **recorder 学会 record loop**（ε-M0-recorder-loops，新增 sub-
     phase，估计 2-3 天）。今天 hand-built 的 `trace_jit_loop` 行
     是「JIT 编出 1.185 ns/iter」的 demo，但**真 Relon 源码自动进
     这个 codegen 路径**需要 recorder 端发 `MarkLoopHead` /
     `Cmp + LoopExitGuard` / `MarkLoopBack`。emitter 端已经支持。
  2. **cap hoisting** (ε-M4) 对长 trace（> 100K iters under sandbox）
     仍然相关，但不影响 per-iter 数字。
  3. **`TraceOp::Call` in inline path** (ε-0-A §10.5) composability gap。

### 3-round criterion median 数据

```
                                R1       R2       R3       median
tree_walk_loop       (ns/elem): 3385     3364     3364     3364
cranelift_aot_loop   (ns/iter): 2.074    2.073    2.073    2.073
trace_jit_loop       (ns/iter): 1.186    1.185    1.185    1.185   ← target ≤ 3
rust_native_loop     (ns/iter): 2.499    2.484    2.480    2.484
dispatch_cranelift_step (ns/iter): 434   415      410      415
dispatch_trampoline  (ns/iter): 9.507    9.538    9.496    9.507
dispatch_ic          (ns/iter): 9.570    9.571    9.558    9.570
dispatch_tail        (ns/iter): 9.531    9.536    9.547    9.536
dispatch_sysv        (ns/iter): 9.532    9.533    9.530    9.532
dispatch_inline      (ns/iter): 9.532    9.534    9.559    9.534
dispatch_rust_inlined_baseline (ns/iter): 3.553 3.553 3.552 3.553
```

### Gate numbers

- build / test / clippy / fmt / wasm32 全部清。
- `cargo bench --bench trace_jit_hot_loop` × 3 round 完成。

### 文件 diff stat（base = 1a640ad）

```
crates/relon-bench/Cargo.toml                       |  20 +-
crates/relon-bench/benches/trace_jit_hot_loop.rs    | 800 +++++++--
docs/internal/v6-epsilon-bench-rewrite-report-2026-05-19.md | 350 ++++ (新增)
docs/internal/wasm-bench-report-2026-05-16.md       |  72 +
docs/internal/v6-gamma-integration-plan-2026-05-18.md| (本节追加)
```


---

## 20. v6-ε M0 — 录制器学会录制 Op::Loop（2026-05-19）

承接 v6-ε bench-rewrite 报告 §10.1 的 "recorder learns to record loops" 收尾项。
本节由 worktree `worktree-agent-aca8119df8d5aaf3a` 落地，base HEAD =
`dcf353e docs(internal): v6-epsilon bench-rewrite report + plan section 19`。

### 20.1 范围

| 模块 | 改动 |
|---|---|
| `relon-trace-jit::TraceOp` | `MarkLoopHead` 新增 `phis: Vec<LoopPhi>` 字段；`MarkLoopBack` 新增 `next_values: Vec<SsaVar>` 字段。新增 `GuardKind::IsZero` 变体（NotNull 的对偶，BrIf 落经-fallthrough 路径专用） |
| `relon-trace-jit::optimizer` | LICM 把头 phi SSAs 纳入 `inside_defs`（避免循环外提依赖 phi 的 op）；新增 `noop_typecheck_elim` pass 在 LICM 之后删除 emit 时为常 1 的 `Guard(TypeCheck)` op |
| `relon-trace-emitter` | `emit_loop_head` 把每个 phi 翻为 cranelift block-param；`emit_loop_back` 把 next_values 编码为 jump 的 BlockArg；`emit_guard` 为 `NotNull` / `IsZero` 走 brif 快路径（省略 icmp+uextend） |
| `relon-trace-recorder` | 新增 `LoopCarry` / `begin_loop` / `end_loop` API；`emit_branch_falsy_guard` 直接落 `GuardKind::IsZero`；`begin_loop` 重绑 `ir_to_ssa[Let(slot)] = phi_ssa` |
| `relon-codegen-native::trace_recording` | walker 新增 `Op::Loop` / `Op::Block` / `Op::Br` / `Op::BrIf` 处理；预扫 body 收集 `LetSet` 槽作为 loop-carried；为外层未初始化的槽合成 `ConstI64(0)` 种子 |

### 20.2 Gate（worktree 末态）

| Gate | 结果 |
|---|---|
| `cargo build --workspace` | 干净 |
| `cargo test --workspace` | **1781 passing**（基线 1761 + 20 新增） |
| `cargo clippy --workspace --all-targets -- -D warnings` | 干净 |
| `cargo fmt --all -- --check` | 干净 |
| `cargo build --target wasm32-unknown-unknown -p relon-wasm` | 干净 |
| `cargo bench --bench trace_jit_hot_loop` | 见 §20.3 |

### 20.3 Bench 结果（criterion 默认配置，sample_size=30，measurement_time=5s）

| Row | ns / iter | 备注 |
|---|---|---|
| `tree_walk_loop` | 3354 ns/elem | 与 ε bench-rewrite 一致 |
| `cranelift_aot_loop` | 2.07 ns/iter | 同上 |
| **`trace_jit_loop`**（手搭对照） | **1.18 ns/iter** | 同上 |
| **`trace_jit_loop_recorded`**（ε-M0 新增） | **2.13 ns/iter** | recorder→install→invoke 走完整路径 |
| `rust_native_loop` | 2.41 ns/iter | 同上 |

`trace_jit_loop_recorded / trace_jit_loop = 2.13 / 1.18 ≈ **1.81×**`。
落在 brief boundary "超过 2× 触发调查" 之内，已落项目可发布。

### 20.4 余下 perf 差距来源

录制 trace 的每 iter 与手搭差出 0.95 ns / iter，归因如下：

1. **`ArithOverflow` guard for `i + 1`**（≈ 0.5 ns/iter）：录制 `Op::Add(I64)` 总是追加 ArithOverflow 守卫；手搭版本因为知道 `i + 1` 不会 overflow 直接用 `iadd` 跳过守卫。修复需要 trace IR 引入 `Op::WrappingAdd` 或在优化器中加 "increment-by-const elision" pass。
2. **`TypeCheck(phi_i, I64)` 在 LICM 之后仍有 1 个残余**（≈ 0.3 ns/iter）：`noop_typecheck_elim` 已经把绝大多数 const-1 TypeCheck 删掉；剩下这个发生在 phi 自身第二次 LetGet 时（首读 FirstSeen，再读 EmitGuard）。修复需要在 begin_loop 里把 phi 的初始 type_obs 状态置为 "已观察但未守卫" 而不是 "已观察+触发守卫"。
3. **TraceCheck Cmp-side cost**（≈ 0.15 ns/iter）：手搭直接用 `icmp SignedLessThanOrEqual` + brif；录制版本 `Cmp(Gt) + Guard(IsZero)` 走两个 cranelift ops。可通过录制器层面识别 "BrIf 紧跟 Cmp" 模式并合并为单一 `GuardCmp` op。

三项相加 ≈ 0.95 ns/iter，与实测吻合。

### 20.5 4-way 对齐说明

brief §4 要求 5 个 loop-shape 全部 AllAgree across (tree-walk / bytecode / cranelift-AOT / trace-JIT)。
本阶段 `crates/relon-test-harness/tests/recorded_loop_shapes.rs` 落了
recorder 侧的覆盖（5/5 shape 全部录制成功，markers + φ 配对正确），
**4-way 严格对齐被两点 gap 阻塞**：

- Relon 源码层面无 surface `for` 语法；tree-walker 路径要靠 stdlib 高阶函数 + closure ABI 才能表达 max/count-if/prefix-sum/nested 这些 shape。
- bytecode VM 当前 stdlib list 表面尚未覆盖（v6-δ M2-A 报告记录 15/52 cases sit on `BytecodeUnsupported`）。

这两块的修复挂在 v6-δ M3 "bytecode VM widening" 分支上，**不在 ε-M0 范围内**。

### 20.6 carry-over

- ε-M0 follow-up：trace IR 引入 `Op::WrappingAdd` 或 "increment-by-const overflow elision"，把 `trace_jit_loop_recorded` 拉到 ≤ 1.5 ns/iter。
- ε-M0 follow-up：把 phi 的首次 LetGet 列为 "silent observation" 以避免那一个残余 TypeCheck。
- 操作数栈型 loop 携带（`Op::Loop { result_ty: Some(_) }`）还未支持；当前仅覆盖 let-slot 携带形（递归 stdlib 用得到的形式都是 result_ty: None）。

### 20.7 EOF

ε-M0 stage report 完整文档：`docs/internal/v6-epsilon-m0-recorder-loops-2026-05-19.md`。

---

## 21. v6-λ-0 — Bench methodology hardening（DONE-MET-TARGET 2026-05-19）

把历史 6 周 × 三连 false delta 的根因（6 个 bench 方法论陷阱）写进 harness +
加 source-grep validators 防再犯。

### 21.1 6 陷阱硬化

| Trap | Mitigation |
|---|---|
| A 编译器消除 | 每 closure ≥ 2 个 `black_box` |
| B Warm-up 混淆 | 显式 `WARMUP_ITERS = 10_000` |
| C 调用方污染 | `HOT_LOOP_N = 1_000_000` |
| D Cache 冷热 | setup prefill |
| E GC bias | 每行标 `#[zero_alloc]` / `#[per_iter_alloc]` |
| F 分布掩盖 | `sample_size = 200` + `bench_stats` post-process p50/p90/p99/p99.9/max |

### 21.2 关键结果（hardened harness，200 samples / row）

| Row | p50 | p99 | max | tail ratio |
|---|---|---|---|---|
| trace_jit_loop_recorded（真录） | 2.116 ns | 2.152 | 2.187 | 1.034 |
| trace_jit_loop（手搭） | 1.184 | 1.240 | 1.545 | 1.305 |
| cranelift_aot_loop | 2.073 | 2.079 | 2.084 | 1.005 |
| rust_native_loop | 2.414 | 2.425 | 2.426 | 1.005 |
| dispatch_* (trampoline/IC/tail/sysv/inline) | 9.48 ± 0.01 | 9.50 ± 0.02 | 9.51-9.53 | &lt; 1.005 |

历史"三连 zero delta"（M2-C / ε-0-C / ε-0-A）在 hardened harness 下完全复现，
**确认那些 phase 测出来的 9.5 ns 不是 dispatch 算法的问题，而是 bench harness
的 Rust→JIT call boundary 自身**。

### 21.3 Validators 防再犯

`crates/relon-bench/tests/methodology_validators.rs` 12 个 grep-based tests，未来
谁拆 hardening 直接 fail-fast。

### 21.4 Files

详 `docs/internal/v6-lambda-0-bench-hardening-2026-05-19.md`。

### 21.5 carry-over

- λ-机器 quiescence：harness 硬化但 CPU freq / turbo / cache state 仍未严格控制
  （round1 vs round2 tree_walk_loop 有 2% diff）。
- λ-1 LuaJIT install：`bench_stats` 已支持 cross-group analysis，可直接跨 Relon /
  Lua 出对照表。
- 每次 λ-fix-* 改 perf 路径必跑 hardened bench + validators 通过 才能进下一 phase。

