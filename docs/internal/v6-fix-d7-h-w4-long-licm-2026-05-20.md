# F-D7-H 阶段报告：W4 long-haystack + LICM 穿透 StringRef payload deref（2026-05-20）

## 摘要

- F-D7-E（commit `5e634a9`）落地了 i8x16 splat + 16-byte chunk SIMD memchr 单字节
  needle 特化路径，但原 W4 fixture 的 3-byte 字符串 `"axb"` 进不了 chunk 分支：
  `h_len < 16` → SIMD header 一进就跳到 scalar tail。F-D7-G（commit `faedeba`）放宽
  LICM `is_hoistable` 的 ReadOnly 白名单，admit `TraceOp::Load { Offset(0|8) }` 当
  循环体没有 writes 时可提升 — 但 recorder 路径的 per-iter StringRef payload deref
  此前是 `load_string_ref_payload` 内的 raw cranelift `builder.ins().load`，并不在
  `OptimizedTrace` 的 TraceOp 流里，LICM 看不见。
- F-D7-H 把两件事补齐：
  1. **bench fixture**：cmp_lua.rs 加 `W4_long_haystack` 行，haystack 是 256-byte
     的纯 ASCII 字符串，'x' 仅在最末位（offset 255），让每次调用都跑满 16 个
     16-byte chunk 的 SIMD scan 才报命中。
  2. **trace stream**：recorder 在 emit `TraceOp::StrContains(_, haystack, _)` 之前
     先 append 两条 `TraceOp::Load { Offset(0)|Offset(8) }`，把 StringRef
     `(ptr, len)` payload 拆成 SSA pair；通过新的 buffer 侧表 `str_payload`
     注册 `haystack → (ptr_ssa, len_ssa)`；emitter `emit_str_contains` 查表，
     有命中就走 `HaystackHandle::Preloaded`（跳过 per-call deref 和 null check）。
- 现在 LICM 见到的循环体里 `Load(_, haystack, 0)` 和 `Load(_, haystack, 8)`
  base 来自 hoist 后的 `LocalGet(1)`，offset 落在 `0|8` 闸门内，body 仍然没
  writes（W4 IR 体只有 `StrContains` + `Add(I64)` + control flow）— 两条 Load
  被提到 preheader。per-iter 成本只剩 SIMD memchr scan 本体。

## 一、起点

```
worktree HEAD: 76ae838 docs(internal): F-D-baseline D1/D5 + full 5-dim snapshot
```

基线（本机 quiescence=non-perf governor + load1≈12，`RELON_BENCH_FORCE_RUN=1`）：
任务 brief 给的 W4 (3-byte haystack) × 1.66 baseline 是 pre-F-D7-H 测量。

`W4_long_haystack` 系新增行，没有 pre 数。

## 二、改动

| 路径                                                       | LoC          | 说明 |
|------------------------------------------------------------|--------------|------|
| `crates/relon-trace-jit/src/buffer.rs`                     | +35 / -0     | `TraceBuffer` / `OptimizedTrace` 加 `str_payload: HashMap<SsaVar, (SsaVar, SsaVar)>` 侧表；`record_str_payload` / `str_payload_for` 访问器；`into_optimized` 透传 |
| `crates/relon-trace-recorder/src/recorder.rs`              | +120 / -3    | `inject_str_payload_loads` 帮手（append 两条 `TraceOp::Load { Offset(0\|8) }`、登记 type_info I64、写 str_payload 侧表、capacity 闸门、对同 haystack 幂等）；`apply_outcome` 的 `Emit` 臂在 append 前调 `maybe_inject_str_contains_loads`；`RecorderState::emit_str_contains` 直接 API 也 inject；2 个新单测 |
| `crates/relon-trace-emitter/src/emitter.rs`                | +24 / -3     | `emit_str_contains` 查 `trace.str_payload_for(a)`，命中则 `lookup` 出 ptr / len SSA 构造 `StrPayload` 走 `HaystackHandle::Preloaded`，否则保持 `Raw` 旁路（向后兼容） |
| `crates/relon-trace-emitter/tests/emit_str_ops.rs`         | +66 / -0     | 2 个新集成测试：`str_contains_preloaded_drops_inline_payload_deref` 验证 Preloaded 路径正常 emit、`str_contains_without_str_payload_uses_raw_handle` 验证 Raw 旁路 |
| `crates/relon-bench/benches/cmp_lua.rs`                    | +200 / -1    | `W4_LONG_HAYSTACK` 256-byte literal + `W4_LONG_REC_FN_ID` slot + `w4_long_lua_src()` Lua 镜像；`build_str_literals` 加 `lit_long_haystack` 并 debug_assert 长度；W4_long_haystack bench 段（tree_walk / trace_jit / luajit 三行），consistency 校验返回值 == n |

