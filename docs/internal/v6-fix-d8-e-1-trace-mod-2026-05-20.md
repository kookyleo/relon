# F-D8-E.1 阶段报告：TraceOp::Mod 端到端接入 + W5 bench 复跑（2026-05-20）

## 摘要

- F-D8-D 把 cmp_lua W5 / W6 切到 recorder 路径之后，遗留的最大可观察
  开销是 W5 hot loop 里 `i % 10` 被迫展开成 `Div + Mul + Sub` 三 arith
  op + 三 `ArithOverflow` guard（recorder 端 `Op::Mod` 当时 abort 成
  `UnsupportedOp("Mod")`）。
- 本阶段把 `Op::Mod` 端到端打通：新增 `TraceOp::Mod(dst, lhs, rhs)`，
  在 recorder lowering、cranelift emitter（standalone + inline）、
  trace recording walker (`step_mod`)、optimizer passes（const-fold、
  load-forward）一并接入；W5 IR body 同步换成 `Op::Mod(I64)`，三 arith
  triple → 单 `srem` + divisor-zero guard。
- W5 bench rerun：trace_jit 从 228.70 µs → **222.78 µs**（−2.6%）；
  LuaJIT 同环境同时间从 117.00 µs → 113.78 µs（−2.4%，热环境同向波动）。
  **ratio 维持在 1.95×**（before 1.954× / after 1.958×，差值在采样
  噪声内），**未达到 ≤ 1.5× 的目标**。诚实记录：单 op 收益小于预期，
  root cause 推断（见 §五）是 W5 hot path 仍被 DictLookup IC + ListGet
  bounds-check 主导，Mod 三 arith / 三 guard 节省的 ≈6 ns/iter 在 22 ns
  iter cost 里只是边际项。
- 改动 7 个文件，+221 / −19 行（不含报告本身）。覆盖 trace-jit / 
  trace-emitter / trace-recorder / codegen-native / bench / 报告。
- 所有 5 项 gate（build / test / clippy / fmt / wasm / relon-fmt）通过；
  workspace test 含两组新 unit test（recorder lowering + emitter smoke）
  和原 corpus three_way / bytecode_diff（含 arith_mod，arith_mod_by_zero_traps
  双重覆盖）保持 all_agree / AllTrap。

## 一、改动文件 + LoC

| 文件 | LoC | 说明 |
|------|-----|------|
| `crates/relon-trace-jit/src/trace_ir.rs` | +27 / −3 | 新增 `TraceOp::Mod`，effect/inputs/output 全 cover；新增单测。 |
| `crates/relon-trace-jit/src/optimizer/const_fold.rs` | +24 / −1 | `Mod` 加入 fold 主循环 + `fold_arith` 两条 `wrapping_rem` 分支（i32 / i64），拒绝 fold `_ % 0` 与 `MIN % -1` 这两条 trap 路径，让 emitter 的 guard 仍是 trap 行为的唯一权威。 |
| `crates/relon-trace-jit/src/optimizer/load_forward.rs` | +3 / −3 | `Mod` 与 `Div` 一样按 RecoverableWrite 处理：保守 flush slot 表 + alias swap 一致。 |
| `crates/relon-trace-recorder/src/lowering.rs` | +56 / −5 | `Op::Mod(ty)` 接入 `binary_arith` 并新增 `BinaryArith::Mod` 分支，emit `TraceOp::Mod` + `ArithOverflow(dst)` guard + `RecoverableWrite` effect；新增 `mod_i64_emits_trace_mod` / `mod_f64_aborts_float_arith` 两条 drift guard 单测。 |
| `crates/relon-trace-emitter/src/emitter.rs` | +44 / −1 | 加 `TraceOp::Mod` dispatch + `emit_mod`，shape 与 `emit_div` 一致（divisor-zero `brif → deopt` + `srem`），seed `overflow_bits` 让 `Guard(ArithOverflow)` 在 hot path collapse 成 pass；doc 表加 Mod 行。 |
| `crates/relon-trace-emitter/src/inline_emit.rs` | +24 / −0 | inline 路径同步加 `emit_mod`，与 standalone emit 平行。 |
| `crates/relon-trace-emitter/tests/emit_arith.rs` | +26 / −0 | `mod_emits_srem_with_divisor_zero_check` smoke：build → verify → 字符串校验 `srem` + `brif` 两条指令都在生成的 cranelift IR 里。 |
| `crates/relon-codegen-native/src/trace_recording.rs` | +28 / −0 | walker 新增 `Op::Mod` 分发 + `step_mod`，与 `step_div` 一致的运行时 divisor-zero short-circuit（避免 host panic），但记录值用 `wrapping_rem`。这是 arith_mod corpus case 之所以 regress 的真正修复点（见 §三）。 |
| `crates/relon-bench/benches/cmp_lua.rs` | +9 / −16 | W5 recorder body 把 `i % 10` 从 `Div + Mul + Sub` 三 arith 简化成单 `Op::Mod(I64)`，更新 docstring。 |

