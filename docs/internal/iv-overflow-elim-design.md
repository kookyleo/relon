# Induction-Variable Overflow Elimination — Design

**Status**: 待实施
**Owner**: subagent worktree
**起点 commit**: `acdcc4a` (W4/W12 优化完工后的 main)
**Bench 目标**: W4_string_contains / W4_long_haystack trace_jit ratio **< 1.0× LuaJIT** (实现 "全面超越" 阶段 A 唯一缺口)

## 背景

W4 当前 trace_jit ratio 1.33× LuaJIT，所有其他 trace-jit-applicable workload 已经超越 LuaJIT (8/10 < 1.0×)。

实验测得（commit `acdcc4a` 之后临时加 strip pass + bench s90）：

| 版本 | W4 trace_jit | LuaJIT | Ratio |
|---|---|---|---|
| Guards ON (现状) | 19.33 µs | 14.53 µs | **1.33×** |
| ArithOverflow Guards 完全剔除 | **12.90 µs** | 14.54 µs | **0.89×** ← 反超 |

证实：W4 的 1.33× gap **全部**来自 loop body 里的 `Guard(ArithOverflow(i_next))` + `Guard(ArithOverflow(count_next))` —— 每 iter 2 个 brif。剔掉后 cranelift DCE 把 `sadd_overflow` 降到 `iadd`，差距完全消失。

之前 env-gated 实验 (只改 guard predicate 成 `iconst(1)`) 没动 brif 指令本身，所以测不出差别。

## 目标

在 trace 优化阶段，**安全地**剔除可证明不会溢出的 `Guard(ArithOverflow(_))` ops，让 cranelift DCE 把 `sadd_overflow` 降成纯 `iadd`。

## 当前 W4 trace IR（LICM 之后）

```
# Pre-loop (preheader)
ConstI64 SsaVar(0) = 0          # i_seed
ConstI64 SsaVar(2) = 0          # count_seed
LocalGet SsaVar(7) = arg0       # n
LocalGet SsaVar(9) = arg1       # haystack
LocalGet SsaVar(10) = arg2      # needle
Guard NotNull(SsaVar(9))
Load SsaVar(12) = [SsaVar(9)+0]  # haystack ptr
Load SsaVar(13) = [SsaVar(9)+8]  # haystack len
StrContains SsaVar(11)           # ← hoisted out of loop
ConstI64 SsaVar(18) = 1          # step

# Loop
MarkLoopHead loop_id=0 phis=[
  LoopPhi { init: SsaVar(2), phi: SsaVar(4) },  # count
  LoopPhi { init: SsaVar(0), phi: SsaVar(5) },  # i
]
Cmp Ge SsaVar(8) = SsaVar(5) >= SsaVar(7)  # i >= n
Guard IsZero(SsaVar(8))                    # exit when true
Add SsaVar(15) = SsaVar(11) + SsaVar(4)    # count += hit
Guard ArithOverflow(SsaVar(15))            # ← 待剔除 #1
Add SsaVar(19) = SsaVar(5) + SsaVar(18)    # i += 1
Guard ArithOverflow(SsaVar(19))            # ← 待剔除 #2
MarkLoopBack loop_id=0 next_values=[SsaVar(15), SsaVar(19)]
Return SsaVar(15)
```

## 安全证明

记：
- `i` 的 init = `c0 = 0`, step = `c1 = 1`
- `count` 的 init = `0`, step = `hit` (LICM-hoisted, observed type Bool, range [0,1])
- 退出条件: `Cmp Ge i, n` + `Guard IsZero` → 退出时 i ≥ n，进入下一次迭代时 i < n

**对 `i + 1` 不溢出的证明**:
- 在 `Add SsaVar(19) = i + 1` 执行点，因为 Guard IsZero 在前，所以 i < n
- max(i + 1) = (n - 1) + 1 = n
- 需要 n ≤ i64::MAX → 在 trace 入口前加一个 `Guard(n ≤ i64::MAX - c1)` 

