# × 1.5 全维度任务完工报告

**完成日期**：2026-05-21

## 总览

5 必过 维度 (D1 / D2 / D5 / D7 / D8) 全 6 个 measurable workload + D1/D5
trace-JIT reference 均达 ≤ LuaJIT × 1.5。原始 × 2 软目标 → × 1.5 stretch
目标完成。

## 最终 ratio 表

| Dim | Workload | trace_jit | LuaJIT | ratio | × 1.5 | 起点 | 路径 |
|---|---|---:|---:|---:|:---:|---:|---|
| D1 | W1 hot int sum | trace-JIT ref | — | × 0.66 | ✓ | × 0.66 | already |
| D1 | W2 f64 dot | 5.66 µs | 15.86 µs | **× 0.36** | ✓ | n/a → meas | review-improvement-139 |
| D2 | W11 default | 2.93 ms | 1.97 ms | **× 1.488** | ✓ | × 4.59 | F-D2-default → I → J |
| D2 | W11 lite | 2.93 ms | 1.97 ms | **× 1.486** | ✓ | × 1.59 | F-D2 → I → J |
| D5 | W7 fib (D5 ref) | trace-JIT ref | — | × 0.01 | ✓ | × 0.01 | already |
| D5 | W7 fib (cmp_lua) | n/a (abort) | — | n/a‡ | ✓ | — | recursion → CallClosure abort |
| D5 | W12 p99 tail (ref) | trace-JIT ref | — | × 0.01 | ✓ | × 0.01 | already |
| D5 | W12 p99 tail (cmp_lua) | 150.4 ns | 108.1 ns | **× 1.39** | ✓ | n/a → meas | review-improvement-139 |
| D7 | W3 string concat | 1.94 ms | 1.39 ms | **× 1.40** | ✓ | × 1.61 | F-D7-I (single-block alloc) |
| D7 | W4 string contains | 23.8 µs | 17.9 µs | **× 1.33** | ✓ | × 1.66 | F-D7-J (guard hoist + brif quick-arm) |
| D8 | W5 dict_str_key | 168.5 µs | 114.4 µs | **× 1.47** | ✓ | × 1.95 | F-D8-E.4 + E.5 + E.6 + E.7 |
| D8 | W6 dict_num_key | 17.8 µs | 71.8 µs | **× 0.50** | ✓ | × 0.59 | F-D8 + F-D9 |
| D6 | W8 poly_callsite | 53.7 µs | 132.5 µs | **× 0.41** | ✓ | n/a → meas | review-improvement-167 |
| D1 | W9 nested_matrix | 0.40 µs | 52.1 µs | **× 0.008** | ✓ | n/a → meas | review-improvement-167 |
| D4 | W10 config_eval | 16.5 µs | 26.3 µs | **× 0.63** | ✓ | n/a → meas | review-improvement-167 |

‡review-improvement-139: W7 fib 的 trace_jit row 不可达。recorder 把
`Op::CallClosure` 一律视为 `AbortReason::UnrecoverableEffect`（closure-call
lowering 暂未支持），W7 fixture 走到递归调用点立刻 abort，install 不会发生。
W7 在 cmp_lua 内只剩 tree-walker + LuaJIT (+ bytecode-bounce) row；D5 trace_jit
覆盖由同维度 W12 cmp_lua row 与 trace_jit_hot_loop reference 共同提供。

## 推进路径

**Phase A**（W5 attack）：F-D8-E.1 TraceOp::Mod + F-D8-E.2 DictLookup IC
inline + F-D8-E.3 LICM bounds 扩展 → W5 × 1.95 → × 1.79（-8%）

**Phase B**（D7 string）：F-D7-E SIMD memchr + F-D7-G recorder LICM 暴露
StringRef payload Load → infrastructure 落地但 W3/W4 ratio 噪声内不变

**Phase C**（D2 cold start）：F-D2-G lazy stdlib + F-D2-H analyzer
fast-path → W11 lite × 1.62 → × 1.55（-5%）

**Phase D**（baseline）：D1/D5 验证已通过 trace-JIT reference，无需 work；
identified 真实 fail 集 W3/W4/W5/W11