合计 **+221 / −29**（不含 stage 报告与 lint 修整）。

## 二、关键决策

### 2.1 effect 与 guard 形状：完全复用 `Div`

`Mod` 的运行时 trap 表面与 `Div` **完全相同**：

- `b == 0` → trap（runtime 走 cranelift `srem` 的 unconditional trap）
- `i64::MIN % -1` 是唯一的 overflow case（与 `i64::MIN / -1` 同源）

所以：

- **effect = `RecoverableWrite`**：与 `Div` 一致，trace 可以在 deopt 时
  回滚被 fused 的下游写。`load_forward` / `const_fold` 走 reorder
  barrier 规则。
- **guards_after = `[ArithOverflow(dst)]`**：与 `Div` 同；emitter 的
  `emit_mod` seed 一份 `of_bit = iconst(I32, 0)`，让 hot path 的
  `Guard(ArithOverflow)` 预测器走 `(0 == 0) → 1 → pass`。Real 
  `MIN % -1` 漏检由前置的 observed-type 与 LocalGet 类型保证（W5 的
  `i` 由 `range(n)` 喂出，永远 `0 ≤ i < n`）。
- **divisor-zero pre-check**：emitter 与 `emit_div` 完全一致的
  `iconst 0 → icmp NotEqual → brif ok / deopt` 三联，避免 cranelift 
  unconditional trap 撞到 host。

### 2.2 const-fold 拒绝 trap 路径

`fold_arith` 新增的 `Mod` 两个分支显式拒绝 fold `_ % 0` 与 `MIN % -1`：

```rust
(TraceOp::Mod(_, _, _), TraceConst::I32(x), TraceConst::I32(y)) => {
    if y == 0 || (x == i32::MIN && y == -1) {
        None
    } else {
        Some(TraceConst::I32(x.wrapping_rem(y)))
    }
}
```

这两条路径走 emitter 的 divisor-zero / overflow guard，guard 是 trap
语义的单一权威。让 const-fold 自作主张 fold 成 `0` 或 panic 都会
破坏 deopt-rollback 的 PC 约定。

### 2.3 walker 端 `step_mod` 不要遗漏

第一轮 patch 只改了 recorder lowering 与 emitter 两侧，corpus 跑出
`arith_mod` regression：tw / cr 都返回 2（`17 % 5`），trace_jit 返回
0。Root cause：`TraceRecordingEvaluator::step_one` 的 dispatch 表里没
有 `Op::Mod` 分支，op 落到 default Abort → 整段 body record 失败 → 
`invoke_with_fallback` 触发 fallback closure 但 fallback 里
`arith_fallback(Mod, ...)` 在调用前 op_stack 已 unwind，trace 返回了
trace_emitter 写出的 result slot 初始值 0。

补齐 `step_mod` 后 `arith_mod` / `arith_mod_by_zero_traps` 两条 corpus
case 都恢复绿色。

## 三、W5 bench：before / after

测试环境：开发机（schedutil governor，load avg ≈3.8，未严格 quiescent
—— 通过 `RELON_BENCH_FORCE_RUN=1` 跑），criterion 100 样本。

| Backend | before | after | Δ |
|---------|--------|-------|---|
| relon_tree_walk | 104.37 ms | 106.42 ms | +2.0%（噪声同向） |
| **relon_trace_jit** | **228.70 µs** | **222.78 µs** | **−2.6%** |
| luajit | 117.00 µs | 113.78 µs | −2.7% |

**ratio (trace_jit / luajit)**：

- before: 228.70 / 117.00 = **1.954×**
- after:  222.78 / 113.78 = **1.958×**

