# v6-δ M2-A Stage Report (2026-05-19)

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-19
Base HEAD: `d5c61e8 feat(trace-jit): v6-delta M1 clear 5 residuals + real hot-loop bench`
Worktree branch: `main` (worktree at `/ext/relon/.claude/worktrees/agent-a316302bad18f26ca`)
Final HEAD: `1e76cc4 test(harness): 4-way differential corpus + bytecode parity gate`
Status: M2-A 落地（scaffolding only）。新 crate `relon-bytecode`、
`Backend::Bytecode` facade 接入、4-way 差分 harness、4-prong sandbox
prong 全部 PASS；partial-resume mid-expression PC rehydration 留给 M2-B。

Companion docs:
- `docs/internal/v6-delta-m1-stage-report-2026-05-19.md`
- `docs/internal/v6-gamma-integration-plan-2026-05-18.md`（§14 DONE）

---

## 0. TL;DR

- 新 crate `relon-bytecode`：stack-based bytecode VM 直接消费
  `relon_ir::Op`；每个编译函数携带 `ir_pc_map: Vec<ExternalPc>`，
  `Evaluator::resume_from_pc` override 路由 deopt 的 `external_pc`
  回 bytecode index。M2-A 核心 deliverable 就是这张 PC 表 +
  `resume_from_pc` trait override。
- 4-way 差分 harness（tree-walk / cranelift / trace-jit / bytecode）：
  52-case corpus 0 mismatches，ArithControl tier 23 AllAgree + 4
  AllTrap（trap-on-trap 等价）= 27/28 干净；1 `BytecodeUnsupported`
  是 `let_chain`（cranelift analyzer reject 的同一 case）。
- 4-prong sandbox prong 测试：bounds / trap / capability / resource
  逐个落地，每个 prong 至少一个 case 通过 `RuntimeError` 变体断言。
  resume-from-pc replay 测试覆盖 entry-PC + unknown-PC + 已知-trap-PC
  三条 happy/fallback 路径；mid-expression PC rehydration 显式标
  注为 M2-B 工作（`DeoptStateSnapshot` 不携带 SSA value stack，VM
  resume 触发 `BcVmError::StackUnderflow`）。
- Gate green：`cargo build --workspace` / `cargo test --workspace`
  / `cargo clippy --workspace --all-targets -- -D warnings` /
  `cargo fmt --all -- --check` /
  `cargo build --target wasm32-unknown-unknown -p relon-wasm` 全部
  清。
- 测试总数 **1729 passing**（M1 baseline 1703 + 净新增 26：
  bytecode crate unit + smoke + sandbox + 4-way diff corpus 系列）。

## 1. What landed

### 1.1 feat(bytecode): new crate `relon-bytecode`

`crates/relon-bytecode/` 全新 crate。8 个文件 ~2100 LoC：

- **`src/op.rs`** — `BcOp` flat opcode enum（arith / cmp / control
  flow / locals / Trap / Jump 系），`BcFunction { ops, locals,
  ir_pc_map }` 容器，`ExternalPc = u64` 类型 alias，
  `bc_index_for_pc(external_pc)` 路由器（sentinel `0` 留给函数入口）。
- **`src/compile.rs`** — `compile_function(func, in_offset_map,
  return_offset_map) -> BcFunction`。两遍 walk：pass 1 emit ops 带
  `usize::MAX` 占位 branch target，pass 2 patch；标签栈 + label_depth
  解析按 wasm verifier 的 `pop_label` 规则。Schema-aware：
  `LoadField {offset}` → 用 `OffsetTable` 找到对应 local slot 翻成
  `LocalGet(slot)`；`StoreField {offset}` → `LocalSet(return_base +
  slot)`；`LetGet`/`LetSet` 落在 `(input_args + return_fields + idx)`
  位置避免冲突。`build_offset_to_local(layout) -> BTreeMap` 暴露给
  evaluator 用。
