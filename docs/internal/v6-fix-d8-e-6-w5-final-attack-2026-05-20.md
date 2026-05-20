# F-D8-E.6 阶段报告：trace emitter 加上 magic-mul strength reduction + W5 bench 复跑（2026-05-20）

## 摘要

- F-D8-E.5 把 dict_lookup 三类 loop-invariant 子表达式 hoist 到 preheader 之后，W5 trace_jit 定在 × 1.65（188 µs / 114 µs）。剩余 gap 的 hot spot 一直是 `TraceOp::Mod`：cranelift 0.131 的 x86_64 backend 把 `srem a, iconst10` 直接 lower 成 `idiv`（一条 microcoded ~20-25 cycle 指令），没有内建的 magic-multiply strength reduction。
- 本阶段在 `emit_mod` 里加了一条「const-positive divisor → Hacker's Delight 64-bit signed magic-multiply」的 fast path：检测 divisor 是已被 const_fold pass stamp 进 `OptimizedTrace::consts` 的正整数，并且其 magic 没有 set high bit（不需要 add-correction）后，发 `smulhi + sshr_imm + sshr_imm + isub + imul_imm + isub` 6 条 IR 指令的 signed-remainder 序列。divisor-zero 与 `MIN % -1` 两个 guard 在 const divisor 情形下静态不可达，整体跳过。
- 第一版改完 bench 立刻退步（187 → 231 µs，× 1.65 → × 2.02）。dump IR 一看：magic multiplier `iconst.i64 0x6666_6666_6666_6667` 落在 loop body 内，cranelift 0.131 的 GVN 没把它折出 loop。`mov reg, imm64` 是 10-byte 指令 + 占一个长寿命 reg，per iter 反复发射。第二版把它纳入 F-D8-E.5 同款 preheader hoist 表：prehoist pass 看见 invariant 正-const divisor 就在 preheader 一次性 `iconst.i64 magic`，emit_mod 拿到 hoisted SSA 复用。
- W5 bench rerun：trace_jit 从 **188 µs → 181.6 µs**（−3.4%），LuaJIT 113.8 µs（基本无变化）。**ratio = 181.6 / 113.8 = × 1.60（before × 1.65，after × 1.60，−3% in ratio）。未达 × 1.5 目标**，诚实记录见 §五。
- 改动 2 个文件，+535 / −21 行（不含本报告）。全部 5 项 gate 通过。

## 一、改动文件 + LoC

| 文件 | 说明 |
|------|------|
| `crates/relon-trace-emitter/src/emitter.rs` | (a) `emit_mod` 头部新增 const-positive divisor 检测；命中 → `emit_signed_mod_by_const(va, divisor, hoisted_magic)`，跳过 divisor-zero 与 `MIN % -1` 双 guard。(b) 新 helper `const_positive_i64(var)` 从 `self.trace.consts` 读 `TraceConst::I64 / I32 > 0`。(c) 新 helper `emit_signed_mod_by_const`：Hacker's Delight signed magic mul + 后 shift + 符号修正 + `imul_imm divisor` + `isub`。(d) 新模块级 fn `signed_div_magic_i64` / `magic_supported_divisor`：Granlund–Montgomery 搜索 64-bit signed magic，并 gate 掉「需要 add-correction」的那一类（magic ≥ 2^63）— 这类暂不实现，dispatcher 自动 fallback 到 srem 路径。(e) `TraceEmitterState` 新增 `hoisted_mod_magic: HashMap<SsaVar, ir::Value>`；prehoist pass 在 invariant 正-const divisor 命中时在 preheader emit 一次 `iconst.i64 magic` 并 cache。(f) 新增 5 条 unit test：`signed_div_magic_matches_published_values_for_small_divisors`、`magic_supported_divisor_rejects_add_correction_class`（覆盖 100 / 1024 等需要 add-correction 的 divisor）、`signed_mod_magic_matches_native_srem_for_sample_grid`（与 `i64::wrapping_rem` 在多 dividend 上对拍）、`mod_with_const_divisor_lowers_to_magic_mul`（IR 中存在 `smulhi` / 不存在 `srem`）、`w5_shape_mod_in_loop_uses_magic_path`（loop-shape 验证 hoist + magic 同时生效）。 |
| `crates/relon-codegen-native/src/trace_install.rs` | 在已有的 `tracing::trace!` IR dump 旁加了 `RELON_DUMP_TRACE_IR` 环境变量分支：当变量被设置时，emitter 出来的 cranelift IR 直接 eprintln 出来。F-D8-E.6 中的「magic 落在 loop body 还是 preheader」靠它一次定位；保留下来作为后续 perf phase 的 debug 工具。 |

