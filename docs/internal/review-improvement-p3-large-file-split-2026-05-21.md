# P3: large-file split (IR Unicode + stdlib)

跟进 review-improvement P3 audit 中的 large-file findings。本 phase
落地 2/4 目标 (Unicode 模块 + stdlib 域拆)，typecheck / infer 延后到
P3-phase2。

## 目标 + 拆分前/后 LoC

| 目标 | 拆分前 | 拆分后 | 状态 |
|------|--------|--------|------|
| `relon-ir/src/{case_folding,full_case_folding,full_case_folding_data,combining_marks,whitespace,normalization,normalization_data,ascii_fold_simd}.rs` (8 flat files, 13950 LoC) | flat at crate root | 全部移入 `relon-ir/src/unicode/` 子目录 + `unicode/mod.rs` 入口 (45 LoC doc) | done |
| `relon-ir/src/stdlib.rs` (8727 LoC monolith) | 单文件 | 拆为 `stdlib/{mod, signatures, registry, index, defs, case_fold, normalization}.rs` (282/193/177/132/1980/3352/2783 = 8899 LoC) | done |
| `relon-analyzer/src/typecheck.rs` (5765 LoC) | — | 未拆 | **deferred to P3-phase2** |
| `relon-analyzer/src/infer.rs` (1808 LoC) | — | 未拆 | **deferred to P3-phase2** |

LoC delta: +217 (净增量；几乎全部来自每个 sub-module 头部新增的
crate-level doc 注释 + super::imports + 注明域归属说明)。

## 落地 1: Unicode 子目录

8 个 Unicode-相关文件全部迁入 `relon-ir/src/unicode/`，`lib.rs`
保留 `pub use crate::unicode::{case_folding, combining_marks,
whitespace, normalization, normalization_data, full_case_folding,
ascii_fold_simd};` 的 re-export，外部 3 个 caller
(relon-evaluator/stdlib.rs, relon-codegen-native/codegen/const_pool.rs,
relon-bench/benches/ascii_case_fold.rs) 无需改动。

`unicode/mod.rs` 含 45-line 模块综述 (UCD 14.0.0 pinning、各 sibling
角色、`tools/gen_normalization_tables.py` /
`tools/gen_full_case_folding.py` 再生触发点)，是未来 Unicode 版本
bump 的一站式入口。

内部 cross-reference (`crate::normalization_data::*` /
`crate::combining_marks::encode_ranges_bytes` 等 doc-link) 转用
`super::xxx`，符合 module locality。`full_case_folding_data.rs` 保留
为 `include!()` 注入而非 `pub mod`，与原本布局一致 (避免重复 namespace
+ 让 cargo fmt 跳过该 generated 数据文件)。

## 落地 2: stdlib 域拆

`stdlib.rs` 旧 single-file 拆为 6 子模块 + `mod.rs` (re-export 公开
surface)：

| Sub-module | LoC | 职责 |
|------------|-----|------|
| `signatures` | 193 | `StdlibFunction` 类型 + `*_INDEX` 常数 (CASEFOLD_LOOKUP / COMBINING_MARK / IS_WHITESPACE / DECOMP_LOOKUP / CCC_LOOKUP / COMPOSE_LOOKUP / FULL_CASEFOLD_LOOKUP / FINAL_SIGMA_CHECK) |
| `registry` | 177 | `builtin_stdlib()` master ordered list；wasm wire-format 契约的单一来源 |
| `index` | 132 | `stdlib_function_index` / `_count` / `_method_index` / `_closure_arg_signature` 查找 |
| `defs` | 1980 | 非 Unicode body builders (length / list_*_length / abs / min / max / is_empty / concat / substring / starts_with / contains / list_int_sum/max/map/filter/fold) + 共享 `tt()` op-tag helper |
| `case_fold` | 3352 | `upper` / `lower` / `title` / locale 三组 surface bodies + `CaseFoldMode` enum + `case_fold_body_inner_body` (1500-LoC core pipeline) + `__casefold_lookup` / `__is_combining_mark` / `__is_whitespace` / `__full_casefold_lookup` / `__final_sigma_check` 内部 helpers + 共享 `range_membership_helper` / `range_search_loop_body` |
| `normalization` | 2783 | `nfd` / `nfkd` / `nfc` / `nfkc` surface bodies + `NormForm` enum + `normalize_body_ops` (2000-LoC core pipeline) + `__decomp_lookup` / `__ccc_lookup` / `__compose_lookup` 内部 helpers |
| `mod` | 282 | 模块综述 doc + sub-mod 声明 + `pub use` 重新导出 + 所有 IDX 稳定性单元测试 (b4 / b5 / b6 / b7 / d7d) |

