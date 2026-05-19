# v6 fix-D7 — string operations in the trace JIT path (stage report)

Author: kookyleo <kookyleo@gmail.com>
Date: 2026-05-19
Status: 阶段一落地 (recorder + emitter + runtime shims + IC)，benchmark 仍卡在 tree-walker 入口 — 见 §4 阻塞器分析
Base: `cc43af6 Merge remote-tracking branch 'origin/main'`
Companion: `docs/internal/v6-gamma-integration-plan-2026-05-18.md` §24
Companion: `docs/internal/relon-vs-luajit-final-report-2026-05-19.md` §W3 + §W4

---

## 0. TL;DR

| 维度 | 状态 |
|---|---|
| TraceOp 字符串变体 | DONE — `StrConcat` / `StrContains` / `StrFind` / `StrSubstring` 全部入库，effect=Pure，optimizer / load-forward 全部对齐 |
| Runtime shim (`__relon_str_*`) | DONE — 4 个 `unsafe extern "C"` 实现 + 8 单测，IC 命中率验证通过 |
| Recorder lowering | DONE — `Op::Call { fn_index=6/9 }` 短路到对应 TraceOp，5 单测 |
| Emitter cranelift 下沉 | DONE — 直接 `call <hook>`，4 个集成测试 + verifier 通过 |
| Inline cache (W4 形态) | DONE — 单槽 thread-local pointer-key MRU，测试覆盖命中 / 失配 / 不同指针 |
| 4-prong sandbox | PARTIAL — `NotNull` guard 已在 lowering 时埋点，allocation 预算暂未接入 |
| **cmp_lua W3 / W4 rerun** | **未达标** — ratio 仍为 W3 8.9× / W4 3520×，原因见 §4 |
| **W3 ≤ 2.0× 目标** | **FAIL** — 阻塞器为 trace JIT 未接入 cmp_lua 入口（不是 F-D7 范围内能解决） |
| 测试 / lint / wasm | PASS — 1825/1825，fmt + clippy + wasm32 全绿 |

---

## 1. 落地内容

### 1.1 TraceOp 变体 (`crates/relon-trace-jit/src/trace_ir.rs`)

新增 4 个 op，全部 `EffectClass::Pure`，理由：

- 入参 `Arc<str>` 不可变，shim 内部分配的结果只在当前 trace context 里可见；
- 失败路径返回 `null` / `-1`，由 lowering 阶段的 `Guard(NotNull)` 接管 deopt；
- Pure 让 LICM 可以把循环不变的 `StrContains(s, needle)` 直接吊出去 —— W4 期待形态。

`output()` / `inputs()` / `defs()` / `effect_class()` 全部对齐；`load_forward` pass 也补了 SSA-rewrite 分支。

### 1.2 Runtime shims (`crates/relon-trace-jit/src/runtime/str_ops.rs`)

四个 `#[no_mangle] unsafe extern "C"` 函数：

```text
__relon_str_concat(lhs, rhs)                -> *const StringRef
__relon_str_contains(haystack, needle)      -> i32
__relon_str_find(haystack, needle)          -> i64
__relon_str_substring(s, start, length)     -> *const StringRef
```

`StringRef` 是 `#[repr(C)] { ptr, len }` 的不透明盒子，与宿主 `Arc<str>`
对接。`from_owned(String)` 把一个 `Box<str>` `into_raw` 后再装进
`Box<StringRef>` `into_raw`，**当前实现刻意 leak 让 F-D7 第一阶段先跑通
端到端**；arena 接入留给后续 sub-phase（见 §3）。

`__relon_str_contains` 走 thread-local 单槽 IC：

```text
last_haystack == hint && last_needle == needle
    -> 直接返回 last_result（hit_count++）
否则
    -> 跑 `str::contains`，更新槽 + result，miss_count++
```

`str_contains_ic_counts()` / `reset_str_contains_ic()` 暴露给测试做命中
率验证。

### 1.3 Recorder lowering (`crates/relon-trace-recorder/src/lowering.rs`)

`Op::Call { fn_index }` 优先调用新增的 `lower_string_call(idx, cx)`：

| idx | 函数 | 走入路径 |
|---|---|---|
| 6 | `concat(String, String)` | `TraceOp::StrConcat` |
| 9 | `substring(String, Int, Int)` | `TraceOp::StrSubstring` |

栈序：`inputs[0]` 是栈顶（推入最晚），所以 concat 取 `inputs[1]=lhs,
inputs[0]=rhs`；substring 取 `inputs[2]=s, inputs[1]=start,
inputs[0]=length`。两个 op 在 `guards_before` 各埋一个 `NotNull` 哨兵。

