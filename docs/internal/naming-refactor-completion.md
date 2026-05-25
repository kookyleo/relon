# Naming Refactor 完工报告 — Dart-style JIT/AOT 二分法

**完工日期**：2026-05-26
**起点 commit**：`3aae373` (full supersession 完工报告 push)
**Refactor design**：`docs/internal/bytecode-coverage-expansion-design.md` Naming alignment 章节
**Subagent**：`ae8a91962d73213ca` worktree-isolated

## 落地内容

### 3 个 commits

| 短 hash (cherry-picked) | Subject |
|---|---|
| `9e009be` | `refactor(codegen-native): rename CraneliftAotEvaluator to AotEvaluator` |
| `ba76d6d` | `feat(relon): add JitEvaluator canonical entry for Dart-style split` |
| `37c9ac2` | `feat(bench): add relon_jit + relon_aot rows to cmp_lua panel` |

### Type rename

- `CraneliftAotEvaluator` → `AotEvaluator`（drop `Cranelift` impl-detail prefix）
- 136 处 callsite sed-replaced（32 个 `.rs` / `.toml` 文件）
- 保留 `#[deprecated]` 别名 `pub use evaluator::AotEvaluator as CraneliftAotEvaluator;` —— 1-2 season 防破坏外部 user

### `JitEvaluator` Dart-style 顶层

- 新文件 `crates/relon/src/jit.rs` (~280 lines)
- `pub struct JitEvaluator` wraps tree_walk + bytecode
- `pub enum JitTier { TreeWalk, Bytecode, Trace }` 预留 variant
- v1 wrapper: bytecode 优先，fallback tree_walker，**未实现 counter-driven tier escalation**（per directive "第一版仅 wrapper"，留 follow-up）
- `crates/relon/src/lib.rs` re-export `JitEvaluator` / `JitTier` / `AotEvaluator`（cfg feature 桥接保持 wasm 不引入 cranelift）

### Bench panel

- `crates/relon-bench/benches/cmp_lua.rs` Dart-style panel loop：12 workload × `relon_jit` + `relon_aot` 双 row
- 现有 `relon_tree_walk` / `relon_bytecode` / `relon_trace_jit` row 全保留作 tier breakdown
- AOT row 默认 n/a（codegen-native 当前不覆盖所有 workload，per design 接受）

## Bench 回归 panel (s90, taskset -c 2, --sample-size 50 --measurement-time 8)

**Supersession 状态**: 10/10 trace-jit-applicable workload < 1.0× LuaJIT ✓ 无回归

| Workload | trace_jit | LuaJIT | Ratio (旧 row) | relon_jit (新 row) |
|---|---|---|---|---|
| W1 | n/a | 14.5 µs | — | 1.230 ms (≈ tree_walk) |
| W2 | 4.14 µs | 12.81 µs | **0.32×** | 3.40 ms (≈ tree_walk) |
| W3 | 334 µs | 1.12 ms | **0.30×** | 5.78 ms (≈ tree_walk) |
| W4 | 12.96 µs | 14.54 µs | **0.89×** | 34.92 ms (≈ tree_walk) |
| W4_long | 12.98 µs | 14.55 µs | **0.89×** | 35.95 ms (≈ tree_walk) |
| W5 | 90.63 µs | 103.05 µs | **0.88×** | 48.34 ms (≈ tree_walk) |
| W6 | 19.44 µs | 54.63 µs | **0.36×** | 30.13 ms (≈ tree_walk) |
| W7 | n/a | 913 µs | — | 131.5 ms (≈ tree_walk) |
| W8 | 38.79 µs | 107.62 µs | **0.36×** | 51.17 ms (≈ tree_walk) |
| W9 | 382 ns | 42.93 µs | **0.009×** | 6.30 ms (≈ tree_walk) |
| W10 | 10.23 µs | 17.16 µs | **0.60×** | 4.57 ms (≈ tree_walk) |
| W12 | 67.89 ns | 110.31 ns | **0.62×** | 489.88 ns (?) |

注：
- `relon_jit` row 当前 = tree_walk 速度（v1 wrapper 没 escalate 到 trace_jit）。**这不是 bug，是 v1 设计**。Follow-up 项目要加 counter-driven tier escalation。
- `relon_aot` row 全 n/a（codegen-native 不覆盖这些 workload，per design）。
- W12 relon_jit 489.88 ns 比 tree_walk 1.29 µs 快 2.6×；可能 W12 short-trace path 走了 bytecode tier（M2-A scalar envelope 覆盖 `x + 1`）。

## Open follow-ups

1. **JitEvaluator counter-driven tier escalation** — v1 wrapper 用 bytecode 优先 + tree_walker fallback，trace tier 自动 wire 但缺顶层调度。要让 `relon_jit` row 跟 `relon_trace_jit` 重合。
2. **`relon_aot` row 跑通** — 需要 codegen-native 扩到能 compile 更多 workload。跟 bytecode 扩张 (#262) 部分重叠。
3. **`Backend::CraneliftAot` 枚举未改名** — directive 没要求，单独 deprecation 周期再处理。

## 下一阶段

**Bytecode 覆盖扩张 (#262)** 立项。详细方案 `docs/internal/bytecode-coverage-expansion-design.md` Phase B-1..B-4。

完工状态:
- 3 个 commits 已 cherry-pick 到 main，**未 push**（等用户确认）
- 任务 #261 → completed
- 任务 #262 → in_progress (启动 bytecode subagent)
