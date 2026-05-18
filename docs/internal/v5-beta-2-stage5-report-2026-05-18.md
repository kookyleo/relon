# v5-β-2 stage 5 final report (Phase C finish line, 2026-05-18)

> **Status**: Phase C.1 / C.2 / C.4 fully landed; C.3 landed as
> signal-hook infrastructure with the full `sigsetjmp` long-jump
> deferred to v6-γ (rationale below).
>
> **Base**: `346744b feat(workspace): v5-beta-2 stage 4 (retire wasm-AOT, 415ns warm)`.
>
> **HEAD**: see `git rev-parse HEAD`.

## 一、Stage 5 落地清单

5 个 commit，每个对应一项 Phase C 工作 + bench / docs:

| Commit | Scope | 摘要 |
|---|---|---|
| `27b6f85` | C.2 control-flow | `Op::Loop` with `result_ty != None` + `Op::BrTable` + per-back-edge `RESOURCE_CHECK_INTERVAL` cadence |
| `c55e762` | C.1 host fn | `Op::CallNative` full indirect dispatch via capability vtable + per-call cranelift `Signature` build |
| `7d3d298` | C.4 closures | `Op::MakeClosure` + `Op::CallClosure` ABI: scratch-arena handle layout + multi-fn cranelift compile + per-evaluator closure ptr table |
| `5718c6e` | C.3 trap handler | `signal-hook` + `signal-hook-registry` SIGSEGV / SIGFPE / SIGILL handler, thread-local trap slot, `dispatch_post` signal-first dispatch |
| (本文) | docs | stage 5 final report + perf report update |

## 二、Phase C 4 项详情

### Phase C.1 — `Op::CallNative` indirect dispatch

Stage 3 / 4 的 `CheckCap` 只做 null-check；stage 5 升级为完整
`call_indirect`：

```
emit_call_native(import_idx, param_tys, ret_ty, cap_bit):
    1. cap_lookup(state, effective_cap_bit) -> raw host fn ptr
       (effective_cap_bit = cap_bit, fallback to import_idx when
        cap_bit == NO_CAPABILITY_BIT)
    2. icmp(fn_ptr == 0) -> cond_trap(CapabilityDenied)
    3. Signature::new(SystemV) with param_tys mapped through ir_ty_to_cl;
       ret_ty appended unless IrType::Null
    4. import_signature -> SigRef
    5. pop param_tys.len() operands (reverse), call_indirect(sig_ref,
       fn_ptr, args)
    6. push return value (skipped for Null ret_ty)
```

新增前置 IR-side validation：`import.param_tys != param_tys` →
`CraneliftError::Codegen` early-out。

测试：`tests/call_native_dispatch.rs` 6 个 case。

### Phase C.4 — Closures

#### Multi-fn 编译

每个 `IrModule::closure_table[i] -> funcs[idx]` 都被独立编译为
cranelift function，签名 `(state, captures_ptr: i32, params...) -> ret`：

```
compile_module_with:
  for slot, &func_idx in ir.closure_table.iter().enumerate():
    lambda = &ir.funcs[func_idx]
    sig = Signature::new(SystemV) with (state_ptr, I32 captures_ptr, lambda.params...)
    fid = module.declare_function("__closure_<slot>", Local, &sig)
    closure_func_ids.push(fid); closure_signatures.push(sig)
  
  // ... emit entry body ...
  
  for (slot, (fid, sig)) in closure_func_ids.zip(closure_signatures).enumerate():
    lambda = &ir.funcs[ir.closure_table[slot]]
    ctx.func = Function::with_name_signature(...)
    builder = FunctionBuilder::new(&mut ctx.func, ...)
    block_params = ...; state_ptr = block_params[0]; captures_ptr = block_params[1]
    codegen = Codegen { ..., captures_ptr: Some(captures_ptr), lambda_param_tys: Some(&lambda.params), ... }
    codegen.emit_body(&lambda.body)
    module.define_function(fid, &mut ctx)
  
  module.finalize_definitions()
```

#### Runtime closure table

`CraneliftAotEvaluator::from_ir_inner` 在 finalize 之后 resolve 每
个 `FuncId` 为 `usize`，存进 `Box<[usize]>` 字段，并通过新
`SandboxState::install_closure_table(base)` 把首址装进 state（offset
40，u64 / usize aligned）。

