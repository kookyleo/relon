# Performance Baseline (2026-05-12)

> Snapshot 性质文档：当时 commit 的 baseline 数字。
> 用途：阶段 §G ADR 写作前的事实底线，不要求长期同步。
> 立项时回到 [`roadmap.md §G`](./roadmap.md) 看后续路径。

## Snapshot context

- commit: `e8bf99b`（`feat(lsp): textDocument/references via reverse forward-ref table`）
- rustc: `rustc 1.93.0 (254b59607 2026-01-19)`
- profile used: `release`（`lto = "fat"`, `codegen-units = 1`, `strip = true`），
  WASM 侧额外跑了 `release-small`（`opt-level = "z"`, `panic = "abort"`）做对比
- machine: Linux x86_64, Xeon E5-2609 v4 @ 1.70 GHz, 16 logical cores（host 是一台
  老款 server-grade 测试机，绝对数字偏保守；相对比例有参考价值）
- bench corpus（`crates/relon-bench/src/main.rs`，纯 `std::time::Instant` 计时）：
  - `simple`: `{ val: 1 + 2 * 3 / 4.0 }` —— 算术表达式 + 一个 record field
  - `complex`: `{ "list": [x * 2 for x in range(1000) if x % 2 == 0], "check": &sibling.list }`
    —— 含 1000-elem comprehension + sibling 引用
  - 每条单跑一次冷启动；`simple` 再跑 1000 次取均值作稳态参考

## Desktop numbers

| benchmark | sample size | wall time / op | notes |
| --- | --- | --- | --- |
| simple parse (cold) | 1 | 165.897 µs | 含 alloc/JIT-cache 预热成本 |
| simple eval (cold) | 1 | 227.425 µs | 含 module resolver 首次装配 |
| simple parse (steady) | 1000 avg | 24.949 µs | 稳态参考 |
| simple eval (steady) | 1000 avg | 43.496 µs | 稳态参考 |
| complex parse (cold) | 1 | 152.334 µs | source 字节数与 simple 同量级 |
| complex eval (cold) | 1 | 6.980482 ms | 含 1000-elem comprehension + sibling 解析 |

原始 stdout 关键行（截留 + 关键数字）：

```
--- Relon Performance Benchmark ---
Simple Arithmetic ('{ val: 1 + 2 * 3 / 4.0 }'):
  Parse: 165.897µs
  Eval : 227.425µs
  Total: 393.322µs

Complex Logic (Range 1000 + Sum):
  Parse: 152.334µs
  Eval : 6.980482ms

Average over 1000 iterations (Simple):
  Mean Parse: 24.949µs
  Mean Eval : 43.496µs
```

冷启动 parse 比稳态高约 6×、eval 高约 5×，反映 first-call 路径上有一次性
开销（模块装配 + allocator warm-up）。`complex.eval` 与 `simple.eval` 稳态
之比约 6.98 ms / 43 µs ≈ 160×，与 1000-elem comprehension 量级一致；说明
解释器在 comprehension hot loop 上是线性的、没有非预期的二次行为。

## WASM build sanity

`rustup target add wasm32-unknown-unknown` 已就位。本节只验证 library crates
能否直 build；bin crates（`relon-cli` / `relon-lsp` / `relon-fmt`）含
`std::env::args` / `std::fs::*` / `lsp-server` 等 wasm-unfriendly 依赖，按
任务约束不在范围内。

| crate | target | profile | result | rlib size | notes |
| --- | --- | --- | --- | --- | --- |
| `relon` | wasm32-unknown-unknown | release | ok | 400 312 B (391 KB) | facade，re-export only |
| `relon-parser` | wasm32-unknown-unknown | release | ok | 1 420 486 B (1.36 MB) | winnow + miette 依赖 |
| `relon-analyzer` | wasm32-unknown-unknown | release | ok | 2 086 084 B (1.99 MB) | 最大 rlib，含 schema/symbol/diag |
| `relon-evaluator` | wasm32-unknown-unknown | release | ok | 1 691 802 B (1.61 MB) | 含 stdlib intrinsic |
| `relon` | wasm32-unknown-unknown | release-small | ok | 259 416 B (253 KB) | -35% vs release |
| `relon-parser` | wasm32-unknown-unknown | release-small | ok | 1 594 912 B (1.52 MB) | +12% vs release（`opt-level=z` 对 winnow 反优化）|
| `relon-analyzer` | wasm32-unknown-unknown | release-small | ok | 1 979 240 B (1.89 MB) | -5% vs release |
| `relon-evaluator` | wasm32-unknown-unknown | release-small | ok | 1 663 096 B (1.59 MB) | -2% vs release |

**注意**：上表是 `.rlib` 体积（含 metadata + bitcode 的中间产物），**不是
链接后 deployable `.wasm` 体积**。library crate 只产 rlib；实际部署 size
需要 cdylib + `wasm-opt` 走一遍，本任务边界外，留给 ADR。rlib 数字此处仅
作"代码总量量级"的上界参考与 release / release-small profile 之间的相对
对比。

结论：library 侧 wasm32 build 路径无阻塞，4 个 crate 都能在两个 profile 下
干净编译；无任何 `getrandom` / `mio` / `socket` / `SystemTime` 等需要
polyfill 的 wasm32 报错。

## Coverage gaps（现有 bench 没覆盖的）

- 仅 2 段 source corpus，无法分维度归因（parser vs evaluator vs reference
  resolution vs schema validation）。
- 无 schema-rooted dispatch 路径的专项基准（无 `#schema` 触发的命名空间
  函数 / 值方法实测）。
- 无 forward-ref / `&sibling` 链路深度的扫描型测试，无法体现 analyzer 反向
  表（`textDocument/references` 用的那张）在评估期的影响（如果有）。
- 无大对象 / 深嵌套 record 的内存峰值数据，eval 路径上的 alloc 频度未量化。
- 无并发 / 多租户场景（多 `Evaluator` 实例并发 eval 同 `Context`）。
- 无 native function 调用边界 overhead 数据（`NativeFnGate` capability
  check 的均摊成本）。
- 计时仅 `std::time::Instant` 单次或简单均值，无 criterion 风格的方差 /
  outlier 统计；当前数字应视为 order-of-magnitude，不是严谨 benchmark。
- WASM 侧只验证了 build，无运行时性能数据；wasm 解释器 dispatch 是否与
  desktop 同数量级未测。

## Open questions for ADR

- 桌面端瓶颈维度是 evaluator dispatch、reference resolution、schema 验证还是
  comprehension hot loop？本轮没拆 profiler（无 `perf` / `cargo flamegraph`
  数据），只有 wall time。
- WASM 端部署 size 预算是多少？由 embedder 决定。当前 rlib 总和 ≈ 5.6 MB
  release（4 crate），cdylib + `wasm-opt -Oz` 通常可压到原始 `.wasm` 的
  20–40%，但需实测确认。
- 多租户 / 多实例场景下的优先级是 throughput（吞吐）还是 latency（尾延
  迟）？两者的优化方向（pooling vs IR caching vs JIT）会发散。
- comprehension hot loop 的 6.98 ms / 1000-elem 是否进入 acceptable 区间？
  取决于目标场景（配置时 evaluate-on-save 可接受 ms 级；运行时 eval-per-
  request 需要 µs 级）。
- 是否需要在 desktop / wasm 各定一组独立 KPI（比如 desktop 看 µs/op，
  wasm 看 size + 启动延迟）？还是统一指标？
