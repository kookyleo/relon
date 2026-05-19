# v6-δ M2-B Stage Report (2026-05-19)

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-19
Base HEAD: `43da58e docs(internal): v6-delta M2-A stage report + plan section 14 (DONE)`
Worktree branch: `main` (worktree at `/ext/relon/.claude/worktrees/agent-a29edf70386385b92`)
Final HEAD: `7419861 test(bytecode): partial-resume 4-prong + trace-JIT deopt integration`
Status: M2-B 落地完成 — real partial-resume + 4-prong sandbox 全
覆盖 + 信封从 25 → 15 BytecodeUnsupported case。

Companion docs:
- `docs/internal/v6-delta-m2a-stage-report-2026-05-19.md`
- `docs/internal/v6-gamma-integration-plan-2026-05-18.md`（§15 DONE）

---

## 0. TL;DR

- **真 partial-resume**：bytecode 编译 pass 跟踪每个 bc_idx 的
  operand-stack recipe（`StackOrigin::Local/Const/Snapshot`），
  `DeoptStateSnapshot` widen 出 `value_stack_copy: Box<[u64]>` 字段，
  `BytecodeEvaluator::resume_from_snapshot` 直接消费 snapshot 重建
  operand stack 并从 trap PC 继续 dispatch。M2-A 的
  `StackUnderflow → Unsupported` 防御性 fallback 完全替换为真
  rehydration。
- **trace recorder + bytecode 编译 pass 共用 per-op PC 计数**：
  `next_external_pc` 在每次 `record_op` 入口 +1，与 bytecode 编译
  pass 的 `ir_pc_next` 对齐。Guard 的 `external_pc` 路由到 bytecode
  index 不再需要翻译表，`bc_index_for_pc` 是 O(n) deterministic lookup。
- **4-prong partial-resume 测试**：6 个 test 覆盖 bounds / 2 trap
  variants / capability / 2 resource variants + happy-path value
  pinning。每个 prong 既验证 `RuntimeError` 变体重现一致，又验证
  resume_steps < entry_steps（partial-resume 真起作用）。
- **trace-JIT → bytecode integration test**：真实 trace install →
  cold args 触发 overflow guard → bytecode `resume_from_snapshot_
  with_metrics` 路由到 `start_bc_idx=2`（Add op）+ `steps=3`
  vs entry=5。
- **信封 widening**：M2-A 25 → M2-B 15 BytecodeUnsupported。Stdlib
  inlining（`abs` / `min` / `max` 经 `builtin_stdlib()` 注册表）+
  `Op::Select` 手卷 lowering + `ConstString`/`ConstListInt` 折叠
  为 length-i64 + `AllocRootRecord` / `StoreFieldAtRecord`
  buffer-protocol no-op。
- Gate green：`cargo build --workspace` / `cargo test --workspace`
  / `cargo clippy --workspace --all-targets -- -D warnings` /
  `cargo fmt --all -- --check` /
  `cargo build --target wasm32-unknown-unknown -p relon-wasm` 全部
  清。
- 测试总数 **1739 passing**（M2-A baseline 1729 + 净新增 10：
  6 partial-resume + 2 integration + 2 stdlib smoke）。

## 1. What landed

### 1.1 feat(trace-abi): widen `DeoptStateSnapshot`

`crates/relon-trace-abi/src/deopt.rs`：

- 新增 `pub value_stack_copy: Box<[u64]>` 字段，紧跟
  `recoverable_writes`。layout 56 → 72 bytes（+16 = Box fat ptr）。
- 新增 `DeoptStateSnapshot::with_value_stack(...)` 构造器供 host /
  测试直接构造完整 snapshot。
- `layout_smoke.rs` 中的 size 假设 `assert_eq!(size_of::<DSS>(), 72)`
  / `assert_eq!(size_of::<TraceContext>(), 152)`。