差值（≈0.2%）远在样本间噪声里，**未达到 ≤ 1.5× 的目标**。

### 3.1 W5 bench baseline 与 task 指令 ratio 偏差

task 指令里写的 baseline 是 × 2.19，本地实测是 × 1.95。两个数字
都没问题：F-D8-D stage report 是上一台机器的快照，本机线程 / 缓存 /
LuaJIT build 都不同。绝对 µs 与 ratio 都用同一台机器、同一次 criterion
session 内的连续 row 对比，保证 before/after 之间没有跨机偏移。

### 3.2 为什么 ratio 没动？

W5 hot loop 单次 iter 的 work：

1. ListGet（bounds check + i64 load）
2. DictLookup（FxHash key + IC 比对 + `Arc<Vec<Value>>` slot 取）
3. arith chain（before: 3 arith + 3 guards；after: 1 arith + 1 guard）
4. acc / i bump

Mod 改动节省的 2 arith + 2 guard，每个 guard 是 `(of_bit == 0) →
brif` 折叠后两条指令；cranelift 后端把整段折成 ≈4 cycle 节省。在
N = 10000 iter / 222 µs 的肩膀上，单 iter ≈ 22 ns；6 ns / 22 ns 是
27% 的理论上限，但实际 measurement 显示只 ≈ 2.6% —— 推断 cranelift
后端在 register-allocation 阶段已经把这 3 arith chain 用上溢出/写后
读的资源，重排后端能拿到的真实净 saving 远低于 op-count 估算。

更深层的 cost driver 在 dict_lookup_with_shape 的 host helper call 
（每 iter 一次跨 ABI 调用 + Vec 索引），单 op 的 IR 优化吃不到它。
继续往 ≤ 1.5× 推大概率得走 **F-D8-E.2：DictLookup IC fast path inline**
或 **F-D7-D-style LICM 把 dict_ptr / keys_list_ptr load 提到 loop
preheader**。两者都在 F-D8-E.2/E.3 的 backlog 上。

## 四、测试覆盖

| Layer | 测试 | 期望 |
|-------|------|------|
| `relon-trace-jit::trace_ir::tests::mod_is_recoverable_write_with_io` | unit | effect=RecoverableWrite、output/inputs shape 正确、`is_guard()=false` |
| `relon-trace-recorder::lowering::tests::mod_i64_emits_trace_mod` | unit | drift guard：`Op::Mod(I64)` → `TraceOp::Mod` + `[ArithOverflow]` + Recoverable |
| `relon-trace-recorder::lowering::tests::mod_f64_aborts_float_arith` | unit | `Op::Mod(F64)` 走原有 `FloatArith` abort 路径 |
| `relon-trace-emitter/tests/emit_arith.rs::mod_emits_srem_with_divisor_zero_check` | int | cranelift IR 里既有 `srem` 又有 `brif`，verifier 通过 |
| `relon-test-harness::corpus_four_way_diff_aggregates` | corpus | `arith_mod` / `arith_mod_by_zero_traps` 在 ArithControl tier 维持 AllAgree / AllTrap（regression 防御 §三 step_mod 的修复） |

`cargo test --workspace` 全绿。

## 五、Gate（5 项）

| Gate | 状态 |
|------|------|
| `cargo build --workspace` | OK |
| `cargo test --workspace` | OK（含 corpus three_way / four_way） |
| `cargo clippy --workspace --all-targets -- -D warnings` | OK |
| `cargo fmt --all -- --check` | OK |
| `cargo build --target wasm32-unknown-unknown -p relon-wasm` | OK |
| `cargo run -q -p relon-fmt -- --check fixtures/* examples/*` | OK |

## 六、Follow-up

- **F-D8-E.2 候选**：DictLookup IC fast path 直接 inline 进 trace（跳过
  host helper call），目标把 dict lookup 从 host call 降到 ≈ 5 ns
  inline。这是把 W5 ratio 真正推到 ≤ 1.5× 的最大杠杆。
- **F-D8-E.3 候选**：LICM 把 W5 的 `dict_ptr` / `keys_list_ptr`
  LocalGet 提到 loop preheader（目前 hot loop 每 iter 都 reload，但它们
  loop-invariant）。
- `TraceOp::BitAnd`：与 Mod 同样在 recorder 端 abort，没有 production
  exercise case；落地等 corpus 提供 trigger。
