# Open Follow-up Items Monitoring Loop

**Cron**: `e4858e51` (10 min, :05/:15/:25/:35/:45/:55)
**起点 commit**: `51b13b1` (bytecode coverage 完工 doc)
**Tasks**:
- #263 JitEvaluator tier escalation (subagent `ac7bd80cf86a96e80`)
- #264 IR lowering surface expansion (subagent `aa3344a984890fba4`)
- #265 Deopt PC alignment fix (subagent `a6a6320d59534ed46`)
- #266 W5 variance RCA (subagent `a90a674a865cc7ddc`)

## Iterations

### Iteration 0 (2026-05-26 ~01:55, 起点)

4 subagents spawned, all in_progress。Mapping 在 `docs/internal/.openitem-subagent-map`。等 task-notification 或 cron fire。

### Iteration 1 (2026-05-26 ~02:05)

4 subagents transcript: #263=335KB / #264=386KB / #265=421KB / #266=222KB。全在探索阶段，无 commits。等。

### Iteration 2 (2026-05-26 ~02:15)

Transcripts: #263=579KB (+244KB) / #264=640KB (+254KB) / #265=745KB (+324KB) / #266=273KB (+51KB)。无 commits 落地，全在 active 工作。#266 最慢（RCA 性质，预期）。

### Iteration 3 (2026-05-26 ~02:25)

Transcripts: #263=676KB / #264=820KB / #265=923KB / #266=293KB。**#264 第一个 commit landed**: `3a72486 feat(ir): peephole-inline list.sum(range(...).map(...)) chain`。其他仍在做。

### Iteration 4 (2026-05-26 ~11:35, #265 完工)

**#265 (Deopt PC alignment fix) 完工** + cherry-pick `1992b56` (fix) + `90e9a9d` (test/docs)。
- Layer 2 (resume_via_vm string-aware) + Layer 3 (RecordingRegistrationData accessor) 落地
- Layer 1 (recorder walker schema-aware string handlers) scope-out 到 follow-up
- 3 新 e2e tests 全过；三关 clean

Task #265 → completed。剩 #263/#264/#266 in_progress。

### Iteration 5 (2026-05-26 ~11:45)

剩 3 subagents 进度：
- #263 JitEvaluator: 785KB, no commits yet (still exploring)
- #264 IR lowering: 980KB, **3 commits landed** (peephole sum+map / filter+len / drop strict-mode + tests)
- #266 W5 RCA: 426KB, no commits (RCA 性质，doc 工作)

等。

### Iteration 6 (2026-05-26 ~11:55)

- #263: 848KB, no commits
- #264: 1168KB, **第 4 个 commit**: `d0255b5 feat(ir): add reduce() consumer to range-pipeline peephole`
- #266: 500KB, no commits

#264 进展最快。

### Iteration 7 (2026-05-26 ~12:05)

- #263: **第 1 commit landed**: `6d96d4b feat(jit): auto-escalate JitEvaluator through hot-counter trace tier`
- #264: 1192KB, 仍 4 commits 不变
- #266: 550KB, 仍 no commits

等。

### Iteration 8 (2026-05-26 ~12:15, #263 完工)

**#263 (JitEvaluator tier escalation) 完工** + cherry-pick `363b096`。三关全过。
- fn_id 池 + BcOp→IR Op 直线转换 + hot trigger / trace lookup wire
- TINY_TRACE_OP_THRESHOLD gate 防 W12 形 regression
- Honest scope-out: 真 perf lift 等 #264 surface 扩到位

Task #263 → completed。剩 #264/#266 in_progress。Bench 留 #264 完工后一次性回归。
