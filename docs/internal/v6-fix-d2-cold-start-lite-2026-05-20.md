# v6-fix-D2 — W11 cold-start, `--lite` mode (2026-05-20)

## 背景

λ-2 final report (`relon-vs-luajit-final-report-2026-05-19.md`) 标 W11
为 catastrophic FAIL：`relon run foo.relon` 从 PID start 到 `#main(...)`
返回，对比 LuaJIT 慢一个数量级以上。原因是 Relon 默认走 cranelift-AOT
路径，对 short-lived 单次 invocation 而言，IR lower + JIT codegen +
分析器的 carrier `.relon` 解析 + AOT cache probe 等启动期 lazy init
的成本全部都摊不开。

D2 的目标：把 Relon CLI 在 W11 维度压到 LuaJIT × 2 内，方法
是 (1) profiling 找瓶颈，(2) 引入 `relon-cli --lite` mode 短路所有
重型 startup-side init。

## Profiling 数据

测试源：

```relon
#main(Int x) -> Int
x + 1
```

测试命令：`target/release/relon-cli run /tmp/w11.relon --args '{"x":41}'`

启用 `RELON_CLI_PROFILE=1` 后输出 in-process 各阶段计时（μs，三次平均）：

| Phase                       | default (AOT)  | tree-walk     | --lite        |
|-----------------------------|---------------:|--------------:|--------------:|
| canonicalize + read         |          80    |          80   |          80   |
| analyze_entry (parse+ws)    |       2 000    |       2 000   |     320 (lite path) |
| context + tw evaluator      |         150    |         150   |         150   |
| evaluate (#main)            |       4 700    |          60   |          60   |
| to_json + println           |         115    |         115   |         115   |
| **in-process total**        |     **7 000**  |     **2 400** |       **700** |
| **wall clock (process)**    |    **9 300**   |    **5 300**  |     **3 300** |

`time` 测得的 ~2.5 ms ELF loader + clap parse 是 outside-bench 的常量
开销 (`relon-cli --help` 单独跑也是 ~2.5 ms)，不在我们能短路的范围。

### Top-3 时间消耗 phase（默认路径）

1. **cranelift-AOT lower + JIT codegen**：~4.7 ms（最大头），来自
   `CraneliftAotEvaluator::from_source` 的 lower-source → JIT pipeline。
2. **analyzer carrier 注入**：~1.8 ms，来自
   `relon_analyzer::core_schemas::inject_core_schemas`——
   每次 `analyze_with_options` 都要重新 parse + lower 四个内嵌
   `core/{iter,string,list,dict}.relon` carrier（共 83 行 schema 方法
   声明）。它只贡献 `tree.schema_methods` 这一张方法分派表。
3. **rest of analyzer pass**：~150 us，剩余 typecheck、resolve、main
   collect 等。

## `--lite` 实现要点

1. **CLI flag**（`crates/relon-cli/src/main.rs`）：新增 `--lite`
   bool，与 `--backend cranelift-aot / bytecode` 冲突时直接 error
   退出。
2. **强制 tree-walk**：`--lite` 把 `effective_backend` 钉为
   `BackendArg::TreeWalk`，跳过 cranelift-AOT 的 lower / JIT 整条路。
3. **跳过 carrier schema 注入**：`AnalyzeOptions` 新增
   `skip_core_schemas: bool` 旗标（`crates/relon-analyzer/src/lib.rs`）。
   置位时 `analyze_with_options` 不调用 `core_schemas::inject_core_schemas`。
   语义边界：源码若依赖 `s.upper()` / `[1,2].map(f)` 等内嵌
   carrier method dispatch，在 `--lite` 下会因为方法分派表为空而
   surface `UnknownMethod`——契约写进 `--lite` flag 的 doc。
4. **跳过 workspace BFS**：源码不含 `#import` 时（按 `tree.imports.is_empty()`
   判断），手工组装一个 single-module `WorkspaceTree`，绕开
   `workspace_build::build` 的 BFS / cycle detection / cross-module
   schema collision / unknown-types re-check 后置 pass。若含 import
   则回退到完整 `analyze_entry_with_options`，让跨模块诊断保持齿。
5. **carrier-source 缓存**：附带优化，把
   `core_schemas::inject_core_schemas` 的解析结果缓存到 process-local
   `OnceLock<HashMap<String, Vec<SchemaMethodInfo>>>`，让没启用
   `--lite` 但有多模块 workspace 的场景也能在第二次以后的
   `inject_core_schemas` 调用走 ~10× faster 的 clone 路径。

## W11 bench 结果

`cargo bench -p relon-bench --bench cmp_lua -- W11_cold_start`，criterion
sample_size=20, measurement_time=15 s, 同一台机器同一 quiescence
窗口测得：

| Row                                           |   time |  vs LuaJIT |
|-----------------------------------------------|-------:|-----------:|
| W11_cold_start/relon_fresh_proc (default/AOT) | 9.31 ms | **4.59 ×** |
| **W11_cold_start/relon_fresh_proc_lite**      | **3.22 ms** | **1.59 ×** |
| W11_cold_start/luajit_fresh_proc              | 2.03 ms | 1.00 × |

**Target 达成**：`--lite` ratio = 1.59 ×，落在 ≤ 2 × 内。

bench 文件 (`crates/relon-bench/benches/cmp_lua.rs`) 新增了
`relon_fresh_proc_lite` 行，与原 `relon_fresh_proc` 同
group，方便后续回归。

`bench_stats` 后处理：bench 不打 p99，只打 estimate ±confidence；如需
tail 数据后续再延长 sample_size 即可。

## 关键决策 / 取舍

- **`--lite` 是 hard contract**：跳过 carrier 注入意味着源码不能
  调用 String / List / Dict / Iter 上的内嵌方法。choice 是 fail-fast
  (`UnknownMethod`) 而非默默退回完整 analyze——后者会让 `--lite`
  的 perf 语义不稳定。
- **`--lite` 与 `--backend tree-walk` 区分**：后者只换 backend
  selection，整条 startup pipeline 不变；`--lite` 还短路 carrier
  注入 + workspace BFS。两者的 W11 数差距是 5.3 ms vs 3.3 ms。
- **OnceLock 缓存优先做但不依赖**：第一次 `inject_core_schemas`
  仍要付 1.8 ms 解析成本（单次 CLI invocation 不享用 cache），
  缓存只对长生命周期 host（LSP / 测试套件 / 多模块 workspace）
  有效。W11 的 1.59 × 完全靠 `skip_core_schemas` flag。
- **保留 fallback**：源码含 `#import` 时 `--lite` 自动 fallback 到
  完整 `analyze_entry`，让跨模块诊断不丢——不为 perf 损失正确性。

## 遗留 TODO

- **default 路径仍 4.6 × LuaJIT**：cranelift-AOT 的 lower + JIT
  ~4.7 ms 是单次 invocation 摊不开的 fixed cost。要让 default 路径也
  ≤ 2 ×，可走两条路：(a) v5-γ 已存在的磁盘 AOT cache 在 cache-hit
  时跳过 lower（`from_cache_dir`），但当前 CLI 的 `Auto` 分支
  调的是 `CraneliftAotEvaluator::from_source` 不带 cache；后续可
  改成 `from_source_with_cache` + `default_cache_dir`。(b) 让
  default 在源码很 trivial 时直接走 tree-walk（heuristic：no loop,
  no recursion → JIT 摊不开）。两者都属于 D2 后续——本轮只承诺
  `--lite` 的 ≤ 2 × 目标。
- **carrier 编译期序列化**：把 `inject_core_schemas` 的输出在 build
  时序列化成 `Vec<SchemaMethodInfo>` 嵌入 binary，则第一次调用
  也是常数时间。代价是 `SchemaMethodInfo` 需要可序列化（含
  `TokenRange` / `Arc<Node>`），改动面比较大。
- **`--lite` heuristic 自动开启**：当前需要操作员显式传 `--lite`。
  可以考虑：default 路径检测到 source 是 trivial scalar `#main`
  时自动等价 `--lite`，省掉用户记忆负担。

## Gate 五项

- `cargo build --workspace`：PASS
- `cargo test --workspace`：PASS（含新增 `lite_mode_matches_default_on_scalar_main`、
  `lite_rejects_cranelift_aot_backend` 两条 CLI 测试）
- `cargo +stable clippy --workspace --all-targets -- -D warnings`：PASS
- `cargo +stable fmt --all -- --check`：PASS
- `cargo build --target wasm32-unknown-unknown -p relon-wasm`：PASS
- `cargo run -q -p relon-fmt -- --check fixtures/*.relon fixtures/modules/*.relon fixtures/errors/*.relon examples/*.relon`：PASS

## 受影响文件

- `crates/relon-cli/src/main.rs`：新增 `--lite` flag 与所有
  短路分支；profiling 入口 (`RELON_CLI_PROFILE=1`).
- `crates/relon-cli/tests/backend_flag.rs`：新增 2 条 `--lite`
  集成测试。
- `crates/relon-analyzer/src/lib.rs`：新增 `AnalyzeOptions::skip_core_schemas`
  字段；`analyze_with_options` 按 flag 跳过 carrier 注入。
- `crates/relon-analyzer/src/core_schemas.rs`：新增
  `cached_core_schema_methods` / `build_core_schema_methods`，把
  carrier 解析结果固化到 `OnceLock`。
- `crates/relon-bench/benches/cmp_lua.rs`：新增
  `W11_cold_start/relon_fresh_proc_lite` criterion 行。