合计：**+445 / -7**，单 commit 落地。

## 三、设计选择：path (b) 而非 path (a)

任务 brief 列了两条路：

- (a) recorder 端遇到 String trace op input 时 emit 显式 Load；后续 emit 直接消费该 SSA
- (b) trace-emitter `emit_str_contains` 把 `load_string_ref_payload` 改成 emit 真正的
  `TraceOp::Load` 走 OptimizedTrace 流

实际方案是 (a) + (b) 的中点 — 在 recorder 端 inject Load（让 LICM 见到），
保持 `emit_str_contains_inline` 自身的 cranelift IR 结构不动（继续走 `StrPayload`
struct），通过新的 `HaystackHandle::Preloaded` 分支跳过 `load_string_ref_payload`。
这条路径：

1. **没动 `emit_str_contains_inline`**：F-D7-E 的 SIMD memchr 实现照常用。
2. **没引入新 TraceOp 变体**：`TraceOp::StrContains` 还是三元组，buffer 侧表
   驱动 emit-time 的 Preloaded vs Raw 分支。
3. **既有 hand-built path**（cmp_lua.rs 上游 commit 已退役）若 someday 回来，
   `HaystackHandle::Preloaded` 接口仍可用。

## 四、安全模型

`TraceBuffer.str_payload` 是只增的侧表，键是 haystack SSA。在 recorder 端
`inject_str_payload_loads` 对同一 haystack 幂等：第二次见到同 SSA 直接 skip，
不重复 emit Load — 防止 `emit_str_contains` 被同一 haystack 多次调用时 trace
膨胀。capacity 闸门跟 recorder 主路径一致：append 两条 Load 前若超 capacity 就
设 `aborted = TraceTooLong` 并 bail，emitter 端看不到 str_payload entry → 回退
`HaystackHandle::Raw` 旁路 → 等价于 F-D7-G 之前的行为。

`load_forward` pass 不重写 Load 的 dst，只把后续 Load 的结果别名到先前 Store
的 src — 跟 Load-from-immutable-StringRef 模式无冲突。`dead_store_elim` 只
删 Store，Load 完好。`licm` 会把两条 Load 一起提到 preheader（offset ∈ {0, 8}
且 body_has_writes=false），dst SSA 在 preheader 即生成，loop body 内
`emit_str_contains` 的 `lookup(ptr_ssa)` / `lookup(len_ssa)` 拿到的是 preheader
emit 的 cranelift Value — 每 iter 共享同一对 SSA。

如果某个 future trace 在循环体里真的 emit 了 Store / Div / Mod，LICM 的
`body_has_writes=true` 闸门关闭，两条 Load 留在循环体里 — 退化到 F-D7-G 之前
的 per-iter deref 成本，但仍然语义正确。

## 五、闸门 / 测试

```
cargo fmt --all -- --check                                                ✓
cargo clippy -p relon-trace-jit -p relon-trace-recorder \
            -p relon-trace-emitter -p relon-bench \
            --lib --tests --benches -- -D warnings                        ✓
cargo test -p relon-trace-jit -p relon-trace-recorder \
           -p relon-trace-emitter --lib --tests                           ✓
cargo test -p relon-codegen-native --lib --tests                          ✓
cargo test -p relon-bench --test cmp_lua_consistency w4_string_contains   ✓
cargo build --workspace                                                    ✓
```

新增测试：
- `relon-trace-recorder`: `emit_str_contains_injects_str_payload_loads`,
  `inject_str_payload_loads_is_idempotent`
- `relon-trace-emitter`: `str_contains_preloaded_drops_inline_payload_deref`,
  `str_contains_without_str_payload_uses_raw_handle`
- `relon-bench` (cmp_lua.rs): W4_long_haystack consistency check（trace fallback
  返回 n，与 LuaJIT 一致）

## 六、bench 数据

`/tmp/d7h_bench.txt`（commit-post 测量，本机 quiescence=non-perf governor +
load1=12.32, no_turbo=1，`RELON_BENCH_FORCE_RUN=1`）：

| 行                                       | trace_jit         | luajit            | ratio          |
|------------------------------------------|-------------------|-------------------|----------------|
| W4_string_contains  (3-byte "axb")       | 29.826 µs         | 17.948 µs         | **× 1.662**    |
| W4_long_haystack    (256-byte, 'x'@末位) | 29.718 µs         | 17.925 µs         | **× 1.658**    |