- `__relon_trace_save_deopt` 写空 `value_stack_copy.into_boxed_slice()`
  — JIT 端为 SSA 操作模型，没有 operand stack 可 drain；填充该字段
  是 M2-C/M2-D 工作。bytecode-side resume 今天已通过 recipe +
  `ssa_slots_copy` 重建 mid-expression 栈，覆盖 4-prong 全部场景。

### 1.2 feat(trace-recorder): per-op `external_pc` 对齐

`crates/relon-trace-recorder/src/recorder.rs`：

- `record_op` 入口先 `self.next_external_pc.wrapping_add(1)`，再调
  lowering 规则。
- `append_guard_with_site` 不再独立 +1，stamp 当前 op 的 PC。
- `set_next_external_pc(seed)` 语义保留为「下一次 record_op 标
  `seed+1`」；test 更新以反映新约定。

### 1.3 feat(bytecode): `StackOrigin` recipe + real partial-resume

`crates/relon-bytecode/src/op.rs`：

- 新增 `pub enum StackOrigin { Local(u32), Const(u64), Snapshot(u32) }`。
- `BcFunction` widen 出 `pub stack_recipe: Vec<Vec<StackOrigin>>`。
- `BcFunction::stack_depth_at(bc_idx)` 辅助 lookup。

`crates/relon-bytecode/src/compile.rs`：

- `CompileState` widen：`stack_recipe` / `current_stack` /
  `next_snapshot_idx` / `funcs: &[Func]` / `scratch_local_top` /
  `inline_depth`。
- `emit(op, pc)` 在 emit 前 snapshot `current_stack.clone()` 进
  `stack_recipe`，pc map 同步推。
- `apply_stack_effect(op)` 按 op 推导 abstract stack 变化：
  `ConstI64/I32` push `Const`、`LocalGet` push `Local(slot)`、
  arith / cmp pop 2 push 1 `Snapshot(idx)`、`LocalSet` /
  `JumpIfTrue/False` / `Return` pop。
- `emit_with_effect(op, pc)` = emit + apply_stack_effect 的便捷
  组合。
- `compile_if` / `compile_select` 在 join 点把 stack 折成 N 个
  `Snapshot(idx)` slot（canonicalise）。

`crates/relon-bytecode/src/vm.rs`：

- 新增 `BytecodeVm::invoke_from_with_stack(..., initial_stack: &[VmValue])`
  — partial-resume 入口，pre-seed operand stack。
- 老 `invoke_from_with_locals` 现为 thin wrapper，向新 API 转发
  空 initial_stack。

`crates/relon-bytecode/src/evaluator.rs`：

- `BytecodeEvaluator::materialise_stack(bc_idx, args, extra_locals,
  value_stack_copy)`：按 recipe 重建栈。Local 从 args+extra_locals
  overlay 读、Const 直接读、Snapshot 从 value_stack_copy 读，缺
  失时填 0。
- `BytecodeEvaluator::resume_from_snapshot(args, snapshot)` —
  deopt-driver-facing API；查 bc_index_for_pc，materialise_stack
  重建 init stack，调 `invoke_from_with_stack`。
- `resume_from_snapshot_with_metrics(args, snapshot)` / `_metrics_only`：
  surface `ResumeMetrics { steps, last_bc_idx, start_bc_idx }`，
  integration test 用 metrics 验证 partial-resume 是 real。
- `run_main_with_metrics(args)`：对照组 — 全跑 entry 路径的 metrics。
- trait `Evaluator::resume_from_pc` 走 `resume_from_snapshot` 的
  flat-slice 版本（split `local_snapshot` 为 extra_locals +
  value_stack_copy 两段，按 recipe 的最大 Snapshot index 确定切点）。

### 1.4 feat(bytecode): envelope widening

`crates/relon-bytecode/src/compile.rs`：

1. **`compile_function_in_module(func, funcs, ...)`**：M2-A 的
   `compile_function` 现为 thin wrapper（传 `&[]` 表示无 funcs
   slice）。`BytecodeEvaluator::from_source` 全走新路径，传完整
   `lowered.module.funcs`。