实际：`n` 是从 `LocalGet` 拿的 I64 arg，类型已确定。runtime n 值要 i64::MAX 才会出问题。

**对 `count + hit` 不溢出的证明**:
- max(count) over loop = sum of all `hit` values over iters
- 每个 hit ∈ [0, 1] (Bool 类型保证)，max trip count = (n - c0) / c1 = n
- max(count) ≤ n
- max(count + hit) = n + 1 ≤ n + 1 ≤ i64::MAX 同样的条件

**关键洞察**：单一 entry guard `n ≤ MAX_SAFE_LOOP_BOUND` (取 `i64::MAX / 4` 给 step 留余地) 同时证明两个 in-loop overflow guards 静态死。

## 实施方案

### Step 1: 加新 optimizer pass `iv_overflow_elim`

文件: `crates/relon-trace-jit/src/optimizer/iv_overflow_elim.rs`

```rust
pub struct IvOverflowElim;

impl OptimizerPass for IvOverflowElim {
    fn name(&self) -> &'static str { "iv_overflow_elim" }
    fn run(&self, trace: &mut TraceBuffer) -> PassReport { ... }
}
```

### Step 2: 算法

```text
for each MarkLoopHead..MarkLoopBack region:
    1. 收集 (phi, next_val) 配对
    2. 找退出 idiom: 紧跟 MarkLoopHead 之后的 [Cmp Ge phi, n] + [Guard IsZero(cmp)]
       — 提取被比较的 phi = exit_phi, 边界 = N
    3. 对每个 phi:
       3a. 若 init = ConstI64(c0), next = Add { phi, ConstI64(c1) }, c0 ≥ 0, c1 ∈ [1, 2^32]:
           — 检查这个 phi 是不是 exit_phi 或者跟 exit_phi 一起被同一个 N 限制（即被同一个退出条件覆盖）
           — 标记 Guard(ArithOverflow(next)) 为 dead
       3b. 若 init = ConstI64(0), next = Add { phi, S } 且 S 是 loop-invariant 的 SSA
           — S 的 observed type 是 Bool (range [0,1]) 或 ConstI64(c) 且 |c| ≤ 2^32
           — 同样 exit-bounded 路径下，标记 dead
    4. 若有任何 guard 标记 dead:
       — synthesize 入口 guard: 
         * ConstI64 max_safe = i64::MAX / 4
         * Cmp Lt cmp_safe = N < max_safe
         * Guard IsZero(cmp_safe inverted)  ← 用 Cmp Le + IsZero(...)
         (具体用现有 GuardKind 拼装)
       — 删除所有 dead guards
       — 重建 trace.ops + guards 表
```

**Entry guard 拼装**: 不引入新 GuardKind，用：
```
ConstI64 SsaVar(K) = MAX_SAFE_LOOP_BOUND
Cmp Lt SsaVar(K2) = N < SsaVar(K)        # 1 when safe, 0 when n too big
Guard IsZero(SsaVar(K2))                  # IsZero fires deopt when cmp result is 0 (n太大)
```

Wait, semantically：`Guard(IsZero(v))` fires deopt when `v != 0`. 看现有定义。要查 IsZero 语义后再定。

更安全的写法（不依赖 IsZero 语义反向）:
```
ConstI64 K = -MAX_SAFE_LOOP_BOUND
Add D = N + K       # D = N - MAX. 若 N ≤ MAX 则 D ≤ 0.
Guard NotNull(D)? 
```

不行。最直接：插入 `Cmp Le D = N ≤ MAX_SAFE` 然后 `Guard(IsZero(NotEqual cmp))`。

**最稳的做法是直接 sub + 比较 + IsZero**：
```text
ConstI64 max_b = i64::MAX / 4
Cmp Gt cmp_unsafe = (N > max_b)    # 1 if unsafe, 0 if safe
Guard IsZero(cmp_unsafe)           # deopt 当 cmp_unsafe ≠ 0
```

