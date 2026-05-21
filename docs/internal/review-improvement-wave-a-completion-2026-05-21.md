# Review 改进 Wave A 完工报告

**完成日期**：2026-05-21

## 总览

Wave 1 (P0/P1/P2/P3 phase 1) 落地后 user 加派 Wave A 5 项并发推进。全部完工，无
correctness regression，所有 deferred 项有 stage report 蓝本。

## 完成清单

| ID | 项 | 状态 | LoC | tests |
|---|---|:---:|---:|:---:|
| #128 | P3-phase2: typecheck + infer split (全完) | ✓ | +305 (12 sub-files) | 0 |
| #129 | bytecode M2-B phase 2 (dispatch-time cap consult) | ✓ | +424/-35 | +4 |
| #130 | codegen-native ConstPool OpVisitor + 5 sub-file | ✓ | +2510/-1824 | +3 |
| #131 | trace JIT side-table + pass ordering doc | ✓ | +266/-27 (pure doc) | 0 |
| #132 | 小清扫批 (parser / object-link / fmt / wasm32 const-assert) | ✓ (3/4 active, 1 no-op) | +47/-1 | +2 |

**Tests 2007 → 2038** (+31)。整体 +3552 / -1887。每 phase 全 5 gate 过（fmt /
clippy / test / wasm32 / corpus three_way）。

## 详细落地

### #128 P3-phase2: typecheck + infer 完整拆

P3-phase1 deferred 的 `typecheck.rs` (5765) + `infer.rs` (1808)，本 phase 完整
拆完，超出 scope 预期。

- typecheck/ **9 sub-file**：mod 491 / helpers 369 / index 241 / fn_call 520 /
  spread 382 / pattern 223 / binary 129 / reference 287 / typed_binding 303 /
  tests 3093
- infer/ **3 sub-file**：mod 1088 / walk 433 / tests 319
- Pattern：`impl<'a> super::Walker<'a>` extension-impl，sub-file 直接 touch 父
  struct 私有 field，零 trait 抽象，零 runtime overhead
- 280+ typecheck integration test + 30 infer assertion + stdlib_index_consistency
  全绿

### #129 bytecode M2-B phase 2

phase 1 dormant hook → phase 2 dispatch-time consult。M2-A scaffold 无
capability-sensitive op，所以 scope 调整为 mechanism + helper API。

3 enforcement point：
1. Dispatch-time pre-check（invoke_from_with_stack 遍历 vtable.grants，denial
   → `BcVmError::CapabilityDenied { steps=0 }`）
2. Trap-site enrichment（Trap(CapabilityDenied) 用 `first_denied_bit` 替换
   `u32::MAX` sentinel）
3. 5 helper fn (`consult_capability_gate / consult_gate / consult_all_granted_bits
   / first_denied_bit / decode_cap_bit`) 供 phase 3 lowering 接入

**诚实记录**：from_source scalar path 仍 no-op，phase 3 IR coverage 后通过
helper 自动激活 per-call-site enforcement。

### #130 codegen-native ConstPool OpVisitor + 5 sub-file

**ConstPool OpVisitor PoC**：完整 `impl OpVisitor for ConstPool`。Const-bearing
variant 有体；其它 Ok(()) no-op——加新 Op fail compile 不再被旧 `_ => {}` 吞。
3 byte-identity test 钉布局不变。

**5 sub-file 抽**（mod.rs 3458 → 1906，-45%）：
- `control_flow.rs` 626 LoC
- `record.rs` 254 LoC
- `closure.rs` 199 LoC
- `call.rs` 285 LoC
- `field.rs` 376 LoC

ConstPool 字节相同验证：W3/W5 cmp_lua_consistency 全过。

**Phase 3 蓝本**：build pipeline (~700 LoC, risky) / HotCounter prologue
(~115 LoC, easy) / Codegen emit_op dispatch (~450 LoC, trunk) 待 emit_op arm
变 one-liner 后切 OpVisitor。

### #131 trace JIT side-table + pass ordering doc

Pure doc/refactor +266/-27，**零 code 改**。

1. **Side-table contract**：TraceBuffer head doc 覆盖 5 side-table
   (type_info / consts / const_bytes / str_payload / dict_entry_count_hints) +
   5 共享 invariant (SSA-keyed, recorder 单写者, optimiser 不分新 SSA, 残留 key
   无害, guards 是 trace_pc-anchored 例外)。每 field 每 record_* accessor 加
   回指。OptimizedTrace 3 doc-less field 同步。