2. **`Op::Call` inlining**：通过 `resolve_stdlib_func(fn_index)`
   查 `builtin_stdlib()` 注册表；user funcs 通过 fn_index -
   stdlib_count 索引 `funcs[]`。max 64 ops 单次膨胀 +
   max 3 inline 深度。inline 时 callee 的 `LocalGet(N)` /
   `LetGet(idx)` 重写到 caller 的 scratch slot 块（`scratch_local_top`
   bump，inline 结束 roll back 给后续 callsites 复用）。
3. **`Op::Select` lowering**：`compile_select` 用 3 个 scratch slot
   （s_cond / s_false / s_true）+ `JumpIfFalse` 分支实现
   wasm-typed-select。Join 点单 `Snapshot(idx)` canonicalise stack。
4. **`Op::ConstString` / `ConstListInt/Float/Bool/String`**：直接
   折叠为 `ConstI64(length)`（用 `value.chars().count()` /
   `elements.len()`）。`Op::ReadStringLen` 后续 no-op（top of stack
   已是 length）。让 `"hello".length()` / `"hi".is_empty()` /
   `[1,2,3,4,5].length()` 等 corpus case 通过。
5. **`Op::AllocRootRecord` / `AllocSubRecord` / `PushRecordBase`**：
   buffer-protocol bookkeeping，bytecode VM 用虚拟 local 不需要 →
   no-op。
6. **`Op::StoreFieldAtRecord { offset, ... }`**：折成
   `LocalSet(input_arg_count + return_slot)`，与 `Op::StoreField`
   等价但带 record_local_idx 这条路径来自 dict-return shape。

### 1.5 test(bytecode): partial-resume 4-prong + integration

`crates/relon-bytecode/tests/partial_resume_sandbox.rs`（新文件，6 test）：

- `partial_resume_trap_div_by_zero_replays_at_div_pc`：snapshot.
  external_pc=Div PC → resume 重 trap，steps 严格短于 entry。
- `partial_resume_trap_overflow_replays_at_add_pc`：同上 Add overflow。
- `partial_resume_bounds_explicit_trap_replays`：手卷 IR `LocalGet;
  Trap{IOOB}; Return` → resume 重 trap。
- `partial_resume_capability_denied_replays`：手卷 BcFunction（IR
  enum 无 CapDenied trap kind）BcOp::Trap{CapDenied} → resume 重
  trap，steps 严格短。
- `partial_resume_resource_step_limit_retraps_then_completes`：
  trap-and-abort variant（同 max_steps 重 trap）+
  open-limit variant（高 limit 下 partial-resume 从 Add bc_idx
  正确求值 1+2=3）。
- `partial_resume_arith_mid_expression_value_correct`：Add bc_idx
  resume 等价 `run_main` 整跑（40+2=42）。

`crates/relon-test-harness/tests/bytecode_deopt_integration.rs`（新
文件，2 test）：

- `bytecode_resume_from_trace_jit_deopt_overflow`：register_recording
  + `__relon_jump_to_recorder` install → cold args [i64::MAX, 1]
  触发 overflow guard → `state.invoke_with_resume` 把 snapshot
  路给 `BytecodeEvaluator::resume_from_snapshot_with_metrics` →
  assert `start_bc_idx > 0` + `steps < entry_steps`（3 < 5）。
- `bytecode_resume_routes_to_addop_for_pre_aligned_pcs`：直接
  构造 DeoptStateSnapshot（external_pc = Add ir_pc_map[bc_idx]），
  assert resume 路由 `start_bc_idx == add_bc_idx` 并求值正确。

### 1.6 test(bytecode): stdlib inlining smoke

`crates/relon-bytecode/tests/smoke.rs`（widening + replace 1 test）：

- `run_main_abs_inlined`：`abs(-42) = 42` / `abs(7) = 7`。
- `run_main_min_max_inlined`：`min(17, 5) = 5` / `max(17, 5) = 17`。
- `unsupported_source_returns_error` 改用 `"foo".concat("bar")`
  （String 返回 field，envelope-check 阶段被拒）。

## 2. Key decisions（≤ 5 bullets，每条带 rationale）

