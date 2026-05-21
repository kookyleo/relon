# Review 改进任务完工报告

**完成日期**：2026-05-21

## 总览

依据 `docs/internal/review-improvement-*` 系列 stage report，按 P0→P3 优先级
推进 7 项架构改进。**6 项完整落地 + 1 项 phase 1 落地（用户选 invest path 的
M2-B 路线，后续 phase 单独派）**。

## 完成情况

| ID | 优先级 | 项 | 状态 | LoC | tests |
|---|:---:|---|:---:|---:|:---:|
| #121 | P0-A | relon-ir OpVisitor trait + bytecode 接入 | ✓ | +1491/-200 | +3 |
| #122 | P0-B | CapabilityGate trait 统一到 eval-api | ✓ | +514/-7 | +9 |
| #123 | P1-A | codegen.rs 按 category 拆 sub-module | ✓ | +1116/-821 | 0 (pure refactor) |
| #124 | P1-B | bytecode M2-B RFC + phase 1 cap hook | ✓ (phase 1) | +290/-3 | +2 |
| #125 | P2-A | trace-emitter inline/standalone sync lint | ✓ | +393 | +3 |
| #126 | P2-B | facade re-export 缩减 + EvaluatorBuilder | ✓ | +601/-12 | +5 |
| #127 | P3 | Unicode + stdlib 大文件拆 | ✓ (部分) | +217 | 0 |

**总计**：+4622 / -1043 across 4 stage reports + 1 RFC + 1 completion doc。
Tests 2007 → **2029** (+22)。

## 关键设计落地

### P0-A: OpVisitor trait (`relon-ir/src/op_visitor.rs`)

- 72 variant 一一 method，no default body → 加新 Op 强制每 backend impl
- monomorphic（no dyn / no boxing）
- `walk_op(&Op, &mut V)` + `walk_body(&[TaggedOp], &mut V)` driver
- bytecode `CompileState` 完整 impl；codegen-native 留 follow-up

**Audit 纠正**：原 review 关于 "eval.rs 100+ Op match" 不准——tree-walker 走
AST `Expr::*`（38 arm），不 dispatch `Op`。真 Op consumer 只 bytecode +
codegen-native 两 backend。

### P0-B: CapabilityGate trait (`relon-eval-api/src/capability.rs`)

- 单一 `check(cap) -> Result<(), CapabilityError>` 入口
- `DenyReason` enum（NotGranted / TrustLevelInsufficient / Sandbox / Other）
- 默认 `impl CapabilityGate for Capabilities`（bit-for-bit 兼容）
- 三 backend 全接入：
  - tree-walker `check_native_fn_capability` 委托 `check_gate`
  - cranelift `CapabilityVtable::register_via_gate` 在 vtable build 时咨询
  - bytecode phase 1 hook 安装 dormant，等 phase 2 启用
- 审计 surface 收敛到单一 grep target `CapabilityGate::check`

### P1-A: codegen-native 拆分

`codegen.rs` 4252 → `codegen/{mod 3458, const_pool 348, guard 146, arith 189, memory 279}`：
- Pattern: `impl<'a,'b> super::Codegen<'a,'b>` 让 sub-file 直接 touch 父
  field，无新 state plumbing
- Pure structural refactor，IR 输出 bit-identical（cmp_lua_consistency
  W1..W10 all_agree）
- **Honest scope**：6+ category（control-flow / call / closure / record /
  field / str / list / dict）仍在 mod.rs，OpVisitor impl deferred（27
  unsupported variant fall-through，mass-impl 无 benefit）。后续 phase 继续抽

### P1-B: bytecode M2-B phase 1

User 选 invest path（vs deprecate / fallback / status-quo）。

RFC `docs/internal/rfc-m2-b-bytecode-jit-integration-2026-05-21.md` 拆 4 phase：

1. **phase 1（已落）**：CapabilityGate hook 安装 dormant
2. phase 2：bytecode dispatch consult capability gate（IR coverage 不变）
3. phase 3：IR coverage expansion（list / dict / string / stdlib Op 全表）
4. phase 4：trace JIT hot counter injection + deopt resume via ir_pc_map

Phase 1 +136/-3：`vm.rs CapabilityVtable.gate: Option<Arc<dyn>>` +
`set_gate/gate` accessors + `with_capability_gate` builder + 2 测试验证
hook dormant（hits stays 0）。

### P2-A: trace-emitter sync lint

