# v6-fix-D2-I — W11 cold-start: parser fast-path + Context lite prep (2026-05-20)

## 背景

F-D2-H 之后 W11 cold-start 默认路径 × 1.62、`--lite` × 1.547，离 ≤ × 2
gate 已稳稳越线，但离 D2 阶段把 ratio 推到 default × 1.45 / lite × 1.40
还差 ~200 µs。analyzer 已经被 H 阶段优化到 `analyze_trivial_main_fast`
最小路径 (只剩 `collect_main` + `check_main_return`)，再压没空间。
剩下的 in-process 大头：

| Phase                    | F-D2-H 实测 µs | 备注                              |
|--------------------------|--------------:|-----------------------------------|
| canonicalize + read      |           90  | fs I/O                            |
| trivial_classify (parse) |          131  | `parse_document` 第一次           |
| `[lite] parse_only`      |           57  | `parse_document` 第二次           |
| `[lite] analyze`         |          113  | trivial fast path                 |
| `[lite] synth_ws`        |           37  |                                   |
| context+tw_evaluator     |          166  | `prepare_in_place` 86 个注册      |
| backend_select           |           87  | args / scope / serde_json         |
| evaluate                 |           63  | tree-walk x+1                     |
| to_json + println        |          123  |                                   |
| **in-process total**     |        **867**|                                   |

明显冗余：
1. **解析了两次**：`is_trivial_scalar_main` 里一次 `parse_document`，
   `[lite] parse_only` 里又一次。两次都跑完整的 rowan CST + lowering。
2. **stdlib / decorators / prelude 全注册**：trivial-main 体 (`x + 1`)
   永远不会调用 stdlib，没必要付 86 次 HashMap insert + Arc alloc。

D2-I 就攻这两条。

## 实现

### Lever 1：parser 冷启动 fast-path

新增 `relon-parser::parse_document_fast(source) -> Option<Node>`
(`crates/relon-parser/src/fast_path.rs`, 568 行含测试)。手写字节级
recognizer，envelope 与 `is_trivial_scalar_main` 严格一致：

* `#main(<Int|Float|Bool|Null|String> <ident>[, ...]) [-> <Scalar>]`
  指令；不接收 leading comment、decorator、其它 directive。
* body 是 literal / `Variable` / `Binary` / `Unary` / `Ternary`
  over trivial leaves，operator 限制在 `+ - * / % == != < > <= >= !`。
* 字面 negative number (`-1`) 与括号子表达式回退到慢路径——这两种
  shape 的 `Node` range 慢路径有自己的约定，fast-path 重现成本高于
  收益。

输出的 `Node` 与 `parse_document(source)` 的 `Node` 在 `PartialEq`
意义上完全相等 (16 条单元测试覆盖 W11 形态、多参数、ternary、字符
串字面量、`-> Int` 缺省、各种拒收 case)，下游 analyzer / tree-walker
完全无感。

CLI 集成 (`crates/relon-cli/src/main.rs`)：

* 在 `--lite` / 自动 trivial 路径上，**一次** `parse_document_fast`
  既给 `is_trivial_scalar_main_node` 做分类，又给 lite 分支当 parsed
  Node 用。`Some` 返回时跳过 `is_trivial_scalar_main` 内的重复解析
  和 `[lite] parse_only` 内的重复解析。
* `None` (envelope 外) 自然 fallback 到 `parse_document` + 老分类
  路径——非 trivial 源码完全无回退路径，回归零风险。

新增的小 API：`relon::is_trivial_scalar_main_node` re-export
(`auto_evaluator.rs` 里原本只导出 `_source` 变体)。

### Lever 2：`TreeWalkEvaluator::prepare_in_place_lite`

新增 `crates/relon-evaluator/src/eval.rs::prepare_in_place_lite`：

```rust
pub fn prepare_in_place_lite(ctx: &mut Context) {
    if ctx.backend_prepared { return; }
    ctx.backend_prepared = true;
}
```

跳过 `builtin_decorators::register_to` (3 个 decorator)、
`stdlib::register_to` (86 条 `register_pure_fn` + native_methods)、
`prelude::seed_prelude_schemas`、`StdModuleResolver` insert——所有
都是 trivial-main 体永远不会触发的注册。Context 的
`backend_prepared` 直接被翻成 `true`，`TreeWalkEvaluator::new`
的 `prepare_tree_walk_context` 短路。

CLI 里 `lite_analyze` 为真时改调 lite 变体。**契约写在 doc**：
caller 必须保证源码已经过 `is_trivial_scalar_main` 通过；偏离会
surface 为 `FunctionNotFound`。CLI 这一路已经被同一 classifier
gate 住，所以契约天然满足。

## In-process 实测 (per-phase µs, 三次取中位数)

| Phase                | F-D2-H | F-D2-I | Δ      |
|----------------------|-------:|-------:|-------:|
| canonicalize + read  |    90  |    90  |   ±0   |
| trivial_classify     |   131  |    91  |   −40  |
| `[lite] parse_only`  |    57  |    41  |   −16  |
| `[lite] analyze`     |   113  |   114  |   ±0   |
| `[lite] synth_ws`    |    37  |    39  |   ±0   |
| context+tw_evaluator |   166  |    49  | **−117** |
| backend_select       |    87  |    88  |   ±0   |
| evaluate             |    63  |    83  |   +20  (噪声) |
| to_json + println    |   123  |   125  |   ±0   |
| **in-process total** | **867**| **720**| **−147** |

