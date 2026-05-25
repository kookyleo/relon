# Bytecode Coverage Expansion Loop (Phase C)

**Subagent**：`a3d1582c3cd6020da` worktree-isolated
**Design**：`docs/internal/bytecode-coverage-expansion-design.md` Phase B-1..B-4
**起点 commit**：`37c9ac2` (naming refactor cherry-pick 完工)
**预估工作量**：~3 周

## Phase 目标

- B-1: TraceOp ↔ BcOp 对齐
- B-2: analyzer 扩 surface (closure / list / dict / stdlib)
- B-3: deopt e2e 测试扩张 (W3/W5-shape)
- B-4: cmp_lua deopt_recovery panel row

## Iterations

### Iteration 7 (2026-05-26 ~00:03, Phase C 起点)

Subagent transcript 254KB（启动 ~5 min in），running。
等通知或进度。

### Iteration 8 (2026-05-26 ~00:16)

Subagent transcript 600KB (+346KB)，仍 active。HEAD `37c9ac2`，Phase B-1 探索阶段无 commit。等。

### Iteration 9 (2026-05-26 ~00:26)

Transcript 838KB (+238KB)。**Phase B-1 第一个 commit landed**：`04d5915 feat(bytecode): B-1 add StrContains / StrSubstring BcOps for trace deopt parity`。继续 Phase B-1。

### Iteration 10 (2026-05-26 ~00:37)

Transcript 1075KB (+237KB)。HEAD 仍 `04d5915`，Phase B-1 下一个 op 在做。等。

### Iteration 11 (2026-05-26 ~00:47)

Transcript 1268KB (+193KB)。**Phase B-2 commit**: `2645529 feat(bytecode): B-2 widen scalar envelope to accept String args / returns`。进展顺利，开始 analyzer 扩 surface 工作。

### Iteration 12 (2026-05-26 ~00:57)

Transcript 1416KB (+148KB)。**Phase B-3 commit**: `65bed46 feat(bytecode): B-3 string-shape dispatcher integration tests + string-aware re-pack on trace branch`。Subagent 进展快，B-1/B-2/B-3 in <30 min。等 Phase B-4。

### Iteration 13 (2026-05-26 ~01:07)

Transcript 1501KB (+85KB)。HEAD `65bed46` 不变，Phase B-4 (deopt panel rows) 在做。等。

### Iteration 14 (2026-05-26 ~01:17)

Transcript 1620KB (+118KB)。HEAD `65bed46` 不变，Phase B-4 持续。等。

### Iteration 15 (2026-05-26 ~01:25, Phase C subagent 完工)

Bytecode subagent **完工** + 5 commits 已 cherry-pick：
- `04d5915` Phase B-1 StrContains/StrSubstring BcOps
- `2645529` Phase B-2 widen scalar envelope for String I/O
- `c84c14b` Phase B-3 string-shape dispatcher integration tests
- `d2017ea` Phase B-4 deopt panel + 顺手修 from_ir_legacy 多参数 bug
- `39fbb79` docs status appended

三关验证：
- cargo build ✓
- test 0 failures ✓
- clippy ✓

**B-4 验收 gate (≥5× faster) 大幅满足**：bytecode resume 1.11 µs vs tree_walk 21.4 ms = **~19,000×** 加速。

**B-2 scope-out**：W2-W10 bytecode row 仍 n/a，原因是 IR lowering（`range()` / `list.X` import / Dict return type）不支持 —— 跨 crate 的 multi-week 工作，不属于 bytecode VM 范围。subagent 明确文档化。

下次 fire 跑 release bench 回归验证 + 看 deopt panel 数字是否在 panel 里。

<!-- 后续 iteration 追加 -->