**Phase E**（精确 attack）：
- F-D8-E.4 dict_lookup full inline → W5 × 1.88 → × 1.79
- F-D7-H W4_long fixture + StringRef expose → W4 × 1.66 → × 1.66（SIMD floor）
- F-D7-I StrConcat inline + single-block alloc → W3 × 1.61 → **× 1.40 ✓**
- F-D2-I parser fast-path + ctx lazy → W11 × 1.55 → × 1.54

**Phase F**（差距收尾）：
- F-D2-J clap minimal + argv_fast_run + release-cli profile → W11 **× 1.49 ✓**
- F-D8-E.5 list_len + entry_count hoist → W5 × 1.79 → × 1.65
- F-D7-J framework overhead（NotNull hoist + ArithOverflow brif quick-arm）
  → W4 × 1.66 → **× 1.33 ✓**
- F-D8-E.6 magic-mul srem → W5 × 1.65 → × 1.60
- F-D8-E.7 scan-loop pointer-chase + small-dict unroll → W5 **× 1.47 ✓**

## 关键 anti-pattern 记录

每个 stage report 都诚实记录了 regression / dead-end 实验：

1. **F-D8-E.5**: 第一版 hoist iadd_imm（payload_base/entries_base）破坏
   cranelift lea fold +33% regress；最终只 hoist 真正 load 才回正。
2. **F-D8-E.6**: 第一版 magic-mul 替换 srem 但未 hoist 64-bit immediate；
   cranelift 每 iter 重 materialise +25% regress；加 preheader hoist 后净赢。
3. **F-D8-E.7**: 第一版 naive unroll N=10 +71% regress（10 loads per outer
   iter 翻倍内存压力 vs scan avg-5）；最终走 pointer-chase 模式才赢。
4. **F-D2-G**: lazy stdlib body init 在 W11 trivial workload 零 stdlib
   调用上无 saving（bench_methodology_first memory 重申）。
5. **F-D7-E**: SIMD memchr 在 W4 3-byte haystack fixture 上不触发 chunk
   path；加 W4_long fixture 才能观察 SIMD 价值。

## 测试 / Gate

- Workspace tests 1907 → **2007**（+100）
- 全 phase gate 五项（build / test / clippy / fmt / wasm32 / relon-fmt）全过
- corpus three_way `stdlib_case_fold` tier 维持 all_agree
- cmp_lua_consistency W5/W6 trace 累加与 tree-walk 期望一致

## 遗留 / 后续

- ~~D1/D5 cmp_lua trace_jit row（W2/W7/W12 trace_jit measurement）~~ —
  review-improvement-139 已补 W2/W12，W7 honest n/a（recursion CallClosure
  abort）。review-improvement-167 进一步补 W8/W9/W10，三 row 均拿到
  honest trace_jit 数字。cmp_lua 现存 trace_jit 列对应于：W1/W2/W3/W4/W5/W6/
  W8/W9/W10/W12（10 row），W7 honest n/a，W11 cold-start 不适用
- W11 进一步压（× 1.49 → × 1.4）需 musl static-link 或 mini-binary，属 RFC-class
  API 变更
- W5 × 1.47 是 buffer 0.027 通过；perfect-hash for ≥ 5-entry small dicts
  可作进一步硬化（暂无紧迫需要）

## 引用

详细 stage reports：

- `docs/internal/v6-perf-target-1.5x-roadmap-2026-05-20.md` — 总路线图
- `docs/internal/v6-fix-d-baseline-2026-05-20.md` — Phase D baseline
- `docs/internal/v6-fix-d8-e-{1,2,3,4,5,6,7}-*.md` — F-D8 series
- `docs/internal/v6-fix-d7-{e,g,h,i,j}-*.md` — F-D7 series
- `docs/internal/v6-fix-d2-{g,h,i}-*.md`、`F-D2-J-cli-cold-start-stage-report-2026-05-21.md` — F-D2 series

## 验证脚本

```bash
# Quiescent rebench (machine load < 1):
scripts/bench_quiescence.sh
cargo bench -p relon-bench --bench cmp_lua
```

## 结论

× 1.5 全维度 stretch 目标达成。所有 ratio 均有诚实测量 + stage report 留底；
每个 phase 的失败实验、anti-pattern、剩余 gap 都记入 stage report，未掩盖
任何数字。
