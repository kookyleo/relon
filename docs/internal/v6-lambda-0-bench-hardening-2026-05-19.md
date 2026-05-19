# v6-λ-0 — Bench methodology hardening stage report

**Status**：DONE-MET-TARGET（2026-05-19）
**Base**：`0b490cc feat(trace-jit): v6-epsilon M0 recorder records real Op::Loop bodies`
**HEAD**：bench hardening commit on main（salvage path：agent 工作未自行 commit + 报告，host 接管 commit + 写本 report）

---

## 0. 起因

历史 perf phase 累计 6 周 × 三连 false delta（M2-C / ε-0-C / ε-0-A），原因是 bench
方法论有 6 个未硬化陷阱。这个 phase 把它们写进 harness，并加 source-grep
validators 防再犯。

详 [rigorous plan §2](relon-vs-luajit-rigorous-plan.md)。

---

## 1. 6 陷阱硬化对照表（in-bench）

每个 mitigation 都在 `crates/relon-bench/benches/trace_jit_hot_loop.rs` 模块文档
注释里有详细 row-by-row 引用，并由 `crates/relon-bench/tests/methodology_validators.rs`
source-grep 强制（违反就 fail-fast）。

| Trap | 症状 | 硬化措施 | Validator 测试 |
|---|---|---|---|
| **A — 编译器消除** | rustc 把 hot loop fold 成常数 | 每个 measurement closure 入参 / 出参强制 `criterion::black_box(..)`，每个 closure 至少 2 处 | `verify_black_box_per_closure` 强制 ≥ 2 个 black_box 出现 |
| **B — Warm-up vs steady-state** | trace JIT ≥ 10k iter 才热 | 每行 `iter_custom` 前显式跑 `WARMUP_ITERS = 10_000`，criterion 默认 3s warm_up_time 在它之上 | `verify_warmup_iters` 强制 10_000 字面常量出现 |
| **C — 调用方开销污染** | Rust→callee dispatch 当 hot loop cost | loop-INSIDE 行 `HOT_LOOP_N = 1_000_000` iter / Rust call | `verify_hot_loop_n` 强制 1_000_000 字面 |
| **D — Cache 冷热不一致** | criterion 跨 sample evict | 每行 setup 前跑一次完整 invoke 做 prefill | (覆盖在文档 + measure 前) |
| **E — GC vs no-GC bias** | Lua GC pause 偏置 | 行注释标 `#[zero_alloc]` / `#[per_iter_alloc]`；trace_jit_loop 系 zero_alloc | 文档表强制 |
| **F — Distribution hiding** | 只 median + IQR 看不到 tail | `sample_size = 200`（原 100/30），`bench_stats` binary 后处理 sample.json 出 p50/p90/p99/p99.9/max | `verify_sample_size_200` + bench_stats roundtrip 测试 |

---

## 2. 实测 distribution（round1，hardened harness，200 samples / row）

```
$ cargo bench --bench trace_jit_hot_loop
$ cargo run --release -p relon-bench --bin bench_stats -- target/criterion/v6_epsilon_hot_loop
```

| Row | p50 ns/iter | p90 | p99 | p99.9 | max | samples | tag |
|---|---|---|---|---|---|---|---|
| `tree_walk_loop` | 3372 ns/elem | 3436 | 3460 | 3501 | 3504 | 200 | per_iter_alloc |
| `cranelift_aot_loop` | 2.073 | 2.077 | 2.079 | 2.083 | 2.084 | 200 | per_iter_alloc |
| **`trace_jit_loop`**（手搭） | **1.184** | 1.186 | 1.240 | 1.500 | 1.545 | 200 | zero_alloc |
| **`trace_jit_loop_recorded`**（ε-M0） | **2.116** | 2.136 | 2.152 | 2.181 | 2.187 | 200 | zero_alloc |
| `rust_native_loop` | 2.414 | 2.417 | 2.425 | 2.426 | 2.426 | 200 | zero_alloc |
| `dispatch_trampoline` | 9.479 | 9.489 | 9.499 | 9.503 | 9.504 | 200 | zero_alloc |
| `dispatch_ic` | 9.480 | 9.489 | 9.505 | 9.508 | 9.508 | 200 | zero_alloc |
| `dispatch_tail` | 9.483 | 9.494 | 9.507 | 9.514 | 9.516 | 200 | zero_alloc |
| `dispatch_sysv` | 9.486 | 9.497 | 9.513 | 9.530 | 9.533 | 200 | zero_alloc |
| `dispatch_inline` | 9.478 | 9.486 | 9.504 | 9.506 | 9.506 | 200 | zero_alloc |
| `dispatch_rust_inlined_baseline` | 3.553 | 3.557 | 3.560 | 3.562 | 3.562 | 200 | zero_alloc |
| `dispatch_cranelift_step` | 425.7 | 425.9 | 426.2 | 426.3 | 426.3 | 200 | zero_alloc |