总计：+535 / −21 行；其中 emitter.rs 大头是 magic 算法 + 5 条 test + prehoist 分支。

## 二、关键 IR 形态对比（W5 hot loop block 4）

before（F-D8-E.5）：
```text
block4 (loop body, i % 10 hot site):
    nonzero_b = icmp_ne divisor, 0          # cranelift 优化为常量 1
    brif nonzero_b, ok, deopt
ok:
    lhs_is_min     = icmp_eq i, MIN
    rhs_is_neg_one = icmp_eq divisor, -1
    overflows      = band lhs_is_min, rhs_is_neg_one  # 静态 0
    brif overflows, deopt, safe
safe:
    key_idx = srem i, divisor               # x86_64 IDIV: ~20-25 cycle microcoded
```

after（F-D8-E.6，hoisted magic）：
```text
preheader_block:
    magic_v = iconst.i64 0x6666_6666_6666_6667   # 一次性 mov reg, imm64
    ...
block4 (loop body):
    hi      = smulhi i, magic_v                  # 一条 IMUL + 取 high 64
    q_sh    = sshr_imm hi, 2                     # arithmetic shift
    sign    = sshr_imm i, 63                     # -1 if i<0 else 0
    q       = isub q_sh, sign                    # 修正符号
    prod    = imul_imm q, 10
    key_idx = isub i, prod                       # 6 ops, ~6 cycle 估算
```

整个 `divisor != 0` brif 与 `MIN % -1` band+brif 在 const-divisor 情形下消失。

## 三、W5 bench

`cargo bench -p relon-bench --bench cmp_lua -- W5_dict_str_key`，
`RELON_BENCH_FORCE_RUN=1`。governor `schedutil`，load1 ≈ 2-4。

| 指标 | F-D8-E.5 baseline | F-D8-E.6 after | Δ |
|------|-------------------|----------------|---|
| trace_jit | 188 µs（187.40） | 181.6 µs（181.58） | −3.4% |
| LuaJIT    | 114.6 µs          | 113.8 µs           | 噪声范围 |
| ratio     | × 1.64            | × 1.60             | −2.5% in ratio |

criterion change 检测：第二轮（hoist 落地后）relon_trace_jit 报 `-23.98% … -22.62%`（相对于第一版「magic 没 hoist」的 235 µs 退步基线，确认 hoist 是关键 lever；相对于真正的 F-D8-E.5 baseline 188 µs，净改进 −3.4%）。LuaJIT 抖动 ±1%。

## 四、Gate 五项

1. `cargo fmt --all -- --check`：通过（rustfmt 自动合并 dividends grid）。
2. `cargo clippy -p relon-trace-emitter --all-targets -- -D warnings`：通过。
3. `cargo clippy --workspace --all-targets -- -D warnings`：通过。
4. `cargo test -p relon-trace-emitter`：38 个 lib + 集成全过，含 5 个新增 unit test（见 §一）。
5. `cargo test --workspace --lib` + `cargo test -p relon-test-harness` + `cargo test -p relon-bench --test cmp_lua_consistency` 全过；W5 cmp_lua_consistency 一致性测试（trace-jit 累加 == 解析期望）OK。

## 五、未达 × 1.5 目标 — 诚实记录

任务目标是 W5 ratio 从 × 1.65 压到 ≤ × 1.5。本阶段把 `srem` 替换为 magic-mul + 把 magic-multiplier hoist 到 preheader，实测压到 × 1.60，距离 × 1.5 还差约 0.10（6-10 µs）。剩余 gap 来源：