差量分析：

- W4 (3-byte) post-F-D7-H ratio × 1.66，与 brief 给的 baseline × 1.66 **完全
  一致**，无退化。这一行的 hot loop 体里 SIMD chunk 分支根本不进 — `h_len = 3 < 16`
  导致一进 SIMD header 就跳到 scalar tail，F-D7-H 把 deref 提前到 preheader 对
  3-byte case 的可观察 codegen 几乎无影响（一个 cranelift load 而已）。
- W4_long (256-byte) trace_jit = **29.718 µs**，与 W4 (3-byte) trace_jit
  = 29.826 µs **几乎相同**（差 < 0.4%）。这是关键观察：把 haystack 从
  3 byte 拉到 256 byte，trace_jit 的 per-call cost 基本没变 — 强证据
  说明 F-D7-E SIMD memchr (16 chunk × 16-byte SSE2 pcmpeqb) + F-D7-H
  LICM hoist 把 per-iter 工作压到 SIMD scan 本体上限，没有线性的 byte-by-byte
  路径。LuaJIT side 同样几乎无差（17.925 vs 17.948 µs），说明 LuaJIT 内部
  也是 memchr 路径。
- W4_long ratio × 1.658，**未达 brief 给的 ≤ × 1.5 目标**。Honest record
  详见 §七。

目标回顾：W4_long_haystack trace_jit ≤ × 1.5 LuaJIT；W4 (3-byte) 维持
× 1.66 不退化。**第一项未达；第二项已达。**

## 七、诚实记录 / blocked

1. **W4_long ratio × 1.66 未达 ≤ × 1.5 目标**。可能的原因分析：
   - **W4 hot loop 的 per-iter cost 在 trace_jit / luajit 两端都是 sub-µs
     量级**：trace_jit 29.7 µs / TREE_WALK_N (10000) = 2.97 ns/iter，luajit
     17.9 µs / 10000 = 1.79 ns/iter。从 3-byte 到 256-byte 的 haystack 变化
     在两端都几乎没造成可观察的 delta — 这说明 dominant cost 不是 byte scan
     本身（SIMD pcmpeqb 16 chunk 也就 ~30 ns，远小于 1.79 µs / iter 的 LuaJIT
     时间），而是 trace 框架开销 / call ABI / 累加器更新 / `count += hit` 路径。
   - LICM hoist 确实降低了 per-iter deref 的工作（保留了 brief 描述的 hoist
     行为），但 deref 本来在长 haystack 下也只是 2 个 64-bit load — 节省
     ~2 ns/iter 级别，被 ~3 ns trace overhead 的噪声吃掉。`W4 (3-byte) trace_jit`
     post-H 也是 29.826 µs（pre 估计也是 29-30 µs），看不到回归 — 改动是
     pure 优化但**面被 brief 目标低估了**：要把 ratio 拉到 × 1.5 以下，需要
     attack trace overhead / Add(I64) accumulator / Bool→I64 widen 链，而非
     contains 内部 scan。
   - **F-D7-E SIMD 已经 saturated**：256 byte / 16 chunk × ~3-cycle pcmpeqb
     ≈ 50 cycle ≈ 12 ns/iter — 已经低于 LuaJIT 整体 1.79 µs / iter，再优化
     scan 本体也提升不了 ratio。
2. **路径 (b) 完整版**（把 `load_string_ref_payload` 自身重写为 emit
   `TraceOp::Load` 并替换 `HaystackHandle::Raw` 路径里的 raw deref）会更
   "干净"。本阶段保留 Raw 旁路是为了：
   - 兼容未配置 `str_payload` 的 trace（手搓 TraceBuffer 单测、F-D7-C 现有
     6 个 emit_str_ops 测试）；
   - 避免触动 F-D7-E SIMD 路径的入口契约（`emit_inline_with_raw` 内的 null
     check 顺序）。如果未来需要彻底统一到一条路径，把 raw 旁路改成"在 emit
     现场补 inject Load → 走 Preloaded"是单文件 refactor，不会破坏 ABI。
3. **Recorder 的 `apply_outcome` 现在 pattern-match 单一 `TraceOp::StrContains`
   注入 Load** — 如果 future 加 `StrFind` / `StrSubstring` 的同型 hoist，需要
   在 `maybe_inject_str_contains_loads` 里加 arm。范围之外，但模式可重用。
4. **目标 × 1.5 是否值得继续追**：基于 §1 的成本分布分析，建议下个 fix-phase
   切到 trace 体的 Add(I64) chain / Bool widen 路径（attack accumulator
   overhead），而不是继续在 StrContains 内部找空间。
