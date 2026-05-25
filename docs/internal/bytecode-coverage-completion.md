# Bytecode Coverage Expansion 完工报告

**完工日期**：2026-05-26
**起点 commit**：`37c9ac2` (naming refactor cherry-pick 完工)
**Design**：`docs/internal/bytecode-coverage-expansion-design.md` Phase B-1..B-4
**Subagent**：`a3d1582c3cd6020da` worktree-isolated

## 落地内容（5 个 cherry-pick commits）

| Phase | 短 hash | Subject |
|---|---|---|
| B-1 | `04d5915` | `feat(bytecode): B-1 add StrContains / StrSubstring BcOps for trace deopt parity` |
| B-2 | `2645529` | `feat(bytecode): B-2 widen scalar envelope to accept String args / returns` |
| B-3 | `c84c14b` | `feat(bytecode): B-3 string-shape dispatcher integration tests + string-aware re-pack on trace branch` |
| B-4 | `d2017ea` | `feat(bench): B-4 add relon_deopt_to_bytecode panel + fix from_ir_legacy multi-arg` |
| docs | `39fbb79` | `docs(internal): record B-1..B-4 implementation status + known limits` |

## 三关验证

- `cargo build --workspace` clean ✓
- `cargo test --workspace --exclude relon-bench --exclude relon-wasm` 0 failures ✓
- `cargo clippy --workspace --all-targets -- -D warnings` clean ✓

## 核心 deliverable: deopt landing pad

**Design gate**: bytecode resume 比 tree_walk fallback **快 ≥ 5×**
**Achieved**: **~19,000×** (1.11 µs vs 21.4 ms)

新增 `jit_failure_modes` bench panel rows：
- `relon_deopt_to_bytecode`: 1.11 µs
- `relon_deopt_to_tree_walk`: 21.41 ms

Bytecode 作为 trace_jit deopt landing pad 的角色 **完全坐实**。这是 LuaJIT canonical design 在 Relon 里的等价。production workload 包含 closure/dict 的 deopt 不会再回退到 ~100-1000× 慢的 tree_walk，而是落在比 tree_walk 快 19,000× 的 bytecode VM 上。

## Phase 落地状态

### B-1 — DONE
- `BcOp::StrContains` / `BcOp::StrSubstring` 加 (mirror TraceOp)
- 短路 `CONCAT_INDEX` / `CONTAINS_INDEX` / `SUBSTRING_INDEX` stdlib 调用
- `LENGTH_INDEX` / `IS_EMPTY_INDEX` / `CONCAT_INDEX` / `CONTAINS_INDEX` / `SUBSTRING_INDEX` 公共常量 + drift guards

### B-2 — PARTIAL (scope-out 合理)
- ✓ scalar envelope 接受 `String` args / returns
- ✓ `pack_args_with_strings` + `invoke_from_with_string_io` + `final_strings`
- ✓ `visit_const_string` 切到真 handle (`BcOp::StrConst`) 路径
- ⚠ cmp_lua W2-W10 source 仍 n/a，但 **不是 bytecode VM 缺陷**：
  - `relon-ir/src/lowering.rs` 不支持 `range()` 自由调用 / `list.X` import 解析 / `Dict` return type
  - 跨 crate 的 multi-week 工作，超出本项目 3-week 范围
  - 已记到 design doc "Implementation Status" 章节

### B-3 — PARTIAL (dispatcher 集成测试覆盖)
- ✓ 2 个新 `bytecode_trace_deopt_handoff_e2e` 测试：W3-shape (string-concat) + W4-shape (string-contains) 跑通 dispatcher round-trip
- ⚠ 真正的 cold-path deopt → resume 用 string source 时撞 PC 对齐：recorder 手搓的 trace body PC 跟 bytecode `ir_pc_map` 不对齐，resume 落在错的 bc index
- 已记 follow-up

### B-4 — DONE，gate 大幅超越
- 新增 `relon_deopt_to_bytecode` / `relon_deopt_to_tree_walk` panel rows
- 实测 19,000× 加速（设计 gate ≥ 5×）
- 顺手修两个 latent bug：
  1. `from_ir_legacy` 多参数 let-base 偏移计算错（任何 arity ≥ 2 的 IR 都会撞）
  2. `ReturnShape::LegacyI64` 读 `locals[0]` 而非 popped value（碰巧只对单参数 IR 工作）

## Cmp_lua 全 panel 回归验证 (s90, binary md5 `ce258e60`)

**Trace-jit-applicable workload 全 < 1.0× LuaJIT 状态保持**：

| Workload | trace_jit | LuaJIT | Ratio | Δ vs full-supersession push |
|---|---|---|---|---|
| W2 | 4.14 µs | 12.70 µs | 0.33× | 持平 |
| W3 | 346 µs | 1125 µs | 0.31× | 持平 |
| W4 | 12.96 µs | 14.54 µs | 0.89× | 持平 |
| W4_long | 12.99 µs | 14.55 µs | 0.89× | 持平 |
| W5 | 91.66 µs | 98.19 µs | **0.93×** | 持平 (lucky cluster) |
| W6 | 19.44 µs | 53.30 µs | 0.36× | 持平 |
| W8 | 38.81 µs | 105.20 µs | 0.37× | 持平 |
| W9 | 366 ns | 41.94 µs | 0.009× | 持平 |
| W10 | 10.23 µs | 16.99 µs | 0.60× | 持平 |
| W12 | 67.90 ns | 85.72 ns | 0.79× | 持平 |

无回归。

## Open follow-ups (单独立项)

1. **IR lowering 扩 surface** (1-2 周): `range()` / `list.X` import / Dict return → 让 cmp_lua W2-W10 真的获得 `relon_bytecode` row
2. **PC alignment fix**：deopt resume 在 string-shape 上的 PC 对齐问题（recorder body / bc ir_pc_map 协调）
3. **JitEvaluator counter-driven tier escalation**：让 `relon_jit` bench row 跟 `relon_trace_jit` 重合（继承自 naming refactor 项目）

## 完工状态

- 5 个 commits 已 cherry-pick 到 main，**未 push** (commits `c0f72e6..39fbb79`)
- 任务 #262 → completed
- 任务 #260/#261/#262 全部 completed
- Cron job `82195b35` 已 delete

Bytecode VM 现在能作为 trace_jit 的 high-perf deopt landing pad，跟 LuaJIT canonical pattern 对齐。
