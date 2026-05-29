# cmp_lua 全 panel s90 对比表 (Tier 1-3 扩展完工后)

捕获时间: 2026-05-28
Host: 192.168.213.90 · taskset -c 2 · criterion sample-size 100 · measurement-time 5s
Binary md5: `fea81ac3b47f9469bbdcff0acf9336f2`
Commit: `469d0d8` (post W19/W20 cherry-pick)

## 1. 全表 — 20 workload × 9 backend

`—` 表示该 backend 对该 workload n/a，原因详见 §3。`relon_jit` 列实际是 `JitEvaluator::run_main` 输出，若 active_tier 是 TreeWalker fallback (W5/W8/W9/W10/W13/W16-W20) 时间与 tree_walk 列接近 —— 这是诚实披露的"无 Compiled 路径"信号，**不是 JIT 在跑**。

| Workload | LuaJIT | relon_jit | wasm | wasm_fast | llvm_aot | llvm_aot_fast | rust_native | bytecode | tree_walk |
|---|---|---|---|---|---|---|---|---|---|
| W1_int_sum                | 14.52 µs | —          | 6.27 µs    | 6.10 µs    | —        | —       | —         | 1.222 ms  | 16.606 ms  |
| W2_f64_dot                | 13.09 µs | —          | 1.26 µs    | 1.09 µs    | —        | —       | —         | 242.57 µs | 3.391 ms   |
| W3_string_concat          | 1.160 ms | —          | 2.32 µs    | —          | —        | —       | —         | 2.502 ms  | 5.913 ms   |
| W4_long_haystack          | 14.55 µs | —          | 5.30 µs    | 5.13 µs    | —        | —       | —         | —         | 36.193 ms  |
| W4_string_contains        | 14.54 µs | —          | 5.07 µs    | 4.90 µs    | —        | —       | —         | 5.168 ms  | 35.357 ms  |
| W5_dict_str_key           | 100.13 µs| 50.238 ms¹ | —          | —          | —        | —       | —         | —         | 51.296 ms  |
| W6_list_int_sum_plus_one  | 53.56 µs | 2.047 ms¹  | 14.67 µs   | 14.50 µs   | —        | —       | —         | 2.071 ms  | 30.379 ms  |
| W7_fib                    | 893.39 µs| 20.423 ms¹ | 663.65 µs  | 228.80 µs  | 85.85 µs | 84.97 µs| 96.06 µs  | 20.386 ms | 132.870 ms |
| W8_poly_callsite          | 106.59 µs| 51.029 ms¹ | —          | —          | —        | —       | —         | —         | 52.496 ms  |
| W9_nested_matrix          | 44.79 µs | 6.673 ms¹  | —          | —          | —        | —       | —         | —         | 6.510 ms   |
| W10_config_eval           | 17.48 µs | 4.536 ms¹  | —          | —          | —        | —       | —         | —         | 4.622 ms   |
| W12_p99_tail              | 88.7 ns  | 570.3 ns   | 229.8 ns   | 63.6 ns    | 200.0 ns | 2.9 ns  | 5.3 ns    | 105.0 ns  | 1.28 µs    |
| W13_deep_dict_access      | 3.97 µs  | 4.030 ms¹  | —          | —          | —        | —       | —         | —         | 4.056 ms   |
| W14_schema_validate       | 9.31 µs  | 566.98 µs¹ | 4.15 µs    | 3.97 µs    | —        | —       | —         | 569.54 µs | 3.830 ms   |
| W15_conditional_field     | 4.54 µs  | 261.93 µs¹ | 1.89 µs    | 1.75 µs    | —        | —       | —         | 256.22 µs | 2.154 ms   |
| W16_quicksort             | 1.017 ms | 121.200 ms¹| —          | —          | —        | —       | 150.29 µs | —         | 120.970 ms |
| W17_binary_search         | 6.09 µs  | 3.841 ms¹  | —          | —          | —        | —       | 2.30 µs   | —         | 3.830 ms   |
| W18_prime_count_trial_div | 2.726 ms | 531.540 ms¹| —          | —          | —        | —       | 747.49 µs | —         | 533.210 ms |
| W19_matrix_multiply       | 45.52 µs | 28.767 ms¹ | —          | —          | —        | —       | 12.13 µs  | —         | 28.666 ms  |
| W20_n_body_softened       | 208.37 µs| 239.100 ms¹| —          | —          | —        | —       | 25.16 µs  | —         | 239.420 ms |

¹ `relon_jit` 输出 = TreeWalker fallback (active_tier ≠ Compiled)；与 tree_walk 列同源，非 JIT 编译结果。

