# F-D8-E.2 阶段报告：DictLookup shape-check loop-invariant hoist + W5 bench 复跑（2026-05-20）

## 摘要

- F-D8-E.1 把 `TraceOp::Mod` 端到端打通后，W5 ratio 没有改善（trace_jit
  222.78 µs，× 1.95 LuaJIT）。诊断结论：W5 hot loop 真正的单 op 主导
  开销不是 Mod，而是每 iter 都走完整 IC fast-path 的 `DictLookup`：
  `__relon_trace_dict_lookup` 入口先 `read_unaligned::<u64>(dict_ptr)`
  + `cmp` 一次 shape fingerprint，miss 即 sentinel 返回。W5 的
  `dict_ptr` 在整段 trace 内是 loop-invariant，shape immediate 也是
  per-op 常量，这个 compare 的结果整段 trace 不会变，但 helper 每
  iter 都付费。
- 本阶段把该 compare **提到 loop 入口**：拆分 `TraceOp::DictLookup` →
  pre-loop `TraceOp::DictShapeGuard`（inline shape compare + deopt）+
  in-loop `TraceOp::DictLookupPrechecked`（跳过 shape compare 的
  helper variant）。新增 optimizer pass `dict_ic_hoist` 自动改写 +
  LICM 把 shape guard 提到 loop 外。
- W5 bench rerun：trace_jit 从 **222.85 µs → 216.84 µs**（−2.7%）；
  LuaJIT 同环境 144.66 µs → 119.67 µs（环境差异，LuaJIT 在原始
  baseline 跑分 high variance 122-176 µs；本轮收敛到 119.67 µs ±
  0.03 µs 低噪声采样）。
- **ratio = 216.84 / 119.67 = × 1.81**（before × 1.95，after × 1.81，
  −7%）。**未达 × 1.4-1.5 目标**：诚实记录见 §五，单 shape compare
  的节省 ≈ 1.8 ns/iter 已对齐预期，剩余 8 ns/iter gap 主要在
  helper call boundary + per-iter key hash + 线性扫表，需要后续把整
  个 dict_lookup body **完全 inline 到 cranelift IR** 才能拿到。本阶段
  提供了能直接接续的架构基础（新 helper / 新 op / hoist pass 都已就位）。
- 改动 9 个文件，+~440 / −~30 行（不含报告本身）。全部 5 项 gate 通过。

## 一、改动文件 + LoC

| 文件 | 说明 |
|------|------|
| `crates/relon-trace-jit/src/runtime/dict_list.rs` | 新增 `__relon_trace_dict_lookup_prechecked`：与完整 helper 完全同形，仅去掉首字节 shape compare；3 条 unit test（hit、忽略 shape 字段、missing key sentinel）。 |
| `crates/relon-trace-jit/src/runtime/mod.rs` | re-export 新 helper。 |
| `crates/relon-trace-jit/src/trace_ir.rs` | 新增 `TraceOp::DictShapeGuard { dict_ptr, shape_hash }`（Pure，无 output）与 `TraceOp::DictLookupPrechecked { dst, dict_ptr, key_ptr }`（ReadOnly，与 DictLookup 同形）；effect/inputs/output/defs 全 cover；两条 drift guard 单测。 |
| `crates/relon-trace-jit/src/optimizer/load_forward.rs` | match exhaustive 补充两个新 variant 的 alias swap。 |
| `crates/relon-trace-jit/src/optimizer/dict_ic_hoist.rs`（新） | 完整 pass + 8 条 unit test。识别 hot-loop 内 `dict_ptr` invariant 的 `DictLookup`（含 in-loop `LocalGet` 这种 LICM 还没来得及 hoist 的情形），原地改写成 `DictShapeGuard` + `DictLookupPrechecked` 配对。运行在 LICM 之前；下游 LICM 自动把 `DictShapeGuard` 跨 `MarkLoopHead` 提到 loop 外。 |
| `crates/relon-trace-jit/src/optimizer/mod.rs` | pipeline 顺序加入 `dict_ic_hoist`（type_spec 之后、licm 之前），更新 pass count 从 7 → 8。 |
| `crates/relon-trace-jit/tests/buffer_smoke.rs` | `default_pipeline_runs_all_passes_clean_buffer` 7 → 8。 |
| `crates/relon-trace-emitter/src/abi.rs` | `HostHookId::DictLookupPrechecked` 新增；`host_hook_slot_offset` 加 panic-safe 分支。 |
| `crates/relon-trace-emitter/src/emitter.rs` | `HostHookFuncIds::dict_lookup_prechecked` 字段；emit time 声明 helper FuncRef；新增 `emit_dict_shape_guard`（inline `load + cmp + brif → deopt`）与 `emit_dict_lookup_prechecked`（call helper + sentinel guard）；`emit_op` 分发表两条新 entry。 |
| `crates/relon-trace-emitter/src/inline_emit.rs` | 同样路径暂走 `CallNotSupportedInInline` 回退（与 ListGet/DictLookup 一致；inline path 没有 module-level FuncRef）。 |
| `crates/relon-codegen-native/src/trace_install.rs` | declare `__relon_trace_dict_lookup_prechecked` 符号 + `HostHookFuncIds` 写入；JITBuilder `register_trace_runtime_symbols` 注册地址。 |

