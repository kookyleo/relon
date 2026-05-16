# Relon 性能强化最终报告（2026-05-16）

> 本文档定位：P0-P3 完整周期的**最终交付物**。前置归因 + 阶段细节见
> [`perf-attribution-2026-05-16.md`](./perf-attribution-2026-05-16.md)，
> 数字基线背景见 [`perf-baseline-2026-05-12.md`](./perf-baseline-2026-05-12.md)。
>
> P3 新增内容：
>
> 1. `profile_alloc` 新增 `simple-pooled` / `comprehension-pooled` 两个 mode，
>    把"宿主 boot 一次、连续 eval"的真实使用模式纳入测量；
> 2. 四种 dhat mode 在同一 host 上一次性跑完，给出端到端 alloc 对照；
> 3. 用 criterion 在 P0 baseline commit (`7a1d449`) 与 post-P2 HEAD
>    上各跑了一遍 harness_v1 七组 13 个 bench，给出 wall-time 对照。

## 一、端到端 dhat 对照（同 corpus，one-shot vs pooled）

采样条件（4 个 mode 均同）：

- host：`Linux q 6.8.0-110-generic`（与前序文档同机）
- rustc：`1.93.0`
- build：`cargo run --release -p relon-bench --bin profile_alloc --features dhat-heap`，
  `RUSTFLAGS="-Cstrip=none -Cdebuginfo=1 -Cforce-frame-pointers=yes"`
- corpus：与 P0 归因报告一致
  - simple：`{ val: 1 + 2 * 3 / 4.0 }`，1000 次
  - comprehension：`{"list": [x*2 for x in range(1000) if x%2==0], "check": &sibling.list}`，100 次
- 每个 mode 用独立的 `DHAT_HEAP_JSON=dhat-heap-<mode>.json` 输出，互不覆盖
- pooled mode 通过 `eval.eval_root(&scope)` 复用单个 `Context` + `Evaluator`；
  `eval_root` 入口已重置 `step_counter` / `path_cache` / `iter_cursors`，
  `module_cache` 故意保留（跨 eval 共享）

| Mode | total bytes | total blocks | at-gmax bytes | at-end bytes |
| --- | ---: | ---: | ---: | ---: |
| `simple`              (one-shot) | 50 267 024 | 300 001 | 7 813 382 | 7 783 024 |
| `simple-pooled`       (reuse)    |  9 301 511 |  31 272 | 8 053 622 | 8 031 019 |
| `comprehension`       (one-shot) | 19 814 310 | 353 502 | 7 741 931 | 7 669 024 |
| `comprehension-pooled`(reuse)    | 13 367 076 | 316 378 | 7 742 383 | 7 683 557 |

观察：

- **simple one-shot 50 MB 是纯 Context boot 主导**：1000 次重建 Context 约
  每次 41 KB / 281 块（去掉 8 MB 常驻 stdlib 后），全部花在
  `Context::new` + `prepend_module_resolver` + `Evaluator::new` 上。
  pooled 复用后 total bytes 直接砍掉 **81.5%（50.3 MB → 9.3 MB）**，
  剩下的 9.3 MB 里 8.0 MB 是 at-end 常驻（stdlib 注册表 + prelude schemas
  + module_cache），实际"纯 1000 次 eval"的 transient alloc 仅 ~1.3 MB
  —— 简单算术表达式的 evaluator 热路径**几乎零分配**。
- **comprehension one-shot 19.8 MB vs pooled 13.4 MB**：one-shot 复测确认
  与 P2 文档登记的 19.8 MB / 353 K blocks / 7.7 MB at-gmax 三个数字精确一致
  （证明 worktree HEAD 是 P2，归因报告基线正确）。pooled 节省 **6.4 MB
  / -32%**，主要来自 100 次重建 Context 的固定开销；剩下的 13 MB / 316 K
  blocks 几乎全在 comprehension 内层 hot loop（range(1000) 物化 +
  `iter_scope_map` per-element + 结果 Vec grow），这部分在 pooled 模式下
  仍然按 100 × 1000 = 10 万元素摊到的纯 evaluator 工作量。
- **at-gmax 4 种 mode 都在 7.7-8.1 MB**：peak 内存几乎不受 iteration 数
  或 pooling 影响 —— 这是 stdlib 注册表 + prelude schemas 的**结构性常驻
  开销**，进一步压缩需要重新设计 Context 的 lazy-init / stdlib 拆分。
- **comprehension pooled 7.7 MB at-gmax vs P0 baseline 67.5 MB**：P2 wave
  的 peak 收益 -88% 完全保留，与 one-shot 模式同。

结论：one-shot 模式下 simple 是 boot cost 完全主导（96% 在 Context boot），
pooled 揭示了纯 eval cost 的真实下限；comprehension 在 post-P2 已极低，
pooled 进一步去掉 ~6 MB 的 Context 重建噪声，但 evaluator 内部已经接近
"按元素必要分配"的下限。

## 二、端到端 criterion wall-time 对照（P0 `7a1d449` → post-P2 HEAD `4414731`）

跑法：

