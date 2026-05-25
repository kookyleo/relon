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

<!-- 后续 iteration 追加 -->
