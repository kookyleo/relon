# Bytecode deopt PC alignment follow-up

**Date**: 2026-05-26
**Scope**: `bytecode-coverage-completion.md` open follow-up #3 — PC alignment
fix for `trace_jit → bytecode` deopt handoff on string-shape sources.
**Base**: main HEAD `51b13b1`.

## 背景

`bytecode-coverage-completion.md` 列出了 Phase B-3 的两个限制：

1. **dispatcher 集成测试只覆盖 NoTrace 分支**：string-shape source 的 cold-path
   deopt → bytecode resume 没跑通。
2. **PC 对齐问题**：recorder 手搓 trace body 时分配的 `external_pc` 跟 bytecode
   compile 的 `ir_pc_map` 不重合（因为两者 IR shape 不一样）。

跨三套坐标系——`trace_pc`（recorder buffer 内 op index）、`external_pc`（recorder
的 monotonic IR-PC 计数器，也是 bytecode `ir_pc_map` 的值）、`bc_idx`（bytecode
op stream 索引）——一致性维护，本次 follow-up 解决其中一部分，记录另一部分。

## 根因分析

### Layer 1 — recorder/bytecode body 不一致（**未本期修**）

现在 `bytecode_trace_deopt_handoff_e2e` 的 string-shape test 用
`build_add_body()`（8 个整数 IR op）作 recorder body，但 bytecode source
（`#main(String s) -> String\ns + "!"`）lowered 出 5 个 string IR op。
recorder bumps `next_external_pc` 一次 per `record_op` 调用，所以同一个语义概念
（`Add`）在两边的 PC 是不一样的。

**根本修法**：让 recorder 走 production lowering 的同一份 body。但
`TraceRecordingEvaluator` 的 walker 目前不支持 `LoadField` / `LoadStringPtr` /
`ConstString` / `StoreField` 等 schema-aware op——walker 的 `step_load_field`
要从 operand stack pop 一个 base 指针，而 production-lowered body 中 `LoadField`
是无显式 base 的（cranelift codegen 从 wasm slot 0 隐式读 in_ptr）。

要让 walker 走 production lowering 需要：
- `step_load_field`：当 operand stack 是空、且 offset 命中 schema arg slot 时，
  从 `args` 数组读对应 slot 的值（而不是 unsafe 内存读）。
- `step_const_string`：新增 walker arm，从一个分配器/池里产 StringRef 对象，
  让 recording-time 值跟 production 一致。
- `step_store_field`：新增 walker arm（recorder 已支持 `record_op(&StoreField)`，
  只是 walker 没接入）。

这是中等规模的工作，跨 `relon-codegen-native` + `relon-trace-recorder` 两个
crate，超出本期 3-5 天预算。

### Layer 2 — `resume_via_vm` 不走 string-aware 路径（**本期修**）

`BytecodeEvaluator::resume_from_snapshot_with_metrics` → `resume_via_vm`：

```rust
// 修前：
let packed = self.pack_args(args)?;  // 不处理 String args
let outcome = vm.invoke_from_with_stack(...);  // 不接入 StringArena
self.unpack_return_slots(&outcome.final_locals)  // 不读 final_strings
```

修前对 string source 的 resume：
- `pack_args` 见 String arg 直接 panic 或回 0 placeholder。
- `invoke_from_with_stack` 不会把 string arg 提进 `StringArena`，所以
  `BcOp::StrConcat` 等 op 拿到的 handle 是无效的 0。
- 即便 dispatch 成功，`unpack_return_slots` 不读 `final_strings`，
  `SingleScalarString` 返回 shape 永远拿到空串。

这三个 bug 串成一条线：**只有 integer source 能凑合工作**（pack_args 不报错、
StrConcat 不被 dispatch、unpack 走 LegacyI64/SingleScalarInt 不需要 final_strings）。
string source 在 resume path 上 silent broken。

**修法**：把 `resume_via_vm` 改成走 `pack_args_with_strings` +
`invoke_from_with_string_io` + `unpack_return_slots_with_strings`，三件套全套对
齐 `run_main` / `resume_from_pc` 已有的 string-aware 路径。

### Layer 3 — body alignment 协议缺工具支持（**本期加 surface**）

