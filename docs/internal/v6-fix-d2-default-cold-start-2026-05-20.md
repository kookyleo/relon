# v6-fix-D2 — W11 cold-start, default path 优化 (2026-05-20)

## 背景

F-D2 stage 1（`v6-fix-d2-cold-start-lite-2026-05-20.md`）已经把 `--lite`
路径的 W11 压到 × 1.59，但 default 路径仍 × 4.59，原因有二：

1. **cranelift-AOT lower + JIT codegen** ~4.7 ms 是 single-shot
   invocation 摊不开的固定成本。
2. **CLI 默认走 `CraneliftAotEvaluator::from_source` 而非走
   `from_source_with_cache`**，导致即便磁盘 cache 命中也用不上。

stage 1 的 stage report §三末尾 todo 提了两条 candidate：

- (a) **cranelift-AOT cache hit fast restore**：第二次冷跑应该可以
  跳过 cranelift codegen 走 `from_cache_dir` 的 dlopen-execute。
- (b) **trivial-`#main` 自动 tree-walk**：源码是 `#main(Int x) -> Int : x + 1`
  这种 trivial scalar 形状时跳过 cranelift-AOT init，直接 tree-walk。

本 stage 落地两条，都在 default 路径生效。

## 改动概览

**新增 / 修改文件**:

- `crates/relon/src/auto_evaluator.rs`（+182 LoC）
  - `AutoEvaluator` struct 新增 `is_trivial_main: bool` 字段。
  - `AutoEvaluator::new` 在构造时跑 `is_trivial_scalar_main(source)`
    分类器，结果缓存到 struct 上。
  - `Evaluator::run_main` 看到 `is_trivial_main == true` 时直接走
    `tree_walk.run_main(args)`（通过 trait dispatch），完全跳过
    cranelift-AOT slot 的构造 + cache probe。
  - 新增 pub fn `is_trivial_scalar_main(&str) -> bool` 分类器，
    判断标准刻意保守：
    - 必须 parse 成功；
    - 必须有且只有一个 `#main(...)` directive，且没有 `#import`；
    - 每个 `#main` 参数类型必须是 single-segment scalar builtin
      (`Int` / `Float` / `Bool` / `Null` / `String`)，no generics /
      no optional / no variant_fields；
    - body 必须是 literal / Variable / Reference / Unary over trivial
      leaf / Binary over trivial leaves / Ternary over trivial
      sub-nodes，且 Binary / Unary 的 operator 必须在
      `{Add, Sub, Mul, Div, Mod, Eq, Ne, Lt, Gt, Le, Ge, Not}` 白名单内
      （排除 `Pipe` / `Concat` / `And` / `Or`，避免短路求值 / stdlib
      间接 dispatch 进入热路径）；
    - 任何 `Closure` / `FnCall` / `FString` / `Match` / `Where` /
      `Comprehension` / `VariantCtor` / `List` / `Dict` / `Spread` /
      `Wildcard` / `Type` 全部 disqualify。
  - 7 条 unit test 覆盖 classifier 接受 / 拒绝边界（W11 形状、多
    scalar 参数、List 参数、closure body、library mode、import-bearing
    源、fn-call body）。
- `crates/relon/src/lib.rs`（+1 LoC）
  - 把 `is_trivial_scalar_main` re-export 到 crate 根，让 `relon-cli`
    可以复用同一个 classifier。
- `crates/relon/Cargo.toml`（+1 LoC）
  - dev-dependencies 加 `tempfile = "3"`（cache-hit 测试用）。
- `crates/relon/tests/auto_evaluator_smoke.rs`（+162 LoC）
  - `MAIN_SOURCE` 改为 `"#main(Int x) -> Int\nabs(x) * 2"`（FnCall body，
    classifier 必拒），原因：之前的 `x * 2` 现在会被 classifier 判为
    trivial → tree-walk path，会破坏现有 `run_main_routes_through_aot_and_caches`
    / `concurrent_run_main_only_builds_aot_once` 的 AOT-slot 断言。
  - 新增 4 条测试：
    - `default_path_uses_disk_cache_on_second_call`：路径 (a)。
      把 `XDG_CACHE_HOME` / `HOME` 重定向到 tempdir，跑两次
      `AutoEvaluator::new(non_trivial_src).run_main(...)`，验证：
      第一次跑完磁盘 cache 目录非空；第二次跑出的 `Value` 与第一次
      byte-equal。EnvGuard RAII 在 drop 时还原原值。
    - `default_path_skips_aot_for_trivial_main`：路径 (b)。`AutoEvaluator::new`
      W11 形状源后 `is_trivial_main() == true`，`run_main` 完成后
      `is_aot_initialised() == false`（slot 从未构造）。
    - `trivial_source_parity_default_vs_tree_walk`：相同 trivial 源在
      `Backend::Auto` 和 `Backend::TreeWalk` 下产出 byte-identical
      `Value`。
    - `non_trivial_source_still_routes_through_aot_path`：`List<Int>`
      参数源被 classifier 拒，`run_main` 后 `is_aot_initialised() == true`
      （AOT slot 已被驱动过，无论成功 / 缓存失败）。