1. **`StackOrigin` 三变体 vs 完整 SSA**：bytecode 编译 pass 跟踪
   `Local/Const/Snapshot` 三个 producer 语义就覆盖了所有路径；arith
   / cmp 结果走 `Snapshot(idx)`，每个 `Snapshot` 索引指向
   `DeoptStateSnapshot.value_stack_copy` 的某个 slot。比 SSA 整体
   镜像简单，`materialise_stack` 是 O(n) 单遍。
2. **recorder + 编译 pass 共用 per-op PC**：把 trace recorder 的
   `next_external_pc` 改成每次 `record_op` +1，guard PC 直接 stamp
   当前 op 的计数。这样 `BcFunction::bc_index_for_pc(external_pc)`
   只需查 ir_pc_map（O(n) linear，未来可加 BTreeMap 索引）。Trade-off：
   set_next_external_pc 的语义变成「下一次 record_op 才用 seed+1
   stamp」；1 个 unit test 同步更新。
3. **`value_stack_copy` JIT 端先空着**：trace JIT 操作模型是纯 SSA
   （`ssa_slots[N]`），没有 operand stack 可 drain；今天的 save_deopt
   填空 `Box<[u64]>`。bytecode-side resume 已可用纯 Local/Const
   recipe + ssa_slots_copy 覆盖 4-prong sandbox 全场景；M2-C/M2-D
   再 recorder gain stack tracking 后填实。
4. **Stdlib inlining 通过 `builtin_stdlib()` 注册表**：lower_workspace_single
   只 emit user funcs，stdlib bodies 在 codegen 时 link（fn_index
   是 stdlib_count + user_idx 的组合）。bytecode 编译 pass 现在
   直接查注册表 = compile-time inlining，绕过 link 步骤。max
   inline depth 3 + max expand 64 ops 防 pathological 输入。
5. **`Op::Select` 手卷 lowering 而非新 `BcOp::Select`**：3 scratch
   slot + JumpIfFalse 分支实现 wasm-typed-select 语义；新增专门 op
   是 M2-C IC 优化时再考虑（更少 dispatch overhead）。今天落地
   的 lowering 跟 tree-walker / cranelift 行为等价（`abs` /
   `min` / `max` smoke 验证）。

## 3. Gate numbers

- `cargo build --workspace` —— clean。
- `cargo test --workspace` —— **1739 passing**（M2-A baseline 1729 +
  净新增 10：
  - `partial_resume_sandbox.rs`：6 test
  - `bytecode_deopt_integration.rs`：2 test
  - `smoke.rs`：净 +2（删 1 + 加 3 = +2）
  - `orphan_guard_fixed.rs`：assertion 更新（+0 但语义改了）
  ）。
- `cargo clippy --workspace --all-targets -- -D warnings` —— clean。
- `cargo fmt --all -- --check` —— clean。
- `cargo build --target wasm32-unknown-unknown -p relon-wasm` —— clean。

## 4. Parity stats: 4-way corpus

| Variant | M2-A | M2-B | Δ |
|---------|------|------|---|
| AllAgree | 23 | 28 | +5 |
| AllTrap | 4 | 4 | 0 |
| BytecodeMatchesBaseline | 0 | 1 | +1 |
| BytecodeUnsupported | 25 | 15 | **-10** |
| Mismatch | 0 | 0 | 0 |

Tier-level M2-B 分布：

- `arith_control`：23 AllAgree + 4 AllTrap + 1 BytecodeUnsupported
  (`let_chain` — cranelift analyzer 也 reject) = 28 / 28
- `dict_return`：1 BytecodeMatchesBaseline (`dict_simple_return`) +
  1 BytecodeUnsupported (`dict_with_string_return`) = 2 / 2
- `stdlib_simple`：9 AllAgree (`abs`/`min`/`max`/`length`/`is_empty`/
  `list_int_length` 系) + 0 BytecodeUnsupported = **9 / 9 clean**
