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

### Iteration 1 (2026-05-25 ~23:25, Phase A)

Subagent transcript 468KB。2/3 commits 已 land：
- `7710e56` refactor(codegen-native): rename CraneliftAotEvaluator to AotEvaluator
- `d65d9ba` feat(relon): add JitEvaluator canonical entry for Dart-style split

bench row 应该在做中。仍 running。继续等。

### Iteration 2 (2026-05-25 ~23:35, Phase A 完工审计)

Refactor subagent 完工。Cherry-pick 3 commits 到 main: `9e009be` rename / `ba76d6d` JitEvaluator / `37c9ac2` bench rows。
- `cargo build --workspace` ✓
- `cargo test --workspace --exclude relon-bench --exclude relon-wasm` 0 failures ✓
- `cargo clippy --workspace --all-targets -- -D warnings` ✓
- `cargo bench --no-run` ✓

Release binary `29e86dd9` synced 到 s90，全 panel 跑中 (`bzg1kc37f`)。

### Iteration 5 (2026-05-25 ~23:54, Phase A panel 部分)

Panel partial W1-W6 done: 全 trace_jit < 1.0× LuaJIT。W5 90.63 / 103.05 = **0.88×** (fast cluster)。等 W7-W12 + 新 relon_jit / relon_aot row。

### Iteration 6 (2026-05-26 ~00:00, Phase A 完工 + Phase B 启动)

Panel 全跑完。Verdict:
- 10/10 trace-jit-applicable workload < 1.0× LuaJIT ✓ (无回归)
- 新 `relon_jit` row 存在但当前 = tree_walk 速度 (v1 wrapper 没 tier escalate)
- 新 `relon_aot` row 全 n/a (codegen-native 不覆盖，per design)

任务 #261 → completed。完工报告: `docs/internal/naming-refactor-completion.md`。

**Phase B 启动**: bytecode 扩覆盖 subagent `a3d1582c3cd6020da` spawned。worktree-isolated，预估 ~3 周。subagent ID 写到 `docs/internal/.bytecode-subagent-id`。任务 #262 → in_progress。

下次 fire 进入 Phase C 监控。

<!-- 后续 iteration 追加 -->