- **`src/vm.rs`** — `BytecodeVm` match-based dispatch 循环。
  `BcVmConfig` 控 max_steps / deadline / `CapabilityVtable`；
  `BcVmError` 4-prong 分类（StepLimitExceeded / DeadlineExceeded /
  DivisionByZero / NumericOverflow / JumpOutOfRange /
  IndexOutOfBounds / EmptyList / InvalidUtf8 / CapabilityDenied /
  StackUnderflow），`into_runtime_error(entry_range)` 单口 lift 到
  `RuntimeError`。Arith 走 `checked_*` 系：overflow 路 `NumericOverflow`，
  div/mod by zero 路 `DivisionByZero`，`i64::MIN / -1` 也路 `NumericOverflow`
  保持 tree-walker 等价。`Return` 容忍空栈（buffer-protocol IR 的
  `StoreField + Return` 序列里 Return 时栈已被 StoreField 清空）。
  `invoke_from_with_locals(func, args, start_bc_idx, extra_locals,
  return_slot_count)` 是 partial-resume 入口；返回 `BcRunOutcome`
  附带 `final_locals` snapshot 让 evaluator unpack 多字段 record。
- **`src/evaluator.rs`** — `BytecodeEvaluator` 实现
  `relon_eval_api::Evaluator`。`from_source` 跑 parse + analyze +
  `lower_workspace_single` + bytecode compile；envelope reject 非标量
  参数 / 返回 fields（List / Dict / String / Closure / Schema）。
  非-`run_main` 4 个方法都返回 `Unsupported`（mirror cranelift）；
  `resume_from_pc(args, external_pc, local_snapshot)` 是真 override：
  `bc_index_for_pc(external_pc).unwrap_or(0)` 路由 +
  `invoke_from_with_locals` 重入。Unknown PC 走 entry fallback，
  trap PC 在 args round-trip 后真重跑（测试覆盖）。

### 1.2 feat(relon): wire `Backend::Bytecode` + CLI

`crates/relon/src/lib.rs` + `crates/relon-cli/src/main.rs`：

- `Backend` enum 新增 `Bytecode` variant。
- `BackendError` 新增 `Bytecode(String)`。
- `new_evaluator(source, Backend::Bytecode)` 经
  `BytecodeEvaluator::from_source` 包成 `Box<dyn Evaluator>`。
- CLI `BackendArg::Bytecode` 接 `--backend=bytecode`；library-mode
  `eval_root` rejected（与 cranelift-AOT 一致 — bytecode VM 只跑
  `#main(...)` 入口）。

### 1.3 test(harness): 4-way differential runner

`crates/relon-test-harness/src/four_way.rs` + `tests/bytecode_diff.rs`：

- `FourWayResult` enum：AllAgree / AllTrap / BytecodeMatchesBaseline
  / BytecodeUnsupported / Mismatch。
- `diff_test_4way(source, args)` 在 `diff_test_3way` 之上 overlay
  bytecode VM tier；分类逻辑包含特殊路径：当三-way 已经因为 tw + cr
  都 trap 而走 `TraceJitNotApplicable { reason: "trace_jit_skipped_trap..."`
  且 bytecode 也 trap 时，count 为 `AllTrap`（三个 real backend 同
  trap envelope）。
- `bytecode_vs_treewalk(source, args)` helper 做 strict 1:1 parity
  检查（None = bytecode unsupported，Some(true/false) = 一致 / 不一致）。
- `tests/bytecode_diff.rs`：
  - `corpus_four_way_diff_aggregates`：全 52 case 0 mismatch；
    ArithControl tier ≥ 27 干净（actual 27）；每 case reach passing。
  - `corpus_bytecode_vs_treewalk_strict_parity`：ArithControl 28 case
    跑 strict parity，0 diverged（27 clean + 1 unsupported = `let_chain`）。

### 1.4 test(sandbox): 4-prong + resume_from_pc replay

`crates/relon-bytecode/tests/bytecode_sandbox.rs` — 9 test：

- `sandbox_trap_div_by_zero` / `sandbox_trap_numeric_overflow`：trap-
  prong（DivisionByZero / NumericOverflow）。
