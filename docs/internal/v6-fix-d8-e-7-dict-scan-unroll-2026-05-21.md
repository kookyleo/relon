# F-D8-E.7 阶段报告：dict scan pointer-chase + dict_inline 64-bit imm hoist + 小 dict unroll plumbing（2026-05-21）

## 摘要

- F-D8-E.6 留下 W5 trace_jit 定在 × 1.60（181.6 µs / 113.8 µs）。剩余 gap 的 hot spot 在 dict_inline 的 scan loop（FxHash + 10-entry linear scan）。F-D8-E.6 stage report § 5 列出三条候选 lever：unroll、perfect-hash、dict_inline 内 const hoist。本阶段实施前者的轻量版（小 dict 上线 plumbing）+ 后者（FX_HASH_SEED/PRIME hoist），并新增一条第四 lever —— **dict scan 改造为 incremental entry_ptr pointer chase**，干掉每次 iter 的 `imul scan_idx, 16`，把 scan body 收缩到「load + cmp + brif + iadd_imm 16」。
- 跨 crate 改动落地：`Op::DictGetByStringKey` 添加 `entry_count_hint: Option<u32>`，recorder lowering 透传到 `SubscriptKind::DictLookup`，`RecorderState::emit_dict_lookup_with_hint` 把它存进 `TraceBuffer::dict_entry_count_hints` 边表（key=dict_ptr SSA），emitter 在 `emit_dict_lookup_prechecked` 查表，命中且 ≤ `MAX_INLINE_UNROLL` 时切到 fully-unrolled cmov 链（balanced bor-tree reduction）；其它情况走原 scan loop。
- **第一次 W5 实测大幅退步**：把 W5 fixture entry_count=10 一并走 unroll，trace_jit 348 µs（baseline 181 µs），× 1.60 → × 3.06。dump IR 看到 10 个 `(load + select)` 串行依赖。把 acc 改成 balanced bor-tree 后退到 309 µs，仍是 +71%。**根因**：unroll 把「avg 5 iter × scan_loop」改成「**always 10 entries** of load + cmp + cmov + bor」，memory 总 load 数翻倍；W5 round-robin key 访问的 prediction footprint 也没改善。
- **诚实修订**：把 `MAX_INLINE_UNROLL` 从 16 收到 4（只让 ≤ 4 entry 的小 dict 拿到 unroll win），W5 N=10 走 scan loop。改完 W5 还原到 baseline 181.5 µs。然后两个新 lever 接力：
  - **SEED/PRIME hoist** 到 preheader（FX_HASH_SEED/FX_HASH_PRIME 都是 64-bit imm，每 outer iter 重 emit `movabs reg, imm64`）：trace_jit IR 看 preheader 多出两条 `iconst.i64`，hash body 复用，bench 无 measurable delta（与 E.6 报告预测一致）。
  - **scan pointer chase**：把 `scan_idx + imul 16 + iadd entries_base` 替换为 `entry_ptr` 载体 + `iadd_imm 16`，scan_init 内一次性算 `entries_end`。bench: **181.6 µs → 168.55 µs（−7.2%）**。
- **W5 final**：trace_jit 168.52 µs / LuaJIT 114.38 µs，**ratio = × 1.473**，**达成 × 1.5 目标**（gap 0.027 buffer）。
- 改动 7 个文件，+693 / −34 行（不含本报告）。全部 5 项 gate 通过。

## 一、改动文件 + LoC