- `crates/relon-cli/src/main.rs`（+93 LoC，- 17 LoC）
  - 新增 `trivial_default` 预分类（默认路径 + 非 lite + 源是 trivial
    scalar `#main`）→ 联合 `lite` 形成 `lite_analyze` 旗标。`lite_analyze`
    打开 `AnalyzeOptions::skip_core_schemas`，且让 workspace 走原本
    `--lite` 才走的 single-pass `analyze_with_options` + 手工 synth
    `WorkspaceTree` 路径，跳过 BFS / cycle / 跨模块 collision pass。
  - `BackendArg::Auto` 分支重写：
    - **trivial path (b)**：直接复用 workspace 已构造好的
      `TreeWalkEvaluator`，调 `evaluator.run_main(&scope, args_map)`。
      这条路径不重复 parse + analyze，也不进 cranelift-AOT init。
    - **非 trivial path (a)**：先调 `CraneliftAotEvaluator::from_cache_dir(&content, &cache_dir)`
      探 cache hit，命中则 dlopen-execute；miss 则 `from_source_with_cache`
      跑完整 lower / JIT pipeline 并把结果写回 cache。`cache_dir` 取
      `relon_codegen_native::default_cache_dir()`。

总计：5 个文件，~278 LoC 净增加。

## W11 bench 结果

bench 命令：

```bash
RELON_LUAJIT_BIN=<luajit> RELON_CLI_BIN=<release/relon-cli> \
  RELON_BENCH_FORCE_RUN=1 cargo bench -p relon-bench --bench cmp_lua -- W11
```

criterion `sample_size=20`、`measurement_time=15s`，同一台机器同一
quiescence 窗口（governors=0/16 perf, no_turbo=1, load1=2.70）。

| Row                                                |     time |  vs LuaJIT |
|----------------------------------------------------|---------:|-----------:|
| W11_cold_start / relon_fresh_proc (default, this stage) | **3.32 ms** | **× 1.63** ✓ |
| W11_cold_start / relon_fresh_proc_lite             | 3.28 ms  | × 1.61 |
| W11_cold_start / luajit_fresh_proc                 | 2.04 ms  | × 1.00 |

**对照 stage 1 终值**（`v6-fix-d2-cold-start-lite-2026-05-20.md` §四）：

| Row    | stage 1 终值 | 本 stage 终值 | Δ |
|--------|------------:|-------------:|---:|
| default (relon_fresh_proc) | 9.31 ms (× 4.59) | **3.32 ms (× 1.63)** | **-5.99 ms / -2.96 ×** |
| lite (relon_fresh_proc_lite) | 3.22 ms (× 1.59) | 3.28 ms (× 1.61) | +0.06 ms (噪声) |
| luajit | 2.03 ms | 2.04 ms | +0.01 ms (噪声) |

**结论**：W11 default 路径达成 ≤ × 2 目标（× 1.63）。bench 测的是 W11
trivial scalar `#main` 形状（`#main(Int x) -> Int : x + 1`），所以
default 走的是 **路径 (b) trivial-tree-walk 短路**，没有触碰 AOT slot
也没有探 cache。路径 (a) 的 cache-hit 在 non-trivial 源上仍是主力，
见下面 §"path (a) profiling 截图"。

### in-process profile（`RELON_CLI_PROFILE=1`）

trivial `#main(Int x) -> Int\nx + 1`，default 路径：

```text
canonicalize             +    13us
read_to_string           +   115us
trivial_classify         +   173us
[lite] parse_only        +    72us
[lite] analyze           +   242us
[lite] synth_ws          +    61us
context+tw_evaluator     +   204us
backend_select           +    55us
default_trivial_tree_walk +    43us
evaluate                 +    64us
to_json_value            +    58us
serialise_json           +    55us
println                  +    48us
total                    =  1209us  (≈ 1.2 ms in-process)
```

non-trivial `#main(Int x) -> Int\nabs(x) * 3`，default 路径 + 已 warm cache：

```text
canonicalize             +    11us
read_to_string           +    67us
trivial_classify         +   144us
[probe] parse_only       +    58us
analyze_entry            +  1930us   ← carrier injection 未短路
context+tw_evaluator     +   135us
backend_select           +    40us
default_cache_probe      +   551us   ← path (a) hit
evaluate                 +   150us   ← dlopen-execute
to_json_value            +    34us
serialise_json           +    36us
println                  +    29us
total                    =  3190us  (≈ 3.2 ms in-process)
```

non-trivial cold（首跑，无 cache）：`evaluate` 从 150 µs 涨到 15.8 ms，
其余阶段一致。这条数据说明 path (a) 的 cache-hit 把 cranelift codegen
从 15.8 ms 压到 150 µs。

## 测试

workspace 测试总数：1873 → **1884**（+11，全部是 relon crate 新增）：

- 7 条 unit test 在 `crates/relon/src/auto_evaluator.rs` 的 `tests` mod
  覆盖 `is_trivial_scalar_main` 的接受 / 拒绝边界。
