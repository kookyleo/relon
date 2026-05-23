# P0 + P1 + P2 Wave 完工 report

**完成日期**：2026-05-23  
**触发**：用户 "趁还没有历史包袱至少先把 p0 p1 p2 搞掉吧" + 后续 /loop 10m 自动节拍  
**Loop 停止条件**：累计 ≥ 3 项 skip (P1-3/16/19/20 + P2-17/19 全是 deferred 大 refactor 或 bench-gated)

## 完成

### P0 (6 项 — bug-class)

| ID | commit | 说明 |
|---|---|---|
| P0-6 | `2e223e7` | const_fold Mod 被 effect-class predicate 静默屏蔽 — 真 perf bug 修 |
| P0-1 | `9661d77` | in_method_block stub doc 对齐 + dead key/node drop |
| P0-2 | `a210e2e` | LoopMarker arm 也 push open_loops，两条 emit MarkLoopHead 路径 bookkeeping 一致 |
| P0-3 | `001b7c5` | LoadField/StoreField BoundsCheck(base, base) → NotNull(base)（前者 emitter 上 base<base = 总 false 永远 deopt） |
| P0-3 follow-up | `b62b973` | emit_guard skip 普化到所有 NONE-payload guard (不仅 BoundsCheck) |
| P0-4 | `0415c6f` | emit_* API 也 bump external_pc，与 record_op 对齐 PC 计数 |
| P0-5 | `623dff2` | cli --lite/--backend conflict gate 集中到一处 |

### P1 (10 项落地 / 4 项 deferred)

| ID | commit | 说明 |
|---|---|---|
| P1-8 | `174e7b6` | cst.rs 私有 DirectiveShape 副本删，用 directive.rs canonical |
| P1-18 | `b48321d` | TraceBuffer::rebind_guard_pcs helper，licm/type_spec 共用 |
| P1-10 | `7879eea` | trace_install 13 host hook declares → table-drive |
| P1-14 | `989e036` | try_compare_op_method 与 try_arith_op_method 合并 |
| P1-15 | `20e64ab` | Iter 协议 stringly-typed 集中到 iter_protocol 常量 |
| P1-21 | `ec8aa67` | apply_stack_effect 225 行 mega-match → pop_n + push_snapshot helper |
| P1-13 | `39c3131` | try_call_schema_method 拆 static / receiver dispatch 两 helper |
| P1-9 | `ffc1a45` | 3 legacy entry sites unify (check_legacy_entry_shape + pack_legacy_argv_by_name) |
| P1-22 | `6793c47` | cli main 680 行 → cmd_run hoist (后续 phase 续拆留 follow-up) |
| P1-6 | `58e138c` | lower_type_node 5+ 层嵌套 → attach_enum_variant_fields helper |
| P1-5 | `95f81ed` | lower_schema_method method_start scan → method_name_offset helper |
| P1-4 | `dcaee87` | lower_dict_field dead state 清扫 (trivia scan dedup + dead bool) |
| P1-11 | `47b48ef` | CodegenMode enum 替 captures_ptr/lambda_param_tys implicit 2-state |

**Deferred** (单独 session 评估)：

- **P1-1** NodeIndexer Arc<Node> — 跨 parser/analyzer API 大改
- **P1-2** rayon parallel analyze — 跨 crate state coord
- **P1-3** 双 typecheck dedup — analyze_with_options API 改造，跨 LSP/CLI/wasm 入口
- **P1-7** source.rs ↔ lex.rs 双 lexer 合并 — relon-fmt API change
- **P1-12** emitter ↔ inline_emit 600 行 emit_* dedup — 大重构 + lint 守护重写
- **P1-16** reference.rs step_into_value 3 处 — 行为微妙差异 (error text / display_path)
- **P1-17** TraceOp variant style 统一 — 大 IR API 改
- **P1-19** invoke_pooled_typed_i64 dispatch dedup — W12 hot path，需 bench 验证 inline 边界
- **P1-20** compile_inline_one vs OpVisitor — 行为耦合，inline-aware 处理差异
- **P1-22** phase 续拆 (backend dispatch 4 arms → 4 helper fn) — cmd_run 已 hoist，进一步拆收益递减

### P2 (2 项纯结构化落地 / 19 项 bench-gated 或大重构 deferred)

| ID | commit | 说明 |
|---|---|---|
| P2-8 | `2b00d03` | BcVmConfig Arc-wrap，per-call HashMap clone → refcount bump |
| P2-18 | `2e2e461` + `c35950c` + `7ddb9ed` | Scope.path_node / current_dir / cache_namespace String → Arc<str>，跨 6 crate + 12 test fixture |

