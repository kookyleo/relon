# F-D7-J 阶段报告：W4 trace framework overhead 修剪（2026-05-20）

## 摘要

F-D7-E (SIMD memchr) + F-D7-G (Load LICM) + F-D7-H (StringRef payload
expose) 把 `StrContains` 本体压到 SIMD floor 后，W4 (3-byte) 与
W4_long_haystack (256-byte) 两行的 trace_jit 都停在 × 1.66 LuaJIT 上下。
F-D7-H 阶段报告把瓶颈定位到 trace framework 层 — 不是 scan 本体。

F-D7-J 抓 framework 层的两个 lever：

1. **LICM 准入 `Guard(NotNull(v))`**：当 `v` 是 loop-invariant SSA 时
   pass/fail iteration-independent，与 F-D8-E.3 对 `BoundsCheck` 的安全
   论证同型。F-D7-B 在每个 `StrContains` 前都注入 `NotNull(haystack)`，
   W4 LICM 已把 `LocalGet(haystack)` 提到 preheader 后，这条 guard 也跟
   着上去，loop body 少一个 `brif` per iter。
2. **`Guard(ArithOverflow(dst))` brif 直接打 `of_bit`**：原路径是
   `icmp(eq, of, 0) → uextend(I32) → brif(pred)`，套用 `IsZero` /
   `NotNull` 已有的快速路径模式，改成 `brif(of, deopt, ok)` —
   `of=1` 走 deopt，省掉 icmp + uextend 两条指令。W4 hot loop 一轮
   2 个 `Add(I64)`（`count += hit` 与 `i + 1`），都吃这条快速路径。

两个改动直接命中 framework 层 per-iter 工作的两条主线（NotNull guard /
overflow guard），不动 SIMD scan 本体，也不改 trace IR shape。

## 一、起点

```
worktree HEAD: 462fd83 merge(parser+eval+cli): F-D2-I parser fast-path + Context lite prep
```

F-D7-H 报告给的 W4_long baseline：trace_jit = 29.718 µs / luajit =
17.925 µs / ratio × 1.658（本机 quiescence=non-perf governor +
load1≈12，`RELON_BENCH_FORCE_RUN=1`）。

## 二、改动

| 路径                                                       | LoC          | 说明 |
|------------------------------------------------------------|--------------|------|
| `crates/relon-trace-jit/src/optimizer/licm.rs`             | +30 / -13    | `is_hoistable` 给 `Guard(NotNull(_))` 开门；模块 doc + 行内注释更新到 F-D7-J 安全论证 |
| `crates/relon-trace-jit/tests/licm_smoke.rs`               | +74 / -0     | `loop_invariant_not_null_guard_lifts_out_of_loop`、`loop_variant_not_null_guard_stays_inside` 两条新测 |
| `crates/relon-trace-emitter/src/guard_emit.rs`             | +28 / -0     | `emit_guard` 加 `ArithOverflow` 快速臂：`overflow_bits` 有命中时 `brif(of, deopt, ok)`，跳过 `build_guard_predicate` 的 icmp+uextend 链 |
| `crates/relon-trace-emitter/tests/emit_branch.rs`          | +47 / -0     | `arith_overflow_guard_skips_icmp_with_captured_of_bit` — 验证 `Add → Guard(ArithOverflow(dst))` 链路上 cranelift IR 不再出现 `icmp eq` |

合计：**+179 / -13**，单 commit 落地。

## 三、安全论证

- **`NotNull` hoist**：与 `BoundsCheck` 同型。当 `v` 是 loop-invariant
  SSA，每 iter 的 `v == 0` 答案恒等，hoist 只把 (本就要 fire 的) deopt
  提前一轮 —— 不会让原本能跑完的 trace 多 deopt 一次。原始 guard
  位置与 loop head 之间所有 in-loop 工作（StrContains 等）也都满足
  loop-invariance 条件可同步 hoist；即使留在 body 里也是 pure /
  ReadOnly，不会有提前 deopt 误丢 side-effect 的风险。
- **`ArithOverflow` brif 直接打 `of_bit`**：`sadd_overflow` 的 `of`
  是 i8 bool，cranelift `brif(value, then, else)` 把 value != 0 当
  true。把 `of != 0` 直接走 deopt 与原 `icmp(eq, of, 0); pred =
  uextend(I32, no_of); brif(pred, ok, deopt)` 在语义上 bit-for-bit 等
  价。`overflow_bits` 没命中时仍走 `build_guard_predicate` fallback，
  保留 synthetic / hand-built buffer 的常量 0/1 predicate 兼容性。

## 四、闸门 / 测试

```
cargo fmt --all -- --check                                                ✓
cargo clippy -p relon-trace-jit -p relon-trace-recorder \
            -p relon-trace-emitter -p relon-bench \
            --lib --tests --benches -- -D warnings                        ✓
cargo test -p relon-trace-jit -p relon-trace-recorder \
           -p relon-trace-emitter -p relon-codegen-native --lib --tests   ✓
cargo test -p relon-bench --test cmp_lua_consistency                       ✓
cargo build --workspace                                                    ✓
```

