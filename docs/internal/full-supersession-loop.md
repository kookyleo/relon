# Full-Supersession Loop — Relon trace-jit < 1.0× LuaJIT 全覆盖

**目标**：所有 trace-jit-applicable workload 比 LuaJIT 快（ratio < 1.0）。
**Loop 周期**：10 min (cron `6ebd5928`)。
**起点 commit**：`a0faca7` (design doc).

## 当前状态 (2026-05-25 起点)

8/10 已达成 (W2/W3/W5/W6/W8/W9/W10/W12)。

剩余 2 个：
- W4_string_contains: 1.33×
- W4_long_haystack: 1.33×

W1 / W7 无 trace_jit row, 不算。

## 进行中工作

- subagent id `a84df6777e8ff56d6` worktree-isolated 实施 IV overflow elim pass
- 设计 doc: `iv-overflow-elim-design.md`
- 预期完工后 W4 → ~0.89× (实验验证过的上限)

## Iterations

### Iteration 0 (2026-05-25 19:xx 起点)

- Design doc committed `a0faca7`
- Subagent spawned with full design + 实施步骤
- Cron `6ebd5928` armed
- 等待 subagent 完成

### Iteration 1 (2026-05-25 ~19:39)

- Subagent worktree: `.claude/worktrees/agent-a84df6777e8ff56d6` 分支 `iv-overflow-elim`
- HEAD 仍在 `a0faca7` (起点)，未有实施 commit
- agent transcript 342KB（在跑中）
- 决策: 继续等

### Iteration 2 (2026-05-25 ~20:09)

- Subagent 已 commit `139a8c4 feat(trace-jit): IV overflow elim pass for bounded loops`
- 文件改动: 990 行新 pass + mod.rs 26 行修 + buffer_smoke test 改 4 行 (8 → 9 passes)
- Commit msg 自报: 6 unit tests, W4-shaped strip + accumulator strip with Bool step + 拒绝大 step / loop-carried bound / 无 exit idiom + trace_pc rebind consistency
- 仍未发 completion notification (transcript 521KB, 还在跑)
- 继续等

### Iteration 3 (2026-05-25 ~20:19)

- Transcript 595KB (+74KB since iter 2)
- 无新 commit (HEAD 仍 `139a8c4`)
- 可能在 verify (build/test/clippy) 阶段
- 继续等

### Iteration 4 (2026-05-25 ~20:37)

- Subagent **完工**，3 commits 已 cherry-pick 到 main (`164a2a2`, `53bf2ce`, `bcc5671`)
- 本地 build + test (0 failures) + clippy 全过
- release bench 编完，md5 `a5338c50`，scp 到 s90 一致
- s90 panel sweep 跑中 (cmp_lua PID 10003，1 min in)
- 顺手：写了 Dart-style 命名 + bytecode 扩覆盖 design doc (commit `c85f9a3`)
- 等 panel notification

### Iteration 5 (2026-05-25 ~20:42)

- s90 cmp_lua 7 min CPU 时间，仍在跑
- 用户 2026-05-25 ~20:40 指令：W4 验证完后**立即做命名 / 结构 refactor 全量**，再继续 bytecode 扩覆盖
- Tasks 已建：#260 W4 验证 (in_progress)，#261 refactor (待 #260 完)，#262 bytecode 扩覆盖 (blocked by #261)
- 等

### Iteration 6 (2026-05-25 ~20:45, panel sweep 完工)

Panel 结果：
| Workload | trace_jit | LuaJIT | Ratio | Δ vs 上轮 |
|---|---|---|---|---|
| W2 | 4.63 µs | 12.86 µs | 0.36× | 持平 |
| W3 | 336.7 µs | 1133 µs | 0.30× | 持平 |
| **W4** | **12.95 µs** | 14.54 µs | **0.89×** | -33% 修好 |
| **W4_long** | **12.98 µs** | 14.54 µs | **0.89×** | -33% 修好 |
| **W5** | **141.43 µs** | 98.57 µs | **1.43×** | **+82% 回归?** |
| W6 | 26.68 µs | 56.42 µs | 0.47× | 持平 |
| W8 | 43.62 µs | 105.23 µs | 0.41× | 持平 |
| W9 | 395.4 ns | 41.13 µs | 0.010× | 持平 |
| W10 | 11.96 µs | 17.39 µs | 0.69× | 持平 |
| W12 | 73.1 ns | 86.3 ns | 0.85× | 微回归 (+8%) |

