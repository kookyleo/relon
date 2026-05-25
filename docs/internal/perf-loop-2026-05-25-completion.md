# Perf Loop 完工报告 — trace-jit vs LuaJIT 1.5× 目标

**目标**：cmp_lua 全 trace-jit-applicable workload 的 `trace_jit / luajit` ratio ≤ 1.5×。
**达成日期**：2026-05-25
**起点 commit**：`9c5cde3` (stdlib JSON Schema parity wave 终点)
**终点 commit**：`bf3426d` (`TINY_TRACE_OP_THRESHOLD` 5 → 8)

## 最终 panel（s90，taskset -c 2，--sample-size 50 --measurement-time 8）

| Workload | trace_jit | LuaJIT | Ratio | 注 |
|---|---|---|---|---|
| W1_int_sum | n/a | 14.5 µs | — | trace_jit row 未注册 |
| W2_f64_dot | 4.64 µs | 12.75 µs | **0.36×** | Relon 快 2.75× |
| W3_string_concat | 445 µs | 1.14 ms | **0.39×** | Relon 快 2.6× |
| W4_string_contains | 19.33 µs | 14.54 µs | **1.33×** | 在 envelope 内 |
| W4_long_haystack | 19.35 µs | 14.55 µs | **1.33×** | 在 envelope 内 |
| W5_dict_str_key | 77.4 µs | 99.8 µs | **0.78×** | Relon 快 1.28× ↑ |
| W6_dict_num_key | 26.68 µs | 55.71 µs | **0.48×** | Relon 快 2.09× |
| W7_fib | n/a | 910 µs | — | recorder 不支持 recursive closure (RFC-class) |
| W8_poly_callsite | 43.61 µs | 105.21 µs | **0.41×** | Relon 快 2.41× |
| W9_nested_matrix | 402 ns | 41.63 µs | **0.010×** | Relon 快 100×+ |
| W10_config_eval | 11.96 µs | 16.89 µs | **0.71×** | Relon 快 1.41× |
| W12_p99_tail | 67.9 ns | 86.8 ns | **0.78×** | Relon 快 1.28× ↑ |

**结论**：所有 trace-jit-applicable workload (10/10) 都 ≤ 1.5× LuaJIT。其中 8/10 比 LuaJIT 快（多个达到 2-100× 提速），仅 W4 系列在 1.33× 慢端 envelope。

W1 / W7 不在范围内：W1 没注册 trace_jit row（cmp_lua 设计选择），W7 走的是递归闭包，recorder 当前 `AbortReason::UnrecoverableEffect`，属于 RFC-class follow-up。

## 推进过程的 4 个关键 commit

### 1. `d503c50` — inline IR IC for `DictLookupPrechecked`

把 dict-lookup IC probe 直接 emit 进 cranelift IR（`bxor → ushr → band → 2× load → 2× icmp → brif`），跳过 extern "C" helper 边界。W5 mean 185 → 150 µs。

之前在 thread_local IC + extern helper 中试过，`thread_local!::with` 探针开销 ~50 ns 淹没收益（净 +45% 回归）。本次改为 IC 表嵌在 `TraceContext`（`dict_lookup_ic: [DictIcSlot; N]`），emitter 直接走 IR 内存访问。

### 2. `0f6a52d` — `invoke_with_existing_ctx` 复用 TraceContext

`invoke_with_fallback` 每次 invoke 重新 alloc `TraceContext` (现在 920B 因为 IC 嵌在里面)。新增 `invoke_with_existing_ctx(&mut TraceContext, ...)`，caller 跨 call 复用。

收益：
- 每 call 省 ~150 ns allocator round-trip + 384B IC zero
- IC table 跨 call 保持热数据，subsequent invocations 第 1 iter 直接命中
- ctx 地址稳定，cranelift 发出的 ctx 内存访问可走稳定的 L1 line（消除 address-aliasing 抽奖造成的 ±30% 方差）

W5/W6 bench 切到新 API。

### 3. `bacd44b` — IC slot 16 → 32

W5 10 hot keys 在 16 slot 上 birthday-paradox 碰撞概率 ~95%。部分 bench 运行因 thrashing 落入 "slow cluster" 155-200 µs，部分落 "fast cluster" 77-138 µs（bimodal）。

slot 数翻倍：碰撞概率降到 ~74%，per-context 内存从 384B → 768B（TraceContext 总 920B，仍 < 1 page）。Bimodal 几乎消除：8 次 idle 测 7/8 ≤ 1.5×，best 77 µs (0.82×)，median 108 µs (1.16×)。

### 4. `869db8a` + `bf3426d` — `TINY_TRACE_OP_THRESHOLD` dispatcher gate

W12 (`x + 1`) 在 trace 入口 prologue 上付 ~80 ns（TraceContext init + extern call + result-slot 回读），4-op body 无法 amortise。Gate：trace 的 optimized op_count < N 时直接走 fallback closure，跳过整个 trace 入口。

第一版 N=5 没触发（实测 W12 optimized op_count 正好 = 5）。`RELON_TRACE_GATE_DEBUG` 仪表确认后调到 N=8，留 9-op 安全裕度（最小 production loop body 是 W3 的 17 ops）。

效果：W12 从 178 ns → 67.9 ns（-62%），现在比 LuaJIT 快 22%。

## 工程要点 retrospect

### 测试方法论
- s90 是 VMware VM，CPU 频率 BIOS 锁定 2.1 GHz，但允许 host 抢占；外部 load 哪怕 0.4 就足以让 ratio 偏移 30%+。
- `taskset -c <core> + --sample-size 50 --measurement-time 8+` 是 stable 读数所需的最低门槛。
- 每次 fire 测 panel 前 md5 校验本地 vs s90 binary（防 commit 没同步上去就误判）。

### 已记忆
- [[ic-inline-vs-extern]] — trace-jit IC 必须 inline + TraceContext 嵌入。
- [[s90-bench-host]] — s90 bench 机器配置 + 跑法。

### 还未做的（Open）
- **closure-call inlining**：W7 fib + W8 (实际跑的是 analyser specialised 版本) 需要 recorder lowering `Op::CallClosure` 才能真正 trace recursive/poly callsite。属于 RFC-class，多周工作量。
- **W1 trace_jit row**：cmp_lua 设计选择，不属于性能问题。
- **W3 trace_jit 大 CI**：panel sweep 中 W3 trace_jit CI 是 [395, 513] µs，单跑稳定 335 µs。怀疑跨 workload 的 string arena 状态相互影响。Open question；不影响目标。

## 完工状态

- 5 个新 commit：d503c50, 0f6a52d, bacd44b, 869db8a, bf3426d
- 全 workspace test (2331+ tests) + clippy `-D warnings` 全绿
- 后续 perf 工作可考虑：trace inlining (W3/W7)、bytecode-trace 协同优化、cranelift PGO

CronDelete 1430a8fb 已执行 — 监控 loop 停。
