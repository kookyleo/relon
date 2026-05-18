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
| M4 | 3 runtime helper register + deopt 路径回 generic | 3 天  | guard 失败时 host dispatcher 能读到 `DeoptStateSnapshot` 并把值写回 generic frame；recorder `record_guard` 同步；`__relon_jump_to_recorder` 接真 IR walker | 单 agent           |
| M5 | differential harness 三方对比 + bench            | 4 天  | 52 case 三方一致；deopt path coverage ≥ 4 GuardKind；hot loop micro-bench < 5 ns/iter | 单 agent           |

**总计 ~16-19 天 ≈ 3 周**。原设计稿 §6 估算 8-12 周，prep 阶段已经把 5-9 周工作
做完，**整合 phase 实际剩 3 周**。

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
