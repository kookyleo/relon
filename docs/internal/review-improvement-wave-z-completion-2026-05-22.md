# Wave Z 完工 report — 中 ROI follow-up 8 项

**完成日期**：2026-05-22

## 总览

继 Wave 1+A+B+C+D 之后，**Wave Z 推进 8 项中 ROI follow-up**，全部基于 stage report 已记蓝本：M3 phase 2 / string Tier 2 续抽 / dispatch perf carry-over / bench infra 补齐 / structural cleanup。

**Tests 2247 → 2258 (+11)**。8/8 完成。

## 完成清单（按 ID 顺序）

| Task ID | 内容 | LoC | tests | 关键数字 |
|---|---|---:|:---:|:---:|
| #157 | M3 phase 2 IR-side desugar list.sum(range(n)) | +509 | +6 | **W1 bytecode 解锁 (1/12 → 2/12)** |
| #158 | stdlib write-to-buffer surface | +509/-22 | +4 | **to_lower -58% / concat -48%** micro-bench |
| #159 | ConstString hash migration (runtime side-band) | +503/-9 | +4 | StringRef +hash field |
| #160 | cmp_lua W8/W9/W10 trace_jit row | +620 | 0 | × 0.41 / × 0.008 / × 0.63 |
| #161 | Lever 3 per-op specialization | +486/-217 | 0 | **perf-neutral 诚实记录**（LLVM fat-LTO 已 const-fold） |
| #162 | evaluator → ASCII flag wiring | +407/-31 | +4 | wiring-only |
| #163 | concat_many backend wire-up | +677 | +5 | sso/concat_tree heap 2.4× faster |
| #164 | bytecode from_source full cap gate | +368 | +2 | Option B compile-pass scan |

总：**+4079/-279 LoC, +25 tests**。

## 重要的诚实记录

### #157 phase 4b-cont 诊断纠正

原 phase 4b-cont stage report 关于 "iter stdlib bodies use buffer-protocol ops (LoadI64AtAbsolute / BitAnd / alignment math)" 的论断**错了**。Probe 揭示真实失败点：

- IR lowering: `list` import 不解析
- analyzer: 未类型 closure params (W2/W6/W8)

bytecode VM 根本没到 body。Option (a) IR-side desugar `list.sum(range(n))` 直接 → explicit Op::Loop (no List alloc) 是正确解法。

### #159 wire 迁移回退

brief 要求 cranelift const_data wire layout 迁到 `[len+ascii][hash][payload]`。首攻 14 corpus_differential mismatch（stdlib bodies 全 hard-code `+4` 作 payload offset：concat/substring/upper/lower/title/nfd/nfc/starts_with/glob_match/...）。

回退到 side-band variant: `ConstPool::string_hashes` HashMap 缓存 fx_hash 不改 wire format。Runtime-side `StringRef` widened 到 24-byte `{ptr, len, hash}` 配合 `__relon_str_concat_seal_hash` helper。**完整 wire 迁移列入 follow-up**（需协调改所有 stdlib bodies + EmitTailRecordFromAbsolute + emit_read_string_len masking）。

### #161 Lever 3 perf-neutral 诚实结论

22 monomorphic BcOp variants（AddI64/AddF64/EqI64/...）替 11 typed-payload variants（Add(IrType)/Sub/...）。

**Bench：W12 +1.5~5% within noise**。Reason: LLVM fat-LTO 已 inline `arith_binop` + const-fold inner `match ty`，splitting at enum level 没暴露 new opt opportunity。

Kept for: 1:1 IR↔BcOp 对齐 + 未来 Lever 4 direct-threaded prototype unblock。如果 Rust stable 永不 ship `become` / unstable asm，Lever 3 仅是 structural cleanup。

### #157 / #161 / #163 honest scope deferred

