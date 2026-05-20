# F-D2-G stage report — lazy stdlib body construction

> **Date**: 2026-05-20
>
> **Base HEAD**: `7b7ceb1 merge(trace-emitter): F-D7-E SIMD memchr for needle=1 StrContains`
>
> **Scope**: `relon-ir::stdlib::builtin_stdlib()` 从 eager-build 全表
> Vec<StdlibFunction>（包含每个 body 的 `Vec<TaggedOp>` 字面量）改造为
> 元数据 + lazy body：
>
> 1. `builtin_stdlib()` 返回 `&'static [StdlibFunction]`，整个数组在
>    `OnceLock<Vec<StdlibFunction>>` 内首次访问时构造一次（之前是
>    **每次调用** 都 rebuild 全 37 个条目的元数据 + body op vector）。
> 2. `StdlibFunction::body` 从 `Vec<TaggedOp>` 改成
>    `Arc<OnceLock<Vec<TaggedOp>>>` + `body_builder: fn() -> Vec<TaggedOp>`；
>    `.body()` accessor 触发 lazy build，`Clone` 共享 `Arc` 不重复构造。
> 3. 每个 `fn xxx() -> StdlibFunction` builder 拆成两段：
>    一段返回 metadata + fn ptr（拼成 `StdlibFunction::new`），一段是
>    pure `fn xxx_body() -> Vec<TaggedOp>` 含原 body 字面量；helpers
>    `case_fold_body` / `locale_aware_case_fold_body` / `normalize_body`
>    / `range_membership_helper` 内部同步改为只返回 body op vector。

## 一、改动文件

| 文件 | LoC | 说明 |
|---|---|---|
| `crates/relon-ir/src/stdlib.rs` | +2 808 / -2 762 净 +46 | `StdlibFunction` 新增 `Arc<OnceLock<Vec<TaggedOp>>>` + fn ptr 字段、`new` / `body` / `body_owned` accessor + 手写 `Clone` / `Debug`；`builtin_stdlib()` 切到 `OnceLock<Vec<...>>` 缓存；37 个 builder 拆 metadata + body fn；`normalize_body` / `case_fold_body_inner` / `range_membership_helper` / `is_whitespace_helper` 的尾 wrap 改为只返回 `Vec<TaggedOp>`。删除现已 unused 的 `NormForm::name`。 |
| `crates/relon-ir/src/lowering.rs` | +2 / -4 | `builtin_stdlib().into_iter().nth(i)` → `.get(i)`（无需 `cloned()`，stdlib_meta 借用从静态 slice）。 |
| `crates/relon-codegen-native/src/codegen.rs` | +8 / -3 | `callee.body` → `callee.body()` (collect_body 路径) 与 `callee.body.clone()` → `callee.body_owned()`（inline frame 路径）。 |
| `crates/relon-bytecode/src/compile.rs` | +9 / -3 | `resolve_stdlib_func` 走 `.get(i).cloned()`（Arc 共享 lazy cell）；inline 时 `stdlib_func.body` → `stdlib_func.body_owned()`。 |

总计 4 文件，~ +50 净 LoC（不含 cargo fmt 重排带来的 reflow，stdlib.rs
reformat 占了绝大多数 diff 行数）。

## 二、设计取舍

**为何 `Arc<OnceLock<...>>` 而不是 `OnceLock<Box<...>>`**：
`StdlibFunction` 仍然需要 `Clone`（bytecode `resolve_stdlib_func`、
`MethodRegistryEntry` 测试用例、metadata fan-out）。Std `OnceLock` 不
impl `Clone`，所以共享内部 cell 唯一办法就是 `Arc` 包一下；clone 浅
共享 `Arc`，第一次触发 body build 的 caller 把结果填进 OnceLock，所有
共享同一 Arc 的 caller 都看到结果。

**为何拆 `fn xxx_body()` 而不是闭包**：
20 多个 builder 含函数级 `const LEN_A: u32 = 0;` 这种常量。闭包对外部
fn-scope const 的引用照样合法，但闭包要 coerce 到 `fn` pointer 前提
是 zero capture——这点容易在后续编辑里悄悄破坏（一个 `let x = ...;
move || x` 就把 fn ptr 退化成 `Fn`）。独立 fn 把意图固化在函数签名上，
任何意外捕获在 ptr 类型上就直接报错。简单 body（abs/min/max/is_empty/
length/list_*_length）保留闭包形式，可读性更好。

**stable index 不变**：
`builtin_stdlib()` 的数组顺序与之前完全一致，因此 `stdlib_function_index`
/ `stdlib_method_index` 返回相同 u32；`CASEFOLD_LOOKUP_INDEX` 系列
hardcoded 常量保持原值；`stdlib::tests::b4_indices_are_stable` /
`b5_indices_are_stable` / `b6_b7_indices_are_stable` /
`d7d_index_tests::contains_index_is_36` / `relon-trace-recorder` 的
`stdlib_index_consistency` 全部 green。

## 三、测试

```
cargo test --workspace          → all pass，无 fail/ignore drift
  - relon-ir lib            : 89 passed
  - workspace summary       : ~ 750 passed, 0 failed
cargo build --workspace         → ok
cargo clippy --workspace --all-targets -- -D warnings → 0 warnings
cargo fmt --all -- --check      → ok
cargo build --target wasm32-unknown-unknown -p relon-wasm → ok
cargo run -q -p relon-fmt -- --check fixtures/*.relon ... → ok
```