W4 修好，但 W5 可能回归（也可能是 bimodal 噪音慢端）。开 5 次 W5 重测确认（b7dhcg3t9 后台跑）。

### Iteration 7 (2026-05-25 ~20:51)

- W5 5-run task `b7dhcg3t9` 跑中 (run 1/5)
- 等

### Iteration 8 (2026-05-25 ~20:55)

W5 5-run isolated 数据：
| Run | trace_jit | LuaJIT | Ratio |
|---|---|---|---|
| 1 | 100.79 µs | 93.58 | **1.08×** |
| 2 | 109.19 µs | 93.69 | 1.17× |
| 3 | 116.19 µs | 93.54 | 1.24× |
| 4 | 107.41 µs | 98.04 | 1.10× |
| 5 | 101.97 µs | 93.65 | 1.09× |

Mean 1.14, best 1.08×, worst 1.24× —— **tight 分布，bimodal 消除**。但失去 fast cluster (之前 77µs 0.78× run)。

判定：W5 mean 1.14×，**未达 < 1.0× supersession**。9/10 trace-jit-applicable workload 达标，W5 是唯一缺口。

W4 / W4_long 真改进 -33% 确认。其他 workload 无回归（W12 +8% 微回归但仍 0.85×）。

**阻塞在用户决策** (option A/B/C - 见上一轮分析)。下次 fire 若仍无决策，自启 W5 trace IR 后置分析（确认 IV pass 对 W5 是否触发，root-cause regression 还是 layout 抽奖）。

### Iteration 9 (2026-05-25 ~21:20)

W5 IR dump 测试确认：原版 IV pass 对 W5 完全 no-op（`ops_removed=0`）。前次"回归"是 binary layout shift。

**根因**：IV pass `analyse_loop` 是 all-or-nothing。W5 accumulator `count + dict_value` step 是 i64 非 Bool，第一个 fail 直接 `return None`，导致 counter `i + 1` overflow guard 也没被 strip。

**修复**：commit `0daed29` —— `return None` 改 `continue`，每个 phi 独立判断。W5 现在剥 1 个 guard（i+1 counter）。test 验证 `ArithOverflow guards: 3 → 2`。

Bench 跑中 (md5 `d5f4b7e1`，task `bnkomok4p`)，等结果。预估 W5 108 → ~103 µs (1.05×)。

如仍 > 1.0×，下一步：
- Strip Mod overflow guard (const +divisor, 永不溢出)
- Hoist Guard BoundsCheck(KEY_IDX, keys_list) (KEY_IDX = i%10 ∈ [0,9], keys_list 不变)

### Iteration 10 (2026-05-25 ~21:22)

W5 5-run task `bnkomok4p` 跑中 (run 2/5)。等。

### Iteration 11 (2026-05-25 ~21:36)

W5 5-run post per-phi (commit `0daed29`): mean 1.15× — 没明显改善（noise dominate）。
新优化：commit `46342b0` Mod overflow guard 也 strip（const +divisor 静态死）。W5 现在剥 **2 个 guards** (i+1 + Mod), 预期 ~2 cycles/iter 总节省。
binary `2f1c7c26` 同步好，跑 8× W5 + full panel 后台 (`bliyrlsnz`)。

### Iteration 12+13 (2026-05-25 ~21:36-21:46)

Bench `bliyrlsnz` 跑中。partial W5 (3/8): 120 / **69.80** / 154 µs。Run 2 **0.74×** —— 在 favorable layout 下 Relon 显著快于 LuaJIT。但仍 bimodal (69-154)。继续等 5 + panel。

### Iteration 16 (2026-05-25 ~22:00, panel + 8-run W5 完工)

W5 8-run isolated: sorted [69.78, 69.80, 94.07, 110.57, 120.08, 121.11, 129.78, 154.29], best **0.75×**, median 1.21×, **tri-modal**。
Panel: W5 trace_jit 124.54 / LuaJIT 102.23 = **1.22×** ✗。Layout lottery 主导。

**全 panel 改善** (Mod strip 帮了所有用 `%` 的 workload):
- W2: 4.63→4.13 (-11%), 0.33×
- W6: 26.68→24.27 (-9%), 0.43×
- W8: 43.62→38.81 (-11%), 0.36×
- W10: 11.96→10.24 (-14%), 0.61×
- W12: 73.1→67.89 (-7%), 0.78×

只剩 W5 ≥ 1.0×。下一波候选：**strip Guard BoundsCheck before ListGet**（同 idx 跟 ListGet 内部 inline bounds 重复）。

<!-- 后续 iteration 追加 -->