- `sandbox_bounds_explicit_trap_op`：bounds-prong（hand-rolled IR 带
  `Op::Trap{IndexOutOfBounds}`，lift 到 `WasmIndexOutOfBounds`）。
- `sandbox_capability_denied_via_trap_op` + `vtable_grant_smoke`：
  capability-prong（VM-side `BcOp::Trap(CapabilityDenied)` 路径 +
  `CapabilityVtable::grant`/`is_granted` 表面）。
- `sandbox_resource_step_limit` / `sandbox_resource_deadline_exceeded`：
  resource-prong（max_steps + past deadline 两个口子都路
  `WasmStepLimitExceeded`）。
- `resume_from_pc_at_entry_matches_run_main`：external_pc=0 = run_main
  happy path 等价。
- `resume_from_pc_after_each_prong_replays_trap`：entry-PC resume +
  unknown-PC fallback 都重现 trap；ir_pc_map 含 Div 的 PC（M2-B
  routing target）+ 每个 PC > 0（sentinel `0` 保留）。

### 1.5 test(bytecode): smoke + compile unit tests

`crates/relon-bytecode/tests/smoke.rs` — 12 test：end-to-end 跑
`#main(Int x, Int y) -> Int\nx + y` / `x - y` / `x * y` / `x / y`
/ overflow + div-by-zero + mod-by-zero / `x > y ? x : y` /
`x == y -> Bool` / `(y + 1) where { y: x * 2 }` / stdlib reject
/ step-limit trip。

`crates/relon-bytecode/src/compile.rs` — 3 inline unit tests
（compiles_simple_add / compiles_if_expression /
unsupported_op_surfaces_compile_error）。

## 2. Key decisions（≤ 5 bullets，每条带 rationale）

1. **新 crate 而非 evaluator 内嵌模块**。bytecode VM 完全 standalone：
   依赖 `relon-ir` + `relon-eval-api` + `relon-parser` + `relon-analyzer`，
   没有 cranelift / native-only deps；wasm32 也能直接编。边界清晰
   后 M2-B IC dispatch / M2-C bench 都能就近修改而不污染
   tree-walker / cranelift crate。

2. **Buffer-protocol IR → 虚拟 local，零 arena**。`lower_workspace_single`
   总是 emit `params=[I32 in_ptr, ..., I64 caps]` 的 buffer shape；
   cranelift 要建 arena + BufferBuilder marshalling 才能跑。bytecode
   VM 走另一条路：compile pass 用 schema `OffsetTable` 把每个
   `LoadField {offset}` 翻成 `LocalGet(local_slot)`、`StoreField {offset}`
   翻成 `LocalSet(return_field_base + slot)`，args 直接打进 local 数
   组。代价：放弃 arena 真 bounds-check（M2-A 不需要），收益：VM
   实现 ~600 LoC 而不是 ~3000，且 wasm32 友好。

3. **resume_from_pc M2-A 只交付 ir_pc_map round-trip + 入口/未知 PC**。
   mid-expression PC（如直接落在 Div op 上）需要 operand stack
   rehydration —— 但 trace-jit 的 `DeoptStateSnapshot` 当前只携带
   `ssa_slots_copy`（IR local 视图，不是 bytecode VM 的 operand stack
   视图）。把这个 widen 到能完美 rehydrate 是 M2-B 的本职工作；
   M2-A 只保证 trait surface + PC routing + sandbox prong 复现 OK。
   该 trade-off 任务 brief 明确允许（"trait method can still
   forward to a documented 'M2-A scaffold only' stub for non-trap
   PCs"）。

4. **Match-based dispatch 而非 computed-goto**。stable rustc 上
   computed-goto 要 `naked_functions` / inline asm unstable feature；
   M2-A 是 scaffolding 不是 perf milestone（perf 是 M2-C 的）。
   match dispatch 在 release build 上 LLVM 会自动 jump-table 化，
   性能差距实测在 M2-C bench 之前不构成阻塞。