| 文件 | 说明 |
|------|------|
| `crates/relon-ir/src/ir.rs` | `Op::DictGetByStringKey` 加 `entry_count_hint: Option<u32>` 字段。改 13 行（doc 段 + struct）。 |
| `crates/relon-trace-recorder/src/lowering.rs` | `SubscriptKind::DictLookup` 加 `entry_count_hint`；`Op::DictGetByStringKey` 解构透传；test fixtures 同步加 `entry_count_hint: None`；新增一条 `dict_get_by_string_key_forwards_entry_count_hint` 单测。改 ~80 行。 |
| `crates/relon-trace-recorder/src/recorder.rs` | 新 `emit_dict_lookup_with_hint(_, _, _, entry_count_hint)` 入口；旧 `emit_dict_lookup` 委托过去；`SubscriptDispatch::DictLookup` 调用前者；测试 fixture 加 `entry_count_hint: None`。改 ~55 行。 |
| `crates/relon-trace-jit/src/buffer.rs` | `TraceBuffer` 新增 `dict_entry_count_hints: HashMap<SsaVar, u32>` 边表 + `record_dict_entry_count_hint` setter；`OptimizedTrace` 同字段 + `dict_entry_count_hint(dict_ptr)` getter；`into_optimized` 透传。改 ~45 行。 |
| `crates/relon-trace-emitter/src/dict_inline.rs` | (a) `DictInlineHoists` struct（`entry_count` / `hash_seed` / `hash_prime` 三个可选 SSA）；`emit_dict_lookup_inline_with_hoists` 入口；旧两个入口委托。(b) 主体内 SEED/PRIME 优先用 hoisted SSA。(c) `MAX_INLINE_UNROLL = 4` 常量 + `emit_dict_lookup_inline_unrolled` 函数：N 个独立 load/icmp/select 后通过 `bor_tree_reduce_i64/i8` 做 log(N) 深度归约。(d) **scan loop 改造为 pointer chase**：scan_init 内算 `entries_end = entries_base + entry_count * 16`，header 把 `entry_ptr` 作为 phi 载体，body 直接 `load [entry_ptr]` + `iadd_imm 16` per iter，干掉 `imul scan_idx, 16`。(e) `fx_hash_seed_i64()` / `fx_hash_prime_i64()` 公开 const getter 供 emitter 引用。(f) 4 条新 unit test（unrolled IR verify + select 计数 + W5-shape round-trip）。 |
| `crates/relon-trace-emitter/src/emitter.rs` | (a) `TraceEmitterState` 加 `hoisted_dict_inline_seed/prime: Option<ir::Value>`。(b) `prehoist_loop_invariants` 在见到 `DictLookupPrechecked { dict_ptr: invariant }` 时，除了原本的 `entry_count` hoist 外，新 emit 一次 SEED + PRIME `iconst.i64`，trace-wide 共享。(c) `emit_dict_lookup_prechecked` 命中 `dict_entry_count_hint(dict_ptr)` 且 N ∈ [1, MAX_INLINE_UNROLL] 时走 `emit_dict_lookup_inline_unrolled`，否则走 `emit_dict_lookup_inline_with_hoists` (打包 entry_count/seed/prime 三个可选 SSA)。(d) 两条新 emitter unit test：`dict_lookup_with_entry_count_hint_emits_unrolled_select_chain` 与 `dict_lookup_without_entry_count_hint_keeps_scan_loop`。 |
| `crates/relon-trace-emitter/src/lib.rs` | re-export `DictInlineHoists` / `emit_dict_lookup_inline_unrolled` / `emit_dict_lookup_inline_with_hoists` / `MAX_INLINE_UNROLL`。 |
| `crates/relon-bench/benches/cmp_lua.rs` | W5 IR 的 `Op::DictGetByStringKey` 加 `entry_count_hint: Some(10)`（W5 dict 静态已知 10 entries；保留作为 metadata 即使 N=10 > MAX_INLINE_UNROLL 暂不命中 unroll）。 |
| `crates/relon-codegen-native/src/trace_recording.rs` | test fixture 加 `entry_count_hint: None`。 |
| `crates/relon-bench/tests/w5_w6_recorder_trace.rs` | test fixture 加 `entry_count_hint: None`。 |

总计：+693 / −34 行；emitter.rs + dict_inline.rs 大头是 hoists struct + unroll fn + pointer chase 改造 + 6 条新 test。

## 二、关键 IR 形态对比（W5 dict_inline scan body）