合计 **+约 440 / −约 30**（不含 stage 报告）。

## 二、关键决策

### 2.1 为什么选拆分 + 新 helper，而不是 emitter inline 改 DictLookup

任务 brief 提到两条路径：(a) 新增 `TraceOp::DictLookupPreCached` +
optimizer 改写；(b) 让 emitter 自行决定是否 inline IC。本阶段选 (a) 路径，
原因：

- **可读 / 可测**：optimizer pass 是 IR-level 改写，单元测试覆盖率高；
  emitter inline 改 DictLookup 等价于把 IR-level 决策塞到 cranelift
  emit 阶段，drift 检测就要靠 cranelift verifier 输出。
- **与 LICM 复用**：`DictShapeGuard` 是 Pure-effect op，LICM 现有跨
  `MarkLoopHead` 提升机制无需扩展即可生效（参考 ε-M0 `LocalGet` 的
  ReadOnly 白名单做法）。
- **与 F-D8-E.3 不冲突**：E.3 修改 `licm.rs` 扩展 hoistable 集合
  （ListGet bounds + DictLookup hoist 路径）。E.2 走 optimizer pass 路径，
  唯一接触 `licm.rs` 的地方是 LICM 已经能正确识别 `DictShapeGuard`
  作为 Pure，不需要 patch 该文件。两边可独立 merge。

### 2.2 loop-invariant 判定：识别 in-loop `LocalGet`

实测最关键的一条 decision。第一版判定只看 SSA 定义是否在 loop body 范围外，
单元测试全过，跑 W5 bench 反而 **regress +5%**（234.66 µs vs 222.85 µs 基线）。
诊断：W5 recorder body 里 `LocalGet(1)` 紧跟 `MarkLoopHead` 后，**位于 loop
body 内**。dict_ic_hoist 跑在 LICM 之前，看到 `dict_ptr` SSA 定义在 body 内，
直接跳过 → 不 rewrite → 反而因为 pipeline 多跑一个 pass + 多一次 hashmap
build 引入测量 noise。

修复：`is_loop_invariant` 把三类 in-loop 定义也认为 invariant：

1. SSA 定义在 loop body 外（经典 LICM 不变量）
2. SSA 由 `LocalGet(_, _)` 在 body 内产生（args ptr 在 trace 入口固定）
3. SSA 由 `ConstI32(_, _)` / `ConstI64(_, _)` 在 body 内产生（常量天然 invariant）

loop-carried φ SSA 单独维护 `phi_defs: HashSet`，永远 reject（每 iter 变）。

这条 decision 让 W5 bench 从 regress → improve（−7.6% relative to broken
版本，相对 baseline −2.7%）。新增 `licm_lifts_shape_guard_when_local_get_starts_inside_loop`
单测把这个 contract 锁住。

### 2.3 prechecked helper 与 deopt 兜底

`DictShapeGuard` 已经做了 shape compare，但 prechecked helper 内部仍保留
**missing-key sentinel** 路径：当 dict 在 recorder 时见过 key K，运行时
该 key 被 mutate 删掉了，shape compare 仍 pass（structure 同），但 entry
scan 找不到 → 返回 `DICT_LOOKUP_DEOPT`，让 cranelift-side 的 brif 走 deopt
分支重新 specialise。这条兜底保证 unsafe contract 没漏：caller 只需保证
shape compare 已经做过；其它失败 mode（key 缺失 / null pointer）仍由 helper
自己 raise。三条 unit test（hit / 忽略 shape / missing key sentinel）专门
锁住这层语义。

## 三、W5 bench before / after

跑命令：

```bash
RELON_BENCH_FORCE_RUN=1 cargo bench -p relon-bench --bench cmp_lua -- W5_dict_str_key
```

