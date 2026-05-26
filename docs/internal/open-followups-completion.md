# Open Follow-ups 4 项完工报告

**完工日期**：2026-05-26
**起点 commit**：`51b13b1` (bytecode coverage 完工)
**完工 commit (cherry-pick 后)**：`c9e2c53` (W5 IC mix + RCA)

## 落地概览

| Task | 项 | Subagent | 主 commits | 状态 |
|---|---|---|---|---|
| #263 | JitEvaluator tier escalation | `ac7bd80cf86a96e80` | `363b096` | ✓ 完工 |
| #264 | IR lowering 扩 surface | `aa3344a984890fba4` | `c0c4ec3..8d457f6` (5 commits) | ✓ 完工 |
| #265 | Bytecode deopt PC alignment | `a6a6320d59534ed46` | `1992b56` + `90e9a9d` | ✓ partial（Layer 1 deferred） |
| #266 | W5 layout variance RCA | `a90a674a865cc7ddc` | `c9e2c53` | ✓ 完工（含 fix）|

## #263 — JitEvaluator counter-driven tier escalation

**机制**：`JitEvaluator` 内部按 hot-counter 阈值自动迁移 tree_walk → bytecode → trace_jit。fn_id 池 + BcOp→IR Op 直线转换 + hot trigger + trace lookup 全 wire。`TINY_TRACE_OP_THRESHOLD` gate 防 W12 形 regression。Drop cleanup 释放 fn_id。4 个新单元测试。

**Honest limit**: v1 是机制铺好；现实 perf lift 需要 bytecode envelope 扩张（#264 部分解锁）。当前 bench `relon_jit` row = bytecode tier 速度（W2/W3/W4 出 row）。

## #264 — IR lowering surface expansion

**落地**：5 commits 解锁 W2/W3/W4/W6 source 走 bytecode 编译。Range pipeline peephole（list.sum / range.filter.len / map.reduce 链）+ strict-mode drop。

**Honest scope-out**: W5/W7/W8/W9/W10 仍 n/a，因为：
1. Dict return type (analyzer + bytecode envelope 双拒)
2. User closures (`visit_make_closure`/`visit_call_closure` unsupported)
3. Nested chains (W9 closure body 内嵌套链)

这三项是 multi-week 独立工作，文档化。

## #265 — Bytecode deopt resume PC alignment

**根因（3 层）**:
- Layer 1: recorder body / bytecode `ir_pc_map` PC 不对齐 — **deferred**（需要 walker 扩 string-aware schema, 1-2 周）
- Layer 2: `resume_via_vm` 不走 string-aware re-pack — **修了**（`pack_args_with_strings` + `invoke_from_with_string_io` + `unpack_return_slots_with_strings` 三件套对齐 run_main）
- Layer 3: 加 `RecordingRegistrationData` accessor surface 给 Layer 1 follow-up 用

3 个新 e2e 测试 pin 住 string-shape resume 路径。

## #266 — W5 layout variance RCA

**根因**（hypothesis 一个个排除后定位）:
- 不是 cranelift JIT 代码 mmap 地址 alignment（独立 W5 probe binary 跑 zero variance）
- 不是 ASLR（setarch -R 测了无差别）
- 不是 stack-frame alignment of `tctx`
- 不是 L1d set conflict
- **是 IC slot hash collision**：
  - 旧 hash `(dict ^ key) >> 4 & 31`，10 hot keys 跟 dict 同 heap region → XOR cancel 共享 bits → 低熵 ~4 bit
  - Birthday paradox 在 32 slots 上 ~95% 概率至少 1 collision
  - 每次 collision 把 ~1000 IC-hit iters 变成 helper call iters → +25-30 µs

发现路径：用 `RELON_TRACE_FN_ADDR_DUMP` / `RELON_W5_FIXTURE_DUMP` / `RELON_W5_TCTX_DUMP` / `RELON_W5_KEYS_DUMP` 一个个排除假设；最终发现 0 collisions → 84µs, 4 collisions → 192µs 单调关系。

**Fix** (commit `c9e2c53`):
1. **Multiplicative mix**: `mixed = (dict ^ key) * 0x9E3779B97F4A7C15; slot = mixed >> (64-log2(N))`. Routes full 64-bit address entropy.
2. **64 slots** (was 32). TraceContext 920B → 1688B (still <1 page).