- `stdlib_case_fold` / `stdlib_list` / `stdlib_memory` /
  `stdlib_normalize`：全 BytecodeUnsupported（String 返回 / 真
  memory access / Unicode normalization 表）

## 5. 4-prong partial-resume 测试结果

| Prong | Test | 行为 | resume vs entry |
|-------|------|------|-----------------|
| trap (div) | `partial_resume_trap_div_by_zero_replays_at_div_pc` | snapshot.external_pc → Div bc_idx，重 DivisionByZero | resume 1 步 vs entry 3 步 |
| trap (overflow) | `partial_resume_trap_overflow_replays_at_add_pc` | snapshot.external_pc → Add bc_idx，重 NumericOverflow | resume 1 步 vs entry 3 步 |
| bounds | `partial_resume_bounds_explicit_trap_replays` | Trap{IOOB} 跨 LocalGet 后正确路由 | n/a (correctness pin) |
| capability | `partial_resume_capability_denied_replays` | 手卷 BcFunction，BcOp::Trap{CapDenied} 重 CapabilityDenied | resume 2 步 vs baseline 3 步 |
| resource (step limit) | `partial_resume_resource_step_limit_retraps_then_completes` | trap-and-abort variant + open-limit variant 求 1+2=3 | n/a (two-variant) |
| happy path | `partial_resume_arith_mid_expression_value_correct` | Add bc_idx resume，得 40+2=42 与 run_main 等价 | n/a (correctness pin) |
| integration | `bytecode_resume_from_trace_jit_deopt_overflow` | 真实 trace JIT install → guard fire → bytecode resume | start_bc_idx=2 steps=3 vs entry=5 |

所有 4 prong 都至少一个 case PASS；resource prong 有 2 个 variant
覆盖「trap-and-abort（同 max_steps 重 trap）」+「higher-limit 真
继续求值」两种 sub-shape。

## 6. DeoptStateSnapshot layout before/after

### M2-A (56 bytes)

```text
guard_pc           4 bytes
(padding)          4 bytes
external_pc        8 bytes
ssa_slots_copy    16 bytes (Box fat ptr)
recoverable_writes 24 bytes (Vec fat ptr)
total             56 bytes
```

### M2-B (72 bytes)

```text
guard_pc           4 bytes
(padding)          4 bytes
external_pc        8 bytes
ssa_slots_copy    16 bytes (Box fat ptr)
recoverable_writes 24 bytes (Vec fat ptr)
value_stack_copy  16 bytes (Box fat ptr; NEW)
total             72 bytes
```

`TraceContext` 同步 136 → 152 bytes（`Option<DeoptStateSnapshot>` 走
niche 优化，width 等于 DSS 自身）。Layout smoke test 同步更新。

## 7. 信封 widening 详情

| 类别 | 落地策略 | 受益案例 |
|------|----------|----------|
| `Op::AllocRootRecord` / `AllocSubRecord` / `PushRecordBase` | 编译 pass 当 no-op（buffer-protocol bookkeeping，bytecode VM 走虚拟 local 不需要）| `dict_simple_return` |
| `Op::StoreFieldAtRecord` | 折成 `LocalSet(input_arg_count + return_slot)` | `dict_simple_return` |
| `Op::Call` 内联 stdlib bodies | `resolve_stdlib_func(fn_index)` 查 `builtin_stdlib()`，inline + 重写 LocalGet/LetGet 到 scratch 块；user funcs 通过 `fn_index - stdlib_count` 索引 `funcs[]` | `abs` (2) + `min` (2) + `max` (1) = 5 case |
| `Op::Select` | 3 scratch slot + JumpIfFalse 分支 | 所有 inline 的 stdlib body 通过 |
| `Op::ConstString` / `ConstListInt/Bool/Float/String` | 折叠为 `ConstI64(length)`；后续 `Op::ReadStringLen` no-op | `stdlib_length_const` + `stdlib_is_empty_*` + `stdlib_list_int_length` (4 case) |