## 2. 核心比较

### 2.1 JIT vs LuaJIT (仅含真有 Compiled 路径的 row)

WASM (wasmtime) 走 `WasmEvaluator` Compiled tier，跟 LuaJIT 公平对比的 row：

| Workload | LuaJIT | relon 最佳 | path | ratio |
|---|---|---|---|---|
| W1_int_sum             | 14.52 µs | 6.10 µs   | wasm_fast     | **0.42×** |
| W2_f64_dot             | 13.09 µs | 1.09 µs   | wasm_fast     | **0.08×** |
| W3_string_concat       | 1.160 ms | 2.32 µs   | wasm          | **0.002×** |
| W4_long_haystack       | 14.55 µs | 5.13 µs   | wasm_fast     | **0.35×** |
| W4_string_contains     | 14.54 µs | 4.90 µs   | wasm_fast     | **0.34×** |
| W6_list_int_sum_plus_one | 53.56 µs | 14.50 µs | wasm_fast     | **0.27×** |
| W7_fib                 | 893.39 µs| 84.97 µs  | llvm_aot_fast | **0.10×** |
| W12_p99_tail           | 88.7 ns  | 2.9 ns    | llvm_aot_fast | **0.03×** |
| W14_schema_validate    | 9.31 µs  | 3.97 µs   | wasm_fast     | **0.43×** |
| W15_conditional_field  | 4.54 µs  | 1.75 µs   | wasm_fast     | **0.39×** |

10/10 全部 < LuaJIT。

### 2.2 AOT vs rust_native (Phase L / Z 目标 ≤ 1×)

仅含 llvm-aot 真出 envelope 的 row：

| Workload | rust_native | llvm_aot_fast | ratio |
|---|---|---|---|
| W7_fib       | 96.06 µs | 84.97 µs | **0.89×** ✓ |
| W12_p99_tail | 5.3 ns   | 2.9 ns   | **0.55×** ✓ (单 op call edge) |

W7 fib 是唯一含 recursion + arithmetic kernel 的 row，0.89× 是 Phase L MCJIT CodeModel::Small 修复后的成果。

## 3. n/a 行的诚实披露原因

| Workload | 缺哪些 backend | 原因 |
|---|---|---|
| W1/W2/W3/W4/W4_long/W6/W14/W15 | llvm_aot | gate `paper_win_closed_form_fold_label` —— LLVM -O3 把 arithmetic-progression sum / boolean-fold chain reduce 成 closed-form 多项式 / 常量，IR dump 验证无 loop 指令；若上 row 则是 O(1) 算 vs LuaJIT 走 O(n) 的 paper win |
| W5/W8/W9/W10 | llvm_aot / wasm | gate `paper_win_collapsed_variant_label` —— 这些 workload 的 LLVM source 变体跳过了 production source 的核心 work (dict probe / closure dispatch / list materialise)，被 audit #318 拒 |
| W5/W8/W9/W10/W13 | relon_jit (实际为 TreeWalker) / bytecode / wasm | 当前 JIT envelope + bytecode IR-lift 不接受这些 production source 的某些构造 (dict literal binding / 一等闭包 / 列表字面量等) |
| W11_cold_start | 全 backend (除 luajit_fresh_proc) | 测进程冷启动时间，per spec 用 LuaJIT subprocess 做唯一比对 |
| W13 | bytecode / wasm | dict literal as `#internal` binding 是 wasm walker 拒收的构造 (Z.4 scope-cut) |
| W16-W20 | wasm / llvm_aot | 算法用到的 stdlib (`range` 多 arity / 高阶 closure 跨 module 边界 / matrix indexing) 不在 Z.4 walker envelope 内，正确 fall back to tree-walker，n/a 行 honest 而非 fixture |
| W19/W20 | rust_native exists | rust_native 行通过 `rust_native_dispatch` 派发，保留作为算法上界基线 |

## 4. 待办 / 红线参考

- W12 llvm_aot_fast 2.9 ns 已经过 audit (task #339) —— 单 op `x + 1` 1-invoke-per-iter，非 closed-form fold over loop。3 honesty 关全过 (same algorithm / code path / I/O shape)。
- relon_jit 列 W5/W8/W9/W10/W13/W14/W15/W16-W20 都是 TreeWalker fallback —— 列名上 `_fixture` 后缀已废弃但 reader 仍可能误以为是 JIT 数据，长期解法是 Phase J.2 修 deopt-snapshot loop-carried φ bug 并扩 WasmEvaluator Compiled envelope。