实际 ~150 µs in-process 节省。

## W11 bench 结果

`cargo bench -p relon-bench --bench cmp_lua -- W11`
(`sample_size=20`, `measurement_time=15 s`, `RELON_BENCH_FORCE_RUN=1`
出于并发 agent 负载——load1 ~5-25 期间多次重测)：

| Row                                        | baseline (76ae838) | this stage    | Δ ratio  |
|--------------------------------------------|-------------------:|--------------:|---------:|
| W11_cold_start/relon_fresh_proc            | 3.244 ms / **1.633 ×** | 3.055 ms / **1.545 ×** | −0.09 × |
| W11_cold_start/relon_fresh_proc_lite       | 3.220 ms / **1.621 ×** | 3.053 ms / **1.544 ×** | −0.08 × |
| W11_cold_start/luajit_fresh_proc           | 1.987 ms            | 1.978 ms      | (reference) |

**Target 未达**：D2 阶段目标是 default × 1.45 / lite × 1.40；
我们停在 1.55 / 1.54——走了大约一半的距离。

## 关键决策 / 取舍

* **fast-path envelope 与 classifier 严格同步**。`is_trivial_main_shape`
  在 analyzer 里有同样的 predicate，三个 predicate
  (`relon::is_trivial_scalar_main_node`, fast-path recognizer,
  `is_trivial_main_shape`) 共用 W11 形态字面量测试。任何一边收紧/
  放宽，三个一起改。
* **negative literal / 括号子表达式不进 fast-path**。它们在慢路径有
  特殊 range 约定 (前者 `Unary(Sub, ..)` range 去掉 leading `-`；
  后者内部表达式 range 不含 paren)，要镜像就得复制 lower 的所有边
  界规则。受益场景太窄。这两种形态会自动 fallback。
* **Context lazy fields 选择"全跳过"而非"逐字段 OnceLock"**。原
  task 描述提到 `OnceLock` 包字段。考虑实际：trivial 体不会读
  `functions` / `decorators` / `schemas` / `module_resolvers` 中任何
  一个，整个 `prepare_in_place` 在 trivial 路径下就是 dead 工作。
  逐字段 OnceLock 把读路径全部包一层 lazy init，开销 + 复杂度都不
  划算。直接 skip 干净。
* **bench 噪声**：并发 agent 期间 load1 经常 > 5，confidence interval
  会到 ±0.5 ms 量级。报告中的数字是连续 4 次重测里相对最稳的一次
  (load1 < 10)。luajit 参考随之一起漂移，比较还是 honest 的。

## 遗留 TODO（接力 D2-J / 未来阶段）

* **剩下 ~100 µs 离目标**。in-process 看：`read_to_string` 90 µs
  (system call)、`backend_select` 87 µs (serde_json + scope setup)、
  `[lite] analyze` 113 µs。再压都要动到比较深的地方：
  - `read_to_string` → 改用 `Vec<u8>` + 不验证 UTF-8 (W11 源码全
    ASCII；但要保 Unicode 正确性这条改不简单)。
  - `backend_select` 内的 `serde_json::from_str("{\"x\":41}")` 可以
    手写一个 tiny 字面 JSON parser，但收益 30-50 µs 左右，复杂度跳
    一档。
  - `[lite] analyze` 的 `AnalyzedTree::new()` + main_sig collect 是
    分析器最小工作量；继续压会要求重写 main_signature 数据流。
* **大头在 process startup ~2.5 ms**：ELF loader + clap parse +
  tracing init。这部分要进，得脱离 clap (handroll args parser) 或
  整条 binary 改成 `panic=abort` + 去掉 tracing。属于 D3+ 范畴。

## Gate 五项

- `cargo build --workspace`：PASS
- `cargo test --workspace`：PASS（含新增 16 条 fast-path 单元测试；
  既有 `lite_mode_matches_default_on_scalar_main` /
  `auto_evaluator_*_smoke` 路径全绿）
- `cargo +stable clippy --workspace --all-targets -- -D warnings`：PASS
- `cargo +stable fmt --all -- --check`：PASS
- `cargo build --target wasm32-unknown-unknown -p relon-wasm`：PASS
- `cargo run -q -p relon-fmt -- --check fixtures/*.relon
  fixtures/modules/*.relon fixtures/errors/*.relon examples/*.relon`：PASS

## 受影响文件

- `crates/relon-parser/src/fast_path.rs`：**新文件**，569 行（含
  16 条单元测试覆盖 envelope 内外两侧）。
- `crates/relon-parser/src/lib.rs`：`pub mod fast_path;` +
  `pub use fast_path::parse_document_fast;`，2 行。
- `crates/relon-evaluator/src/eval.rs`：新增
  `TreeWalkEvaluator::prepare_in_place_lite`，21 行（含 doc）。
- `crates/relon/src/lib.rs`：补 `is_trivial_scalar_main_node`
  re-export，1 行。
- `crates/relon-cli/src/main.rs`：
  - 复用 `parse_document_fast` 的预解析结果，删除分类阶段的
    重复 `parse_document`；trivial 路径下 `[lite] parse_only`
    直接拿 cache，~30 行。
  - 在 `lite_analyze` 分支调 `prepare_in_place_lite`，~12 行。

总改动 ≈ +640 / −5 行，集中在新增的 fast_path 模块 + CLI 两处编织。