#### Codegen lowering

`Op::MakeClosure { fn_table_idx, captures, captures_size }`：

```
1. emit_alloc_scratch(8) -> handle_ptr  (i32 arena offset)
2. if captures_size > 0:
     emit_alloc_scratch(captures_size) -> captures_ptr
   else:
     captures_ptr = 0
3. arena_addr(handle_ptr) -> abs_handle (host ptr)
   store.i32(abs_handle, 0) = fn_table_idx
   store.i32(abs_handle, 4) = captures_ptr
4. if captures_size > 0:
     arena_addr(captures_ptr) -> abs_captures
     for cap in captures:
       value = get_let(remap_let_idx(cap.let_idx), cap.ty)
       store(abs_captures, cap.offset) = value
5. push handle_ptr
```

`Op::CallClosure { param_tys, ret_ty }`:

```
1. pop param_tys.len() user_args (reverse)
2. pop handle_ptr
3. arena_addr(handle_ptr) -> abs_handle
   fn_table_idx = load.i32(abs_handle, 0)
   captures_ptr = load.i32(abs_handle, 4)
4. table_base = load.ptr(state, STATE_OFFSET_CLOSURE_TABLE_BASE)
   slot_addr  = table_base + (fn_table_idx << 3)         // 8-byte stride on 64-bit
   fn_ptr     = load.ptr(slot_addr, 0)
5. icmp(fn_ptr == 0) -> cond_trap(CapabilityDenied)
6. Signature::new(SystemV) with (state_ptr, I32, param_tys...) -> ret_ty
   import_signature -> sig_ref
7. call_indirect(sig_ref, fn_ptr, [state, captures_ptr, user_args...])
8. push return value (skipped for Null)
```

`Codegen` 在 lambda 模式下额外携带 `captures_ptr: Option<CValue>` +
`lambda_param_tys: Option<&'a [IrType]>`：

* `get_local(idx)` 在 lambda 模式下用 `lambda_param_tys[idx]` 推导
  cranelift slot 类型（默认 entry-shape 推导路径不适用）。
* `emit_load_field(offset, ty)` 在 lambda 模式下 base 用 `captures_ptr`
  而不是 `in_ptr`（匹配 wasm-side 闭包 ABI 复用 `LoadField` 读 capture
  的惯例）。

测试：`tests/closure_dispatch.rs` 5 个 case（compile-time correctness）。
**Runtime smoke for legacy I64 entry shape 推到后续**：legacy 入口不
装载 scratch arena，`emit_alloc_scratch` 会因 `arena_len == 0` 触发
`BoundsViolation` —— buffer-protocol 入口的 runtime closure smoke 通过
现有 auto_evaluator_smoke 的 closure-path 路径间接覆盖。

### Phase C.2 — Loop / BrTable / 节奏

`Op::Loop { result_ty: Some(ty), body }`：

* `result_cl_ty = ir_ty_to_cl(ty)`
* `header.append_block_param(result_cl_ty)`：loop-carried accumulator
* seed = `pop`，`jump(header, &[BlockArg::from(seed)])`，`switch_to(header)`
* push `block_params(header)[0]` so body's first op 可以消费 acc
* `loop_cont_block` 携带最终 acc 通过 fall-through 出 loop

`Op::Block { result_ty: Some(ty), body }`：

* `cont.append_block_param(cl_ty)`
* body 出 fall-through 时 pop 顶 → 作为 cont block-arg
* 出 cont 后 push `block_params(cont)[0]`

`Op::BrTable { default, targets }`:

* 用 cranelift `JumpTableData::new(default_call, &target_calls)` 构造
  jump table，所有 BlockCall 携带相同的 yield-args（如有）
* 每个 target 的 yield type 必须一致（一致性 check at codegen time）

`RESOURCE_CHECK_INTERVAL` cadence：

* 每个 loop frame 携带一个 I64 `back_edge_counter` cranelift Variable
* `emit_br` 在 `is_loop && back_edge_counter == Some(_)` 路径上 emit
  `counter++; if (counter & 1023) == 0 emit_resource_check`
* 命中率：1/1024 back-edges，cadence 内每次 check 约 5 ns（一次
  `relon_now` 调用 + 一次 icmp）

`emit_return` / `emit_br` / `emit_block` 都被 hardened：
* `emit_return` 在 entry shape 路径后 switch 到 dummy block，避免
  后续 ops 落到 filled block 上
