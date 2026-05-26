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

### Iteration 9 (2026-05-26 ~12:30, #264 完工)

**#264 (IR lowering surface) 完工** + cherry-pick 5 commits (`3a72486..8d457f6`)。
- W2/W3/W4/W6 source 从 n/a → OK (peephole + reduce + strict-mode drop)
- W5/W7/W8/W9/W10 scope-out (Dict return + user closures = multi-week)
- 三关 clean

Task #264 → completed。剩仅 #266 W5 RCA。

Release bench `cmp_lua-7cf7725a808de0d5` 编出，md5 `8b70640796d9fac1d4fc33f6f664f494`。**scp 到 s90 失败：host key changed** —— 等用户确认绕过。Bench 回归测试暂搁。

### Iteration 10 (2026-05-26 ~12:40)

- #266 W5 RCA: 595KB transcript, no commits yet (RCA 性质)
- s90 SSH host key 仍未处理，bench 回归暂搁
- 等

### Iteration 11 (2026-05-26 ~12:50)

s90 SSH host key 重生（s90 重启过），accept-new + 重建 ~/relon-bench-rt + `sudo apt-get install -y luajit` 都搞定。Memory 更新 (`s90-bench-host`)。
- #266 W5 RCA: 676KB transcript, no commits
- Panel `bj9pzsuhd` 跑了 7 min，预计还要 2-4 min

### Iteration 12 (2026-05-26 ~13:05, panel 完工)

Panel verdict:
- **10/10 trace_jit < 1.0× LuaJIT** ✓ 无回归（W5 0.88× lucky cluster）
- 新 bytecode row 4 个: W2 236µs / W3 2.37ms / W4 4.66ms / W6 1.97ms
- 新 `relon_jit` row 4 个 (W1/W2/W3/W4)，跟 bytecode 速度一致 → JitEvaluator 成功 escalate 到 bytecode tier
- 新 `relon_aot` row 1 个 (W1 19.9µs)
- W5/W7/W8/W9/W10 jit/aot/bytecode 仍 n/a（Dict + closure scope-out, per #264 design doc）

剩 #266 W5 RCA 仍跑（699KB transcript, no commits）。等。

### Iteration 13 (2026-05-26 ~13:15)

#266 transcript 713KB (+13KB)，无 commits。等。

### Iteration 14 (2026-05-26 ~13:25)

#266 transcript 723KB (+10KB)，无 commits。增长慢，可能在跑 perf stat 等。再给 1 iter，无进展则 SendMessage 询问。

### Iteration 15 (2026-05-26 ~13:35)

#266 transcript 745KB (+22KB)，仍无 commits。SendMessage 工具在本会话不可用，无法直接 ping。报状态给用户决定 (continue / inspect / stop)。

### Iteration 16 (2026-05-26 ~13:45)

#266 transcript 786KB (+41KB)，仍 alive。增长加速可能是分析阶段 → 写 doc 阶段过渡。等。

### Iteration 17 (2026-05-26 ~13:55)

#266 transcript 797KB (+11KB)。增长又放缓。等。

### Iteration 18 (2026-05-26 ~14:05)

#266 transcript 807KB (+10KB)。等。

### Iteration 19 (2026-05-26 ~14:15)

#266 transcript 843KB (+35KB)。仍 alive，无 commits。可能在跑大量 bench 收 variance 数据。

### Iteration 20 (2026-05-26 ~14:25)

#266 transcript 851KB (+8KB)。等。

### Iteration 21 (2026-05-26 ~14:35)

#266 transcript 894KB (+43KB)。增长加速，可能在 final 写 doc。

### Iteration 22 (2026-05-26 ~14:45)

#266 transcript 921KB (+27KB)。**`docs/internal/w5-variance-rca.md` 文件在 worktree 出现**！doc 写中，未 commit。
