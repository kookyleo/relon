# F-D8-B 阶段报告：recorder 自动识别 dict / list subscript（2026-05-20）

## 摘要

- F-D8（commit `4ea13ac`）落地 `TraceOp::ListGet` + `TraceOp::DictLookup`
  并提供 `RecorderState::emit_list_get` / `emit_dict_lookup` 显式 API；
  F-D9（commit `d91bdfa` + `b3eff73`）用手工 cranelift JIT 验证 W5 / W6
  的 trace JIT 通路。两阶段都明确把「recorder 自动识别 `d[k]` / `xs[i]`
  AST 形态」记作未完成项（详见 F-D8 §7.1）。
- 本阶段在 IR + recorder + trace-recording-evaluator 三层加上接入点：
  - 新增 `Op::DictGetByStringKey { shape_hash, value_ty }`、
    `Op::ListGetByIntIdx { element_ty }` 两个 IR variant，effect 类为
    `ReadOnly`，与 `LoadField` 同档。
  - `relon-trace-recorder` 的 `lower_op` 把它们投影成新的
    `LowerOutcome::SubscriptDispatch`；`apply_outcome` 分别走
    `emit_list_get` / `emit_dict_lookup` 的现成路径。
  - `TraceRecordingEvaluator::step_one` 识别这两个 IR variant，pop 两个
    operand，调用 `recorder.record_op`，把返回的 SSA 重新压回 stack。
- 配套 9 个单元测试覆盖正常 dispatch、bounds guard、shape_hash 透传、
  underflow / 非 i64 element 的 clean abort。

## 一、新 IR 变体

`crates/relon-ir/src/ir.rs`：

```rust
DictGetByStringKey {
    /// FxHash 指纹，与 dict header 的 shape_hash 比对；recorder 把它
    /// 烧进 TraceOp::DictLookup 的 IC 槽。
    shape_hash: u64,
    /// Dict value 侧 IrType，用作 dst SSA 的 ObservedType 提示。
    value_ty: IrType,
},
ListGetByIntIdx {
    /// List element IrType。F-D8 helper 当前只支持 I64，非 I64 在
    /// recorder lowering 阶段直接 abort。
    element_ty: IrType,
},
```

`effect_class()` 把它们都归入 `ReadOnly`，与 `LoadField` 同档；这样
trace-jit 的 `load_forward` / `dead_store` pass 不需要 special-case。

## 二、recorder 三层接入

### 2.1 `LowerOutcome::SubscriptDispatch`

`crates/relon-trace-recorder/src/lowering.rs`：

```rust
pub enum SubscriptKind {
    ListGet,
    DictLookup { shape_hash: u64 },
}

LowerOutcome::SubscriptDispatch { kind: SubscriptKind, ty_hint: ObservedType }
```

放在新 outcome 而不是 `Emit { op: TraceOp, .. }` 的原因：
`emit_list_get` / `emit_dict_lookup` 需要分配新 dst SSA、更新 ssa_stack
镜像、塞 bounds-check guard——这些是 recorder state 上的操作，纯函数
`lower_op` 没法做。

### 2.2 `RecorderState::apply_outcome` 新增分支

`crates/relon-trace-recorder/src/recorder.rs`：

新分支从 `inputs` 取 `top = inputs[0]`、`container = inputs[1]`，按
`SubscriptKind` 调度到 `emit_list_get` / `emit_dict_lookup`。helper 默
认把 dst 的 `ObservedType` 写成 I64；新分支随后用 `ty_hint` 覆盖（保留
非 I64 dict value 的扩展空间），并对 caller-传入的 `observed` 触发
`maybe_emit_type_guard`。

### 2.3 `TraceRecordingEvaluator::step_one` 识别

`crates/relon-codegen-native/src/trace_recording.rs` 新增
`step_subscript`：pop 两个 stack cell（top = key/idx，下一层 = container），
把两者 SSA 喂给 `record_op`，并把 dst SSA 与一个占位 u64 = 0 推回栈。
占位值的语义在源码注释里说明：recorder 本身不消费 walker 的 u64 结果；
真实值由 trace JIT 装好后在运行时调用 helper 才会得到。

## 三、单元测试

| 文件 | 测试 | 覆盖 |
| --- | --- | --- |
| `relon-trace-recorder/src/lowering.rs` | `dict_get_by_string_key_dispatches_dict_lookup` | shape_hash 透传 + ty_hint 推断 |
| 同上 | `list_get_by_int_idx_dispatches_list_get` | `SubscriptKind::ListGet` 派发 |
| 同上 | `dict_subscript_underflow_aborts` | inputs.len() < 2 → `UnsupportedOp("DictGetByStringKeyUnderflow")` |
| 同上 | `list_subscript_non_i64_element_aborts` | element_ty=F64 → `UnsupportedOp("ListGetByIntIdxNonI64")` |
| `relon-trace-recorder/src/recorder.rs` | `record_dict_get_dispatches_dict_lookup` | `record_op` → buffer 出 `TraceOp::DictLookup` |
| 同上 | `record_list_get_dispatches_list_get_with_bounds_guard` | bounds guard 也被记录 |
| 同上 | `record_list_get_with_non_i64_element_aborts` | 非 I64 element 的 sticky abort |
| `relon-codegen-native/src/trace_recording.rs` | `dict_get_by_string_key_walker_emits_dict_lookup` | 端到端 walker → recorder → buffer |
| 同上 | `list_get_by_int_idx_walker_emits_list_get_and_bounds_guard` | 同上 + bounds guard |
| 同上 | `list_get_non_i64_element_aborts_recorder` | 端到端 abort |