- 4 条 integration test 在 `crates/relon/tests/auto_evaluator_smoke.rs`
  覆盖路径 (a) + (b) 的端到端契约（cache-hit、trivial-skip-AOT、
  parity、non-trivial-still-AOT）。

`cargo test --workspace` 全部 PASS（含 stage 1 留下的 `lite_mode_matches_default_on_scalar_main`
+ `lite_rejects_cranelift_aot_backend`）。

## 关键决策 / 取舍

- **trivial classifier 走保守路线**：宁可漏掉一些可以 short-circuit
  的源，也不允许误判一个 closure / list 重度的源走 tree-walk。
  原因：tree-walk 在 hot-loop 类的源上比 AOT 慢一个数量级，误判会
  让 default 路径在那些场景上性能掉头。
- **operator 白名单而非 expr-kind 白名单**：`Binary` 接受但 operator
  必须在 `{+, -, *, /, %, ==, !=, <, >, <=, >=, !}`。排除 `Pipe`
  （`x | f` 等价 `f(x)`，背后是 fn-call 调用）、`Concat`（String
  拼接，性能差距不显著，但收窄白名单减少 false-positive 风险）、
  `And` / `Or`（短路求值在 evaluator 内部走 closure-style，
  observability 不同）。后续若有具体 hot 源需要放宽再做。
- **CLI 默认路径走 `default_cache_dir()` 写 cache**：第一次冷跑要付
  完整 codegen 成本，但 cache pair 写到 `$XDG_CACHE_HOME/relon` 后，
  第二次起 dlopen-execute 命中。对单次 `relon run` 不利，对持续 dev
  loop 有利；trade-off 是用户的 `~/.cache/relon` 会随源码差异长出
  多个 `(source_hash, sandbox_config)` 条目，配套 `default_cache_dir`
  的 GC 在 v5-γ 已规划但尚未实现（遗留 todo）。
- **`AutoEvaluator::new` 仍然跑全套 workspace analyze (via
  `build_tree_walk_evaluator`)**：trivial classifier 失败时 fallback
  到原 AOT 路径需要 source / scope 完整，所以 tree_walk 不能 lazy。
  CLI 侧绕开 `AutoEvaluator` 直接复用 workspace 已构造好的
  evaluator，避免重复分析。后续如果 host 集成也开始关心 CLI cold-
  start 数字，可以考虑给 `AutoEvaluator` 加一个 lazy-tree-walk 入口。
- **cache invalidation 时机**：本 stage 不改 cache invalidation
  策略——`from_cache_dir` 内部已实现 HMAC + metadata + sandbox
  cross-check，任何 drift 都会触发 `invalidate_cache_triple`
  自动清理。`source_hash` 把 source 文本和 sandbox config bind 到
  一起，源码 / sandbox 任何一边变了就 miss，靠 `from_source_with_cache`
  自然覆盖。

## 遗留 TODO

- **trivial classifier 覆盖度**：当前接受范围只覆盖 W11 类
  scalar arith。可以扩展到：
  - String 字面量 / `concat`（如 `#main(String s) -> String : s + "."`）。
  - 简单 ternary 选择形（如 `flag ? a : b`）。
  - 不需要 stdlib 的 `Match` 形（pattern arm 全是 wildcard / literal）。
  扩展前需要先有对应的 hot bench 证明开销。
- **`AutoEvaluator::new` 内部仍 parse 两次**：一次在
  `build_tree_walk_evaluator` 里，一次在 `is_trivial_scalar_main`
  里。可以把第一次 parse 出的 `Node` 透传到 classifier，省 ~70 µs。
  W11 bench 的 trivial 路径 evaluate 阶段才几十 µs，省下来收益不
  显著，但对持续高频调用 `AutoEvaluator::new` 的 host 有意义。
- **cache GC**：`default_cache_dir` 累计的 `(source_hash, ...)` 条目
  目前没有 size cap 也没有 age-based eviction。对 dev loop 跑很久
  的开发机会随源码版本增长。v5-γ 已规划但尚未实现。
- **carrier 编译期序列化**：见 stage 1 同名 todo，未在本 stage 推进。
- **`analyze_entry_with_options` 在含 import 的源上仍是 cold-start
  瓶颈**：本 stage 的 `trivial_default` 短路只覆盖 import-free 源。
  含 import 的 trivial 源（罕见但理论存在）仍走完整 workspace pass。
  可以让 trivial classifier 主动放宽 import-free 限制，但需要在
  CLI 侧 fallback 到完整 `analyze_entry` 路径——和 stage 1 lite path
  保持一致。

## Gate 五项

- `cargo build --workspace`：**PASS**
- `cargo test --workspace`：**PASS**（1884 tests / 133 groups，全部
  通过；含本 stage 新增 11 条）
- `cargo +stable clippy --workspace --all-targets -- -D warnings`：**PASS**
- `cargo +stable fmt --all -- --check`：**PASS**
- `cargo build --target wasm32-unknown-unknown -p relon-wasm`：**PASS**
- `cargo run -q -p relon-fmt -- --check fixtures/*.relon fixtures/modules/*.relon fixtures/errors/*.relon examples/*.relon`：**PASS**