2. **Pass ordering invariants**：optimizer/mod.rs 顶 doc 记 8 pass 依赖（2x
   DeadStoreElim / DictIcHoist BEFORE LICM / NoopTypeCheckElim AFTER LICM）。
   7 pass 文件各加 `## Ordering` section cross-link。

3. **dict/str inline pattern**：module-doc only。helper 提取（InlineDecisionHelper
   <T>）评估后拒绝——dict key u32 vs str key &[u8] 签名异，abstraction 退化
   two-shell-per-type。

### #132 小清扫批

- **(a) parser**：`pub mod lower` → `pub(crate) mod lower`（grep 验证无 out-of-crate
  consumer）+ doc 说明
- **(b) object-link**：cfg-gate 已在 base，no-op
- **(c) fmt**：2 unit test（minified idempotency + Error::Parse surface）
- **(d) trace-jit wasm32**：`STRING_REF_LEN_OFFSET=8` 64-bit hard-code；wasm32
  usize=4B → `#[cfg(target_pointer_width = "64")]` gate const-assert（trace JIT
  不在 wasm32 跑，契约只需 64-bit hold）

## 累计成果（Wave 1 + Wave A）

完工列表（按 ID）：
- ✓ #121 P0-A OpVisitor trait（72 variant + bytecode 接入）
- ✓ #122 P0-B CapabilityGate trait 统一
- ✓ #123 P1-A codegen.rs 拆 5 sub-file (Wave 1)
- ✓ #124 P1-B bytecode M2-B phase 1 (hook dormant)
- ✓ #125 P2-A trace-emitter sync lint
- ✓ #126 P2-B facade re-export 缩减 + EvaluatorBuilder
- ✓ #127 P3 phase 1 Unicode + stdlib 拆
- ✓ #128 P3-phase2 typecheck + infer 拆
- ✓ #129 bytecode M2-B phase 2 (dispatch-time consult)
- ✓ #130 codegen ConstPool OpVisitor + 5 续抽 sub-file
- ✓ #131 trace JIT doc 收口
- ✓ #132 小清扫批

**Tests** 2007 → 2038 (+31)，**架构债务**全部从 review 列单上销账，**deferred
follow-up** 全有 stage report blueprint。

## Deferred (follow-up backlog)

- **bytecode M2-B phase 3**：BcOp::CallNative + IR coverage expansion (list /
  dict / string / stdlib op 全表)
- **bytecode M2-B phase 4**：trace JIT hot counter injection + deopt resume
  via ir_pc_map
- **codegen 续抽 phase 3**：build pipeline (~700) / HotCounter prologue
  (~115) / Codegen emit_op 切 OpVisitor 后整理 dispatch
- **AOT > JIT 失效场景 bench**：构造 trace-JIT abort / 高 deopt / 冷启动
  fixture（cmp_lua 当前未覆盖）
- **dispatch_cranelift_step 415 ns 边界优化**（call setup / vtable lookup）

## 引用

stage reports（按 phase 顺序）：
- `docs/internal/review-improvement-p0-a-op-visitor-2026-05-21.md`
- `docs/internal/review-improvement-p0-b-capability-2026-05-21.md`
- `docs/internal/review-improvement-p1-a-codegen-split-2026-05-21.md`
- `docs/internal/review-improvement-p1-b-bytecode-rfc-phase1-2026-05-21.md`
- `docs/internal/review-improvement-p2-a-inline-sync-2026-05-21.md`
- `docs/internal/review-improvement-p2-b-facade-2026-05-21.md`
- `docs/internal/review-improvement-p3-large-file-split-2026-05-21.md`
- `docs/internal/review-improvement-p3-phase2-typecheck-split-2026-05-21.md`
- `docs/internal/review-improvement-129-bytecode-phase2-2026-05-21.md`
- `docs/internal/review-improvement-130-codegen-continued-2026-05-21.md`
- `docs/internal/review-improvement-131-trace-jit-doc-2026-05-21.md`
- `docs/internal/review-improvement-132-small-cleanup-2026-05-21.md`

RFC：
- `docs/internal/rfc-m2-b-bytecode-jit-integration-2026-05-21.md`

Completion 文档：
- `docs/internal/review-improvement-completion-2026-05-21.md` (Wave 1)
- `docs/internal/review-improvement-wave-a-completion-2026-05-21.md` (本文档)
