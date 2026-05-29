# cmp_lua 全 panel s90 对比表 (Tier 1-4 全 landed 后)

捕获时间: 2026-05-29
Host: 192.168.213.90 · taskset -c 2 · criterion sample-size 20 · measurement-time 5s
Binary md5: `ba3faa6897c2ae258813b8fde226ac8a`
Commit: `0a1afb96` (post Phase 4 W26+W27 landed)

Bench rows: 119

## 全表 — 28 workload × 9 backend

| Workload | LuaJIT | relon_jit | wasm | wasm_fast | llvm_aot | llvm_aot_fast | rust_native | bytecode | tree_walk |
|---|---|---|---|---|---|---|---|---|---|
| W1_int_sum | 14.52 µs | — | 6.28 µs | 6.10 µs | — | — | — | 1.217 ms | 16.676 ms |
| W2_f64_dot | 12.97 µs | — | 1.27 µs | 1.09 µs | — | — | — | 236.64 µs | 3.377 ms |
| W3_string_concat | 1.164 ms | — | 2.38 µs | — | — | — | — | 2.452 ms | 5.769 ms |
| W4_long_haystack | 14.55 µs | — | 5.31 µs | 5.12 µs | — | — | — | — | 36.568 ms |
| W4_string_contains | 14.55 µs | — | 5.08 µs | 4.90 µs | — | — | — | 5.187 ms | 36.607 ms |
| W5_dict_str_key | 98.39 µs | 55.357 ms | — | — | — | — | — | — | 55.185 ms |
| W6_list_int_sum_plus_one | 52.78 µs | 2.026 ms | 14.67 µs | 14.51 µs | — | — | — | 2.008 ms | 30.176 ms |
| W7_fib | 899.58 µs | 20.659 ms | 229.23 µs | 228.82 µs | 85.95 µs | 85.00 µs | 96.06 µs | 20.569 ms | 132.390 ms |
| W8_poly_callsite | 105.43 µs | 50.972 ms | — | — | — | — | — | — | 51.584 ms |
| W9_nested_matrix | 44.27 µs | 6.931 ms | — | — | — | — | — | — | 6.878 ms |
| W10_config_eval | 17.17 µs | 4.506 ms | — | — | — | — | — | — | 4.518 ms |
| W12_p99_tail | 90.1 ns | 558.2 ns | 228.4 ns | 62.7 ns | 200.0 ns | 2.9 ns | 4.8 ns | 103.8 ns | 1.30 µs |
| W13_deep_dict_access | 3.99 µs | 4.700 ms | — | — | — | — | — | — | 4.696 ms |
| W14_schema_validate | 9.27 µs | 561.00 µs | 4.17 µs | 3.98 µs | — | — | — | 561.44 µs | 3.531 ms |
| W15_conditional_field | 4.45 µs | 253.87 µs | 1.99 µs | 1.76 µs | — | — | — | 253.33 µs | 2.164 ms |
| W16_quicksort | 1.033 ms | 132.220 ms | — | — | — | — | 152.23 µs | — | 132.100 ms |
| W17_binary_search | 6.12 µs | 3.851 ms | — | — | — | — | 2.29 µs | — | 3.849 ms |
| W18_prime_count_trial_div | 2.696 ms | 532.020 ms | — | — | — | — | 747.78 µs | — | 534.310 ms |
| W19_matrix_multiply | 44.34 µs | 31.271 ms | — | — | — | — | 12.08 µs | — | 31.417 ms |
| W20_n_body_softened | 209.85 µs | 268.950 ms | — | — | — | — | 25.13 µs | — | 267.880 ms |
| W21_match_dispatch | 136.22 µs | 46.455 ms | — | — | — | — | — | — | 46.479 ms |
| W23_dict_spread | 2.820 ms | — | — | — | — | — | — | — | 89.055 ms |
| W24_list_comprehension | 88.96 µs | — | — | — | — | — | — | — | 10.769 ms |
| W25_pipe_chain | 45.00 µs | — | — | — | — | — | — | — | 35.565 ms |
| W26_fstring_interp | 66.15 µs | 2.573 ms | — | — | — | — | — | — | 2.634 ms |
| W27_stdlib_dict | 10.097 ms | — | — | — | — | — | — | — | 164.950 ms |
| W28_float_mixed_ops | 77.39 µs | — | — | — | — | — | — | — | 21.010 ms |
| W30_strict_mode_baseline | 52.96 µs | — | — | — | — | — | — | 1.990 ms | 30.391 ms |

