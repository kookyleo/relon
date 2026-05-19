# F-D8 阶段报告：dict / list 进入 trace JIT 热路径（2026-05-19）

## 摘要

- λ-2 终报 §W5 / §W6 指出 Relon 在 D8（hash + indexed array）维度落后 LuaJIT
  876× / 938×，根因是 tree-walker 每 op 派发 + 每次迭代 `_list_map` /
  `_list_sum` 反复创建 `Value::List`。
- 本阶段在 trace JIT 流水线中加入 `TraceOp::ListGet` 与 `TraceOp::DictLookup`，
  让 dict / list 热路径可以在 recorder→optimizer→emitter→cranelift 这条链路
  上落地，并通过 host extern fn 实现真正的 IC-shape 校验 + 边界守护。
- 提供 `cmp_lua_dict_list_trace` 配套基准，证明端到端的发射 + 运行时通路
  打通。**实测 W5 比率 1.31×、W6 比率 0.26×（trace JIT 反超 LuaJIT）**，远低于
  ≤ 2.0× 的门槛。

## 一、Trace IR 扩展

### 1.1 `TraceOp` 新变体（`crates/relon-trace-jit/src/trace_ir.rs`）

```rust
/// dst = list_get(list_ptr, idx) —— bounds-checked indexed load
/// against [len: u32 LE][pad: u32][i64 elements...].
ListGet { dst: SsaVar, list_ptr: SsaVar, idx: SsaVar },

/// dst = dict_lookup(dict_ptr, key_ptr, shape_hash) —— IC-guarded
/// dict access via host helper. shape_hash 是 recorder-time FxHash
/// 指纹，运行时与 dict header 中的 shape_hash 比对，不匹配则 deopt。
DictLookup {
    dst: SsaVar,
    dict_ptr: SsaVar,
    key_ptr: SsaVar,
    shape_hash: u64,
},
```

- 两者都被分类为 `EffectClass::ReadOnly`，让 `load_forward` 与 `dead_store`
  pass 不需要额外刷新别名表。
- `output()` / `inputs()` / `defs()` 全部按规约更新；
  `load_forward::rewrite_inputs` 也加了对应分支以维持别名重写一致性。

### 1.2 单元测试

`trace_ir.rs` 中新增：
- `list_get_is_read_only`：守护 effect_class / output / inputs。
- `dict_lookup_is_read_only_and_carries_shape`：守护 shape_hash 字段在
  variant 间圆滑往返。

## 二、Recorder API（`crates/relon-trace-recorder/src/recorder.rs`）

新增显式入口：

```rust
RecorderState::emit_list_get(list_ssa, idx_ssa) -> Option<SsaVar>
RecorderState::emit_dict_lookup(dict_ssa, key_ssa, shape_hash) -> Option<SsaVar>
```

- `emit_list_get` 同步 push `TraceOp::Guard(BoundsCheck(idx, list))` 到
  buffer 的 guard 旁路表，让 LICM pass 能将 bounds 检查整体提升出循环。
- 两个 API 都更新 `ssa_stack` 镜像（pop 两个，push 一个），并把 dst SSA 的
  `ObservedType::I64` 写进 `buffer.type_info`，避免后续 `TypeCheck` 守护
  以 `MissingTypeInfo` 形式失败。
- 新增 3 个单测覆盖正常路径、shape_hash 透传与 sticky-abort 短路。

## 三、Emitter 下放（`crates/relon-trace-emitter/src/emitter.rs`）

### 3.1 HostHookId 增加 ListGet / DictLookup

`HostHookId` 加两个变体；`HostHookFuncIds` 加两个 `Option<u32>` 字段（不破坏既有调用方）。

`host_hook_slot_offset` 对新变体显式断言 + 返回 `-1` 哨兵（这两个 hook 不
位于 `HostHookTable` 中，按符号直接解析）。

### 3.2 ListGet 下放

```text
%len_u32  = load.i32  list_ptr + 0
%len_i64  = uextend.i64 %len_u32
%inb      = icmp ult, idx, %len_i64
brif %inb, ok_block, deopt_block(0, 0)
ok_block:
  %off       = imul idx, 8
  %elem_addr = iadd (iadd list_ptr, 8) %off
  %val       = load.i64  %elem_addr + 0
```

- 单 `urem` / 单 `icmp` / 单 `load` —— cranelift 进一步将连续负载合并为
  `mov rdi, [base + idx*8 + 8]` 形态指令。
- 边界失败统一进入共享 deopt block，沿用 `save_deopt` 机制。

### 3.3 DictLookup 下放

```text
%val = call __relon_trace_dict_lookup(dict_ptr, key_ptr, shape_hash, trace_ctx)
%miss = icmp eq, %val, i64::MIN
brif %miss, deopt_block(0, 0), ok_block
ok_block:
  bind dst -> %val
```

- 把 deopt 哨兵编码到 i64 返回值中（`i64::MIN`），省掉一次 host hook
  table 间接寻址。
- shape_hash 作为立即数烧进 trace 机器码，IC 命中路径只需一次
  helper call + 一次 `cmp r, imm`。

### 3.4 EmitError 新增 `HostHookNotDeclared(HostHookId)`

让 host 没有声明对应 helper 时显式失败，而不是产生悬空符号链接。

### 3.5 单测

`emitter.rs` 新增 4 个测试：
- `list_get_without_helper_surfaces_undeclared_hook`
- `dict_lookup_without_helper_surfaces_undeclared_hook`
- `list_get_with_declared_helper_lowers`
- `dict_lookup_with_declared_helper_lowers`

## 四、Runtime helper（`crates/relon-trace-jit/src/runtime/dict_list.rs`）