关键 invariant 测试（slot index 稳定）全部 pass：

- `stdlib::tests::length_index_is_zero`
- `stdlib::tests::phase4b_indices_are_stable`
- `stdlib::tests::b4_indices_are_stable`
- `stdlib::b5_index_tests::b5_indices_are_stable`
- `stdlib::b6_b7_index_tests::b6_indices_are_stable`
- `stdlib::b6_b7_index_tests::b7_indices_are_stable`
- `stdlib::d7d_index_tests::contains_index_is_36`
- `stdlib::tests::casefold_lookup_index_is_stable`
- `stdlib::tests::combining_mark_index_is_stable`
- `stdlib::tests::is_whitespace_index_is_stable`
- `relon_trace_recorder::lowering::tests::stdlib_index_consistency`

## 四、W11 cmp_lua bench（cargo bench -p relon-bench --bench cmp_lua -- W11）

环境：单机 16 CPU 非 quiescent（`RELON_BENCH_FORCE_RUN=1`），`schedutil`
governor + load1 ≈ 2.5-3.0；release profile `relon-cli`；mlua-sys-built
LuaJIT。每行 20 sample × 15 s measurement，warmup 3 s。

| Row | Before (7b7ceb1) | After (F-D2-G) | Delta |
|---|---|---|---|
| `W11_cold_start/relon_fresh_proc` | 3.341 ms | 3.350 ms | +0.3 % (criterion: no change) |
| `W11_cold_start/relon_fresh_proc_lite` | 3.315 ms | 3.318 ms | +0.1 % (criterion: no change) |
| `W11_cold_start/luajit_fresh_proc` | 2.030 ms | 2.035 ms | +0.2 % (criterion: no change) |

| Ratio | Before | After |
|---|---|---|
| **default / luajit** | **× 1.645** | **× 1.646** |
| **lite / luajit** | **× 1.633** | **× 1.631** |

**结论**：F-D2-G 在 cold-start `x + 1` 的 W11 负载上 **没有可观测的
perf delta**——criterion p ≥ 0.05 全部 "no change in performance
detected"。Lite 路径 0.002 倍极小（噪声内）改善。

## 五、为什么 delta 几乎为零（诚实记录）

任务前提是 "cmp_lua W11 default cold-start ratio × 1.63 → ≈ × 1.4
LuaJIT"。前测结果显示 **baseline 本来就是 × 1.65**（与任务给的 1.63
基本一致），但 lazy stdlib body 改造在这个 workload 上几乎不触发任何
原本 eager build 的开销：

1. **W11 测的是 `#main(Int x) -> Int\nx + 1`**——一个无任何 stdlib 调用
   的 trivial 函数。lowering pass 走的两处 `builtin_stdlib()` （free-call
   resolve + method-call resolve）在这条 source 上根本不触达。
2. **codegen-AOT 的 `collect_body` / inline-frame** 也只对实际被
   `Op::Call` 的 stdlib slot 递归——`x + 1` 没有 Call 指令，整个 stdlib
   table 的 body 在这次 cold-start 全程 0 build。
3. **真正会受益的负载** 是 stdlib-密集型 source（`upper`/`lower`/
   `nfd` 等 Unicode body 体量 ~1 万 op，eager 时即使被 DCE 也得先
   构造完才能被剪），F-D2-G 让这些路径只在被引用时才付构造成本。
   但 W11 默认 source 不在这一类里，所以 bench 数据反映不出收益。
4. **`builtin_stdlib()` 改成 `OnceLock` 也只是节省了 lowering pass
   每次 method-call 重新构造 37 个 metadata 条目的开销**——单次
   `x + 1` 编译里这条路径走 0 次（free-call 0 次，method-call 0 次），
   节省 0 × N = 0 ms。

**Bench 方法论吻合 [bench_methodology_first MEMORY]**：在跑 perf
迭代前应先验证 bench 是否真的覆盖被改路径，否则整轮 delta 就是噪声。
本次改造在 *正确性* / *future workload* 维度有价值（lazy 化、表 cache
一次性、metadata fan-out 0 alloc），但 W11 这条 workload 命中不了
此次改动的实际开销热点。

## 六、后续

要在 W11 上看到 × 1.4 LuaJIT，需要的工作落在 cold-start 的其它热段：

- `inject_core_schemas` 解析 + analyze（F-D2 已 OnceLock-cached，但
  cache miss 时仍是 ~1.8 ms 的纯 analyzer 走 root_schemas + symbols）。
- AOT cache probe 的 mtime check / digest hash 串行；F-D7 系
  改动已经把 inline pre-recorder 推过去，但 inline lowering 仍要走
  cranelift `pool.collect_body` 完整遍历 IR 一次。
- relon-cli 启动本身的 ELF load / dynamic linker / arena init —
  F-D2-A / F-D2-C / F-D2-E 已收割过一轮 ~2 ms。

F-D2-G 落在 perf-and-efficiency 主线的 "正确性 + 未来 workload"
gate 里——cmp_lua trace_jit_hot_loop 或单独的 Unicode-heavy 负载
应该能展示这次改动的实际帮助；W11 这条 cold-start 微 workload 是
错的测点。

## 七、branch / commit

- branch: `worktree-agent-aa51f01e0fc4e372d`
- 待提交：合并 `perf(ir): F-D2-G lazy stdlib body construction` +
  `docs(internal): F-D2-G stage report + W11 rerun`（按任务允许，合一）