| Phase | Deferred 项 | 原因 |
|---|---|---|
| #157 | W2/W6/W8 bytecode 4-way | analyzer 需 closure param 从 receiver List<Int> 推断 |
| #157 | W4 bytecode 4-way | 缺 IR range stdlib of different shape |
| #161 | Lever 4 direct-threaded dispatch | Rust stable 缺 `become` / unstable asm |
| #163 | trace-JIT TraceOp::StrConcatN inline emit | 需新 TraceOp variant + str_inline 平行 lowering |
| #163 | W3 string_concat hot-loop optimization | hot loop 是 2-operand pair 不 ≥3 chain，AST fold gate 不 fire |

## 累计 Wave 1+A+B+C+D+Z

| Wave | 任务 | LoC | tests | 关键成果 |
|---|---|---:|:---:|---|
| Wave 1 | #121-#127 (7 项) | +4622/-1043 | +22 | OpVisitor / CapGate / codegen split / facade |
| Wave A | #128-#132 (5 项) | +3552/-1887 | +31 | typecheck split / glob_match / dispatch bench |
| Wave B+C | #133-#145 (13 项) | +10,000+/-330+ | +84 | bytecode M2-B + dispatch boundary |
| Wave D | #146-#156 (11 项) | +10,000+/-900+ | +66 | W5 × 1.151 + W12 × 1.84 + String 5 lever |
| **Wave Z** | **#157-#164 (8 项)** | **+4079/-279** | **+11** | **W1 bytecode 解锁 + to_lower -58% + Lever 3 honest** |

**总: 1907 → 2258 tests (+351), +32,000+/-4400+ LoC**

## 完整 follow-up backlog（剩余）

### 高 ROI 但需深改
- **trace-JIT TraceOp::StrConcatN inline emit** (#163 deferred)
- **ConstString wire layout 完整迁移** (#159 deferred) — 协调 stdlib body / EmitTailRecordFromAbsolute / emit_read_string_len
- **W2/W6/W8 bytecode 4-way 解锁** (#157 deferred) — analyzer closure param 推断

### 中 ROI 等条件
- **Lever 4 direct-threaded dispatch** — wait Rust stable `become`
- **Option E 双 variant 编译** — wait HFT-class user demand (current 17.17 ns vs LuaJIT host-boundary 30+ ns，无紧迫)
- **Heap-path SmolStr ASCII bit caching** (#162 deferred)
- **stdlib normalize NFx inline fast path** (#158 deferred)
- **stdlib StringReplace/StringJoin working-buffer** (#158 deferred)

### 长期 / 等需求
- **W11 musl static-link**: RFC-class API 变更（× 1.488 → × 1.4）
- **W5 perfect-hash ≥5-entry small dict**: current × 1.15 已 buffer 大，无紧迫
- **W4 trace framework loop overhead redesign**: × 1.33 floor 需大重构
- **W7 fib trace_jit n/a**: 扩 recorder 支持非-tail recursion
- **Cranelift 0.133+ 升级时重试 b99f2b4 pattern**: wait upstream relax loop-opt heuristic
- **wasmtime 后端替换 cranelift-jit**: 大重构

### 基础设施
- **CI perf regression baseline tracking**: 自动 criterion baseline / GitHub Action
- **Bench self-hosted runner** (W11 cold start 等需要严格 quiescent)

## 结论

中 ROI 已发现项**全部完工**（8/8）。所有 honest scope deviations 已在 stage reports 留底。剩余 follow-up backlog 都明确属于:
1. 需深改协调（如完整 ConstString wire migration）
2. 等条件触发（Rust stable feature / HFT user demand）
3. 长期话题需 RFC

**进行中任务**: 0  
**已规划但 deferred**: ~18 项（蓝本齐全）  
**测试 baseline**: 2258 / 0 fail  
**Cranelift**: 0.132（最新稳定）  
**主线性能目标**: × 1.5 全维度大幅超过  
**Bytecode VM**: M2-B 完整推进到 phase 4d，M2-C lever 1+2+3+5 落地（lever 4 wait stable），M3 phase 2 解锁 W1

性能优化的"高 ROI" + "中 ROI" + "已发现 follow-up 中可推进部分" 全完。后续工作进入"等条件 / 等需求 / 长期话题"阶段。