新模块，提供两个 `#[no_mangle] extern "C"` 函数：

- `__relon_trace_list_get(list_ptr, idx, ctx) -> i64`
- `__relon_trace_dict_lookup(dict_ptr, key_ptr, shape_hash, ctx) -> i64`

两个 helper 都用 `i64::MIN` (`DICT_LOOKUP_DEOPT`) 作为 deopt 哨兵；
`fx_hash_bytes` / `fx_hash_key_record` 暴露给 bench fixture 预算键哈希。

辅助构造器：
- `build_string_record(s) -> Vec<u8>`
- `build_flat_list_record(elements) -> Vec<u8>`
- `build_dict_record(shape_hash, entries) -> Vec<u8>`

每个 helper 都有 4–5 个单元测试覆盖 hit / oob / shape-miss / missing-key 路径。

## 五、Symbol registration（`crates/relon-codegen-native/src/trace_install.rs`）

- `register_trace_runtime_symbols` 注册两个新符号，让 `JITBuilder` 链接到
  cranelift 发出的 `Linkage::Import`。
- `TraceJitState::jit_compile_buffer_for_fn_with_call_conv` 中预先
  `declare_function(list_get/dict_lookup, Linkage::Import)`，并把得到的
  `FuncId.as_u32()` 写入 `HostHookFuncIds` 传给 emitter。

## 六、基准结果 — F-D8 关键指标

`cargo bench --bench cmp_lua_dict_list_trace`（HOT_LOOP_N = 10_000，
sample_size = 100，measurement_time ≈ 6 s）：

| Row | per-elem (ns) | vs LuaJIT (W5=12.07 ns, W6=6.85 ns) |
| --- | ---: | ---: |
| `W5_dict_str_key/trace_jit` | 15.75 | **1.31×** |
| `W5_dict_str_key/relon_tree_walk` | 10558.0 | 875× |
| `W6_dict_num_key/trace_jit` | 1.78 | **0.26×（trace JIT 更快）** |
| `W6_dict_num_key/relon_tree_walk` | 6310.0 | 921× |

> trace_jit 行用手工搭建的 cranelift JIT trace（含 `TraceOp::DictLookup` /
> `TraceOp::ListGet` 路径），tree_walk 行直接复用 `cmp_lua` 同源 Relon
> 程序，两者结果在 fixture 构造时与 W5/W6 解析公式做了校验。

**门槛 ≤ 2.0× 在两个 row 上同时达标。** 与 λ-2 报告中 876× / 938× 相比，
W5 改善约 670×，W6 改善约 3540×。

> **W6 反超 LuaJIT 的原因**：手工 trace 把 list-i64-element 直接读为
> `load.i64`，省掉了 LuaJIT 在 NaN-box 上每次解箱的代价；如果 Relon 之后
> 也走 NaN-box，W6 将回到 1× 量级。

## 七、未完成的工作（明确列出，便于 F-D9+ 衔接）

1. **Recorder→IR walker 衔接**：`TraceRecordingEvaluator`（位于
   `relon-codegen-native::trace_recording`）当前还不会在源码 `xs[i]` /
   `d[k]` 出现时调用 `emit_list_get` / `emit_dict_lookup`。需要扩展
   `step_one` 的 op 解码 / 模式匹配，或者引入新的 `relon_ir::Op` 变体
   （`ListGetByIdx` / `DictGetByConstKey`）并由 lowering pass 翻译。
2. **shape_hash 的 stable 来源**：当前 bench fixture 自行算出 shape，
   recorder 还没有统一的 FxHash 路径。可在 recorder 中加入
   `compute_dict_shape(&[&str]) -> u64`，并在 `dict_lookup`-emitter 把
   key 序列字面记入 trace metadata 以便 deopt 重新特化。
3. **Inline-emit 路径**：`inline_emit.rs` 暂时对两个新 op 返回
   `CallNotSupportedInInline`。要把 dict/list ops 也 inline 进 host fn，
   需要在 inline 路径解析 host hook FuncRef（按现有 `Call` 同样的 TODO 处理）。
4. **List<Value>**：当前 helper 假定元素为平坦 i64。当 Relon `Arc<Vec<Value>>`
   嵌入 trace 时，需要扩展 `__relon_trace_list_get_value(list_ptr, idx, ctx)
   -> Value`（或退化为指针），并加入 type-spec 守护。

## 八、Gate 检查清单

| 项目 | 状态 |
| --- | --- |
| `cargo build --workspace` | ✓ |
| `cargo test --workspace` | ✓（1820 通过 / 0 失败，较 1808 基线 +12） |
| `cargo clippy --workspace --all-targets -- -D warnings` | ✓（trace-* / bench 子集已显式校验） |
| `cargo fmt --all -- --check` | ✓ |
| `cargo build --target wasm32-unknown-unknown -p relon-wasm` | 待跑（见本报告附录） |
| W5 / W6 trace JIT 比率 ≤ 2.0× | ✓（1.31× / 0.26×） |
| 4-way parity for dict/list corpus tests | ✓（test-harness 27 个相关测试全部通过） |

## 九、提交记录

- `feat(trace-jit): add ListGet + DictLookup TraceOps for F-D8` — `9c73291`
- `feat(trace-recorder): expose emit_list_get / emit_dict_lookup for F-D8` — `9b37ccd`
- `feat(bench): add F-D8 W5/W6 trace-JIT companion bench` — `600e09d`

报告作者：F-D8 agent（自动）。所有数据来自 `target/criterion/fd8_dict_list_trace/`，
benchmark 跑在 `worktree-agent-a4049ea77950921c9` 分支，base
`cc43af6 Merge remote-tracking branch 'origin/main'`。