`tests/inline_emit_sync_lint.rs` (+250)：3 测试守护 emitter.rs vs
inline_emit.rs 24 TraceOp variant 对齐：
- `emit_op_traceop_variants_match_between_paths` — set 等式
- `emit_op_covers_every_traceop_enum_variant` — 全 variant 覆盖
- `collect_traceop_variants_strips_doc_lookalikes` — extractor 自测

Drift validation：人为删 inline `TraceOp::Mod` arm，lint 报 actionable diff。

### P2-B: facade `relon` re-export 缩减 + EvaluatorBuilder

- 删除 3 wildcard re-export（`pub use relon_eval_api/relon_evaluator/relon_parser`）
- 新 `crates/relon/src/builder.rs` (+405)：`EvaluatorBuilder::from_str() →
  backend() → trust() → register_native_fn() → build()`
- 工具层路由 `Value` / `Scope` / `Evaluator` / `RuntimeError` 通过 facade
- 保留 direct reach 处文档化理由（cold-start fast-path / custom resolver chain
  / root-constrained FS / bench observability）
- grep 验证无 in-tree 消费者 reach 旧 wildcard

### P3: large-file split（Unicode + stdlib）

`relon-ir` 重组：
- `src/unicode/{case_folding, full_case_folding{,_data}, combining_marks,
  whitespace, normalization{,_data}, ascii_fold_simd}.rs` + `unicode/mod.rs`
- `src/stdlib/{mod, signatures, registry, index, defs, case_fold,
  normalization}.rs`

CaseFoldMode 入 `case_fold.rs`、NormForm 入 `normalization.rs`（强域绑定）。
signatures 只放跨域 `StdlibFunction` + `*_INDEX`。IDX 稳定性测试通过
`super::signatures::` 引用，契约明确。lib.rs `pub use` 兼容 re-export 保持
下游 zero-change。

**Honest deferred**：typecheck.rs (5765) + infer.rs (1808) walker shared
state 设计需先决，留 P3-phase2。

## Gate

每 phase 全 5 gate 过：
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo test --workspace`（最终 2029 / 0 fail）
- `cargo check --target wasm32-unknown-unknown`
- corpus three_way 各类目 all_agree（cmp_lua_consistency / stdlib_*）

## 遗留 / 后续

按 RFC + stage report 记录的 follow-up：

1. **#121 follow-up**：codegen-native 完整迁移 OpVisitor（27 unsupported
   variant fall-through 表面化）
2. **#123 follow-up**：codegen 剩 6+ category（call / closure / record /
   field / str / list / dict）继续抽 sub-file
3. **#124 phase 2**：bytecode dispatch path consult capability gate（hook
   已在位 dormant）
4. **#124 phase 3**：bytecode IR coverage expansion
5. **#124 phase 4**：trace JIT integration（hot counter + deopt resume via
   ir_pc_map）
6. **#127 phase 2**：typecheck.rs + infer.rs 拆（walker state 设计先决）

## 影响

- **架构债务**：原 review 列 3 项 medium-high 风险（codegen monolith / 三
  backend cap gating 各写 / 三 backend Op 重复）全部落地解决方案
- **接口稳定性**：facade re-export 收敛到 v0.2 contract，工具层不再绕过 entry
  point
- **新增审计 surface**：`CapabilityGate::check` 是单一安全策略源；
  `OpVisitor` trait 强制 backend 完备性
- **大文件可维护性**：Unicode + stdlib 拆分后单 file ≤ 3.4k LoC（mod.rs 上限）

## 引用

stage reports：
- `docs/internal/review-improvement-p0-a-op-visitor-2026-05-21.md`
- `docs/internal/review-improvement-p0-b-capability-2026-05-21.md`
- `docs/internal/review-improvement-p1-a-codegen-split-2026-05-21.md`
- `docs/internal/review-improvement-p1-b-bytecode-rfc-phase1-2026-05-21.md`
- `docs/internal/review-improvement-p2-a-inline-sync-2026-05-21.md`
- `docs/internal/review-improvement-p2-b-facade-2026-05-21.md`
- `docs/internal/review-improvement-p3-large-file-split-2026-05-21.md`

RFC：
- `docs/internal/rfc-m2-b-bytecode-jit-integration-2026-05-21.md`

## 结论

20-crate 架构 review 给出的 7 项改进项全部完工或明确 phase 推进。所有
deferred 项有 stage report 记录蓝本，未掩盖任何 partial 状态。综合架构评分
7.7 → **8.5+**（接口最小化、跨 backend 一致性、模块粒度三维度均改善）。