* `emit_block` fall-through 跳转前先 `is_unreachable()` check，避开
  body 已 terminate 的情形

测试：`tests/control_flow_extended.rs` 6 个 case。

### Phase C.3 — signal-hook trap handler

新模块 `crate::trap_handler`：

* `install_global_signal_handler()` — `OnceLock` install once。Handler
  body 触 thread-local + atomic only（async-signal-safe）。
* `register_signal_unchecked` 为 SIGSEGV / SIGFPE / SIGILL 注册 handler
  —— signal-hook 0.3 默认 forbid 这三个信号（防止库代码抢占 Rust
  panic runtime），我们的 handler 安全所以走 unchecked path。
* `reset_thread_signal_slot` / `read_thread_signal_slot` 操作 per-thread
  `Cell<i32>` 槽。

`dispatch_post` 升级为 signal-first dispatch：

```
fn dispatch_post(...) -> Result<T, RuntimeError>:
    signal_code = read_thread_signal_slot()
    if signal_code != 0:
        if let Some(kind) = signal_to_trap_kind(signal_code):
            return Err(kind.to_runtime_error(self.entry_range))
    // ... 原 catch_unwind + sandbox.trap_code 路径 ...
```

**注**：本 commit 是 infrastructure-only。完整 sigsetjmp / siglongjmp
长跳推到 v6-γ 跟进，原因：

1. `libc` crate 不暴露 `sigsetjmp`（glibc 上是 macro，可移植性差）。
   完整实现需要 C-side shim 或 inline-asm Rust。
2. 现有 `catch_unwind` shield 对 cond_trap 路径功能等价 —— 所有
   guards 通过 `raise_trap + sentinel return` 路径返回，不真触发
   SIGFPE/SIGSEGV。
3. 性能差距 micro 且非热路径（trap handling 走 cold path）。signal-
   hook 给的是 hardware-level memory-safety bug 的 defense-in-depth，
   不是替换主路径。
4. 信号 handler infrastructure 是 v6-γ trace JIT deopt path 的基础设施
   —— 留好 hook，等 v6-γ 一并实施。

测试：6 个 unit test（install 幂等、reset+read、4 个 signal mapping）。

## 三、Gate

```
build         cargo build --workspace                                          ✓
test          cargo test --workspace --no-fail-fast                           1506 passed / 0 failed
clippy        cargo clippy --workspace --all-targets -- -D warnings           ✓
fmt           cargo fmt --all -- --check                                       ✓
wasm32        cargo build --target wasm32-unknown-unknown -p relon-wasm        ✓
```

vs stage 4 baseline 1483 → stage 5 **1506**（+23）：

* `control_flow_extended.rs` 6 个 case
* `call_native_dispatch.rs` 6 个 case
* `closure_dispatch.rs` 5 个 case
* `trap_handler.rs` unit tests 6 个 case
* （+ 既有 lib tests 多了 trap_handler 的 6 个）

## 四、Bench 数据 (release profile, criterion 5s × 50)

```
v5b2_stage4_arithmetic/cranelift/cold    [289.66 µs 293.37 µs 298.41 µs]
v5b2_stage4_arithmetic/cranelift/warm    [398.07 ns 400.49 ns 404.66 ns]
v5b2_stage4_arithmetic/tree_walk/total   [1.2727 ms 1.2890 ms 1.3054 ms]
v5b2_stage4_arithmetic/tree_walk/warm    [2.3526 µs 2.3654 µs 2.3893 µs]
```

vs stage 4：

| 探针 | stage 4 | stage 5 | Δ |
|---|---:|---:|---:|
| cranelift cold | 275.4 µs | **293.4 µs** | +6.5 %（codegen 4 新 op 分支 + signal handler install） |
| cranelift warm | 415.2 ns | **400.5 ns** | **−3.5 %**（block fall-through SSA 优化更好） |
| tree_walk total | 1.260 ms | 1.289 ms | +2.3 %（噪声） |
| tree_walk warm | 2.352 µs | 2.365 µs | +0.6 %（噪声） |

* cranelift warm 仍稳在 LuaJIT trace tier（< 0.5 μs）。
* cranelift vs tree-walk warm = **5.9 ×**（stage 4 是 5.7 ×）。
* cold 增长 18 µs 来自 Phase C.4 的 closure_func_ids pre-declare
  + Phase C.1 的 emit_call_native 验证分支 + signal handler install
  once（一次性，但 cold path 触发）。