before（F-D8-E.6 baseline，每 outer iter 重 emit + per-iter scan_idx → imul 链）：
```text
scan_init:
    entry_count = load.u32 [dict_ptr+8] + uextend       # E.5 起已 hoist
    entries_base = iadd_imm dict_ptr, 12                # 留在 init
scan_header(scan_idx):                                  # back-edge target
    exhausted = icmp_eq scan_idx, entry_count
    brif exhausted, deopt, scan_body
scan_body:
    sixteen    = iconst.i64 16                          # 每 iter 重 emit
    entry_off  = imul scan_idx, sixteen                 # IMUL 3 cycle
    entry_addr = iadd entries_base, entry_off           # IADD
    entry_hash = load.i64 [entry_addr]
    is_hit     = icmp_eq entry_hash, final_hash
    brif is_hit, hit_block, scan_next
scan_next:
    next_scan = iadd scan_idx, 1
    jump scan_header(next_scan)
```

after（F-D8-E.7，pointer chase）：
```text
preheader_block:
    seed_v  = iconst.i64 -3750763034362895579           # F-D8-E.7 SEED hoist
    prime_v = iconst.i64 0x0100_0000_01b3               # F-D8-E.7 PRIME hoist
    entry_count = ... hoisted ...                       # E.5
scan_init:
    entries_base = iadd_imm dict_ptr, 12
    total_bytes  = imul entry_count, 16                 # 一次 IMUL on hot trace path
    entries_end  = iadd entries_base, total_bytes
scan_header(entry_ptr):                                 # phi: entry_ptr 直接载体
    exhausted = icmp_eq entry_ptr, entries_end
    brif exhausted, deopt, scan_body
scan_body:
    entry_hash = load.i64 [entry_ptr]                   # 直接寻址 entry_ptr，no imul
    is_hit     = icmp_eq entry_hash, final_hash
    brif is_hit, hit_block, scan_next
scan_next:
    next_ptr = iadd_imm entry_ptr, 16                   # 单一 IADD imm
    jump scan_header(next_ptr)
```

W5 hash_body 同时复用 hoisted PRIME（v24 in IR dump），相比 E.6 每 outer iter 重 emit `iconst.i64 0x0100_0000_01b3` 少一条 `movabs reg, imm64`。SEED 同样 hoist。

## 三、W5 bench

`cargo bench -p relon-bench --bench cmp_lua -- W5_dict_str_key`，
`RELON_BENCH_FORCE_RUN=1`。governor `schedutil`，load1 ≈ 2-4。

| 指标 | F-D8-E.6 baseline | F-D8-E.7 after | Δ |
|------|-------------------|----------------|---|
| trace_jit | 181.6 µs（181.58） | 168.55 µs（168.52） | **−7.2%** |
| LuaJIT    | 113.8 µs          | 114.38 µs           | +0.5%（噪声） |
| ratio     | × 1.60            | **× 1.47**          | −8.1% in ratio |

第二轮独立 run 复测：trace_jit 168.52 µs（criterion 报 `No change`），LuaJIT 114.38 µs。**ratio = 168.52 / 114.38 = × 1.473，已达成 × 1.5 目标**（buffer 0.027）。

criterion change 检测：trace_jit 报 `-7.16% .. -7.09%` (p < 0.05，「Performance has improved」)；LuaJIT 抖动 ±0.3%。

W6 row 同步 sanity check：trace_jit 32.95 µs / LuaJIT 68.73 µs（× 0.48，relon faster），与 baseline 一致，无 regression。

## 四、Gate 五项

1. `cargo fmt --all -- --check`：通过（rustfmt 把 `for n in [..]` 数组拆多行）。
2. `cargo clippy -p relon-trace-emitter --all-targets -- -D warnings`：通过。第一遍命中 `manual_range_contains`，改用 `(1..=cap).contains(&n)` 后清掉。
3. `cargo clippy --workspace --all-targets -- -D warnings`：通过。
4. `cargo test -p relon-trace-emitter`：42 个 lib + 集成全过，含 6 个新增（unrolled IR verify ×2、unrolled select count ×1、emitter unrolled/scan path 区分 ×2、dict_lookup_forwards_entry_count_hint ×1）。
5. `cargo test --workspace --lib` + `cargo test -p relon-test-harness` + `cargo test -p relon-bench --test cmp_lua_consistency` 全过；W5 / W6 recorder trace + cmp_lua_consistency W5 一致性（trace-jit 累加 == tree-walker 期望）OK。