5. **Cranelift-AOT envelope 内的 corpus 全覆盖即可**。bytecode VM
   reject 的 source（stdlib_simple / list / dict / closure /
   case_fold / normalize / memory tier）正好也是 cranelift `from_source`
   或 lower pass reject 的范围。4-way harness 通过
   `BytecodeUnsupported` 软通过路径处理这些 case，不要求 bytecode VM
   现在就实现 stdlib body —— 那是 M2-B / 后续 tier。corpus 通过率
   不退化（52/52 reach passing variant；0 mismatches）。

## 3. Gate numbers

- `cargo build --workspace` —— clean。
- `cargo test --workspace` —— **1729 passing**（M1 baseline 1703 +
  M2-A 净新增 26：
  - 3 compile.rs unit tests
  - 12 smoke.rs end-to-end
  - 9 bytecode_sandbox.rs 4-prong + resume
  - 2 bytecode_diff.rs harness
  ）。
- `cargo clippy --workspace --all-targets -- -D warnings` —— clean。
- `cargo fmt --all -- --check` —— clean。
- `cargo build --target wasm32-unknown-unknown -p relon-wasm` —— clean。

## 4. Architecture decisions（再次浓缩，方便阅读）

| Decision | Rationale |
|----------|-----------|
| 新 crate `relon-bytecode` | 独立边界 + wasm32 友好；future M2-B/M2-C 修改就近 |
| 虚拟 local 翻 LoadField/StoreField | 零 arena → VM 实现 600 LoC vs 3000；M2-A scope 内不需要真 arena bounds |
| Match-based dispatch | stable rustc + LLVM jump-table 自动化；M2-A 非 perf 目标 |
| resume_from_pc 只 PC 路由 + entry/unknown | DeoptStateSnapshot widen 是 M2-B 工作；trap PC 重入由 args round-trip 解决，验证过 |
| Cranelift envelope 内全覆盖 + BytecodeUnsupported 软通过 | 不强求 stdlib body lowering（M2-B+ 内容）；harness 已通过软通过路径处理 |

## 5. Parity stats: bytecode-vs-treewalk

### Full corpus (4-way diff): `corpus_four_way_diff_aggregates`

| variant | count | note |
|---------|------:|------|
| AllAgree | 23 | tw + cr + trace + bytecode 同 value |
| AllTrap | 4 | tw + cr + bytecode 同 trap（trace 走 wrapping）|
| BytecodeMatchesBaseline | 0 | (cranelift envelope 内 bytecode 都进 AllAgree)|
| BytecodeUnsupported | 25 | stdlib_simple/list/case_fold/normalize/memory/dict_return 整 tier + `let_chain` |
| Mismatch | **0** | correctness gate green |

Tier-level：

- arith_control: 23 AllAgree + 4 AllTrap + 1 BytecodeUnsupported (=
  `let_chain`) of 28
- stdlib_simple: 9 BytecodeUnsupported of 9
- stdlib_memory: 4 BytecodeUnsupported of 4
- stdlib_case_fold: 5 BytecodeUnsupported of 5
- stdlib_list: 2 BytecodeUnsupported of 2
- stdlib_normalize: 2 BytecodeUnsupported of 2
- dict_return: 2 BytecodeUnsupported of 2

### Strict parity (`corpus_bytecode_vs_treewalk_strict_parity`)

ArithControl 28 case 走 bytecode vs tree-walker bit-by-bit：

- 27 干净（value_bit_eq / trap_equivalent 通过）
- 0 diverged（correctness gate green）
- 1 Unsupported（`let_chain`，cranelift analyzer reject 的同一 case）

## 6. 4-prong sandbox prong test 结果

