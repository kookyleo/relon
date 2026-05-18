# Relon 性能报告 — v5-β-2 stage 4（2026-05-18）

> 本文档定位：v5-β-2 stage 4 收官的**性能交付物**。继承
> `docs/internal/wasm-bench-report-2026-05-16.md`（已 [archived]）
> 的"性能报告"角色，但记录对象换成 cranelift-AOT vs tree-walk —
> wasm-AOT 后端已在 stage 4 退役（commit `b6b4470`）。
>
> Bench 入口：`cargo bench -p relon-bench --bench cranelift_aot_vs_tree_walk`。
> 源码：`crates/relon-bench/benches/cranelift_aot_vs_tree_walk.rs`。

## 一、执行摘要

针对纯算术 `#main(Int x, Int y) -> Int : x + y` 场景，criterion 0.5
在 release profile（fat LTO + codegen-units = 1）下采集 50-sample ×
5s 测量窗口的数据（stage 4 现场实测，2026-05-18）：

| 探针 | 中位数 | low (95 % CI) | high (95 % CI) | 含义 |
|---|---:|---:|---:|---|
| `cranelift_cold` | **275.4 μs** | 275.3 μs | 275.6 μs | 合成 IR + cranelift JIT compile + finalize |
| `cranelift_warm` | **415.2 ns** | 413.1 ns | 419.9 ns | 复用 evaluator，单次 `run_main(args)` 全开销 |
| `tree_walk_total` | **1.260 ms** | 1.250 ms | 1.272 ms | 每次 iter 重建 Context + TreeWalkEvaluator |
| `tree_walk_warm` | **2.352 μs** | 2.348 μs | 2.361 μs | 复用 walker，单次 `run_main` |

数字与 stage 1 同套件（`v5b1_arithmetic`，archived，cranelift cold
245 μs / warm 390 ns）量级一致 —— buffer-protocol entry shape 在
stage 2 落地之后单 op 路径上多了一对 4-arg marshaling，导致 warm
路径多 ~25 ns、cold 路径多 ~30 μs。这是符合预期的常数级开销，并未
改变 cold/warm tier 的定性结论。

三条结论性观察：

1. **cranelift warm invoke 415 ns**：已落进 LuaJIT trace tier 量级
   （目标 0.3-0.5 μs）。整条路径只有 `Arc::as_ptr` → `catch_unwind`
   wrapper → 直接 `extern "C"` 调用 + 4-arg buffer-protocol marshal。
2. **cranelift cold 275 μs**：跳过 parse / analyze / lower 三关，
   单纯 JIT compile + finalize。比 wasm-AOT stage 1 的 4.20 ms cold
   start 快约 **15×**（wasm 路径要付双层 cranelift：wasm encode +
   wasmtime 内部 cranelift compile）。
3. **tree-walker warm 2.35 μs**：cranelift warm 比 tree-walker warm
   快 **5.7×**，差距来自 IR dispatch loop overhead 与 schema look-up
   chain；cranelift 的 native code 把这两块全部固化到机器码里。
   cranelift cold（275 μs）摊销到一次 run_main 也只需 ~660 次 warm
   调用就能赢过 tree-walker warm —— 典型 long-running 服务进程
   一秒内就能达成。

## 二、Stage 4 现场实测

> 实测命令：`cargo bench -p relon-bench --bench cranelift_aot_vs_tree_walk`。
> 测试机：dev workstation（同 stage 3 报告）。release profile：fat
> LTO + codegen-units = 1，criterion measurement_time = 5s, sample = 50。

stage 4 现场实测（2026-05-18）：

```
v5b2_stage4_arithmetic/cranelift/cold   [275.29 µs 275.44 µs 275.62 µs]   (3/50 outliers, all high mild)
v5b2_stage4_arithmetic/cranelift/warm   [413.14 ns 415.21 ns 419.93 ns]   (6/50 outliers)
v5b2_stage4_arithmetic/tree_walk/total  [1.2503 ms 1.2599 ms 1.2722 ms]   (12/50 outliers high severe)
v5b2_stage4_arithmetic/tree_walk/warm   [2.3477 µs 2.3519 µs 2.3606 µs]   (3/50 outliers)
```

`tree_walk/total` 的 12 个 high-severe outlier 反映 tree-walker 冷启动
路径包含 `relon_analyzer::analyze` + `Context` 装配 + `prepare_in_place`
（stdlib / decorator 注入），上面随 GC pause / 调度抖动较大；中位数
仍然稳定在 1.26 ms。

archived 对照基线（stage 1 cranelift β-1，2026-05-18 早些时同机
跑出来的 `v5b1_arithmetic`）：

```
v5b1_arithmetic/cranelift/cold   245.5 µs        (stage 4 多 ~30 µs，来自 buffer-protocol prologue + tail-cursor init)
v5b1_arithmetic/cranelift/warm   390.4 ns        (stage 4 多 ~25 ns，4-arg marshal)
v5b1_arithmetic/wasm/cold        4.20 ms   [archived]
v5b1_arithmetic/wasm/warm        1.09 µs   [archived]
```

## 三、Cranelift backend 当前覆盖范围（v5-β-2 stage 3 + stage 4）

stage 3 报告（`docs/internal/v5-beta-2-stage3-report-2026-05-18.md`）
记录的 51/52 corpus 覆盖度在 stage 4 保持不变 —— stage 4 只做了
"删除 wasm-AOT" + bench 改造，不改 IR lowering 矩阵。