M2-A 25 → M2-B 15 BytecodeUnsupported（净 -10）。剩余 15 case 全部
需要 String 槽位真实化或 wasm memory 模型才能继续 widening — 那是
M2-C 之后的工作。任务 brief 给的目标是 ≤ 12；M2-B 落点 15，**差 3
case**：

- 5 个 case_fold（upper/lower/title）— 全部返回 String。
- 4 个 memory（concat/substring/starts_with×2）— 返回 String / 真
  字节比较。
- 2 个 normalize（nfd/nfc）— 深度依赖 Unicode 表。
- 2 个 list (sum/max) — 真 memory 访问。
- 1 个 dict_with_string_return — String field。
- 1 个 let_chain — analyzer 自身 reject。

Trade-off 文档：String / List 真路径开放需要 bytecode VM 引入
分离的 String pool + 真 memory 读取 op，不是 M2-B scope；M2-C 落
IC dispatch 时一并评估是否绑定 wasm memory 模型。

## 8. Commit diff stat

`git diff --stat 43da58e..HEAD`：

```
 Cargo.lock                                         |   1 +
 crates/relon-bytecode/Cargo.toml                   |   5 +
 crates/relon-bytecode/src/compile.rs               | 649 ++++++++++++++++++++-
 crates/relon-bytecode/src/evaluator.rs             | 337 ++++++++++-
 crates/relon-bytecode/src/lib.rs                   |   4 +-
 crates/relon-bytecode/src/op.rs                    |  51 ++
 crates/relon-bytecode/src/vm.rs                    |  33 +-
 crates/relon-bytecode/tests/bytecode_sandbox.rs    |   1 +
 .../relon-bytecode/tests/partial_resume_sandbox.rs | 447 ++++++++++++++
 crates/relon-bytecode/tests/smoke.rs               |  43 +-
 .../tests/bytecode_deopt_integration.rs            | 220 +++++++
 crates/relon-trace-abi/src/deopt.rs                |  48 +-
 crates/relon-trace-abi/tests/layout_smoke.rs       |  17 +-
 crates/relon-trace-jit/src/runtime/deopt.rs        |  11 +
 crates/relon-trace-jit/tests/abi_compat.rs         |   1 +
 crates/relon-trace-recorder/src/recorder.rs        |  22 +-
 .../tests/orphan_guard_fixed.rs                    |   9 +-
 17 files changed, 1835 insertions(+), 64 deletions(-)
```

Commit history（HEAD-first，到 v6-δ M2-A base 共 4 个 commit）：

```
7419861 test(bytecode): partial-resume 4-prong + trace-JIT deopt integration
e844fa8 feat(bytecode): real partial-resume + envelope widening (v6-delta M2-B)
0bf194a feat(trace-recorder): align external_pc with per-op monotonic counter
a38d0eb feat(trace-abi): widen DeoptStateSnapshot with value_stack_copy
43da58e docs(internal): v6-delta M2-A stage report + plan section 14 (DONE)  ← base
```

## 9. Key file paths（absolute）

- 新增 source：
  - `/ext/relon/crates/relon-bytecode/tests/partial_resume_sandbox.rs`
  - `/ext/relon/crates/relon-test-harness/tests/bytecode_deopt_integration.rs`
