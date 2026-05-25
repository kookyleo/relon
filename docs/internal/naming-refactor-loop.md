# Naming Refactor + Bytecode Coverage Monitoring Loop

**Phase A**：Dart-style 二分法命名 (refactor subagent `ae8a91962d73213ca`)
**Phase B/C**：Bytecode 覆盖扩张 (refactor 完工后 spawn 新 subagent + 监控)
**Cron 周期**：10 min (cron `82195b35`，:03/:13/:23/:33/:43/:53)
**起点 commit**：`3aae373` (full supersession 完工报告 push)
**Refactor design**：`docs/internal/bytecode-coverage-expansion-design.md` "Naming alignment" 章节
**Bytecode design**：同上 Phase B-1..B-4
**任务追踪**：#261 (refactor) + #262 (bytecode)

## 当前状态

- Full supersession 已完工 + push (commit `3aae373`)
- 10/10 trace-jit-applicable workload < 1.0× LuaJIT
- Refactor subagent 实施中，预估 1-2 天

## Iterations

### Iteration 0 (2026-05-25 ~22:45, 起点)

- Subagent spawned (3-step: rename / JitEvaluator wrapper / bench row)
- Cron `bb9157dd` armed
- 等 task-notification

<!-- 后续 iteration 追加 -->