## 五、未走完的方案 / 诚实记录

### 5.1 完全 unroll W5（N=10）退步 +71%

第一版把 W5 fixture 的 entry_count_hint=Some(10) 配 `MAX_INLINE_UNROLL = 16` 直接走 fully-unrolled 路径。bench 实测 trace_jit 348 µs（baseline 181 µs），ratio × 3.06。诊断：

- unroll 把「scan_loop avg 5 iter 后 brif 出来」改成「always do 10 个 `(load_hash + load_val + icmp + select)`」，memory load 数从 ~5 → 10，**单 outer iter 总 work 翻倍**。
- W5 key 是 round-robin → branch predictor 在 scan_loop 上可能 mispredict 但 OoO 重叠掩盖；unroll 后没有分支可预测，纯吞吐受 load 数主导。
- 第二版改成 balanced `bor` tree 归约（depth = log2(N) = 4）+ 独立 `select` lanes：309 µs，仍 +71%。
- 第三版把 `MAX_INLINE_UNROLL` 收到 4，W5 N=10 自动走 scan loop → 181.5 µs（无 regression）。

教训：unroll 在 round-robin / 均匀 hit-position 分布下不是 net win。若 access pattern 是 hot-key dominant（前 1-2 entry 命中 ≥80%），unroll 有正收益。当前实现保留完整 unroll plumbing（recorder hint → side table → emitter dispatcher → `emit_dict_lookup_inline_unrolled`），cap 在 4 entries 让小 dict（config struct、lookup table）受益，W5-class 走 scan loop。后续真实程序的 dict access 分布观测会决定是否调大 cap。

### 5.2 SEED/PRIME hoist 收益 ≈ 0 µs

E.6 报告 §五 预测「dict_inline 内部 const hoist 收益 ≤ 2-3 µs」。实测 IR 看 preheader 多了 2 条 `iconst.i64`，hash body 直接复用 hoisted SSA，但 bench 在 SEED/PRIME hoist 单独落地的轮次（保留 scan loop 老形态时）没有 measurable delta（181 µs → 181 µs）。

最终 −7% 收益完全来自第 5.3 节的 pointer chase 改造。SEED/PRIME hoist 收益被 cranelift 0.131 register allocator 在 hot loop 内 IR 长度变短抵消（多一个 hoisted 寄存器分配，可能挤掉别的临时值）。**保留这个 lever 不是冗余**：它清理了 dict_inline 的 const-pollution，让后续可能的进一步优化（dict_inline 别处的 `iconst 16` / `iconst 0`）更容易合并。

### 5.3 真正的 lever：scan loop pointer chase（−7%）

观察 E.5/E.6 baseline scan body 的 IR：每 iter 三条 `iconst 16; imul scan_idx, sixteen; iadd entries_base, entry_off` 占了 scan body 一半指令。改成 phi 携带 `entry_ptr` + `iadd_imm 16` per iter：

- IMUL 是 3-cycle latency 在 x86_64（micro-fused 与 IADD 串行），改成 `iadd_imm 16` 是 1-cycle。
- scan body 指令数从 7 → 5（icmp + brif + load + icmp + brif → load + icmp + brif + iadd_imm + jump）。
- iter 平均 5 → 单 outer 节约 ~(3 × 5 - 1 × 5) = 10 cycle ≈ 3 ns。2000 outer iter → 6 µs 估算；实测 -13 µs，超出估算 2×（其它二级效应：寄存器分配减压、code size 缩短带来 i-cache footprint 减小）。

`entries_end` 在 scan_init 算一次，留在 scan_init 而非 preheader：cranelift 把 dict_ptr 维持 invariant，scan_init 在 hash_loop 之后的 outer iter 内执行一次，imul 在 scan_init 而非 scan_body 内，scan_body 完全干净。

## 六、提交

```
perf(trace-jit): F-D8-E.7 W5 scan pointer-chase + dict-inline imm hoist + small-dict unroll plumbing
docs(internal): F-D8-E.7 stage report + W5 final
```

合一为单 commit 提交。