| Prong | Test name | Lifted RuntimeError |
|-------|-----------|---------------------|
| bounds | `sandbox_bounds_explicit_trap_op` | `WasmIndexOutOfBounds` |
| trap (div) | `sandbox_trap_div_by_zero` | `DivisionByZero` |
| trap (overflow) | `sandbox_trap_numeric_overflow` | `NumericOverflow` |
| capability | `sandbox_capability_denied_via_trap_op` | `WasmCapabilityDenied`（via `BcVmError::CapabilityDenied`）|
| capability vtable smoke | `vtable_grant_smoke` | n/a — verifies grant + is_granted surface |
| resource (steps) | `sandbox_resource_step_limit` | `WasmStepLimitExceeded` |
| resource (deadline) | `sandbox_resource_deadline_exceeded` | `WasmStepLimitExceeded` |

所有 4 prong 都至少一个 case PASS；resource prong 有 2 个 case
（max_steps + deadline）覆盖两个口子。

## 7. resume_from_pc behavior summary

| 场景 | 结果 | Test |
|------|------|------|
| entry-PC (external_pc = 0) | 等价 run_main | `resume_from_pc_at_entry_matches_run_main` |
| unknown-PC | fallback 到 entry，args + local_snapshot 不丢 | `resume_from_pc_after_each_prong_replays_trap` |
| 已知-trap-PC（再跑 div/0）| 同 RuntimeError 变体重现 | 同上 |
| 已知 PC + 空 operand stack（LocalSet 之后等 boundary）| 直接重入（路径与 entry 等价，本 milestone 没单独写 test 但通过 unknown-PC fallback 覆盖）| 同上 |
| 已知 PC + 非空 operand stack（mid-expression）| **M2-B work** — `BcVmError::StackUnderflow` lift 到 `RuntimeError::Unsupported`。`DeoptStateSnapshot` 需要 widen 才能 rehydrate operand stack | n/a |

## 8. Commit diff stat

`git diff --stat d5c61e8..HEAD`：

```
 Cargo.lock                                       |  15 +
 Cargo.toml                                       |   1 +
 crates/relon-bytecode/Cargo.toml                 |  29 ++
 crates/relon-bytecode/src/compile.rs             | 473 +++++++++++++
 crates/relon-bytecode/src/evaluator.rs           | 434 +++++++++++
 crates/relon-bytecode/src/lib.rs                 |  68 ++
 crates/relon-bytecode/src/op.rs                  | 140 ++++
 crates/relon-bytecode/src/vm.rs                  | 586 ++++++++++++++++
 crates/relon-bytecode/tests/bytecode_sandbox.rs  | 289 +++++++++
 crates/relon-bytecode/tests/smoke.rs             | 121 +++++
 crates/relon-cli/Cargo.toml                      |   1 +
 crates/relon-cli/src/main.rs                     |  39 +-
 crates/relon-test-harness/Cargo.toml             |   2 +
 crates/relon-test-harness/src/four_way.rs        | 227 +++++++
 crates/relon-test-harness/src/lib.rs             |   1 +
 crates/relon-test-harness/tests/bytecode_diff.rs | 196 ++++++
 crates/relon/Cargo.toml                          |   5 +
 crates/relon/src/lib.rs                          |  20 +
 18 files changed, 2645 insertions(+), 2 deletions(-)
```

Commit history（HEAD-first，到 v6-δ M1 base 共 3 个 commit）：

```
1e76cc4 test(harness): 4-way differential corpus + bytecode parity gate
9028f00 feat(relon): wire Backend::Bytecode + CLI --backend=bytecode
b70d44e feat(bytecode): v6-delta M2-A bytecode VM crate with IR-PC map
d5c61e8 feat(trace-jit): v6-delta M1 clear 5 residuals + real hot-loop bench  ← base
```

## 9. Key file paths（absolute）

- 新增 source：
  - `/ext/relon/crates/relon-bytecode/Cargo.toml`
  - `/ext/relon/crates/relon-bytecode/src/lib.rs`
  - `/ext/relon/crates/relon-bytecode/src/op.rs`
  - `/ext/relon/crates/relon-bytecode/src/compile.rs`
  - `/ext/relon/crates/relon-bytecode/src/vm.rs`
  - `/ext/relon/crates/relon-bytecode/src/evaluator.rs`
  - `/ext/relon/crates/relon-bytecode/tests/smoke.rs`
  - `/ext/relon/crates/relon-bytecode/tests/bytecode_sandbox.rs`
  - `/ext/relon/crates/relon-test-harness/src/four_way.rs`
  - `/ext/relon/crates/relon-test-harness/tests/bytecode_diff.rs`
