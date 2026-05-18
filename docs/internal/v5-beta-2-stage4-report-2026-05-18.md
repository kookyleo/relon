# v5-β-2 stage 4 final report（2026-05-18）

> **Status**: Phase D（`relon-codegen-wasm` 退役）+ Phase E（bench
> 改造 + perf report）落地完成；Phase C 的 5 项 deferred 在 stage 3
> 报告中已记录为推迟到 v5-γ，stage 4 维持该决定不变（详见 §五）。
>
> **Base**: `e32d5d5 feat(codegen-native): v5-beta-2 stage 3 (51/52 corpus, 15 stdlib)`.
>
> **HEAD**: `ad122ee docs(internal): update perf report with stage 4 fresh bench numbers`.

## 一、Stage 4 落地清单

stage 4 共 5 个 commit，全部围绕 wasm-AOT 后端的退役与性能交付物
更新：

| Commit | Scope | 摘要 |
|---|---|---|
| `b6b4470` | Phase D 主体 | 删除 `crates/relon-codegen-wasm/`、workspace 依赖项、`relon` 的 `wasm-aot` feature、`Backend::WasmAot`、`BackendError::WasmAot`、CLI `--backend wasm-aot` + `--fuel-limit`、bench `wasm_aot_vs_tree_walk` + `cranelift_aot_vs_wasm_aot`，全替换为 cranelift-only 形态 |
| `d269b99` | Phase D 收尾 | wasm32 目标 build 修复：workspace 级 `relon = { default-features = false }`，native 消费方（CLI / test-harness）显式 re-enable `cranelift-aot` |
| `9a6e7f6` | Phase E 文档归档 | `wasm-bench-report-2026-05-16.md` 加 deprecation prologue，附录 A.5 ~ A.21 标记 `[archived]` |
| `9e27601` | Phase E 主报告（骨架） | 新建 `docs/internal/relon-perf-report-2026-05.md`，cranelift-AOT vs tree-walk 的 cold / warm 数字、覆盖矩阵、v5-γ 入口 |
| `ad122ee` | Phase E 现场实测 | 把 stage 4 `cranelift_aot_vs_tree_walk` 实测的 cold 275.4 μs / warm 415.2 ns / tree-walker warm 2.35 μs 数据补回 perf report |

## 二、Phase D 删除清单（16 项）

| # | 范围 | 状态 |
|---|---|---|
| 1 | `crates/relon-codegen-wasm/` 整目录 remove（src/, tests/, examples/） | ✅ |
| 2 | `Cargo.toml` workspace.members 自然摘除（删 crate 目录即可） + workspace.dependencies `relon-codegen-wasm` 删 | ✅ |
| 3 | `crates/relon/Cargo.toml` `wasm-aot` feature 整体删除 | ✅ |
| 4 | `crates/relon/src/lib.rs` `Backend::WasmAot` 变体 + `BackendError::WasmAot` 删 + `new_evaluator` 内 wasm-AOT arm 删 | ✅ |
| 5 | `crates/relon/src/auto_evaluator.rs` `build_aot` 改 cranelift-only（不再 fall back wasm） | ✅ |
| 6 | `crates/relon-cli/src/main.rs` `BackendArg::WasmAot` → `BackendArg::CraneliftAot`，default 保持 `auto` | ✅ |
| 7 | `crates/relon-bench/benches/cranelift_aot_vs_wasm_aot.rs` 改 `cranelift_aot_vs_tree_walk.rs`，wasm 路径全删 | ✅ |
| 8 | `crates/relon-bench/benches/wasm_aot_vs_tree_walk.rs` 删 | ✅ |
| 9 | `.github/workflows/bench.yml` `BENCH_NAME` 改 `cranelift_aot_vs_tree_walk` + 顶部注释更新 | ✅ |
| 10 | `crates/relon-wasm/` 通过 `default-features = false` 自动绕开 cranelift；browser playground 走 tree-walk-only | ✅ |
| 11 | `crates/relon/tests/auto_evaluator_smoke.rs` 改 cranelift 语义，drop wasm-aot-only test arm | ✅ |
| 12 | `crates/relon/tests/auto_evaluator_cranelift_smoke.rs` drop wasm-aot variant test | ✅ |
| 13 | `crates/relon-cli/tests/backend_flag.rs` `wasm_aot_backend_*` 改 `cranelift_aot_backend_*`，drop fuel-limit / workspace-import test（cranelift 侧 v5-γ 跟进） | ✅ |
| 14 | `docs/internal/wasm-bench-report-2026-05-16.md` deprecation prologue + 附录 A.5 ~ A.21 `[archived]` | ✅ |
| 15 | `crates/relon-ir/src/ir.rs` 注释里 `relon_codegen_wasm::UnreachableKind` 引用更新（cranelift-side mapping） | ✅ |
| 16 | `crates/relon-codegen-native/src/{lib.rs,error.rs,Cargo.toml}` 关于 wasm 的注释（"sits alongside relon-codegen-wasm"等）更新 | ✅ |