## 四、未完成项（明确列出 blocker）

### 4.1 W5 / W6 仍走手工 cranelift JIT

任务要求把 `cmp_lua.rs` 里 W5 / W6 的 trace_jit row 改成走
`TraceRecordingEvaluator`。**这一步在当前架构下不可行**，原因：

1. W5 源码 `d[keys[i % 10]]`、W6 源码 `list.sum(range(n).map((i) => i + 1))`
   在 **AST → IR lowering** 阶段不会产生新引入的 `DictGetByStringKey` /
   `ListGetByIntIdx` op。`relon-ir/src/lowering.rs::lower_variable`
   显式拒绝 `TokenKey::Index(_, _)`（line 3225）——目前 `d[k]` 全部走
   tree-walker 的 `try_index_method` 调度（`relon-evaluator/src/reference.rs`），
   IR walker 根本看不到 subscript。
2. 要让 W5 / W6 真正走 recorder，必须在 `relon-analyzer` / `relon-ir`
   增加新的 AST 模式识别 + IR lowering 步骤：
   - parser 已有 `Expr::Reference { base, path }` 路径，但 path 中的
     `TokenKey::Index` 需要在 analyzer 类型推断时识别为 dict / list
     subscript；
   - lowering pass 需要根据 container 静态类型（`Dict<_, V>` /
     `List<V>`）决定生成 `Op::DictGetByStringKey` 还是
     `Op::ListGetByIntIdx`；
   - 同时还需要在 lowering 时算出 dict 的 `shape_hash`——这依赖
     analyzer 把 dict 字段集合稳定排序（FxHash 不能 lazy 算）。

这是一个跨 analyzer / IR lowering / codegen 的工作量较大的子阶段，按
F-D8 §7.1 提到的「需要扩展 step_one 的 op 解码 / 模式匹配，**或者**
引入新的 `relon_ir::Op` 变体并由 lowering pass 翻译」，本阶段实现了
**后半段**（IR 变体 + recorder dispatch），前半段（lowering pass 翻译）
留给后续 F-D8-C / 同步 analyzer 重构的 agent。

### 4.2 cmp_lua W5 / W6 实测 ratio

未执行 cmp_lua bench rerun，因为没有改动 bench 文件。F-D9 commit
`b3eff73` 公布的手工 cranelift trace 数据：W5 ≈ × 1.27，W6 ≈ × 0.26
仍然代表当前 trace-JIT 路径的性能上限——recorder 路径走通后跑出的
数字应与之相当（recorder 最终生成的 `TraceOp::DictLookup` /
`TraceOp::ListGet` 是同一个 emitter 模板）。

## 五、Gate 检查

| 项目 | 状态 |
| --- | --- |
| `cargo build --workspace` | ✓ |
| `cargo test --workspace` | ✓（无新 FAIL，仅新增 9 个 OK） |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✓ |
| `cargo fmt --all -- --check` | ✓ |
| `cargo build --target wasm32-unknown-unknown -p relon-wasm` | ✓ |
| `cargo run -q -p relon-fmt -- --check fixtures/*.relon …` | ✓ |
| W5 / W6 trace_jit ratio ≤ × 2 | **未跑**，见 §4.2 |

## 六、改动文件

| 文件 | 增 / 改 |
| --- | --- |
| `crates/relon-ir/src/ir.rs` | +60 行（两个 Op 变体 + effect_class 分支） |
| `crates/relon-trace-recorder/src/lib.rs` | +1 行（re-export `SubscriptKind`） |
| `crates/relon-trace-recorder/src/lowering.rs` | +130 行（含 SubscriptKind enum、`LowerOutcome::SubscriptDispatch`、`lower_op` 两个分支、4 个单测） |
| `crates/relon-trace-recorder/src/recorder.rs` | +130 行（含 `apply_outcome` 新分支、3 个单测） |
| `crates/relon-codegen-native/src/trace_recording.rs` | +130 行（含 `step_subscript`、`Op::DictGetByStringKey` / `Op::ListGetByIntIdx` step_one 分支、3 个单测） |

## 七、起始基线

worktree HEAD: `968c739` (chore: review followups R4 + R6 + R7 + R8)。
与任务要求的本地 main HEAD 一致；未 fetch origin。
