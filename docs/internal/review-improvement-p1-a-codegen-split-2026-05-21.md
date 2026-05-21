# P1-A: codegen-native categorical split

`crates/relon-codegen-native/src/codegen.rs` 原始 4252 LoC monolith
(`ConstPool` + `Codegen` 大 impl + standalone helpers + tests).
按 category 切到 `codegen/` sub-directory，并发的 `emit_op` /
`collect_op` dispatch 仍留在 `mod.rs`。本 phase 切 4 个 category，
为后续 OpVisitor 接入打 framework。

## Audit: 99-arm Op surface 分布

`emit_op` match 当前 cover ~50/77 Op 变体（其余仍走
`other => Err(Codegen("unsupported"))`）。按 category 分布:

| Category   | arms                                                | LoC ~ |
|------------|------------------------------------------------------|-------|
| 算术       | Add/Sub/Mul/Div/Mod/BitAnd × {I64, I32}              | 200   |
| 比较       | Eq/Ne/Lt/Le/Gt/Ge × {I64, I32, Bool}                 | 60    |
| 控制流     | If / Block / Loop / Br / BrIf / BrTable / Select / Return / Trap | 350   |
| 局部变量   | LocalGet / LetGet / LetSet                           | 60    |
| 内存       | Load/Store/{I32,I64,F64,I8U,I8}AtAbsolute, Memcpy, AllocScratch{,Dyn} | 280   |
| 字段       | LoadField / StoreField                               | 140   |
| 字符串/列表 | ConstString / ConstList{Int,Float,Bool} / ReadStringLen | 100  |
| Record     | AllocRoot/SubRecord, StoreFieldAtRecord, PushRecordBase, EmitTailRecord | 600 |
| Call       | Call (stdlib inline) / CallNative / CallClosure / MakeClosure / CheckCap | 700 |
| Unicode    | CaseFold/Decomp/CCC/Composition/Whitespace/CombMark/Turkish addr | 250 |
| Trap/Guard | Trap / cond_trap / emit_resource_check / emit_host_fn_call | 200 |
| ConstPool  | scan + 18 字段 layout                                | 310   |

## 拆分策略

选择 **mod.rs hub** 形式（codegen/ 子目录、mod.rs dispatch）。
原因:
- `Codegen` struct 状态多 (locals/let_locals/stack/inline_frames/
  label_stack/record_locals/captures_ptr 等 20+ 字段)，重新分配到
  多文件成本高
- Rust child-mod 内 `impl<'a, 'b> super::Codegen<'a, 'b>` pattern
  天然允许多文件 impl，子文件可直接读写父 struct 的私有字段
- 增量风险最小: 每个 sub-file 独立编译、独立 review

## 8 sub-module 各自 LoC (本 phase 落地 4 个)

| File              | LoC | 内容                                                |
|-------------------|-----|----------------------------------------------------|
| codegen/mod.rs    | 3458 | Codegen struct + emit_op/emit_body dispatch + 其余 |
| codegen/const_pool.rs | 348 | ConstPool struct + collect_op (Unicode + String + List) |
| codegen/guard.rs  | 146 | trap_code / make_*_signature / declare_vtable_data / emit_indirect_host_call |
| codegen/arith.rs  | 189 | emit_{add,sub,mul,div,mod,bitand}_{i32,i64} / emit_cmp / emit_cmp_i32 |
| codegen/memory.rs | 279 | emit_alloc_scratch{,_dyn,_static} / arena_addr / emit_{load,store}_*_at_absolute / emit_memcpy_at_absolute |

后续 phase 待切: str / list / dict / call / closure / control-flow /
record / field 这 6+ 个 category。

## OpVisitor 接入设计

P0-A 已在 `relon-ir/src/op_visitor.rs` 落地了 `OpVisitor` trait
(77 method, exhaustive)。本 phase **没有**立即让 `Codegen` impl
`OpVisitor` —— 原因:

1. `emit_op` 当前 ~50 arm 已 cover, ~27 arm 仍 fall-through 到
   `unsupported`。一次性 impl OpVisitor 强制每个 method 给 body,
   会扩大 surface 风险。
2. ConstPool 应该走独立 visitor pass (`impl OpVisitor for ConstPool`)
   —— 这是更干净的设计, 但需要先把 `collect_op` 完整 cover ~16 Op,
   留给下个 phase.
3. 当前 `Codegen::emit_op` 的 match 已经是一行一 arm
   (`Op::Add(IrType::I64) => self.emit_add_i64()?`),
   把 match 替换成 `walk_op(op, self)` 只是表层语法替换。

下一 phase 计划:
- ConstPool 改 impl OpVisitor (其 method bodies 都是 add-to-pool)
- Codegen 仍保留 emit_op match (因为 27 个 unsupported arm 需要
  unified fallback path), 但每个 arm body 委托到 sub-module method.

## 关键 bench: W3 / W5 cross-impl 一致性

由于本 phase 是 pure structural refactor（IR 输入 + cranelift IR
输出 bit-identical），perf 不会变。functional parity 已通过
`cmp_lua_consistency::w3_string_concat` 和 `w5_dict_str_key`
test verify。每个 W 用例 lua vs cranelift-AOT vs tree-walker
三路 all_agree.

`cargo test -p relon-bench --test cmp_lua_consistency`:
```
running 10 tests
test w1_int_sum ... ok
test w2_f64_dot ... ok
test w3_string_concat ... ok          ← string concat
test w4_string_contains ... ok
test w5_dict_str_key ... ok           ← dict_str_key
test w6_dict_num_key ... ok
test w7_fib ... ok
test w8_poly_callsite ... ok
test w9_nested_matrix ... ok
test w10_config_eval ... ok
test result: ok. 10 passed; 0 failed
```

micro-bench 数字 (criterion run) 没单独跑：splits 不修改 IR,
不修改 emit 序列, perf 必然不变.

## LoC delta

- `codegen.rs`: 4252 LoC -> 删除
- `codegen/mod.rs`: 3458 LoC (-794)
- `codegen/{const_pool, guard, arith, memory}.rs`: +962 LoC
- 净增长: +168 LoC (sub-file 各自的 module doc header + use 声明)

mod.rs 主体压缩 ~19%, 余下 3.4k 还在, 但已经把 ConstPool /
trap helper / arith / memory 4 个独立 category 抽走, 剩余以
control-flow / call / record / dict / field 为主.

## Gate 结果

- `cargo fmt --all --check`: clean
- `cargo clippy --workspace --all-targets -- -D warnings`: clean
- `cargo test --workspace`: 2027 passed, 0 failed
- `cargo check -p relon-wasm --target wasm32-unknown-unknown`: clean
- `cmp_lua_consistency`: 10 W-workload all_agree

## 未完成 / 留给下个 phase

- str / list / dict / call / closure / control-flow / record / field
  8+ category 未抽 (mod.rs 仍 3458 LoC, 主要是 record + call + control-flow)
- `OpVisitor` impl 未上线 (ConstPool 改 visitor pass, Codegen 维持
  match 但每 arm 委托)
- 27 个 Op variant 仍走 `unsupported` fallback — surface 没扩大, 是
  历史遗留, 与本 phase 拆分无关.