## Tier 4 新增 8 workload honest 覆盖详情

| Workload | LuaJIT | tree_walk | 其他 honest backend | LuaJIT/tree_walk ratio |
|---|---|---|---|---|
| W21_match_dispatch | 136.22 µs | 46.479 ms | relon_jit 46.455 ms | 0.0029× |
| W23_dict_spread | 2.820 ms | 89.055 ms | — | 0.0317× |
| W24_list_comprehension | 88.96 µs | 10.769 ms | — | 0.0083× |
| W25_pipe_chain | 45.00 µs | 35.565 ms | — | 0.0013× |
| W26_fstring_interp | 66.15 µs | 2.634 ms | relon_jit 2.573 ms | 0.0251× |
| W27_stdlib_dict | 10.097 ms | 164.950 ms | — | 0.0612× |
| W28_float_mixed_ops | 77.39 µs | 21.010 ms | — | 0.0037× |
| W30_strict_mode_baseline | 52.96 µs | 30.391 ms | bytecode 1.990 ms | 0.0017× |

## 全 panel JIT vs LuaJIT (Compiled tier 真有 row 的 workload)

| Workload | LuaJIT | relon 最佳 | path | ratio |
|---|---|---|---|---|
| W1_int_sum | 14.52 µs | 6.10 µs | wasm_fast | 0.42× |
| W2_f64_dot | 12.97 µs | 1.09 µs | wasm_fast | 0.08× |
| W3_string_concat | 1.164 ms | 2.38 µs | wasm | 0.00× |
| W4_long_haystack | 14.55 µs | 5.12 µs | wasm_fast | 0.35× |
| W4_string_contains | 14.55 µs | 4.90 µs | wasm_fast | 0.34× |
| W5_dict_str_key | 98.39 µs | 55.357 ms | relon_jit (TreeWalker fallthrough)¹ | 562.65× |
| W6_list_int_sum_plus_one | 52.78 µs | 14.51 µs | wasm_fast | 0.27× |
| W7_fib | 899.58 µs | 85.00 µs | llvm_aot_fast | 0.09× |
| W8_poly_callsite | 105.43 µs | 50.972 ms | relon_jit (TreeWalker fallthrough)¹ | 483.47× |
| W9_nested_matrix | 44.27 µs | 6.931 ms | relon_jit (TreeWalker fallthrough)¹ | 156.54× |
| W10_config_eval | 17.17 µs | 4.506 ms | relon_jit (TreeWalker fallthrough)¹ | 262.47× |
| W12_p99_tail | 90.1 ns | 2.9 ns | llvm_aot_fast | 0.03× |
| W13_deep_dict_access | 3.99 µs | 4.700 ms | relon_jit (TreeWalker fallthrough)¹ | 1179.41× |
| W14_schema_validate | 9.27 µs | 3.98 µs | wasm_fast | 0.43× |
| W15_conditional_field | 4.45 µs | 1.76 µs | wasm_fast | 0.39× |
| W16_quicksort | 1.033 ms | 132.220 ms | relon_jit (TreeWalker fallthrough)¹ | 128.01× |
| W17_binary_search | 6.12 µs | 3.851 ms | relon_jit (TreeWalker fallthrough)¹ | 629.42× |
| W18_prime_count_trial_div | 2.696 ms | 532.020 ms | relon_jit (TreeWalker fallthrough)¹ | 197.34× |
| W19_matrix_multiply | 44.34 µs | 31.271 ms | relon_jit (TreeWalker fallthrough)¹ | 705.19× |
| W20_n_body_softened | 209.85 µs | 268.950 ms | relon_jit (TreeWalker fallthrough)¹ | 1281.63× |
| W21_match_dispatch | 136.22 µs | 46.455 ms | relon_jit (TreeWalker fallthrough)¹ | 341.03× |
| W26_fstring_interp | 66.15 µs | 2.573 ms | relon_jit (TreeWalker fallthrough)¹ | 38.89× |
| W30_strict_mode_baseline | 52.96 µs | 1.990 ms | bytecode | 37.57× |