- **dict scan loop 仍占主导**：FxHash over 短 key（W5 的 `"a".."j"` 全是 1-byte 键）+ 10-entry linear scan。前者每次 hash_loop 跑 1 byte，但 `iconst.i64 FX_HASH_PRIME (0x0100_0000_01b3)` 和 `iconst.i64 FX_HASH_SEED (-3750763034362895579)` 仍每个 outer iter 重 emit；后者每次平均 5 次 hash 比较。Magic-mul 拿掉的是 `i % 10` 的 ~20 cycle，但 dict scan 每 outer iter 的 ~30-50 cycle 还在。
- **dict_inline 内部 const 没有 hoist 路径**：本阶段的 hoist 框架（`hoisted_*` 边表）只覆盖 emitter.rs 直接 emit 的 op；dict_inline.rs 模块内部的 `iconst.i64 SEED / PRIME / 16` 仍在 dict_lookup_inline 的 inner blocks 内反复 emit。给 dict_inline 加可选的 hoisted SEED/PRIME 参数（F-D8-E.5 已对 entry_count 这么做过）能再剥一层，但收益估算 ≤ 2-3 µs（const 加载在寄存器固定后只剩 0-1 cycle 差），不足以独立顶起 × 1.5 收敛。
- **dict scan 真正的 lever 是 unroll / perfect-hash**：W5 的 entry_count = 10 是 dict 头里的运行时 u32，但在 fixture 层是 compile-time 确定的。把 `entry_count` 提升到 `TraceOp::DictLookupPrechecked` 的 immediate（recorder side-table 拿出来的 `Option<u32>`），emitter 命中后 unroll 10 个 hash compare、不发 loop —— 这是 task description 方案 1 的实现路径，但需要 recorder 端同时改动（trace_ir + lowering + side-table），跨 crate 的改动量比本阶段大一个数量级。或者方案 2 的 perfect-hash 路径：record 阶段在已知 dict literal 的固定 keys 上预生成 PHF，runtime 一次 `hash & mask → slot index → 验证 key match → load value`，完全消除 linear scan 与 FxHash 主循环。两条路都在 F-D8-E.7 候选范围内。
- **本阶段的 anti-pattern 教训**：第一版只替换了 `srem` 但没 hoist magic const，bench 直接退步 +25%。这是 F-D8-E.5 报告里「load 可以 hoist，address-mode iadd_imm 不要 hoist」的镜像版 ——「短 IR 序列 + 64-bit imm」是 cranelift register allocator 的 anti-pattern，必须配合 preheader hoist 一并落地。第二版（hoist 后）反而比 baseline 略快，证明 magic-mul 替换本身是 net win，只是收益被 64-bit imm 加载抵消了一半。

要继续向 × 1.5 收敛，下一步候选：
- F-D8-E.7：recorder side-table 把 dict literal 的 `entry_count: Option<u32>` 暴露给 emitter；emitter 在 `entry_count ≤ 16` 时把 dict scan 展开成 `icmp + brif` chain，消除 scan 循环 overhead 与 entry_count 比较。
- F-D8-E.8：对 dict literal 的固定 key 集合做 record-time perfect-hash 构造；emitter 拿到 PHF 表（≤ 16 entries 的 fixed array）后把 lookup 收缩成「短 hash & mask → 一次 hash compare → 一次值 load」。
- F-D8-E.9：给 dict_inline.rs 也接入 F-D8-E.5/E.6 的 hoist 边表（SEED / PRIME / 16 等内部 const），让 emitter 在 outer loop 命中 invariant `dict_ptr` 时把 dict_inline 的全部 const SSA 复用——这是最便宜的最后一击，预计 1-2 µs 增量。

## 六、提交

```
perf(trace-jit): F-D8-E.6 W5 magic-mul mod + preheader hoist for const i % 10
docs(internal): F-D8-E.6 stage report + W5 rerun
```

合并为单 commit 提交。