- 修改的 source：
  - `/ext/relon/Cargo.toml`（加 `relon-bytecode` 到 workspace deps）
  - `/ext/relon/crates/relon/Cargo.toml`（依赖加 relon-bytecode）
  - `/ext/relon/crates/relon/src/lib.rs`（Backend::Bytecode + BackendError::Bytecode + new_evaluator 分支）
  - `/ext/relon/crates/relon-cli/Cargo.toml`（依赖加 relon-bytecode）
  - `/ext/relon/crates/relon-cli/src/main.rs`（BackendArg::Bytecode + dispatch + library-mode reject）
  - `/ext/relon/crates/relon-test-harness/Cargo.toml`（依赖加 relon-bytecode）
  - `/ext/relon/crates/relon-test-harness/src/lib.rs`（pub mod four_way）
- 更新 docs：
  - `/ext/relon/docs/internal/v6-gamma-integration-plan-2026-05-18.md`（§14 v6-δ M2-A DONE）
- 新增 docs：
  - `/ext/relon/docs/internal/v6-delta-m2a-stage-report-2026-05-19.md`（本文）

## 10. Carry-over to M2-B + M2-C

### M2-B target

1. **Operand-stack rehydration**：widen
   `relon_trace_abi::DeoptStateSnapshot` 携带 SSA value stack
   snapshot；`BytecodeVm::invoke_from_with_locals` 入口接受 initial
   operand stack 参数；`BytecodeEvaluator::resume_from_pc` 把
   snapshot 翻译成 initial operand stack。M2-A 留下的
   `StackUnderflow` mid-expression resume 路径修复。
2. **Inline-cache slot per call site**：bytecode VM 的 `Call` 系 op
   现在直接 reject。M2-B 引入 `BcOp::CallNative { ic_slot }`
   + `CapabilityVtable` 真 host-fn 指针 dispatch；polymorphic
   inline cache 一 slot 一 monomorphic 类型。
3. **真 capability vtable**：M2-A 的 `CapabilityVtable` 只跟踪 grant
   位（`Vec<bool>`）；M2-B 把它升级为 `Vec<Option<HostFnPtr>>` 类似
   cranelift `sandbox::CapabilityVtable`，让真 `#native` call 路径
   work。

### M2-C target

1. **Bench**：`crates/relon-bench/benches/` 加 `bytecode_dispatch` 系
   bench：cold `from_source` / warm `run_main` / 对比 trace_jit。
   目标 trace_jit_warm 推到 3-5 ns/iter（vs M1 9.52 ns/iter）。
2. **IC slot 命中率监控**：trace-recorder side 加 IC slot 命中率
   计数；bench 报告附录里出每 op 类型的 IC 击中率 vs cranelift 直
   译路径。
3. **可能的 dispatch 重构**：根据 M2-C bench 数据决定要不要换
   threaded-code / computed-goto / direct-threaded dispatch；
   match-based 在 SQLite 上跑得不错的先例（VDBE）说明本路线下
   3-5 ns 大概率不需要换底层模型。

### Minor sweep（v6-δ M2 末尾再处理）

- M2-A 的 `Op::Trap` 只覆盖 IR-level 已有的 IndexOutOfBounds /
  EmptyList / InvalidUtf8 三种；新增 `CapabilityDenied` 走的是
  bytecode VM-side（M2-A 测试通过 hand-build BcFunction 触发）。
  待 IR enum 扩到 Capability 后这条路径在 source-to-source 也能 fire。
- M2-A 的 `corpus_bytecode_vs_treewalk_strict_parity` 只 cover
  ArithControl；其他 tier 因 bytecode unsupported 跳过。M2-B widen
  envelope 后这个测试可以覆盖更多 tier。

EOF