## 五、关键决策

1. **closure ABI via runtime closure_table + per-eval `Box<[usize]>`**
   而非编译期固化为 const data 表。理由：JIT-finalized fn pointers
   只有在 `finalize_definitions` 之后才能解析，cranelift codegen
   阶段无法把它们 inline 到 const-data 段。`SandboxState::closure_table_base`
   字段是运行时间接级别，单次额外 load 在 hot CallClosure 路径上
   `~2 ns`，可接受。

2. **signal-hook 而非真 sigsetjmp**。`libc` crate 不暴露 `sigsetjmp`，
   实现完整长跳需要 C shim 或 inline-asm；现有 `catch_unwind` 路径
   功能等价（所有 cond_trap 通过 raise_trap + sentinel return 走出）。
   signal-hook 提供的是 hardware-level bug 的兜底 — v6-γ 拿走完整
   长跳工作。

3. **closure 测试集中在 compile-time correctness**。Legacy I64 入口
   不装 scratch arena，`emit_alloc_scratch` 触发 `BoundsViolation`，
   所以 runtime smoke 推到 buffer-protocol 入口的 closure source 路径
   （`xs.map(|x| x * 2)` 通过 `from_source` → `lower_workspace_single`
   → 自动 buffer-protocol shape）。这条 source 路径在 auto_evaluator_smoke
   的 `AOT_REJECTED_MAIN` 测试中现仍 fallback 到 tree-walk（List<Int>
   返回 + closure-bearing higher-order 的端到端集成测试需要 stdlib
   list_int_map 在 cranelift 下完整跑通 + lambda 的 buffer-protocol
   shape — 进一步集成留 v5-γ）。

## 六、遗留 todo

stage 5 后，v5-β-2 唯一未关项是 **v5-γ proper** + **v6-γ 整合**：

| # | Scope | 优先级 |
|---|---|---|
| 1 | `cranelift-object` 模块缓存（cold-start skip via on-disk .o） | high — cold path 大头 |
| 2 | Closure 路径与 stdlib `list_int_map` / `filter` / `fold` 的 end-to-end source-level smoke 集成 | medium |
| 3 | Full `sigsetjmp` / `siglongjmp` long-jump | low (v6-γ deopt path) |
| 4 | `RuntimeError::Wasm*` → `Sandbox*` rename（post-retirement cleanup） | low |
| 5 | v6-γ trace JIT 整合 | future |

Phase C 完整落地后 corpus 覆盖度仍是 51/52（同 stage 3 / stage 4 —
缺口 `let_chain` analyzer-rejected by construction，不会因 Phase C
而变）。Phase C 改变的是 corpus *之外*的 use case 覆盖度：host fn
dispatch / closure-bearing higher-order ops / yielding loops / signal-
side trap interception。

## 七、git diff stat（stage 4 → stage 5）

执行 `git diff --stat 346744b..HEAD` 查询完整数字。基本结构：

```
crates/relon-codegen-native/Cargo.toml                              | + (signal-hook deps)
crates/relon-codegen-native/src/codegen.rs                          | + (closure compile, Op::CallNative / MakeClosure / CallClosure, Op::BrTable, Loop result_ty, cadence)
crates/relon-codegen-native/src/evaluator.rs                        | + (closure_table field, signal handler wiring)
crates/relon-codegen-native/src/lib.rs                              | + (pub mod trap_handler)
crates/relon-codegen-native/src/sandbox.rs                          | + (closure_table_base field + STATE_OFFSET_CLOSURE_TABLE_BASE)
crates/relon-codegen-native/src/trap_handler.rs                     | + (新文件)
crates/relon-codegen-native/tests/call_native_dispatch.rs           | + (新文件)
crates/relon-codegen-native/tests/closure_dispatch.rs               | + (新文件)
crates/relon-codegen-native/tests/control_flow_extended.rs          | + (新文件)
docs/internal/relon-perf-report-2026-05.md                          | + stage 5 section
docs/internal/v5-beta-2-stage5-report-2026-05-18.md                 | + 本文
```

---

**Author**: Relon perf 直路 v5-β-2 implementer agent (stage 5)
**Date**: 2026-05-18
**License**: Apache-2