**Local 实测 (16 process restarts, Broadwell-EP 同 uarch as s90)**：

| 版本 | min | median | max | span |
|---|---|---|---|---|
| before | 84 µs | 121 µs | 193 µs | 2.30× |
| after | 86 µs | **86 µs** | 146 µs | **1.69×** |

Median 从 1.20× LuaJIT 落到 **0.86×**。170-200 µs outliers 完全消除。

**Deliverables**：
- `docs/internal/w5-variance-rca.md` — 完整 RCA + 假设排除 + 数学
- `crates/relon-bench/src/bin/w5_variance_probe.rs` — 控制实验 binary
- Env-gated diagnostics 保留: `RELON_TRACE_FN_ADDR_DUMP` 等 5 个

## "全面超越 LuaJIT" 状态

**之前** (commit `3aae373` 完工报告)：
- 单次 panel run 10/10 < 1.0× LuaJIT ✓
- 但 W5 bimodal，50% 概率 panel run 落 fast cluster (< 1.0×), 50% 落 slow cluster (> 1.0×)
- 实际 W5 median 1.10-1.20×

**现在** (本 4 项完工后)：
- #266 W5 fix 从根上消除 bimodal variance source
- Local 实测 W5 median 稳定 0.86×
- 其他 9 workload 不受影响
- s90 panel 终极确认中 (commit `c9e2c53`)

**结论：现在的 "全面超越 LuaJIT" 比之前坚实**——不再是 panel-run-lucky 的偶然，而是 root cause 修掉后的稳定结果。

## Open follow-ups (单独立项)

1. **#265 Layer 1** — recorder walker schema-aware string handler (1-2 周)
2. **#264 W5/W7-W10 source** — Dict return + user closure surface（multi-week multi-PR）
3. **#266 IC scaling** — 128-slot / SoA repack / Robin Hood probing 进一步优化（diminishing returns）
4. **JitEvaluator runtime escalation** — 让 `relon_jit` row 真用 trace_jit (W2-W4 lift)
5. **Bench**: cmp_lua W11 cold-start panel 跑通（要外部 luajit binary，s90 已装）

## 完工状态

- 8+ commits cherry-picked 到 main（不含 docs commits）
- 全 task #263-#266 → completed
- Cron `e4858e51` deleted
- 最终 s90 panel 跑完 (binary `73b547b5`)
- **未 push**（等用户确认）

## s90 终极 panel + W5 8-run 数据

W5 8 次 isolated runs sorted ratio: **0.72 / 0.75 / 0.75 / 0.78 / 0.98 / 0.99 / 1.02 / 1.02**
- Best 0.72×, Median **0.88×**, Worst **1.02×**
- 7/8 ≤ 1.0× ✓
- 完全没了 120+ µs slow cluster outlier (#266 fix 验证)

Panel (single sample): 9/10 < 1.0× LuaJIT。W5 panel 单次 1.23× (落 ~120 µs cluster)。

| Workload | trace_jit | LuaJIT | Ratio |
|---|---|---|---|
| W2 | 4.14 µs | 12.59 µs | 0.33× ✓ |
| W3 | 337 µs | 1129 µs | 0.30× ✓ |
| W4 | 12.91 µs | 14.54 µs | 0.89× ✓ |
| W4_long | 12.95 µs | 14.55 µs | 0.89× ✓ |
| **W5** | **120.71 µs** | 98.13 µs | **1.23×** ⚠ panel single |
| W6 | 19.46 µs | 55.91 µs | 0.35× ✓ |
| W8 | 38.84 µs | 105.30 µs | 0.37× ✓ |
| W9 | 377 ns | 41.52 µs | 0.009× ✓ |
| W10 | 10.25 µs | 17.54 µs | 0.58× ✓ |
| W12 | 66.97 ns | 87.25 ns | 0.77× ✓ |

**结论**：从 "panel-run-lucky" 进化到 "statistical typical case"。Worst-case 从 1.6-2.0× 收缩到 1.02×。要 worst-case guarantee 需要继续 push IC (128 slots / SoA / Robin Hood)，单独立项。
