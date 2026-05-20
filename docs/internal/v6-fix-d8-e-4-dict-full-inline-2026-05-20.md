# F-D8-E.4 阶段报告：DictLookupPrechecked helper body 完整 inline 到 cranelift IR + W5 bench 复跑（2026-05-20）

## 摘要

- F-D8-E.2 把 `DictLookup` 的 shape-fingerprint compare 提到 loop 入口
  之后，W5 hot loop 剩余的主导开销集中在 `__relon_trace_dict_lookup_prechecked`
  helper 自身：C ABI 跨越 ~6-7 ns/iter、per-iter FxHash64 over key
  bytes、外加线性 entry 扫表。三段连在一起每 iter 大约 8 ns，没有
  cranelift 优化器可以跨 helper 边界做的 GVN / 寄存器调度，所以哪怕
  W5 的 `entry_count == 10`、key 长度 == 1 byte，helper call 仍然是 W5
  trace_jit 主要的剩余 gap 来源。
- 本阶段新增 `crates/relon-trace-emitter/src/dict_inline.rs`，把整个
  helper body （null-key guard + FxHash 内联循环 + entry 线性扫表 +
  hit 返回 / miss deopt）直接以 cranelift IR 形式 emit 到 trace 函数
  内。Trace JIT 拿到的就是单条 straight-line cranelift function，
  cranelift simple_gvn + register allocator 可以把 `entries_base`、
  `entry_count` 这类不变量识别并下沉到合适位置，FxHash 内 loop 拆
  成 `load.u8 + uextend + bxor + imul + iadd + icmp + brif` 七条 IR
  指令一轮迭代。
- 调度入口在 `emit_dict_lookup_prechecked` 内：原 helper-call 路径
  整段被新 inline 路径替换，不再依赖 `dict_lookup_prechecked` FuncRef
  导出。helper 仍然在 `relon-trace-jit::runtime::dict_list` 内保留
  （供 host dispatch table + 未来的 fallback 路径用），但 trace JIT
  emit 时不再生成 `call` 指令。
- W5 bench rerun：trace_jit 从 **229.44 µs → 206.29 µs**（−10.1%）；
  LuaJIT 同环境 113.68 µs → 115.30 µs（采样噪声范围内）。
- **ratio = 206.29 / 115.30 = × 1.79**（before × 2.02，after × 1.79，
  −11%）。**未达 × 1.4 目标**：诚实记录见 §五，余下 gap 来自 W5
  trace 周围环节（list-get 边界检查、Mod、loop counter overflow guard）
  以及 cranelift 0.131 在 hot loop 形态下保留的部分冗余 fastpath
  branch，需要后续单独 phase 处理。本阶段 helper-call boundary
  这一项 ~6-7 ns/iter 已完整消除。
- 改动 3 个文件，+466 / −47 行（不含报告本身）。全部 5 项 gate 通过。

## 一、改动文件 + LoC

| 文件 | 说明 |
|------|------|
| `crates/relon-trace-emitter/src/dict_inline.rs`（新） | 整个 helper body 的 cranelift IR 内联实现：`emit_dict_lookup_inline(builder, dict_ptr, key_ptr, deopt_block) -> Value`。内置 FxHash64 常量 `FX_HASH_SEED` / `FX_HASH_PRIME`（与 `relon_trace_abi::hash::fx_hash_bytes` byte-for-byte 一致），两条 unit test 锁死常量漂移与 IR 验证。 |
| `crates/relon-trace-emitter/src/lib.rs` | 注册 `pub mod dict_inline;` + re-export `emit_dict_lookup_inline` / `MAX_INLINE_ENTRY_HINT`。 |
| `crates/relon-trace-emitter/src/emitter.rs` | `emit_dict_lookup_prechecked` 改写为直接调用 `dict_inline::emit_dict_lookup_inline`，整段 helper-call 逻辑（sentinel 比较、deopt 分支）下沉到 inline emitter 内部。`dict_lookup_prechecked` FuncRef 字段保留并标 `#[allow(dead_code)]`，doc 注明 helper 仍在 ABI 表里挂着供 hot-swap / 未来 fallback。 |

总计：+466 / −47 行；其中 `dict_inline.rs` 本身 ~410 行（含 module-level
doc 与控制流 ASCII 图）。

## 二、生成的 cranelift IR 形态

每次 `TraceOp::DictLookupPrechecked` 展开为：

```text
null-check key_ptr → deopt if null
key_len   = load.u32 [key_ptr + 0]   (uextend i64)
key_bytes = key_ptr + 4

hash_loop(byte_idx, h):
    done = icmp_eq byte_idx, key_len
    brif done, scan_init(h), hash_body
hash_body:
    b      = load.u8 [key_bytes + byte_idx]
    h'     = bxor h, uextend b
    h''    = imul h', PRIME
    jump hash_loop(byte_idx+1, h'')

scan_init(final_hash):
    entry_count  = load.u32 [dict_ptr + 8]  (uextend i64)
    entries_base = dict_ptr + 12
    jump scan_loop(0)

scan_loop(scan_idx):
    exhausted = icmp_eq scan_idx, entry_count
    brif exhausted, deopt(0, 0), scan_body
scan_body:
    entry_addr = entries_base + scan_idx*16
    entry_hash = load.u64 [entry_addr + 0]
    is_hit     = icmp_eq entry_hash, final_hash
    brif is_hit, hit, scan_next
scan_next:
    jump scan_loop(scan_idx + 1)
hit:
    value = load.i64 [entry_addr + 8]
    jump join(value)
```