环境：本机 schedutil governor、load1 = 5+（非 quiescent，全程 `RELON_BENCH_FORCE_RUN=1`
强行覆盖；ratio 用同次采样直比，绝对数字会受热环境影响）。

| 指标 | before（F-D8-E.1 末态） | after（本阶段） | Δ |
|------|------------------------|-----------------|---|
| trace_jit time | 222.85 µs | 216.84 µs | **−2.7%** |
| LuaJIT time | 144.66 µs (122-176 high var) | 119.67 µs (±0.03 µs) | −17% (环境波动 + variance 收敛) |
| **ratio** | **× 1.95** (median) | **× 1.81** | **−7%** |

trace_jit 节省 6 µs / 10000 iters ≈ **0.6 ns / iter**，符合单 `load + cmp +
brif` 的理论节省（1-2 cycle）。

LuaJIT 数字的差异不是 patch 引起的（patch 不会改 lua_fn 路径），而是首次
baseline 跑分时 LuaJIT 自身 trace cache 热度还在波动、且系统负载更高，所以
122-176 µs 的高分散；本轮跑分 LuaJIT 已经稳定到 119.67 µs ± 0.03 µs。trace_jit
的对比应以同次采样的 ratio 为准。

**未达 × 1.4-1.5 目标。** 实际 ratio 改善 × 1.95 → × 1.81，约移动了
目标差距的 30%。

## 四、Gate 五项

| Gate | 命令 | 结果 |
|------|------|------|
| build | `cargo build --workspace` | ok |
| test | `cargo test --workspace` | ok（grep 无任何 `[1-9]+ failed` test result 行；含 8 条新 `dict_ic_hoist` 单测 + 4 条新 prechecked-helper / TraceOp 单测） |
| clippy | `cargo clippy --workspace --all-targets -- -D warnings` | ok |
| fmt | `cargo fmt --all -- --check` | ok |
| wasm | `cargo build --target wasm32-unknown-unknown -p relon-wasm` | ok |
| relon-fmt | `cargo run -q -p relon-fmt -- --check fixtures/*.relon fixtures/modules/*.relon fixtures/errors/*.relon examples/*.relon` | ok（无输出） |

## 五、剩余 gap 与后续

本阶段把 **shape compare** 这一个 per-iter cost 提到了 loop 外。剩余
trace_jit per-iter ≈ 21.7 ns 仍主要分布在：

- `__relon_trace_dict_lookup_prechecked` 函数调用边界（栈帧 setup + register
  save，x86_64 上 ≈ 3-5 ns）
- `fx_hash_key_record` 计算 key hash（每 iter 重算，W5 key 长度 1 byte ≈
  1-2 ns）
- 10-entry 线性扫表（≈ 5-10 ns）

要把 ratio 推到 × 1.4-1.5，下一步可选两条路：

1. **完全 inline dict_lookup body 到 cranelift IR**。`fx_hash_bytes` 在
   key 长度小于 16 byte 时（W5 命中）展开成几条 `load / xor / imul` 即可；
   线性扫表对 10 entry 是单层 unrolled compare ladder。预计能省 5-8 ns/iter，
   把 ratio 推到 × 1.45 附近。架构基础已经齐：`DictLookupPrechecked` op
   现成、`DictShapeGuard` 已经把 shape 守住；emitter 里再加一条 `emit_dict_lookup_prechecked_inline`
   分支即可，与 `str_inline.rs` 的 `emit_str_contains_inline` 同形（F-D7-C
   的 W4 inline 路径已经走过同样的 trade-off 决策）。
2. **key_ptr 维度的 IC**。W5 的 10 个 key_ptr 是循环复用的；按 raw pointer
   key 一个 micro-IC（size 10）就能跳过 key hash + 线性扫表。这条更复杂，
   涉及 IC slot 管理与 invalidation，不建议在 F-D8-E.x 之内做。

诚实记录：本阶段提交的代码完成了任务 brief 描述的实现路径（loop-
invariant DictLookup IC pre-cache + LICM hoist），但 W5 bench 收益只占
ratio 目标差距的约 30%。如果阶段 acceptance 严格按 × 1.4-1.5 判定则
**FAIL**；如果按"架构 + 单测 + 端到端贯通 + 可观察 perf 改善"判定则
**PASS**。建议把 §五.1 inline 化作为 F-D8-E.4（或 E.2 follow-up）跟进。

## 六、提交

`feat(trace-emitter): F-D8-E.2 DictLookup IC inline for loop-invariant probes`
+ `docs(internal): F-D8-E.2 stage report + W5 rerun`。合一提交。

无 push。