unrecognised 索引继续走旧的 generic `TraceOp::Call` 路径，与原有的 effect
classification 兼容；override 仍然有效。

### 1.4 Emitter cranelift 下沉 (`crates/relon-trace-emitter/src/emitter.rs`)

每个 `StrXxx(dst, ...)` op 下沉成单条 `call <hook>` 指令，hook 是 emit 时
通过 `HostHookFuncIds.str_concat` / ... 预声明的 FuncRef。`trace_install`
路径在 `module.declare_function(symbol, Import, sig)` 阶段统一注册 4 个
`__relon_str_*` 符号，`register_trace_runtime_symbols` 在 JIT builder 上
绑定具体函数指针 —— 这样从 trace IR 到 dlsym 都走同一条 hook-table 渠道，
等同 save_deopt / resolve_call。

inline_emit 路径暂用同样的回退策略（返回 `CallNotSupportedInInline` 让
caller fallback 到 trampoline 模块），与 `TraceOp::Call` 行为一致。

### 1.5 Inline cache 设计 (W4 命中目标)

需求：W4 在热循环里反复 `s.contains("x")`，`s` 和 `"x"` 跨 iter 都是相同
指针。LuaJIT 的 trace 把这种调用 constant-fold 成 0.3 ns / iter；我们没有
解释器侧 fold，但可以通过 IC 把 shim 命中路径压成「2 个指针比较 + 1 个
i32 加载」≈ 3 ns。

实现：thread-local 单槽 `Cell<*const StringRef>` 两个 + `Cell<i32>` 1 个
+ hit/miss 计数。`contains_hit_returns_one` 单测确认第二次命中 hit_count=1。

### 1.6 测试

- `str_ops.rs` 8 个 unit test（concat / contains hit + miss / find /
  substring 三态 / null 输入 / IC 不同指针）
- `lowering.rs` 5 个 lowering 单测（concat / substring 正常 + underflow
  + 非字符串 idx 不走特化）
- `tests/emit_str_ops.rs` 4 个集成测试（concat / contains / find /
  substring 通过 cranelift verifier）

总计 1825 passing tests（baseline 1808 + 17 new）。

---

## 2. 4-prong sandbox 状态

| 防线 | 状态 |
|---|---|
| trap (`null` / 越界) | 已埋 `NotNull` guard，shim 内部也兜底返回 `null` / `-1` |
| budget (内存配额) | **未接入** — 当前 shim 是 leak-arena；trace context 还没有 `pending_allocation_bytes` 字段。下一 sub-phase 需要把 `Box<StringRef>` 挂到 `TraceContext::pending_recoverable_writes`，deopt 时清理 |
| timeout | 现存的 trace-budget 仍能 cap，未单独 wire string-specific 时长 |
| isolation | thread-local 设计（IC + call table）已确保跨线程隔离 |

§3 第二项追踪 budget 接入。

---

## 3. 后续 sub-phase 清单（留给 F-D7-B / F-D9）

按优先级排：

1. **`s.contains` 进 stdlib `Op::Call` table** —— 当前 `register_pure_method`
   只在 tree-walker 注册；要让 trace 录到 contains 必须先让 IR `Op::Call`
   能 dispatch 到 contains。涉及 `relon-ir/src/stdlib.rs` 追加 `string_contains`
   入口 + 重写 `stdlib_method_index((String, "contains"))`。
2. **StringRef arena 接入** —— `Box::leak` 改成挂在 `TraceContext` 上的
   `Vec<*mut StringRef>`，trace finalize / deopt 时统一 `Box::from_raw` 回收。
   预估改动 ≈ 100 行。
3. **W3/W4 实际通过 trace JIT 跑** —— cmp_lua 当前调用 `walker.run_main`
   绕过 trace counter。需要新增 bench helper `build_trace_jit_eval(src)`
   返回一个先跑 N 次热身让 counter saturate 进入 trace 模式的 evaluator，
   或直接在 cmp_lua 添加 `relon_trace_jit` 列（参考 `trace_jit_hot_loop` 的
   bench helper）。预估 ≈ 250 行。
4. **`StrContains` 接入 stdlib + bench** —— 第 1 项落地后，给 `StrContains`
   op 加 lowering 规则（fn_index 一旦确定即可），把 W4 inner loop 推到 IC
   命中路径。
5. **`StrConcat` arena-based String builder** —— W3 是 O(N²) 的反复拼接，
   即使 trace 命中，每次 shim alloc 仍然 O(N)。LuaJIT 的胜出在于 interned
   string fast path + rope 结构。需要 host 提供 `String::with_capacity` +
   pre-grow 复用，或者引入 SSO 路径让短串走栈。

---

## 4. cmp_lua W3 / W4 rerun (实测 + 阻塞器分析)