即便修了 Layer 2，要测 production-style 的 end-to-end deopt → resume 需要让
recorder 走真实 body。新增 `BytecodeEvaluator::recording_registration_data()`
返回 `(body, param_tys)`，host 可以把它喂给
`relon_codegen_native::register_recording`，从而避免手搓 fixture body 引发
Layer 1 的不一致。

API 设计：
- bytecode crate 内提供 `RecordingRegistrationData { body, param_tys }`，
  没 cranelift 依赖。
- `relon-codegen-native::trace_install::RecordingRegistration` 加
  `From<RecordingRegistrationData>` impl，在 boundary 处零成本搬字段。

这只是 surface，**还不够** Layer 1 的 walker 改造跟上才能真正用。但
follow-up 工作可以从这个 seam 开工，不用再回头改 evaluator 内部结构。

## 落地

### 代码改动

| File | 改动 |
|---|---|
| `crates/relon-bytecode/src/evaluator.rs` | 加 `RecordingRegistrationData` 结构 + `entry_body` 字段 + `recording_registration_data()` 方法；`resume_via_vm` 改走 string-aware 三件套；`resume_from_snapshot_with_metrics` + `resume_from_snapshot_metrics_only` 用 `unpack_return_slots_with_strings` |
| `crates/relon-bytecode/src/lib.rs` | re-export `RecordingRegistrationData` |
| `crates/relon-codegen-native/src/trace_install.rs` | `From<RecordingRegistrationData> for RecordingRegistration` impl |
| `crates/relon-test-harness/tests/bytecode_trace_deopt_handoff_e2e.rs` | +3 个新测试：`resume_from_snapshot_string_concat_round_trips_at_strconst` / `resume_from_snapshot_string_at_return_lifts_final_strings` / `recording_registration_data_surfaces_production_lowered_body` |

### 测试覆盖

新加 3 个测试：

1. **resume_from_snapshot_string_concat_round_trips_at_strconst**：
   手搓 `DeoptStateSnapshot` 让 `external_pc` 落在 string source 的 `StrConst`
   bc_idx 上，确认 resume 跑完 StrConst → StrConcat → LocalSet → Return 后
   返回 `"hello!"`（即 input + "!"）。**修前这个测试会 WasmIndexOutOfBounds**
   因为 `resume_via_vm` 不接入 StringArena，input handle 是 0。

2. **resume_from_snapshot_string_at_return_lifts_final_strings**：
   resume 直接落到 `Return` op 上，绕过 StrConcat。验证
   `unpack_return_slots_with_strings` 的 final_strings 提取路径在 resume 路径
   上能 fire。**修前这个测试会返回 `""`**（unpack 路径没读 final_strings）。

3. **recording_registration_data_surfaces_production_lowered_body**：
   pin 新 accessor 的契约——返回的 body 跟 `lower_workspace_single` 的输出
   逐 op 对应，param_tys 反映 user-declared `#main` 签名。验证 `From` 转换
   到 `RecordingRegistration` 工作。

### 验收三关

- `cargo build --workspace` clean ✓
- `cargo test --workspace --exclude relon-bench --exclude relon-wasm`
  全 pass（含新加 3 个测试）✓
- `cargo clippy --workspace --all-targets -- -D warnings` clean ✓

## 仍未关闭的部分

**Layer 1 walker 改造**留作后续 follow-up：

- recorder walker 加 `step_load_string_ptr` / `step_const_string` / 让
  `step_load_field` 兼容 schema-arg-slot 形态。
- 加完后，bytecode evaluator 的 `recording_registration_data()` 可以直接
  喂给 `register_recording`，再配合一个 source 自然 deopt 的 fixture（例如
  `if s.is_empty() then "empty" else s + "!"` warm-up 用 `"hello"`、cold-path
  用 `""`），就能跑完整的 end-to-end deopt → resume e2e。
- 估算 1-2 周。

## 文件清单

| 路径 | 用途 |
|---|---|
| `crates/relon-bytecode/src/evaluator.rs` | resume path string-aware fix + accessor |
| `crates/relon-bytecode/src/lib.rs` | re-export |
| `crates/relon-codegen-native/src/trace_install.rs` | boundary From impl |
| `crates/relon-test-harness/tests/bytecode_trace_deopt_handoff_e2e.rs` | 新测试 |
| `docs/internal/bytecode-deopt-pc-alignment-2026-05-26.md` | 本文档 |
