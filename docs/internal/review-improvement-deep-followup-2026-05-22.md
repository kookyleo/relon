# Deep Follow-up 完工 report — #168 + #169

**完成日期**：2026-05-22

## 总览

两 deep follow-up 推完。一个 full code land (#168)，一个 honest Plan C audit-only (#169)。

**Tests 2258 → 2282 (+24)**。

## 完成情况

### #168 TraceOp::StrConcatN inline emit ✅ FULL CODE

trace-JIT 不再对 Op::StrConcatN sticky abort。Hot loop chain ≥ 3 string concat 进 trace-JIT inline path 而非 fallback cranelift AOT。

**Scope**：
- `TraceOp::StrConcatN { dst, operands: Vec<SsaVar> }` effect_class = Pure
- recorder lowering 移 abort，emit TraceOp + N NotNull guards
- MAX_INLINE_STR_CONCAT_N = 4，N<3 / N>4 abort 独立 diagnostic
- emitter `emit_str_concat_n` cranelift IR: sum total_len + stack-spill operand ptr table + single call `__relon_str_concat_n_alloc` + seal_hash (Tier 1b)
- runtime helper `__relon_str_concat_n_alloc`: single [header|payload] block + sequential memcpy
- HostHookId / HostHookFuncIds / trace_install / register_trace_runtime_symbols 全链路 wiring
- trace-recording walker `step_str_concat_n` 避 catch-all over-pop

**Tests**: 24 new (2 variant + 7 helper + 8 recorder + 3 emitter + 4 e2e 含 32-iter hot loop)。

**Honest deferred**:
- N > 4 recorder abort → cranelift AOT 单 alloc 路径接管（per task spec acceptable）
- inline_emit host-fn inline path 仍 CallNotSupportedInInline，与 sibling StrConcat / StrContains 对齐

**Commit**: `4d4af16`, 7 commit split (refactor → feat → test → docs)

### #169 ConstString wire migration ⚠️ HONEST PLAN C (AUDIT ONLY)

agent 经审计判 atomic 4B → 12B wire flip **超 safe single-phase budget**，选 Plan C 仅交付 audit 不动 code。

**Why not done**:
- 55 个 `Op::ConstI32(4)` site 分布 3 files：
  - `defs.rs` (13) — 全 string-payload 可清 flip
  - `case_fold.rs` (14) — 混合 string-payload + CP-buffer header
  - `normalization.rs` (26+) — densest mix，CP-buffer 和 String record 共字面量 `4` 但含义不同
- 任何 misclassification = **silent corruption**（per #140 / #159 / #164 教训）
- 6 surface 需协调：const_pool + buffer (decode_pointer_header 与 List 共享需先拆) + record EmitTailRecordFromAbsoluteAddr + field emit_read_string_len mask + stdlib bodies + ET_REL cache version bump
- 缺 wire_format_smoke test（corpus_differential 只看答案不查 wire bytes，misclass MatchOk 可能携错字节）

**Plan C deliverable**：
- `docs/internal/review-improvement-169-conststring-wire-full-2026-05-22.md` 289 行 stage report：
  - per-site 审计 (53 stdlib + 6 surface 全分类)
  - 5 blocker 列表 (normalization.rs per-line, wire_format_smoke 缺, cache-version 政策, ListString/ListSchema 未覆盖, wasm32 backend 未审计)
  - 6-commit next-wave 蓝本顺序（constant → split decode helpers → wire_format_smoke gate → atomic flip → cache bump）
- corpus_differential baseline preserved: 0 mismatch (60 cases / 55 match_ok / 4 match_trap / 1 unsupported)

**Future recommendation**: 后续若推 wire flip，按 6-commit 蓝本顺序 + 先加 wire_format_smoke gatekeeper test 才进 atomic flip。

## 关键诚实记录

### #168 cap 4 决策

N > 4 fallback abort 是 deliberate choice per task spec。Reason:
- unroll N >> 4 makes inline emit 复杂 (per-operand cranelift IR 步骤多)
- N > 4 string concat chain 罕见
- cranelift AOT 已有 #163 single-alloc 路径，fallback 不退化 perf

### #169 Plan C 选择

agent 没硬塞 partial wire flip 是**正确判断**。前述 4 次 incident 教训:
- #140 cwd-drift incident
- #159 14 mismatch 报警
- #164 wire flip 第一次回退
- 任何 partial wire migration 都可能 silent corruption

agent 用 Plan C 留下: (1) bench 0 regression (2) audit 文档 6-commit 路线图。

## 服务事故

期间 5 次 Anthropic 服务 529 overload 早死（~215s, 0 tool use, 单次失败）：
- 第 1 次 #168 + #169 派工，2 个都 529
- 第 2 次 retry，2 个都 529
- 第 3 次 retry，2 个都 529
- 第 4 次 retry，**#169 成功完成**，#168 又 529
- 第 5 次 retry #168，**成功完成**

总耗时被 server overload 拉长 ~1 小时。最终都跑通。

## Cumulative 6-wave 累计

| Wave | 任务数 | tests | LoC |
|---|:---:|:---:|---:|
| 1 (P0-P3) | 7 | +22 | +4622/-1043 |
| A | 5 | +31 | +3552/-1887 |
| B+C | 13 | +84 | +10,000+/-330+ |
| D | 11 | +66 | +10,000+/-900+ |
| Z (中 ROI) | 8 | +11 | +4079/-279 |
| **deep** | **2** | **+24** | **+1223/-16 + audit doc** |
| **总计** | **46** | **+238** | **+33,000+/-4500+** |

**Tests 1907 → 2282 (+375)**。

## 剩余 backlog

按 ROI 排:

### 短期可做
- 无（高/中 ROI 已全完）

### 中期等条件
- **ConstString wire migration (full atomic flip)** — 6-commit 蓝本已留，先加 wire_format_smoke test → atomic flip → cache bump
- **Heap-path SmolStr ASCII bit caching** (#162 deferred)
- **stdlib normalize NFx inline fast path** (#158 deferred)
- **stdlib StringReplace/StringJoin working-buffer** (#158 deferred)

### 长期 / 等需求 / RFC-class
- **Lever 4 direct-threaded dispatch** — wait Rust stable `become`
- **Option E 双 variant 编译** — wait HFT-class user demand
- **W11 musl static-link** — RFC API change
- **W4 trace framework loop overhead redesign** — large rewrite
- **W7 fib recorder 支持非-tail recursion** — recorder design
- **Cranelift 0.133+ retry b99f2b4 IR sentinel-skip** — wait upstream loop-opt heuristic relax
- **wasmtime backend 替换 cranelift-jit** — 大重构

### 基础设施
- **CI perf regression baseline tracking** — automated criterion baseline
- **Bench self-hosted runner** — W11 cold start 严格 quiescent

## 结论

中 ROI follow-up 8 项 (Wave Z) + 深 follow-up 2 项 (本 phase) 全告一段落。
- 进行中: 0
- 已规划 deferred 待长期条件: ~17 项
- Tests baseline: **2282 / 0 fail**
- 主线性能目标 × 1.5 大幅超
- bytecode VM: M2-B 完整 + M2-C lever 1+2+3+5 + M3 phase 2
- trace-JIT: 5 string lever + StrConcatN inline 全套

性能优化"可见 ROI 项"全完。后续要再推进需触发外部条件（用户 workload 新需求 / Rust stable feature / cranelift upstream）。