¹ relon_jit 走 `JitEvaluator::run_main` 但 active_tier ≠ Compiled，输出与 tree_walk 同源，**不是真 JIT 编译数据** —— 仅记录作为 envelope expansion 待办信号。


## Tier 4 真 blocker (未 landed)

| Workload | 原计划特性 | Blocker | 解决路径 |
|---|---|---|---|
| W22_ref_resolution | `&root` / `&sibling` / `&uncle` per-iter path 解析 | evaluator circular-reference guard 拒所有 reduce 闭包内 `&root` 解引用 (`crates/relon-evaluator/src/reference.rs:777-786`)；唯一不撞 cycle 的形态 (`cfg_ref: &sibling.cfg`) pre-resolve 成 `Value::Dict` alias，与 W13_deep_dict_access 走同一路径，booking 在 `W22` 违反 same-code-path 关 | evaluator 放宽 ref-into-sealed-subtree guard 后重启 |
| W29_null_coalesce | `??` null-coalesce 运算符 | parser / Operator enum / IR `Op::Coalesce` 全栈未实现 (Phase 3 agent probed: `parse_coalesce` 不存在；`Operator::Coalesce` 无变体；3 个 source 形态全部 `parse error: expected expression`) | `??` 端到端 (parser + IR + evaluator) 落地后重启 |

## n/a 行的诚实披露 (Tier 4 新增)

| Workload | 缺哪些 backend | 原因 |
|---|---|---|
| W21_match_dispatch | wasm / aot / llvm_aot / bytecode / rust_native | `#brand` + `match` 跨 funcref 边界不在 Z.4 walker envelope；bytecode M2-A rejects `AnonDictReturn(List)`；Rust `enum + match` 把 runtime brand-string 比对折叠为 compile-time variant tag = paper-win shape (gate: `paper_win_brand_dispatch_label`) |
| W23/W24/W25 | wasm / aot / llvm_aot / bytecode / rust_native | Dict spread / Comprehension AST / Pipe operator 均不在 M2-A bytecode envelope；Rust 等价形态全部触发 closed-form fold / 不同 allocator 路径 (gate: `paper_win_container_sugar_label`) |
| W26_fstring_interp | wasm / aot / llvm_aot / bytecode / rust_native | `Op::FString` + `_len` 不在 M2-A envelope；Rust `format!` digit-bucket const-fold = paper-win |
| W27_stdlib_dict | wasm / aot / llvm_aot / bytecode / rust_native | 模块解析器 + dict literal in reduce closure 不在 M2-A；Rust `HashMap::keys().count()` const-fold = paper-win |
| W28_float_mixed_ops | wasm / aot / llvm_aot / bytecode | Float `#main` 返回类型不在 Phase E typed surface (待 Z.4.x)；Z.1 wasm program set 无 Float lowering |
| W30_strict_mode_baseline | llvm_aot / rust_native | 同 W6 受 `paper_win_closed_form_fold_label` gate；strict 模式 baseline 对比 W6 走 W6 自身 backend column |

## 与 Tier 1-3 panel (commit `a669084`) 的对比要点

- 总 workload: 20 → **28** (+8 honest, +2 blocker doc-only)
- Bench rows: 100 → **119** (+19 honest)
- 覆盖特性域: 算法 / numeric / list / dict / closure → **match / brand / ref (blocked) / spread / comprehension / pipe / fstring / non-list stdlib / float-mixed / strict-mode / null-coalesce (blocked)**
- "Relon vs LuaJIT" 不再只在 numeric algo kernel 成立; Tier 4 workload 在 tree-walker tier 上慢于 LuaJIT 是诚实结果 (envelope expansion 即可缩差，但当前 0 gate hack)