要确认 `Guard IsZero(v)` 触发 deopt 的条件是 `v != 0`。看 `relon-trace-emitter/src/guard_emit.rs` IsZero 那部分。

### Step 3: 流水线位置

`iv_overflow_elim` 放在 LICM **之后**、最后的 DeadStoreElim **之前**：

```rust
Box::new(licm::LICM),
Box::new(noop_typecheck_elim::NoopTypeCheckElim),
Box::new(iv_overflow_elim::IvOverflowElim),    // ← 新增
Box::new(dead_store::DeadStoreElim),
```

原因：
- LICM 之后才会有清晰的"循环边界外的 invariant"。N (LocalGet n) 此时已经在 loop 外。
- 跑在 DeadStoreElim 之前，被剔除的 guard 留下的 sadd_overflow 的 of_bit 没人引用，DeadStoreElim 会清。

### Step 4: 单元测试

`crates/relon-trace-jit/src/optimizer/iv_overflow_elim.rs` 内 `#[cfg(test)] mod tests`:

1. `test_strip_loop_counter_overflow_guard` — 构造一个 W4 形 trace, 验证 ArithOverflow on `i+1` 被删, entry guard 插入
2. `test_strip_accumulator_overflow_guard` — 验证 `count + Bool` 形 accumulator 被删
3. `test_keeps_overflow_guard_when_step_too_large` — c1 > 2^32 的 case 保留
4. `test_keeps_overflow_guard_when_n_not_invariant` — N 在 loop 内变化 case 保留
5. `test_keeps_overflow_guard_when_no_exit_pattern` — 没有 Cmp Ge + IsZero 退出 case 保留

### Step 5: bench 验证

- 本地: `cargo test --workspace --exclude relon-bench --exclude relon-wasm` 全过
- `cargo clippy --workspace --all-targets -- -D warnings` 干净
- `cargo bench -p relon-bench --bench cmp_lua --no-run` 编出 release
- scp 到 s90: `~/relon-bench-rt/cmp_lua`
- 跑全 panel: `taskset -c 2 + --sample-size 50 --measurement-time 8`
- W4 应该 ≤ LuaJIT (target < 1.0×)
- 其他 workload 不要回归

### Step 6: 完工 commit

Commit 单独一个: `perf(trace-jit): elide ArithOverflow guards on bounded induction vars`

写到 `docs/internal/`：完工报告 + 最终 panel ratios。

## 边界 / Open Questions

1. **MAX_SAFE_LOOP_BOUND 选 `i64::MAX / 4` 是否过紧？** — 给 step c1 留 2^61 余地。实际 step c1 几乎都 ≤ 100。可选 `i64::MAX / 2` 更宽。
2. **嵌套循环** — 第一版只处理单层循环。嵌套循环里 inner phi 的 N 可能是 outer phi 的函数；先 skip，留 follow-up。
3. **Sub/Mul step** — 第一版只处理 Add step。Sub/Mul 后续扩展。
4. **deopt 路径正确性** — entry guard 触发 deopt 时，需要 trace 的 GuardSite 表能正确映射到 bytecode resume PC。新插入的 guard 没有原生 external_pc，用 trace_pc = 0 (entry) 应该 ok。要测。

## 实施 deliverables

1. `crates/relon-trace-jit/src/optimizer/iv_overflow_elim.rs` (新文件，~250 行)
2. `crates/relon-trace-jit/src/optimizer/mod.rs` 改 (加 module decl + pipeline 一行)
3. 5 个单元测试 in `iv_overflow_elim.rs`
4. s90 bench panel 验证 W4 < 1.0×, 其他无回归
5. `docs/internal/iv-overflow-elim-completion.md` 完工报告

预估工作量: **1-2 天**（实施 + 测 + 验证）。