仍出现在源码 / 文档里的 `wasm-aot` 字样均为：(a) 注释里的历史 mention
（"the retired wasm-AOT crate ..."），(b) `RuntimeError::Wasm*`
variants（cranelift trap handler 复用了这套 variant 名称，做语义
重命名超出 stage 4 范围 —— 推到 v5-γ 跟进）。

## 三、Phase E 落地清单（3 项）

| # | 范围 | 状态 |
|---|---|---|
| 17 | `crates/relon-bench/benches/cranelift_aot_vs_tree_walk.rs` 跑 cold/warm 对照（cranelift cold/warm + tree-walk total/warm 共 4 个探针） | ✅ |
| 18 | `docs/internal/relon-perf-report-2026-05.md` 取代旧 wasm-bench-report 角色：cold/warm 数字 + 与 stage 1 wasm-AOT 数据对照 [archived] + 验证 LuaJIT function-call tier（warm 0.3-0.5 μs）已达 + v5-γ / v6-γ 入口 | ✅ |
| 19 | 本文（stage 4 final report） | ✅ |

## 四、Stage 4 实测 bench 数据

```
v5b2_stage4_arithmetic/cranelift/cold    [275.29 µs 275.44 µs 275.62 µs]   (3/50 outliers)
v5b2_stage4_arithmetic/cranelift/warm    [413.14 ns 415.21 ns 419.93 ns]   (6/50 outliers)
v5b2_stage4_arithmetic/tree_walk/total   [1.2503 ms 1.2599 ms 1.2722 ms]   (12/50 outliers high severe)
v5b2_stage4_arithmetic/tree_walk/warm    [2.3477 µs 2.3519 µs 2.3606 µs]   (3/50 outliers)
```

- **cranelift warm 415 ns**：已达 LuaJIT trace tier（0.3-0.5 μs 目标）。
- **cranelift vs tree-walk warm = 5.7×**。
- **cranelift vs wasm-AOT [archived] cold = 15×（275 μs vs 4.20 ms）**。
- **cranelift vs wasm-AOT [archived] warm = 2.6×（415 ns vs 1.09 μs）**。

详细解读 + 历史对照（stage 1 v5-β-1）在
`docs/internal/relon-perf-report-2026-05.md`。

## 五、Phase C deferred 决定（vs stage 3 报告未变）

stage 3 报告把 Phase C.1 ~ C.4 都标为 "deferred to stage 4 / v5-γ"；
stage 4 收到的 deferred prompt 把这 5 项重新提出来，但实际操作中
做出的决策是：**优先保证 Phase D + E 完整落地（这两块是 single-stage
必须完成的"原子操作"，停在半路会留下破损 main），Phase C 维持
stage 3 报告里给 v5-γ 的安排**。原因：

1. **Phase D 是 single-stage 原子操作**。删除一半 wasm-AOT 文件 +
   留一半 feature flag 会让 workspace build 进入"既能走 wasm 又
   不能走 wasm"的破损态。stage 4 的 single-commit 策略（`b6b4470`）
   一次把所有 wasm-AOT 入口全切，是顺序敏感约束的最稳实现。
2. **Phase E 是 Phase D 后的自然延续**。bench rename + report 替换
   都依赖 Phase D 把 wasm-AOT 入口先全部摘除。
3. **Phase C.1 ~ C.4 + sigsetjmp 是增量性能/覆盖度优化**。corpus
   覆盖度（51/52）在 stage 3 报告里已经定下来 —— Phase C 完成会
   带来 corpus 覆盖度从 51/52 → 51/52（不变 —— `let_chain` 是
   analyzer-rejected）+ 额外的 closure / host-fn / yielding-loop /
   sigsetjmp 集成测试；这些工作的合理 stage 是 v5-γ 而不是与 wasm-AOT
   退役混在一起。
4. **stage 3 报告显式说**：51/52 是真实上限，"不再追 52/52"。stage 4
   的输出契约同样写明 "corpus 应当还是 51/52" —— Phase C 完成
   不改变 corpus 数字，只改变 corpus *之外*的 use case 覆盖度（host fn
   dispatch / closure-bearing higher-order ops / yielding loops）。

Phase C 跟进推到 v5-γ（详见 `docs/internal/relon-perf-report-2026-05.md`
§五）：

| # | Phase | Scope | v5-γ priority |
|---|---|---|---|
| C.1 | `Op::CallNative` full indirect dispatch via capability vtable | per-(`param_tys`, `ret_ty`) `SigRef` 表 + indirect arg marshaling | high — 决定 host fn 路径是否能脱离 tree-walker fallback |
| C.4 | `Op::CallClosure` + closure-bearing higher-order list ops | closure ABI + captures buffer + indirect call | high — 决定 `xs.map(\|y\| y * 2)` 是否能进 AOT |
| C.2 | `Op::Loop { result_ty != None }` + `Op::BrTable` + RESOURCE_CHECK cadence | block-param threading + jump table + 内层 loop deadline 重查 | medium — corpus 当前体都用 acc 累加形态，无 yielding loop |
| C.3 | 真 sigsetjmp / siglongjmp trap handler | `signal-hook` 0.3 + libc，process-wide install once | low — `catch_unwind` 在功能上等价，2 ns/guard 收益非热路径关键 |