| 维度 | 覆盖 | 备注 |
|---|---:|---|
| `arith_control` | 27/28 | 唯一缺口 `let_chain` 是 analyzer-rejected，tree-walk-only by construction |
| `stdlib_simple` | 9/9 | length / substring / starts_with / abs / min / max / list_int_sum / list_int_max / list_string_sum |
| `stdlib_memory` | 4/4 | concat / substring / starts_with（scratch arena） |
| `stdlib_case_fold` | 5/5 | upper / lower / title + Greek / Turkish locale 变体 |
| `stdlib_list` | 2/2 | list_int_sum / list_int_max（pure iteration shapes） |
| `stdlib_normalize` | 2/2 | nfc / nfd / nfkc / nfkd |
| `dict_return` | 2/2 | 含 sub-record + tail-cursor 协议 |
| **总计** | **51/52** | **唯一缺口为 analyzer-only case，非 cranelift 缺陷** |

## 四、与 wasm-AOT 历史对照（[archived]）

stage 1 的 wasm-AOT 性能档案完整保留在
`docs/internal/wasm-bench-report-2026-05-16.md`，附录 A.5 ~ A.21 标记
为 `[archived]`。核心对照数据（stage 1 `v5b1_arithmetic` 套件，
2026-05-18 同机实测）：

| 后端 | cold | warm |
|---|---:|---:|
| cranelift-AOT (stage 4) | 275 μs | 415 ns |
| cranelift-AOT (stage 1) | 245 μs | 390 ns |
| wasm-AOT [archived] | 4.20 ms | 1.09 μs |
| 倍率（cranelift stage 4 优 = ↑）| 15× | 2.6× |

wasm-AOT 在 cold start 上劣势主要来自双层 cranelift（自身 wasm
encode + wasmtime 内部 cranelift compile）；warm 上劣势主要来自
wasmtime `Store::new + Linker::instantiate` + buffer marshal 的
固定开销。这两块都不是 codegen-wasm 实现质量问题，而是 wasmtime
本身的成本结构 —— 这也是 v5-β-2 stage 4 选择直接退役 wasm-AOT 的
直接原因（cranelift 既快又覆盖更广）。

## 五、v5-γ / v6-γ 入口

stage 4 已完成的：

- ✅ wasm-AOT 后端 + crate + facade + CLI 全删干净（commit
  `b6b4470 chore(workspace): retire relon-codegen-wasm crate +
  wasm-AOT facade`）。
- ✅ bench 改造为 `cranelift_aot_vs_tree_walk`。
- ✅ wasm-bench-report-2026-05-16 标记 deprecation prologue + 附录
  A.5 ~ A.21 标记 `[archived]`。
- ✅ 本报告（relon-perf-report-2026-05）替代 wasm-bench-report 的
  性能交付物角色。

stage 4 保留的 deferred 项（来自 stage 3 报告，stage 4 维持原
deferred 状态，下放给 v5-γ 跟进）：

1. **`Op::CallNative` 完整 indirect dispatch（Phase C.1）** — 当前
   `CheckCap` 已验证 vtable 槽非空；下一步 `call_indirect` + per-
   `(param_tys, ret_ty)` cranelift `SigRef` 表 + 指针 indirect arg
   marshaling。
2. **`Op::CallClosure` + 闭包入参 list 高阶（Phase C.4）** — 闭包
   ABI（`closure_table` on `IrModule` 已 plumb 到 codegen 入口）+
   captures buffer 实例化 + `call_indirect` against captured fn ptr。
3. **`Op::Loop { result_ty: Some(_) }` + `Op::BrTable` + 内层 loop
   `RESOURCE_CHECK_INTERVAL` 重查节奏（Phase C.2）** — 当前 IR
   bodies 全部 `result_ty: None` + 显式 acc 累加变量；带 yield 的
   loop 走 v5-γ。
4. **真 `sigsetjmp` / `siglongjmp` trap handler（Phase C.3）** —
   `signal-hook` 0.3 + libc，进程级 install once。当前
   `catch_unwind` 路径在功能上等价；2 ns/guard 收益不是热路径关键。
5. **`cranelift-object` 模块缓存** — 把 .o 序列化到磁盘，cold
   start 可跳过整个 JIT compile（仅做 mmap + relocation）。设计稿
   见 `docs/internal/v5-gamma-cranelift-object-cache-design.md`。

v6-γ 远期：trace JIT 与本路径正交。设计稿
`docs/internal/v6-gamma-trace-jit-design.md`。

## 六、Bench 数据采集方式

```bash
cd /path/to/relon
cargo bench -p relon-bench --bench cranelift_aot_vs_tree_walk
# 默认 measurement_time=5s × sample_size=50
# 输出落在 target/criterion/v5b2_stage4_arithmetic/*/estimates.json
```

CI 上 bench 跑的是同一份 bench binary（见
`.github/workflows/bench.yml`）；CI 数字噪声较大（共享 runner，
无 CPU pin），所以以本地 perf-runner 数字为准。

## 七、Gate（stage 4 final）

stage 4 收尾 gate（feature 调整后，`cranelift-aot` 是 `relon`
crate 的 default feature，CLI / 测试不再需要显式 feature flag）：

| Gate | 命令 | 结果 |
|---|---|---|
| build | `cargo build --workspace` | ✓ |
| test | `cargo test --workspace --no-fail-fast` | 1483 passed / 0 failed |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | ✓ |
| fmt | `cargo fmt --all -- --check` | ✓ |
| wasm32 | `cargo build --target wasm32-unknown-unknown -p relon-wasm` | ✓ |

baseline 1790 → stage 4 1483 是因为 wasm-AOT 移除带走了 ~300
个 wasm-codegen specific tests（aot_cache_smoke、binary_handshake_smoke
等 18 个 smoke files）。stage 4 没有引入新测试 —— Phase C 系列
deferred 项推到 v5-γ，所以 test 数量目前是 1483。

---

**Author**: Relon perf 直路 v5-β-2 implementer agent（stage 4）
**Date**: 2026-05-18
**License**: Apache-2