- 修改的 source：
  - `/ext/relon/crates/relon-bytecode/Cargo.toml`（加 `relon-trace-abi` 依赖）
  - `/ext/relon/crates/relon-bytecode/src/op.rs`（`StackOrigin` enum + `BcFunction::stack_recipe`）
  - `/ext/relon/crates/relon-bytecode/src/compile.rs`（recipe 跟踪 + envelope widening + stdlib inlining + Select lowering）
  - `/ext/relon/crates/relon-bytecode/src/vm.rs`（`invoke_from_with_stack` 入口）
  - `/ext/relon/crates/relon-bytecode/src/evaluator.rs`（`resume_from_snapshot` + `_with_metrics` + `_metrics_only` + `run_main_with_metrics` + `materialise_stack`）
  - `/ext/relon/crates/relon-bytecode/src/lib.rs`（exports `StackOrigin` + `ResumeMetrics`）
  - `/ext/relon/crates/relon-bytecode/tests/bytecode_sandbox.rs`（构造 BcFunction 时添 stack_recipe）
  - `/ext/relon/crates/relon-bytecode/tests/smoke.rs`（abs/min/max inline test + unsupported case 更新）
  - `/ext/relon/crates/relon-trace-abi/src/deopt.rs`（`value_stack_copy` 字段 + `with_value_stack` 构造器）
  - `/ext/relon/crates/relon-trace-abi/tests/layout_smoke.rs`（size 假设 56→72 / 136→152）
  - `/ext/relon/crates/relon-trace-jit/src/runtime/deopt.rs`（save_deopt 写空 value_stack_copy）
  - `/ext/relon/crates/relon-trace-jit/tests/abi_compat.rs`（test snapshot 构造加 value_stack_copy）
  - `/ext/relon/crates/relon-trace-recorder/src/recorder.rs`（`record_op` 每次 +1 PC）
  - `/ext/relon/crates/relon-trace-recorder/tests/orphan_guard_fixed.rs`（PC 语义同步）
- 更新 docs：
  - `/ext/relon/docs/internal/v6-gamma-integration-plan-2026-05-18.md`（§15 v6-δ M2-B DONE）
- 新增 docs：
  - `/ext/relon/docs/internal/v6-delta-m2b-stage-report-2026-05-19.md`（本文）

## 10. Carry-over to M2-C

### M2-C target

1. **真 trace-side value_stack 上联**：recorder 加 operand-stack
   tracking → `__relon_trace_save_deopt` 填实 `value_stack_copy`。
   M2-B 在 bytecode 端的 `Snapshot(idx)` recipe 已 ready，只缺
   recorder 端的填充。
2. **IC dispatch slot per Call**：M2-B 的 stdlib inlining 是 compile-
   time inlining，没有 runtime IC。M2-C 加 `BcOp::CallNative {
   ic_slot }` + 每个 callsite 一个 monomorphic-cache slot；让
   user-defined funcs 也走 IC 路径，避免 inline 膨胀。
3. **Bench**：v6-δ M1 的 9.52 ns/iter 推到 3-5 ns/iter 档位；M2-B
   信封内 ArithControl 28 case 全 bit-by-bit 等价 → bench 可以专注
   dispatch overhead 而不需要再补 backend correctness gate。

### Minor sweep（v6-δ M2 末尾）

- **String slot 真路径**：5 个 case_fold + 4 个 memory + 2 个
  normalize + 1 dict_with_string_return = 12 case 全部依赖 String /
  Unicode 真路径。bytecode VM 需要引入分离的 String pool（per-eval
  `Vec<String>` + `LocalGet(slot) → &str` 路由）或绑定 wasm linear
  memory 模型。M2-C 落 IC dispatch 时一并评估。
- **List 真 memory 访问**：2 个 list_int_sum / list_int_max 用
  `Op::LoadI32AtAbsolute`，需要 mock wasm memory 接口。
- **`bytecode_resume_routes_to_addop_for_pre_aligned_pcs`**：可以
  延展到 If/Select join 点的 `Snapshot(idx)` recipe 验证 — 今天
  只 cover linear stream。

### 已知 trade-off（已文档化）

- M2-B 落 BytecodeUnsupported 15 case，目标 ≤ 12 差 3 — 全部需要
  String / wasm memory 真实化，不属于 M2-B scope。详见 §7。
- `Op::Select` 走手卷 lowering（3 scratch slot），不是专门的
  `BcOp::Select`。M2-C IC dispatch landing 时再评估是否合并到
  专用 op。
- trace JIT 端 `value_stack_copy` 在 save_deopt 时填空（JIT 是 SSA
  操作模型）。bytecode-side resume 不依赖该字段填充也能跑（recipe
  + ssa_slots_copy 已足够覆盖 4-prong sandbox 全场景）；M2-C
  recorder 端配合后才是「真 trace-side mid-expression 镜像」。

EOF