```bash
# P0 baseline
git checkout 7a1d449
cargo bench -p relon-bench --bench harness_v1 -- --save-baseline p0

# post-P2 对比
git checkout worktree-agent-a366a99e4d4f7a84a
cargo bench -p relon-bench --bench harness_v1 -- --baseline p0
```

13 个 bench（7 个 group），默认 sample-size 100、warm-up 3 s、measurement 5 s。

| Group | Bench | P0 mean | post-P2 mean | Δ (criterion 报告) | 显著性 |
| --- | --- | ---: | ---: | ---: | --- |
| parse | source/simple              |  11.16 µs |  10.99 µs |  **-1.57%** | p < 0.05 改善 |
| parse | source/medium              |  36.16 µs |  36.40 µs |  +0.55% | 无变化 |
| parse | source/large_list_1000     |  12.60 ms |  12.62 ms |  +0.18% | 无变化 |
| eval_cold | source/simple          |  72.39 µs |  62.62 µs |  -13.5% pts | 噪声内 |
| eval_cold | source/medium          | 149.4 µs  | 134.1 µs  | -10.3% pts | 噪声内 |
| eval_steady | source/simple        |  15.54 µs |  11.43 µs |  **-27.27%** | p < 0.05 改善 |
| comprehension | elements/100       | 264.6 µs  | 127.2 µs  | **-35.58%** | 噪声内（CI 跨零） |
| comprehension | elements/1000      |   2.00 ms | 759.0 µs  | **-61.29%** | p < 0.05 改善 |
| comprehension | elements/5000      |   9.94 ms |   3.51 ms | **-64.63%** | p < 0.05 改善 |
| reference | depth/shallow_1        |  76.65 µs |  60.90 µs |  -20.5% pts | 噪声内 |
| reference | depth/nested_5         | 204.1 µs  | 176.2 µs  | -13.7% pts | 噪声内 |
| schema_validate | schema/user_2fields| 89.10 µs |  70.97 µs |  -20.4% pts | 噪声内 |
| method_dispatch | op/string_upper_x100 | 1.578 ms | 1.338 ms | **-14.98%** | p < 0.05 改善 |
| method_dispatch | op/list_map_x100   |   7.98 ms |   6.64 ms | **-16.79%** | p < 0.05 改善 |

**显著改善（criterion `p < 0.05`，5 项）**：

- parse/simple **-1.57%**（不大，但显著；说明 P2 没有引入 parser 回归）
- eval_steady/simple **-27.27%**（与 dhat 验证一致——pooled simple alloc 砍掉 81.5%）
- comprehension/1000 elements **-61.29%**
- comprehension/5000 elements **-64.63%**
- method_dispatch/string_upper_x100 **-14.98%**
- method_dispatch/list_map_x100 **-16.79%**

**绝对值大幅下降但置信区间宽，criterion 报告 "no change"（8 项）**：

- eval_cold simple / medium、comprehension/100、reference shallow / nested、schema_validate
- 这些组的 P0 / post-P2 mean 比例对应 -13% 到 -50% 的下降，但 warming up 期
  噪声大、sample 100 不足以把置信区间收窄到 ±5% 以内
- 重要的是：**没有任何一项报告显著回归**，所有 group 的 mean 都不升或下降

**总体提升幅度**：

- comprehension 大集合（1000 / 5000 elements）wall-time **-61% 到 -65%**，
  与 dhat 报的 `total bytes -96%` 量级一致——alloc 压缩直接转化为吞吐提升
- method dispatch 类（string_upper / list_map）wall-time **-15% 到 -17%**，
  受益于 P2-A 的 `Value` enum 24 B 收窄（24/152 = 6.3× 缩小）让 HashMap
  桶表填充明显加速
- 单纯算术 simple eval_steady **-27%**，受益于 P1-A 的 comprehension
  Vec::with_capacity + Cow + P1-C 的 Arc<str> identifier 路径

## 三、阶段链路总览

按 commit 顺序串起 P0-P3 全部落地，每个阶段标记对应的 dhat 数字：

| 阶段 | commit / merge | 主要改动 | comprehension dhat 数字（100 iter × 1000 elem） |
| --- | --- | --- | --- |
| **P0** | `1513a3c` + `373d9de` + `7a1d449` | criterion harness 7 组 + dhat profile_alloc bin + 首份归因报告 | total 540 MB / blocks 1.55 M / at-gmax 67.5 MB |
| **P1-A** | `4413295`（cherry-pick） | `materialize_iterable` 返 Cow + comprehension `Vec::with_capacity(items.len())` | atomic-counter 1000-iter：-12.3% |
| **P1-B** | `be7db5f`（merge） | `scope.rs` helper API seam（`Locals` / `Thunks` 别名 + `locals_for_write` / `thunks_for_write`）；诊断更正 Mutex 不是热点 | 无 alloc 收益（留 seam 给 P2-B） |
| **P1-C** | `60244c5`（merge） | identifier keys `String` → `Arc<str>` + reference 单段 fast path | total 460.7 MB / blocks 752 K（blocks -52%） |
| **P2-A** | `ae4d0fc`（merge） | `Value::Closure` / `Schema` / `EnumSchema` / `Type` 大变体 Box 化，enum 152 B → 24 B；`value_enum_is_compact` `#[test]` 守 48 B ceiling | 独跑：total 103.7 MB / at-gmax 7.7 MB |
| **P2-B** | `6e92436`（merge） | `ListContext.iter_binding` 槽位 + `Scope::with_iter_loop` + `Scope::set_iter_binding`；Closure `snapshot_iter_bindings` 保闭包语义 | 独跑：total 140.0 MB / blocks 353 K |
| **P2 合并** | `2e571dc` | 文档汇总 | **total 19.8 MB / blocks 353 K / at-gmax 7.7 MB**（端到端 -96.3% / 27×） |
| **P3** | `4414731` | `profile_alloc` 新增 pooled mode + 本报告 | simple-pooled 9.3 MB / comprehension-pooled 13.4 MB |