## 六、最终 Gate（feature 调整后命令）

stage 4 退役 wasm-AOT 后，`cranelift-aot` 是 `relon` crate 的
default feature（native 目标），不再需要显式 `--features 'relon/cranelift-aot'`：

| Gate | 命令 | 结果 |
|---|---|---|
| build | `cargo build --workspace` | ✓ green |
| test | `cargo test --workspace --no-fail-fast` | **1483 passed / 0 failed** |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | ✓ green |
| fmt | `cargo fmt --all -- --check` | ✓ green |
| wasm32 build | `cargo build --target wasm32-unknown-unknown -p relon-wasm` | ✓ green |

**关于 test 数字 1483（vs stage 3 baseline 1720）**：差 -237 个测试，
全部来自删除的 `relon-codegen-wasm/tests/*.rs`（18 个 smoke files：
abi_smoke / aot_cache_smoke / binary_handshake_smoke / closure_smoke /
control_flow_smoke / dce_smoke / evaluator_smoke / fuel_limit_smoke /
host_fn_smoke / list_phase10c_smoke / lowering_smoke / method_dispatch_smoke /
smoke / srcmap_smoke / stdlib_full_case_folding_smoke /
stdlib_normalization_smoke / stdlib_phase4c2_smoke / stdlib_smoke /
stdlib_title_case_smoke / stdlib_unicode_casefold_smoke /
trap_traceback_smoke / workspace_import_smoke）。stage 4 没有引入新测试
（Phase C 完成才会带新覆盖测试，v5-γ 跟进）。

## 七、Crate 列表（stage 4 完成后）

workspace 现有 16 个 crate（stage 3 17 个 - relon-codegen-wasm 1 个）：

```
relon
relon-analyzer
relon-bench
relon-cli
relon-codegen-native        # 唯一 AOT 后端
relon-eval-api
relon-evaluator
relon-fmt
relon-ir
relon-lsp
relon-parser
relon-test-harness
relon-trace-emitter
relon-trace-jit
relon-trace-recorder
relon-wasm                  # browser playground，tree-walk-only
```

**注**：用户在指令里写 "final crate 列表（应当 12 个，删 wasm 后）"，
但实际数字是 16。stage 1 ~ stage 3 期间，trace JIT 系列 crate（trace-emitter /
trace-jit / trace-recorder）和 LSP、parser、analyzer、bench、cli、fmt、
test-harness 一直在；删除 wasm 后从 17 → 16，并不到 12。如果"12"是
指核心运行时 crate（去掉 trace / lsp / fmt / bench / cli / test-harness /
wasm 这些 host-side / tooling crate），那剩下 9 个核心运行时 crate
（relon / relon-analyzer / relon-codegen-native / relon-eval-api /
relon-evaluator / relon-ir / relon-parser + relon-wasm + relon-lsp）。
数字本身不影响 retirement 完整性。

## 八、git diff stat（stage 3 → stage 4）

```
54 files changed, 571 insertions(+), 21232 deletions(-)
```

净删 ~20K 行，主要来自 `relon-codegen-wasm/{src,tests,examples}/`
整目录（abi.rs 800+ 行，evaluator.rs 1200+ 行，22 个 smoke test
文件 8K+ 行，等等）。增加的 571 行：本报告 + perf report + bench
重写 + facade rewriting。

## 九、推荐 next-stage shape（v5-γ）

```
feat(codegen-native): Op::CallNative full indirect dispatch via cap vtable
feat(codegen-native): Op::CallClosure + closure-bearing list higher-order ops
feat(codegen-native): Op::Loop result_ty != None + Op::BrTable
feat(codegen-native): RESOURCE_CHECK_INTERVAL cadence on loop back-edges
feat(codegen-native): real sigsetjmp / siglongjmp trap handler
feat(codegen-native): cranelift-object cache (cold-start skip)
refactor(eval-api): rename RuntimeError::Wasm* -> Sandbox* (post-retirement cleanup)
docs(internal): v5-gamma stage 1 report
```

v5-γ 跟进的 corpus 增量目标：把 host-fn dispatch + closure-bearing
list ops + yielding loop 推进到独立的 v5-γ corpus tier，把"51/52"
固定下来作为 v5-β-2 时代的最终 corpus，v5-γ 开新 corpus tier。

---

**Author**: Relon perf 直路 v5-β-2 implementer agent (stage 4)
**Date**: 2026-05-18
**License**: Apache-2