**Deferred**：

- **P2-17** ValueDict.map BTreeMap<String,_> → SmolStr — 271 .map.* 操作位点跨 crate，serde wire 兼容性风险
- **P2-19** OffsetTable cache for relocate_pointers — recursive 需 RelocCtx 设计 + mutually-recursive fn 签名改
- **P2-1/2/3/4/5** evaluator hot path Vec/clone — 需 bench 验证 (closure body deep clone / List O(N²) thunks / caps.call_relon Evaluator / (String,String) HashMap key / check_type SchemaData clone)
- **P2-6/7/9/10/11/12/13** bytecode + codegen-native + trace-jit perf items — bench-gated
- **P2-14/15/16** trace-recorder perf — bench-gated
- **P2-20/21** analyzer perf — bench-gated

### Formalization backlog (用户后续 ask 加入)

- **F-1** Miri CI sweep on unsafe modules (1d, 高 ROI)
- **F-2** Kani BMC JIT str/dict helpers (2-3d, 中-高)
- **F-3** Capability/sandbox TLA+ spec (1-2w, 中)
- **F-4** Trace JIT deopt invariant prop-test (3-5d, 中-高)
- **F-5** ConstString wire-format smoke gate (1d, 中)

doc: `docs/internal/formalization-targets-2026-05-23.md`

## 累计

| Wave | Commits | 净 delta | tests |
|---|---:|---|:---:|
| Simplify 20-crate | 11 | +284 / -816 | 2330 |
| Tier A 收敛 | 3 | +28 / -129 | 2330 |
| CI fix + roadmap doc | 2 | +418 / -1 | 2330 |
| **本 wave (P0+P1+P2 + loop)** | **26** | **+~600 / -~1200** | **2330** |
| **累计 since 2026-05-22** | **42** | **+~1330 / -~2150** | **2330** |

Loop 期间 commits: `2e223e7..7ddb9ed`，全部 push 至 origin/main。

## Gate

每 commit 通过：
- `cargo test --workspace`: 2330 / 0 fail
- `cargo clippy --workspace --all-targets -- -D warnings`
- `cargo fmt --all --check`
- (intermittent) `cargo check -p relon-wasm --target wasm32-unknown-unknown`

CI: SEC-2 wave 修过的 `corrupted_ir_cache_invalidates_pair` linker-guard 仍在 (commit `a53cb39`)，预期当前 main 上 CI 全绿。

## 关键观察

1. **bug-class P0 全清**：6 项里 4 项是真 bug (Mod fold silently 屏蔽 / BoundsCheck 总 deopt / PC 不一致 / LoopMarker book stack 漏 push)。其余 2 项是 doc 不对齐 + cli 决策分散。
2. **state machine 重构** (P1-4/5/6/13) 都用同一模式：抽 phase / pattern 子 fn，主 fn 主线下浅 1-2 层。
3. **Arc<str> 化** (P2-18 full) 真正消除了 Scope per-element clone 的 String malloc — 是 evaluator hot path 上 dhat 可量化项的 enable，不需 bench 即可断言"没引回 perf"。
4. **跨 crate dedup** (DirectiveShape / rebind_guard_pcs / 3 legacy entry sites / try_compare vs try_arith / Iter 协议) 都是 5-100 行的明确收敛，无设计 trade-off。
5. **table-drive** (host hook declares / pop_n + push_snapshot) 是机械收敛，~60-200 行变 ~25-130 行。
6. **deferred 的 P1 大头都是 single-session 大 refactor** (parser 350 / emitter 600 / cli 680 / TraceOp variant unify)，每项 1-2 天专注。loop 模式不适合这些。

## 结论

P0 全完，P1 大部分 + P2 纯结构化全部落地。剩余 backlog 全是 (a) 需 bench 验证的 perf 项 or (b) 跨多 crate / 多文件的大 refactor，需要专门 session 而非 loop iteration。当前 main 状态：

- HEAD `7ddb9ed`
- tests 2330 / 0 fail
- net +~1330/-~2150 since 2026-05-22 wave start
- backlog: 9 P1 + 19 P2 + 5 F items，每项 trigger 条件已写入对应 doc

**当前 simplify / refactor 维度可见 ROI 项基本清空**。后续要再推进需要触发外部条件：bench 平台稳定、新 backend RFC、用户报告 bug、或专门 1-3 天 session 投入大 refactor。