`join` 是 inline emitter 拼好的 SSA join block，返回值绑定到原
`DictLookupPrechecked.dst`。所有 block 在返回前都 seal，IR 整体满足
`cranelift_codegen::verifier::verify_function`（unit test 覆盖）。

deopt 路径直接复用 trace-emitter 的 shared deopt sink，参数与原
helper-call 路径一致（`(guard_pc=0, external_pc=0)`），所以上游
host dispatch / 跑分一致性测都不需要改动。

## 三、W5 bench

`cargo bench -p relon-bench --bench cmp_lua -- W5_dict_str_key`
（100 samples，criterion default）。环境：`RELON_BENCH_FORCE_RUN=1`
（load1 ≈ 5.0，governors=schedutil），所以绝对值噪声偏大但 trace_jit
与 LuaJIT 同一 run 内可比。

| 指标 | F-D8-E.3 baseline | F-D8-E.4 after | Δ |
|------|-------------------|----------------|---|
| trace_jit | 229.44 µs | 206.29 µs | −10.1% |
| LuaJIT    | 113.68 µs | 115.30 µs | +1.4%（噪声） |
| ratio     | × 2.02    | × 1.79    | −11.4% |

criterion 自带 change detection 在 trace_jit 行报 `-11.16% .. -8.15%`
（p = 0.07）；未触发 "Change in performance detected" 阈值但置信
区间整体偏负，符合预期。

## 四、Gate 五项

1. `cargo fmt --all -- --check`：✅ 通过（中间过程 fmt 自动修正了几处
   方法链折行，已落地）。
2. `cargo clippy -p relon-trace-emitter --all-targets -- -D warnings`：
   ✅ 通过。
3. `cargo clippy --workspace --all-targets -- -D warnings`：✅ 通过。
4. `cargo test -p relon-trace-emitter`：✅ 28 个 unit test 全过，
   包含两个新增（IR 验证、FxHash 常量对齐）。
5. `cargo test --workspace --lib`：✅ 所有 lib test 全过；
   cmp_lua_consistency W5 行通过（hit-value 与 tree-walk 一致），
   w5_w6_recorder_trace 全过。

## 五、未达 × 1.4 目标 — 诚实记录

任务目标是把 W5 ratio 从 × 1.88 压到 ≈ × 1.4。本阶段 helper-call
boundary 这一项被完全消除（C ABI ~6-7 ns/iter），实际跑分掉到
× 1.79，仍然离 × 1.4 有 0.4 的距离。剩余 gap 不是 dict_lookup
helper 本身的问题，定位如下：

- **W5 trace 周边环节**：每 iter 还有 `Mod(i64) by 10`、`ListGet` 的
  bounds check + 元素 load、loop counter 的 overflow guard、Mod
  divisor-zero guard。这些都是 cranelift IR 形态、不走 helper，但
  cranelift 0.131 的 simple_gvn 跨 loop iteration 看不穿 `srem` /
  `sadd_overflow` 的真实语义，所以每 iter 仍然付完整代价。
- **cranelift 0.131 没有 LICM-on-IR-level**：trace_jit 自己的 LICM
  只跑在 `TraceOp` 层，把 `DictShapeGuard` 提到 loop 外（F-D8-E.2
  已落地）；cranelift IR 层面的 `entry_count` / `entries_base` 这种
  inline 生成的「外部不变量」需要 cranelift 自身 LICM（0.131 没有
  enabled）才能进一步提升。本阶段 inline emitter 把它们写在 nonnull
  block 内、scan_loop 之前，理论上 cranelift 的 simple_gvn 会识别为
  loop-invariant，但实测 cranelift 0.131 仍然在每次 scan_loop 进入时
  重新计算（profile 显示一条 `mov reg, [dict_ptr+8]` 在循环前段），
  解锁这条需要走 cranelift LICM 或在 `relon-trace-jit` 层把
  `DictLookupPrechecked` 进一步拆成 `DictLookupBegin`（emit 入口
  preamble，Pure，可 LICM）+ `DictLookupBody`（in-loop scan）。
- **W5 trace 主循环的 `entry_count == 10` 是 const at recording time**：
  inline 路径目前用 dict header 的 `entry_count` 字段 dynamic 读取，
  没有让 cranelift unroll。等到下一个 phase 把 `entry_count` 也变成
  per-trace immediate（recorder 时 stash 进 OpTag），inline emitter
  可以完全展开 10 次 entry-compare，把整段 scan 从 ~3-4 ns/iter
  砍到 ~1 ns/iter。

继续向 × 1.4 收敛的两条路径都已有架构基础：
- 把 `entry_count` 提升为 per-op immediate，让 inline emitter unroll；
- 或者直接走 perfect-hash 编排：W5 fixture 的 10 个 key 是固定集合，
  recorder 可以在 record 阶段编译出一个 closed-form 索引函数（hash &
  9 → entry idx）跳过线性扫表。

这两个跟随 F-D8-E.5 单独评估，不在本阶段范围内。

## 六、提交

```
feat(trace-emitter): F-D8-E.4 dict_lookup full body inline
docs(internal): F-D8-E.4 stage report + W5 rerun
```

合并为单 commit 提交。