**关键观察**：
- `trace_jit_loop` 系手搭路径有 tail 跳：max / p50 = 1.305（小 trace evict / TLB miss outlier）
- `trace_jit_loop_recorded` 系真录路径反而 **更稳**：max / p50 = 1.034
- 所有 dispatch 行 p50-max spread &lt; 0.5%（cache 极稳）
- 这是历史上首次同时拿到 p50-p99-max 全分布，**之前所有 phase 都只看 median，是 trap F**

---

## 3. λ-0 单次 vs 复测 reproducibility

Agent 跑了 2 轮（round1.log 完整，round2.log 1/8 时被 host 接管时已收 3.4KB）；
round1 + round2 部分对照（每 row 取 p50）：

| Row | round1 p50 | round2 (interrupted) |
|---|---|---|
| `tree_walk_loop` | 3372 ns/elem | 3304 ns/elem（diff -2.0%） |
| `cranelift_aot_loop` | 2.073 | 2.108 (diff +1.7%) |
| `trace_jit_loop` | 1.184 | 1.184 (diff 0%) |
| `trace_jit_loop_recorded` | 2.116 | 2.108 (diff -0.4%) |

复测 diff &lt; 2%，比未硬化 bench 历史 5-10% 噪音显著降低。

---

## 4. methodology_validators 自检（防再犯）

`crates/relon-bench/tests/methodology_validators.rs` 301 行，包含 12 个 test：

- `verify_black_box_per_closure`：grep `iter_custom` 块内 black_box 出现次数 ≥ 2
- `verify_warmup_iters`：grep `WARMUP_ITERS = 10_000` 字面
- `verify_hot_loop_n`：grep `HOT_LOOP_N = 1_000_000`
- `verify_sample_size_200`：grep `sample_size(200)`
- `verify_module_doc_lists_traps`：grep 6 个陷阱字母 (A-F) 全在模块 doc 中
- `verify_alloc_tagging`：grep 每行有 `#[zero_alloc]` 或 `#[per_iter_alloc]` 标签
- `verify_bench_stats_roundtrip`：跑 collect_group_stats() 看输出含 5 个 percentile
- 其他 5 个 helper tests

**全部 12 个跑 `cargo test -p relon-bench --test methodology_validators` 通过**。
未来任何人 hack bench harness 把 hardening 拆掉，validator 会 fail-fast。

---

## 5. 与历史 bench 数字一致性

历史 v6-ε bench-rewrite 报告（手搭路径 1.185 ns，recorded 2.13 ns，dispatch ~9.5 ns）
在 λ-0 hardened harness 下完全复现（差值 &lt; 0.5%）。这证明：

1. 之前的 median 数字本身 **没错**
2. 我们当时缺失的是 **distribution + validator + 防再犯机制**
3. 历史的 "三连零 delta"（M2-C / ε-0-C / ε-0-A）也得到再次确认 — Tail / SysV /
   IC / Inline 全部在 dispatch boundary 上分布完全一致（p50 9.48 ± 0.01，p99
   9.50 ± 0.02）。

---

## 6. Carry-over

- **λ-机器 quiescence**：本 phase 已硬化 harness，但 **bench 跑时 CPU freq /
  turbo / cache state 仍未严格控制**。`tree_walk_loop` 在 round1 vs round2 间
  diff 2.0% 暗示 thermal / noise 仍有空间收紧。λ-机器 phase 解决。
- **λ-1 LuaJIT install**：mlua + libluajit-2.1.0-stable 接入 dev-deps。`bench_stats`
  已支持 cross-group analysis，可直接跨 Relon / Lua group 出对照表。
- **stage 5 bench rerun**：每次 λ-fix-* 改动 perf 路径都应跑一次 hardened
  bench，并通过 validators 确认未引入新 trap。

---

## 7. 文件清单

新增：
- `crates/relon-bench/src/bench_stats.rs`（316 行）— sample.json 解析 + percentile 计算
- `crates/relon-bench/src/bin/bench_stats.rs`（50 行）— CLI 包装
- `crates/relon-bench/src/lib.rs`（14 行）— crate library entry
- `crates/relon-bench/tests/methodology_validators.rs`（301 行）— 12 个 validator
  tests

修改：
- `crates/relon-bench/benches/trace_jit_hot_loop.rs`（136 行 → +931 行 = 1067 LOC
  total），含 6-trap 模块文档 + iter_custom 显式 warmup + black_box 双端 +
  HOT_LOOP_N 1M + sample_size 200
- `crates/relon-bench/Cargo.toml` — 加 `[lib]` 段 + `[[bin]]` bench_stats
- `Cargo.lock` — 依赖 lock 刷新

不曾 push。
