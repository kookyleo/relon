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

<!-- 后续 iteration 追加 -->