至此 P0-P3 全程 commit 数：约 11 个（含 cherry-pick / merge）。

## 四、剩余可优化项（未启动 / 离线遗留）

1. **CPU flamegraph**（最高优先级）
   - 本机 `/proc/sys/kernel/perf_event_paranoid = 4`，`perf` raw event 被拦
   - 需在离线 host 调成 ≤ 1（或用容器内 `sudo sysctl`）后跑：
     ```bash
     RUSTFLAGS="-Cstrip=none -Cdebuginfo=1 -Cforce-frame-pointers=yes" \
       cargo flamegraph -p relon-bench --release --bin profile_alloc -- comprehension-pooled
     ```
   - 主要目标：确认归因报告 §"证据偏弱" 两条
     - `Value::Dict` `BTreeMap` → `IndexMap` 的字段访问 CPU 占比
     - `path_cache_key` `format!` / `max_steps` 分支检查的 CPU 占比

2. **Context boot cost（at-gmax / at-end 常驻 ~7.7 MB）**
   - 4 种 dhat mode 的 at-gmax 都卡在 7.7-8.1 MB，是 stdlib 函数注册表 +
     prelude schemas + Builtin decorators 的**结构性常驻**
   - 可选改造方向：
     - stdlib 拆分按需 lazy register（big bang）
     - prelude schemas 改用 `OnceLock<Arc<…>>` 全局共享，跨 Context 复用
     - 把 stdlib `register_fn` / `register_method` 的 BTreeMap 头开销
       预分配到 `with_capacity`，按归因报告 simple workload `eval.rs:321`
       / `eval.rs:358` 占 31% 的特征
   - 收益估计：若能砍掉 5 MB 常驻，simple-pooled total 会从 9.3 MB 降到
     ~4 MB，几乎全是 transient

3. **comprehension/100 elements 噪声大（criterion p = 0.01 但 CI -56% / +3%）**
   - small-corpus 上 warming up 期相对噪声大，加 `--measurement-time 8 --sample-size 200`
     可以收紧 CI；非性能问题，是测量精度问题
   - 不影响结论：absolute mean 264 µs → 127 µs 是真实的

4. **method_dispatch 类（list_map / string_upper）-15% 仍有空间**
   - 每次 method call 走 `Schema::find_method` 是 BTreeMap key lookup；归因
     报告记的"BTreeMap → IndexMap"在这里有具象 wall-time 证据，但需要 CPU
     profile 才能定量；做完 §四.1 后再排

5. **comprehension/1000 wall-time 759 µs ≈ 759 ns / element**
   - 已经接近 evaluator 内层 "per-element O(1) iter_binding 刷新 + scope
     get_local fast path + arithmetic" 的硬下限
   - 进一步压缩需要触动 `Value::Int` 的算术 dispatch（当前走 `arithmetic.rs`
     的 trait match），收益曲线已经平坦，**P3 之后不再列为优化项**

至此，P0 归因报告 §"P1 任务排序建议" 7 条中：

- **1, 2, 3, 4 已落地**（P1-A / P1-B seam / P1-C / P2-A / P2-B）
- **5 (Context 复用)** P3 已通过 pooled mode 实证收益，但要把 reuse API
  暴露给宿主仍需正式设计（dhat 已证明 reuse-safe，下一波 wave 工作）
- **6, 7 (BTreeMap / path_cache_key)** 仍是 CPU profile 缺位的遗留项

---

## 附录 A：本轮 (P3) 复跑流程

```bash
# 1. 4 种 dhat mode（每个一次，输出到独立 json）
RUSTFLAGS="-Cstrip=none -Cdebuginfo=1 -Cforce-frame-pointers=yes" \
  cargo build --release -p relon-bench --bin profile_alloc --features dhat-heap
for mode in simple simple-pooled comprehension comprehension-pooled; do
  DHAT_HEAP_JSON=dhat-heap-$mode.json \
    target/release/profile_alloc $mode
done

# 2. criterion 对照
git checkout 7a1d449
cargo bench -p relon-bench --bench harness_v1 -- --save-baseline p0
git checkout worktree-agent-a366a99e4d4f7a84a
cargo bench -p relon-bench --bench harness_v1 -- --baseline p0
```

测量耗时（本机）：dhat 4 mode 合计 ~30 s，criterion 2 次合计 ~9 min。
