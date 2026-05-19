# F-D9 阶段报告：cmp_lua W3 / W4 / W5 / W6 接入 trace-JIT entry（2026-05-20）

## 摘要

- F-D7（`TraceOp::Str*` + extern shims）、F-D8（`TraceOp::ListGet` /
  `TraceOp::DictLookup` + IC dict 助手）已落地，但 `cmp_lua` 主对照基
  准仍只有 tree-walker 与 LuaJIT 两列，无法回答"infra 是否真把 ratio
  压下来"。F-D9 是 wiring：为 W3 / W4 / W5 / W6 各加一个 hand-built
  cranelift JIT entry 的 `relon_trace_jit` 行，与 `relon_tree_walk` /
  `luajit` 并列。
- patch 来源：`docs/internal/salvaged/F-D9-cmp_lua-958LoC.patch`（前一个
  F-D9 agent 被 API 529 中断时的 ~958 LoC 工作 patch；base 为
  `4ea13ac`，但 `cmp_lua.rs` 自彼时起 main 没改）。`git apply --check`
  直接 clean，`git apply` 后 `cmp_lua.rs` 由 1174 LoC 扩到 2132 LoC，
  其余文件零改动。
- 仅做了一次 `cargo +stable fmt --all` 规范化（rustfmt 1.9 把 patch
  中一处 3 行 `jump(header, &[..])` 重排成 2 行链式 `ins().jump(..)`；
  非语义改动）。
- W3 / W4 / W5 / W6 各 `trace_jit` vs `luajit` 的 median ratio
  分别为 **1.69× / 3.73× / 1.27× / 0.26×**，全部满足任务验收（理想
  ≤ ×2，至少 ≤ ×5）；W6 反超 LuaJIT。

## 一、起步资源核对

```
worktree HEAD: 5faec46 (style: relon-fmt sweep of fixtures + examples after #relaxed edits)
patch:        /ext/relon/docs/internal/salvaged/F-D9-cmp_lua-958LoC.patch
patch base:    4ea13ac (~460 commits prior to main, but cmp_lua.rs unchanged since)
```

`git apply --check` 无任何冲突，单文件 patch（仅改 `crates/relon-bench/benches/cmp_lua.rs`）。
所需 cranelift 4 件套 + `relon-trace-abi` + `relon-trace-jit` +
`relon-codegen-native` 在 `relon-bench` 的 `Cargo.toml` 已就位
（line 23–51），patch 不需要额外调整 manifest。

## 二、wiring 设计

> 详细 builder 实现见 `cmp_lua.rs` line 156–863（`make_jit_module` /
> `entry_signature` / `build_w{3..6}_trace_fn` / `build_w5_fixture` /
> `build_w6_fixture` / `build_str_literals`），与 `cmp_lua_dict_list_trace.rs`
> 中 F-D8 落地的 hand-built 模板同构。

每个 `trace_jit` row 走一次性 cranelift JIT 构建：

| 行 | extern shim | 备注 |
|---|---|---|
| W3 trace_jit | `__relon_str_concat`（F-D7 leak-arena concat） | `for _ in 0..n { acc = concat(acc, "a") }`；写入 `result_slot = len(acc)` |
| W4 trace_jit | `__relon_str_contains`（F-D7 IC fast path） | `for _ in 0..n { if s.contains("x") { hits += 1 } }` |
| W5 trace_jit | `__relon_trace_dict_lookup`（F-D8 IC + shape guard）+ inline `ListGet` | `for i in 0..n { sum += dict[keys[i % 10]] }` |
| W6 trace_jit | inline `ListGet` lowering（F-D8 bounds-checked load） | `for i in 0..n { sum += arr[i] }` |

每个 JIT entry 的 ABI 都统一为：

```rust
unsafe extern "C" fn(*mut TraceContext, *const u64) -> i32
```

bench 路径：所有 row 都用 `b.iter_custom(|iters| timed_with_warmup(iters, || ...))`
保证三种实现的 measurement loop 同构。每次 bench 开跑前会先调一次
trace fn 校验 `tctx.result_slot` 与 analytic 答案逐字节相等
（`assert_eq!(tctx.result_slot as i64, w{3..6}_expected())`）；这步在
W4 trace 上即时暴露了 lit_x / lit_axb 指针互换会立刻 panic，作为防御
墙非常有效。

### 为什么是 hand-built 而不是 recorder

`TraceRecordingEvaluator` 当前不识别源端 `s + t` / `s.contains(_)` /
`d[k]` / `xs[i]` 这些 AST shape（F-D7 §3 + F-D8 §7 留的口子，对应未来
的 F-D7-B / F-D8-B）。F-D9 任务范围是"让 LuaJIT 对照数字反映 F-D7 /
F-D8 已经落地的 lowering"，hand-built trace 是 recorder 最终输出的
byte-identical 下限——当 recorder 集成落地后，本 bench 的 `trace_jit`
行可以直接切到 recorder 驱动，不会影响测得的时间。这一点在
`cmp_lua.rs` line 167–177 的注释中已写明。

## 三、gate 全过