`mod.rs` 的 `pub use {signatures::StdlibFunction, registry::builtin_stdlib,
index::*}` 保持公共 surface 完全兼容；`relon-ir/src/lib.rs` 的
`pub use stdlib::{...}` 一行 zero diff，下游 `relon-bytecode`、
`relon-trace-recorder`、`relon-codegen-native`、`relon-evaluator`
等无任何调用点变更。

## Cross-ref impact

* `pub use` 路径：无变化。
  `relon_ir::case_folding::...` / `relon_ir::normalization::...` /
  `relon_ir::stdlib::builtin_stdlib` / `StdlibFunction` 等顶层路径
  全部保留 (lib.rs 通过 `pub use crate::unicode::*` 与
  `stdlib/mod.rs` 的 `pub use` 维持)。
* 内部 `crate::normalization::HANGUL_*` 引用 (stdlib 的 normalization
  body builders 引用 Hangul 常数) 因 `pub use crate::unicode::normalization`
  re-export 自动解析。
* 子模块间引用：case_fold + normalization 通过 `super::defs::tt`
  共享 op-tag helper；这是新增的依赖边，编译期校验。
* IDX 常数：原 `pub(crate)` 改为 `pub(crate)` 保持不变 (仍只在
  stdlib 内部消费)；`StdlibFunction::new` 由 `fn` 升级为
  `pub(super) fn` 以让 case_fold / defs / normalization 三个 builder
  模块可以构造 entry。
* `CaseFoldMode` 移入 `case_fold.rs`，`NormForm` 移入
  `normalization.rs` — 这两个 enum 原先就在使用上和域绑死，
  signatures.rs 只保留真正"跨域"的 IDX 常数 + entry struct。

## stdlib_index_consistency 契约

`relon-trace-recorder` 的 `stdlib_index_consistency` 测试以及
stdlib 自身的 b4 / b5 / b6 / b7 / d7d 索引稳定性测试全绿。
`mod.rs` 内的测试 mod 直接 `use super::signatures::{...IDX}` 导入，
保持稳定性断言 source-of-truth 路径清晰。

## Gate 状态

* `cargo fmt --all --check`: clean
* `cargo clippy --workspace --all-targets -- -D warnings`: clean
* `cargo test --workspace`: 2029 passed / 0 failed (与 base 持平)
* `cargo check --target wasm32-unknown-unknown -p relon-ir`: clean

## Deferred to P3-phase2

`typecheck.rs` (5765 LoC) 和 `infer.rs` (1808 LoC) 拆分需要先解决:

1. `Walker` struct 共享 `tree.diagnostics` / `scope_stack` /
   `schema_index` 三大 mutable state，跨文件 split 需要做
   `pub(super) trait WalkerCtx` 抽象或保留 `impl Walker` pattern。
2. `typecheck::visit_internal` 中 ~290-line `match &*node.expr`
   超大 dispatch 是天然 split anchor，但需先决定保留单大 match
   还是用 `Op::Variant` 风格的 sub-fn 分发表，二者维护成本不同。
3. `infer.rs` 体积偏小 (1.8k LoC)，建议先观察 typecheck 拆完后
   `infer::TypeScope` / `infer::SchemaBaseIndex` 是否能复用 typecheck
   的新 sub-mod 结构，再决定是否单独拆。

建议下一轮 P3-phase2 单独立项，附带 prior-audit 的实际 grouping
proposal (typecheck.rs head 注释已经列了 8 大 method group，可作为
拆分蓝本)。

## Branch + commit

* branch: `worktree-agent-a816e7f0e834ecdb9`
* commits:
  * `refactor(ir): split unicode tables into unicode/ submodule`
  * `refactor(ir): split stdlib.rs by domain into stdlib/ sub-modules`
* worktree: `/ext/relon/.claude/worktrees/agent-a816e7f0e834ecdb9`

未 push。
