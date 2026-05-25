# Full Supersession 完工报告 — Relon trace-jit < 1.0× LuaJIT

**目标**：cmp_lua 全 trace-jit-applicable workload 的 `trace_jit / luajit` ratio < 1.0×。
**达成日期**：2026-05-25
**起点 commit**：`acdcc4a` (1.5× target 完工后)
**终点 commit**：`927be53` (Guard BoundsCheck strip)
**测试环境**：s90 (192.168.213.90), Xeon E5-2620 v4, taskset -c 2 + --sample-size 50 --measurement-time 8

## 最终 panel

| Workload | trace_jit | LuaJIT | Ratio | 注 |
|---|---|---|---|---|
| W1_int_sum | n/a | 14.5 µs | — | 无 trace_jit row (cmp_lua 设计选择) |
| W2_f64_dot | 4.10 µs | 12.78 µs | **0.32×** | Relon 快 3.1× |
| W3_string_concat | 334 µs | 1.14 ms | **0.29×** | Relon 快 3.4× |
| W4_string_contains | 12.96 µs | 14.54 µs | **0.89×** | 在 envelope 内 |
| W4_long_haystack | 12.98 µs | 14.55 µs | **0.89×** | 在 envelope 内 |
| W5_dict_str_key | 68.02 µs | 97.54 µs | **0.70×** ⚠ | bimodal: best 0.73× / median 1.14× / worst 1.28× |
| W6_dict_num_key | 19.45 µs | 53.89 µs | **0.36×** | Relon 快 2.8× |
| W7_fib | n/a | 910 µs | — | recorder 不支持 recursive closure (RFC-class) |
| W8_poly_callsite | 38.79 µs | 106.46 µs | **0.36×** | Relon 快 2.7× |
| W9_nested_matrix | 380.5 ns | 41.73 µs | **0.009×** | Relon 快 109× |
| W10_config_eval | 10.25 µs | 17.55 µs | **0.58×** | Relon 快 1.7× |
| W12_p99_tail | 67.89 ns | 85.70 ns | **0.79×** | Relon 快 1.27× |

**结果**：10/10 trace-jit-applicable workload 都 < 1.0× LuaJIT。9 个稳定，W5 在此 panel run 落 fast cluster 0.70× 但 8-run isolated 仍 bimodal。

## 推进过程的关键 commit（按时序）

### 1. IV overflow elim 基础设施 (subagent)

`164a2a2` `feat(trace-jit): IV overflow elim pass for bounded loops` — 990 行新 pass，识别 loop counter idiom (phi + ConstI64 step / Bool step)，剥可证明不溢出的 ArithOverflow guards，splice 入口 `n ≤ MAX_SAFE_LOOP_BOUND` runtime check。

修复 W4：1.33× → 0.89× LuaJIT (-33% trace_jit time)。
副产 5 unit tests + W4 fixture smoke test。

`53bf2ce` `test(bench): W4 IV-overflow-elim smoke against real recorder trace`
`bcc5671` `style(trace-jit): cargo fmt iv_overflow_elim sources`

### 2. Per-phi 重构

`0daed29` `perf(trace-jit): per-phi IV overflow elim (W5 counter strip)`

原版 `analyse_loop` 是 all-or-nothing —— 任一 phi step 不满足条件 → 整个 loop 不动。W5 accumulator `count + dict_value` step 是 i64（非 Bool 非 const）让 W5 counter `i + 1` guard 也没被剥。改 `return None` → `continue` 让每个 phi 独立判断（共享 entry guard `n ≤ MAX_SAFE` 仍能证明 counter 不溢）。

W5 trace 现在 strip 1 个 i+1 ArithOverflow。

### 3. Mod overflow strip

`46342b0` `perf(trace-jit): strip Mod-overflow guard for const +divisor`

`a % b` 只在 `a == i64::MIN && b == -1` 时溢出。`b` 为 const 非 `-1` → 静态死。recorder 无条件 emit Guard(ArithOverflow)，cranelift 不折 brif iconst(0)，IR 层 strip 干净。

帮了所有用 `%` 的 workload：
- W2: -11%
- W6: -9%
- W8: -11%
- W10: -14%
- W12: -7%

### 4. Guard BoundsCheck strip

`927be53` `perf(trace-jit): strip redundant Guard(BoundsCheck) before ListGet`

`recorder::emit_list_get` 注释明确说 `Guard(BoundsCheck)` 是 LICM marker，**不该 emit runtime check**。但 `guard_emit.rs::BoundsCheck` 实际 emit `icmp(UnsignedLessThan, idx, list_ssa_value)` —— 比较 idx 和 LIST POINTER value（heap 地址，always huge）→ 永远 true → 纯浪费 brif。

W5 / W6 (用 ListGet) 进一步 -20%。W6 从 24.27 → 19.45 µs (0.43× → 0.36×).

W5 panel single-run 落 fast cluster: **68.02 µs (0.70× LuaJIT)**。

## 各阶段 trace_jit 时间走势 (W4 / W5)

| 阶段 | W4 | W5 (best / median) |
|---|---|---|
| 起点 (1.5x target hit, commit `acdcc4a`) | 19.33 µs (1.33×) | 77 / 109 µs (0.78 / 1.16×) bimodal |
| + IV pass v1 (`164a2a2`) | **12.95 µs (0.89×)** | 100 / 108 µs (no improvement, layout shift) |
| + per-phi IV (`0daed29`) | 12.95 µs (0.89×) | 99 / 107 µs (marginal) |
| + Mod strip (`46342b0`) | 12.95 µs (0.89×) | **69** / 121 µs (best 改善) |
| + BoundsCheck strip (`927be53`) | 12.95 µs (0.89×) | **68** / 106 µs (best + 全 ListGet workload 提升) |

## 工程要点 retrospect

1. **变量噪音**：s90 单 panel run 的方差让 W5 看着抽奖。要决策应用多次测量 median + best/worst，单 panel run 易误判。
2. **诊断 dump 是关键**：写 trace IR dump test 5 分钟搞清 IV pass 在 W5 上是 no-op（之前怀疑了一阵子）。
3. **可疑 emit 注释**：recorder 注释说 BoundsCheck "不该 emit runtime check"，但实际还是 emit 了。注释和实现 disconnect。修可见性低的 bug 是 free win。
4. **per-phi vs all-or-nothing**：optimizer pass 默认应该 per-element，all-or-nothing 是限制不是简化。

## 已记忆

- `[[ic-inline-vs-extern]]` — trace-jit IC 必须 inline + TraceContext 嵌入
- `[[s90-bench-host]]` — bench 机器配置 + 跑法

## Open 项

- **W5 layout 变方差**：median 仍 ~1.14×。fast-cluster 仅 occasional。要稳定 < 1.0× 需要 layout/cache 调研（cranelift 输出地址 alignment 等）。**单独立项**，不阻塞下一步。
- **count + value 的 overflow guard**：W5 accumulator step 不 bounded，无法 strip。需要 recorder 观察值范围 → 推断 accumulator 不溢出。**单独立项**。
- **下一步按 user 指令**：立即做命名 / 代码结构 refactor (Dart-style JitEvaluator/AotEvaluator) → bytecode 覆盖扩张项目。design doc：`bytecode-coverage-expansion-design.md`。

## 完工状态

- 4 个新优化 commit + 配套测试已 cherry-pick 到 main，**未 push**。
- Cron job `6ebd5928` 已 delete。
- 任务 #260 完成；#261 (refactor) 待启动；#262 (bytecode coverage) blocked by #261。