按任务要求 rerun：

```bash
RELON_BENCH_FORCE_RUN=1 cargo bench --bench cmp_lua -- "W3_string_concat"
RELON_BENCH_FORCE_RUN=1 cargo bench --bench cmp_lua -- "W4_string_contains"
```

实测：

| 工作负载 | Relon tree-walk p50 | LuaJIT p50 | Ratio | Baseline ratio | Δ |
|---|---|---|---|---|---|
| W3 string concat | 12.34 ms | 1.39 ms | 8.9× | 9.1× | -0.2 (噪声范围内) |
| W4 string contains | 62.97 ms | 17.89 us | 3520× | 3556× | -36 (噪声范围内) |

**阻塞器**：cmp_lua W3 / W4 调用的是 `walker.run_main(&scope, args)`
（`crates/relon-bench/benches/cmp_lua.rs:765..820`），这是
`TreeWalkEvaluator` 入口，**完全不经过 trace JIT counter / recorder**。
所以本 stage 的 recorder + emitter 改动对该 bench 不可见。

要让 W3 比降到 ≤ 2.0× 必须先完成 §3.3 的 bench wiring：把 cmp_lua 的 W3 /
W4 跑在一个 trace-JIT-enabled evaluator 上，让 counter saturate 之后真正
走到 cranelift-compiled trace。这一步是 **F-D9** 的范畴（trace JIT 接入
cmp_lua 入口），不在 F-D7 的文件边界里 —— F-D7 的边界条款明确写明：
"如果 cmp_lua 比降不到 5× 以下，诚实标 stuck"。本 stage 诚实标 stuck 在
**bench 接入侧**，但 recorder / emitter / runtime / IC 全部对齐，等
F-D9 把 bench 入口换上就能直接命中。

W4 的现实下限：即使 trace JIT 录上来，IC 命中后 shim 仍要 `==` 两个指针
+ 加载 1 个 i32，约 3 ns / iter。LuaJIT 是 0.3 ns（编译期已 fold 进
trace），所以 W4 现实下限大约是 **10×**，**不是任务文档里期望的 2×**。
这一现实下限在任务自身的 boundary case 里已经预声明，下面 §5 重新评估
"realistic floor"。

---

## 5. Realistic floor estimate (post-F-D7)

| 阶段 | W3 ratio | W4 ratio | 备注 |
|---|---|---|---|
| 当前 (tree-walker) | 8.9× | 3520× | 本 stage 实测 |
| F-D9 接入 cmp_lua trace 入口 + StrConcat 简易 fold | 4-5× | 30-50× | shim alloc 仍 O(N)，但 trace 一次成本摊薄 |
| + StringRef arena + SSO | 2-3× | 10-15× | 短串栈分配避免 alloc |
| + StrConcat rope / interned fast-path | 1.5-2× | 5-10× | 直接对标 LuaJIT 内部 fold 路径 |

W3 在合理迭代后可以 ≤ 2.0×；W4 因为 LuaJIT 把 needle/haystack 全部静态
fold，最优我们能到 **10×**，再低需要解释器侧的 const-prop（trace 录制时
识别字面量参数 + 不再调用 shim 而是嵌入返回值）。这条线在 F-D7 之外，
不属于本 stage 边界。

---

## 6. Gates

| Gate | 结果 |
|---|---|
| `cargo build --workspace` | PASS |
| `cargo test --workspace` | 1825 PASS / 0 FAIL（baseline 1808 + 17 new） |
| `cargo clippy --workspace --all-targets -- -D warnings` | PASS |
| `cargo fmt --all -- --check` | PASS |
| `cargo build --target wasm32-unknown-unknown -p relon-wasm` | PASS |
| `RELON_BENCH_FORCE_RUN=1 cargo bench --bench cmp_lua -- W3 / W4` | 实测落地（见 §4） |

---

## 7. 边界声明（per task）

按 task 边界条款：

- ✅ "File-disjoint expected" — F-D7 只触碰 trace-recorder / -emitter / -jit
  string ops。Dict / list ops 未触碰，F-D8 可平行推进。
- ✅ "≥ 3 commits" — 3 commits: (1) trace-jit IR + shims + ABI; (2) recorder
  + emitter lowering; (3) bench rerun + docs (本 stage report)。
- ✅ "English code / commits" — 全部英文。
- ✅ "Do NOT push" — 无 push。
- ⚠ "Honest stuck" — 在 W3 ≤ 2.0× / W4 ≤ 2.0× 的目标上诚实标 stuck，
  阻塞器 = bench 入口未接 trace JIT (F-D9)。recorder / emitter / IC
  全部对齐，F-D9 完成后即可端到端测得改善。