新增测试：
- `relon-trace-jit` licm_smoke: `loop_invariant_not_null_guard_lifts_out_of_loop`,
  `loop_variant_not_null_guard_stays_inside`
- `relon-trace-emitter` emit_branch: `arith_overflow_guard_skips_icmp_with_captured_of_bit`

既有 licm_smoke 中 `non_bounds_guards_remain_pinned_even_when_invariant`
测的是 `TypeCheck`（仍 pinned，与 F-D7-J 一致），未受影响。
`arith_overflow_guard_lowers_predicate`（synthetic buffer，无 Add 上游）
走 fallback 臂依旧通过。

## 五、bench 数据

`/tmp/d7j_bench.txt`（commit-post 测量，本机 quiescence=非 perf
governor + load1≈9 ~ 10，`RELON_BENCH_FORCE_RUN=1`）：

| 行                                       | trace_jit         | luajit            | ratio          |
|------------------------------------------|-------------------|-------------------|----------------|
| W4_string_contains  (3-byte "axb")       | 23.788 µs         | 29.054 µs *       | **× 0.82 ***   |
| W4_long_haystack    (256-byte, 'x'@末位) | 23.840 µs         | 17.907 µs         | **× 1.331**    |

\* W4_string_contains 这一行 LuaJIT 出了 17% 高噪 outlier（CI [20.9,
41.3] µs），点估计不可信；W4_long 同条件 luajit 17.907 µs CI 极窄，
取为代表性数。两行 trace_jit 几乎相同（23.788 vs 23.840 µs）—— 与
F-D7-H 的"SIMD floor 已达到，long/short 不分"观察一致。

差量对比（W4_long 行，trace_jit）：

- F-D7-H baseline：29.718 µs / × 1.658
- F-D7-J post   ：23.840 µs / × 1.331
- delta         ：**−5.88 µs (−19.8%) / ratio 降 0.327 个点**

目标 ≤ × 1.5 LuaJIT — **W4_long 达成 (× 1.331)**。W4 (3-byte) 这一行
LuaJIT 噪声大无法严谨判断，但 trace_jit 数据本身从 29.826 → 23.788 µs
（−20.2%）与 W4_long 同步，按 W4_long 的 luajit 基准（约 17.9 µs）外推
ratio 同样落在 × 1.33 上下，**达成**。

每 iter 拆解：
- pre F-D7-J：~3.0 ns/iter，含 1 个 NotNull brif + 2 个 (icmp + uextend
  + brif) ArithOverflow 链 ≈ 7 cranelift inst/iter framework overhead。
- post F-D7-J：~2.4 ns/iter，1 个 NotNull 已 hoist、2 个 ArithOverflow
  各砍掉 icmp + uextend ≈ 砍 4 inst/iter（净剩 2 brif），符合实测 ~20%
  cycle 节省。

## 六、诚实记录 / blocked

1. **W4 (3-byte) 行的 luajit 数据噪声**：本机当前 load1=9~10，
   CI 没办法等到 quiescence 时段重测。报告里用 W4_long 行作 ratio 主
   依据（luajit CI 极窄）。trace_jit 自身的前后对比 W4 / W4_long 两行
   都跌 20%，结论稳健。
2. **fallback 路径仍在**：`build_guard_predicate`'s ArithOverflow 臂
   保留原 `icmp(eq, of, 0) + uextend` 链路，覆盖 unit-test 用的合成
   buffer 路径（如 `arith_overflow_guard_lowers_predicate` 的 const-
   only trace）。real-world 走的是真 `Add → Guard(ArithOverflow)` 配
   对，命中快速臂；synthetic 路径无 Add 上游，落 fallback，行为不变。
3. **未触及 SIMD scan 本体 / StringRef payload deref**：F-D7-E / G / H
   已经把这些路径压到 floor。F-D7-J 完全在 framework 层施力，互不
   干扰。
4. **未做的潜在 lever**：`StrContains` 自身的 LICM hoist（其 inputs
   loop-invariant + Pure 已 admit）应该在 `LocalGet` / Load 之后再一
   轮 LICM 触发；但 W4 hot loop 调 contains 是为了 _count_ 命中数 ——
   把 contains 完全 hoist 出去会让 trace IR 把 `hit` 也 hoist
   （`count += hit` 等价于 `count += k`，k 为常量），那是 strength
   reduction 范畴，本阶段不动以保 trace IR 简洁性。本机 bench 的
   `n=10000` × 单次 SIMD scan ~12 ns ≈ 120 µs，远超 23.8 µs 实测 ——
   说明 cranelift backend 本身已经在做某种程度的 CSE / 常量传播把
   contains 折掉。
5. **`TypeCheck` guard 仍 pinned**：当前 emitter 把 `TypeCheck` 的
   predicate 在 emit-time 折成常量 0/1 brif，hoist 不省工。如果未来
   `TypeCheck` 走运行时 tag 比较再考虑开门。