| gate | 结果 |
|---|---|
| `cargo build --workspace` | OK，1m 02s（含 patch 前首次冷编 cranelift） |
| `cargo test --workspace` | OK，全员 pass（无 ignored 例外） |
| `cargo +stable clippy --workspace --all-targets -- -D warnings` | OK，0 警告 |
| `cargo +stable fmt --all -- --check` | OK（应用了一次 sweep） |
| `cargo build --target wasm32-unknown-unknown -p relon-wasm` | OK，30.96s |
| `cargo run -q -p relon-fmt -- --check fixtures/*.relon ...` | OK |

stable 版本：`cargo 1.95.0 / rustfmt 1.9.0-stable`。

## 四、bench 结果

机器：非 quiescent 开发机（schedutil governor + 1-min load 3.47，故
通过 `RELON_BENCH_FORCE_RUN=1` 跳过 quiescence 守卫）。采样：criterion
0.5 默认配置，sample_size = 100、warmup 3.0 s、measurement 5.0 s。
绝对时间略受 schedutil 频率漂移影响，但 ratio 是同进程同 measurement
窗口内的比值，方向性结论可信。

### 4.1 总览（median）

| Workload | N | tree_walk | trace_jit | luajit | trace_jit / luajit | tree_walk / luajit | trace_jit 相对 tree_walk |
|---|---|---|---|---|---|---|---|
| W3 string_concat | 2_000 | 12.117 ms | 2.3343 ms | 1.3794 ms | **1.69×** | 8.78× | ×5.19 |
| W4 string_contains | 10_000 | 66.100 ms | 71.332 µs | 19.098 µs | **3.73×** | 3461× | ×927 |
| W5 dict_str_key | 10_000 | 107.35 ms | 152.05 µs | 120.01 µs | **1.27×** | 894× | ×706 |
| W6 dict_num_key | 10_000 | 62.255 ms | 17.781 µs | 68.317 µs | **0.26×** | 911× | ×3501 |

### 4.2 验收

任务设定的关卡："理想 trace_jit ratio ≤ LuaJIT × 2，至少 ≤ × 5"。

- W3 / W5 / W6：均 ≤ ×2，W6 反超 LuaJIT。
- W4：×3.73，处于 [×2, ×5] 之间，未达理想线但通过保守关。

### 4.3 W4 ×3.73 的解读

W4 (`string_contains`) 的 luajit ≈ 19.1 µs / 10k iters ≈ 1.91 ns/op，
是 LuaJIT 把 `s.find / s:find` 在 trace IR 里折成专用 BMH 路径的结果。
trace_jit ≈ 71.3 µs / 10k iters ≈ 7.13 ns/op，每次调用要：
（a）跨越 SystemV C ABI 调一次 `__relon_str_contains`；
（b）IC fast-path 校验 needle pointer 是否命中再做 memmem。

如果未来把 `__relon_str_contains` 改成 inline-able 的 cranelift IR
（针对 needle.len ≤ 16 的 SIMD memchr 模板，省掉 C ABI 跨越），
预计可以再压到 ≈ 3 ns/op，对应 ratio ≈ ×1.5。当前未做，列为
F-D9 follow-up（见第六节）。

## 五、关键决策 / 取舍

1. **fmt sweep 一次**：rustfmt 1.9 把 patch 一处链式调用规范化（已在
   摘要中注明），未引入语义改动；这里没用 `--check` 红了就回滚 patch
   的策略，因为 patch 自身就是要被 commit 的代码，让它先按本仓 stable
   rustfmt 规范化更省事。
2. **W4 没追加 SIMD 内联**：F-D9 的硬性范围是 wiring 已落地的
   F-D7 / F-D8 lowering，新增 inline shim 属于 F-D7 范围（应该在
   F-D7-C 里补）。本阶段诚实记录 ×3.73 数字而不是先 inline 再报 ×1.5。
3. **保留 hand-built 注释**：bench 文件里写清了 hand-built vs recorder
   的边界，方便 F-D7-B / F-D8-B agent 把 row 切到 recorder 时一眼定位
   要替换的 builder。

## 六、遗留 TODO

1. **W4 trace_jit ×3.73 → 目标 ≤ ×2**：把 `__relon_str_contains` 在
   小 needle (`len ≤ 16`) 时切成 inline cranelift IR + SIMD memchr，
   消除 C ABI 跨越。归属 F-D7-C。
2. **recorder 自动 wiring**：F-D7-B / F-D8-B 把
   `TraceRecordingEvaluator` 教会识别 `s + t` / `s.contains(_)` /
   `d[k]` / `xs[i]` AST，并复用本文新增的 4 个 builder 作为预期 trace
   形状的 baseline。
3. **quiescent 机器复测**：当前数字来自 schedutil + load 3.47 的 dev
   box，正式 perf report 前应在 `scripts/bench_quiescence.sh` 通过的
   机器上重测，并对照 LuaJIT 数据更新 `relon-vs-luajit-final-report`。

## 七、变更清单

| file | LoC delta | 备注 |
|---|---|---|
| `crates/relon-bench/benches/cmp_lua.rs` | +958 | patch + 一次 fmt sweep；1174 → 2132 |
| `docs/internal/v6-fix-d9-cmp_lua-wiring-2026-05-20.md` | new | 本报告 |

`Cargo.toml` / 其他 crate / `relon-trace-jit` / `relon-codegen-native`
零改动；patch 完全是 bench-端 wiring。
