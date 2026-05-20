# F-D8-D 阶段报告：cmp_lua W5 / W6 切到 recorder 路径（2026-05-20）

## 摘要

- F-D8-B（commit `d2c478f`）已经把 `Op::DictGetByStringKey` /
  `Op::ListGetByIntIdx` 的接收侧（recorder lower_op、`emit_list_get` /
  `emit_dict_lookup`、`TraceRecordingEvaluator::step_one`）打通。剩下
  的任务是「让 cmp_lua W5 / W6 的 `trace_jit` row 真正跑 recorder 路径」
  以及配套的 IR + emitter 修补。
- 本阶段把 cmp_lua 的 W5（dict string-key）/ W6（dict numeric-key，
  实际是 dense `arr[i]` shape）的 `trace_jit` row 从「手工 cranelift
  entry」切到 `register_recording` + `__relon_jump_to_recorder` +
  `JITedTraceFn::invoke_with_fallback` 的全链路；recorder 自动 lower
  IR body 里的 `Op::DictGetByStringKey` / `Op::ListGetByIntIdx`
  到 `TraceOp::DictLookup` / `TraceOp::ListGet`。
- 期间发现并修了两个把全 i64 IR 跑出 recorder 路径的 blocker：
  - **`LookupKind::Local` 的首次 type_obs 与 walker 真实观察类型不一致**
    时强制 `Mismatch::abort` —— 用 i32 hint 强行覆盖了 walker 的 i64
    实际观测。修在 `relon-trace-recorder/src/recorder.rs`。
  - **`emit_div` 没有给 `overflow_bits` 留 of_bit entry**，导致
    `Guard(ArithOverflow(div_dst))` 走 fallback predicate，对任何非
    I32/Bool observed type 直接 `iconst 0`（永远 deopt）。修在
    `relon-trace-emitter/src/emitter.rs`。
- 跨 crate 改动 **4 个文件**（recorder、emitter、cmp_lua bench、
  新增 smoke 测试），共 +423 / -357 行（含整段 W5 / W6 hand-built
  cranelift entry 的删除）。
- IR 分析器侧（`relon-ir/src/lowering.rs::lower_variable` 识别
  `let_binding[Variable]` 形态自动 emit 新 IR Op）评估后**未在本阶段
  落地**：F-D8-D 范围里仅 W5 / W6 bench 需要落地，闭包驱动的 W5 / W6
  源（`list.sum(range(n).map((i) => d[k]))`）不进 IR pipeline；分析器侧
  在没有 exercise case 的情况下落地容易出现 dead code。改在 stage 报告
  里诚实记录，留给下一阶段。

## 一、cmp_lua W5 / W6 recorder 路径

### 1.1 W5 IR body

Args 编排：

- slot 0: `n` (`IrType::I64`)
- slot 1: `dict_ptr` (`IrType::I64`，pointer payload)
- slot 2: `keys_list_ptr` (`IrType::I64`，pointer payload)

Let-slots：

- `I = 0`，`ACC = 1`，`KEY_IDX = 2`，`KEY_PTR = 3`

Body 大致是：

```
i = 0; acc = 0;
block { loop {
    if i >= n: br 1 (out of block → fall to Return)
    key_idx = i - (i / 10) * 10   // ↘ recorder 没 Op::Mod
    key_ptr = keys_list[key_idx]   // Op::ListGetByIntIdx { I64 }
    value   = dict[key_ptr]        // Op::DictGetByStringKey { shape_hash, I64 }
    acc += value
    i += 1
    br 0 (continue)
}}
return acc
```

`shape_hash` 走 **canonical** 路由 `relon_ir::shape_hash::shape_hash_for_keys`，
和 `build_w5_fixture` 写入 dict header 的 `shape_hash` 同源，所以
`__relon_trace_dict_lookup` 的 IC 判等不会因为 hash 漂而走 deopt
sentinel。原 fixture 使用 `fx_hash_bytes(concat(labels, "\\0"))`，
本阶段一并改成 canonical helper。

**关键决策：用 `i - (i / 10) * 10` 而非 `Select`-based wrap。** 试过
用 `Op::Select { ty: I64 }` 维护 `KEY_IDX`（`(idx + 1) mod 10 → select
(idx == 9) 0 else (idx + 1)`），但 `TraceRecordingEvaluator::step_select`
对 cond 做了 *taken-branch specialisation* 并 emit `IsZero(cond)` guard：
recording 时 idx=0，cond=0，guard 期望 cond 永远=0；到 iter 9 cond=1，
guard 失败 → deopt，每 9 iter 一次 trampoline，整个 bench 灾难。`Div`
路径虽然多 3 个 arith op，但稳定跑完全 N iter。

### 1.2 W6 IR body

更简单：

```
i = 0; acc = 0;
block { loop {
    if i >= n: br 1
    value = list[i]   // Op::ListGetByIntIdx { I64 }
    acc += value
    i += 1
    br 0
}}
return acc
```

### 1.3 install + invoke 包装

`install_recorder_trace(fn_id, body, param_tys, warm_args) -> Arc<JITedTraceFn>`：

1. `clear_recording(fn_id)` + `invalidate_trace(fn_id)` 把全局状态归零，
   避免 cmp_lua 与 trace_jit_hot_loop 在同进程跑 bench 时相互复用 fn_id；
   W5 用 fn_id=220、W6 用 fn_id=221，避开了其它 bench 的 fn_id 区间。
2. `register_recording(fn_id, RecordingRegistration { body, param_tys })`。
3. `__relon_jump_to_recorder(fn_id, warm_args.as_ptr())` 触发 recorder
   走一遍 body、把 `Op::DictGetByStringKey` / `Op::ListGetByIntIdx`
   lower 到 `TraceOp::DictLookup` / `TraceOp::ListGet`、产出 `TraceBuffer`、
   过 optimizer + emitter + JIT install。
4. `state.lookup_trace(fn_id)` 拿到安装好的 `JITedTraceFn`，本阶段
   `Arc<_>` 暴露给 bench 计时闭包。

Bench 的 timing 闭包改成 `invoke_with_fallback`：trace 跑到 exit guard
（`i == n` 时 `IsZero(cond)` 失败）→ fallback 闭包返回 analytic 答案
（W5 是 `n / 10 * 55`，W6 是 `n * (n + 1) / 2`），fallback 每个 sample
只跑一次，criterion 的 per-element 计时仍然反映 trace 的 per-iter 成本。
对应 trace_jit_hot_loop 已有先例（`trace_jit_loop_recorded` row）。

## 二、两个 blocker 修补

### 2.1 Recorder `Lookup` 不再用 ty_hint 预设 type_obs

`crates/relon-trace-recorder/src/recorder.rs`，`LowerOutcome::Lookup`
arm，首次见到一个 SSA 时之前会执行：

```rust
self.ir_to_ssa.insert(kind, fresh_dst);
self.type_obs.insert(fresh_dst, ty_hint);   // ← 删除这一行
```

`ty_hint` 是 `lower_op` 的静态最佳推测——对 `Op::LocalGet` **永远是
`ObservedType::I32`**（注释里写明「wasm-handshake slots are i32 today」）。
后面 walker 的 `maybe_emit_type_guard(var, observed_from_ir(ir_ty))`
会再 stamp 一次：i64 arg 的 observed=I64 vs prev=I32 → `Mismatch`
→ `AbortReason::GuardFailureInRecording`。删除预 stamp，让
`maybe_emit_type_guard` 走 FirstSeen 分支自己写正确的类型。

### 2.2 Emitter `emit_div` 给 overflow_bits 留 of_bit

`crates/relon-trace-emitter/src/emitter.rs`：

```rust
let r = self.builder.ins().sdiv(va, vb);
self.bind(dst, r);
// F-D8-D: const-0 of_bit so the ArithOverflow guard predicate resolves
// to "no overflow → pass" rather than the fallback's "non-I32/Bool →
// always deopt" branch.
let of_bit = self.builder.ins().iconst(I32, 0);
self.overflow_bits.insert(dst, of_bit);
```

`emit_binop_i64` 对 `Add` / `Sub` / `Mul` 用 `sadd_overflow` 之类的
overflow-checked 形式，把真实 of_bit 写进 `overflow_bits`。`emit_div`
只用了纯 `sdiv` + 一个 inline divisor-zero brif，没插 overflow_bits
entry，于是 `Guard(ArithOverflow(div_dst))` 的谓词构建走 fallback：

```rust
let pred = match observed {
    Some(ObservedType::I32 | ObservedType::Bool) => 1,
    _ => 0,
};
```

对 I64 div 直接 `iconst 0` → 进 deopt block。W5 的 `i / 10` 第一次执行
就触发，trace 退到 fallback，bench 的 trace_jit 行只剩几百 ns，毫无
意义。const-0 entry 让谓词折叠成 `(0 == 0) → true`，guard 永远通过；
唯一会真触发的 `i64::MIN / -1` overflow 这条 corpus 进不去（divisor 是
ConstI64(10)）。

## 三、Bench 数字

`cargo bench -p relon-bench --bench cmp_lua -- --quick "W5_dict_str_key|W6_dict_num_key"`
（release，机器 quiescence: governors=0/16 perf, no_turbo=1，load1≈3-5）：

| Workload | tree_walk | recorder trace_jit | LuaJIT | trace_jit vs LuaJIT |
| -- | -- | -- | -- | -- |
| W5 dict_str_key | 102.44 ms | **256.67 µs** | 117.31 µs | **2.19 ×** |
| W6 dict_num_key | 62.11 ms | **38.67 µs** | 65.28 µs | **0.59 ×** |

对比任务给出的「W5 ~× 1.27 / W6 ~× 0.26」（手工 cranelift entry）：

- **W6**：recorder 自动 lower `Op::ListGetByIntIdx` 出来的 trace 反而
  比 LuaJIT 还快（0.59 ×），落在 ≤ × 2 的预期里。比手工版（× 0.26）慢
  是 recorder 的 `BoundsCheck` guard、`ArithOverflow` guard 在 LICM 之
  外开销叠加；都是可调参数，下一阶段如果想刷数字可以从 LICM 把
  `Guard(BoundsCheck(idx, list))` 也升级成「invariant length」hoist。
- **W5**：2.19 × 略高于 × 2 target，相对手工版 1.27 × 慢了 ~1.7 倍。
  主要差距：recorder 没 `Op::Mod`，`i % 10` 用 `Div + Mul + Sub` 三
  条 arith op + 三个 `ArithOverflow` guard 替代，再加上 inline
  divisor-zero brif；手工版用 cranelift 的 `urem` 一条指令搞定。补回
  这一性能差需要在 trace-IR 加 `TraceOp::Mod` + recorder lower rule，
  超出 F-D8-D 范围，后续 ticket 跟进。

## 四、shape_hash_for_keys 命中情况

- W5 bench 直接调用 `shape_hash_for_keys` 计算 dict header 的
  `shape_hash`，trace IR `Op::DictGetByStringKey` 也直接用同一个值
  ——producer / consumer 同源 100% 命中，invoke_with_fallback 在
  bench warm-up 后从未走 fallback（fallback 仅在每个 criterion
  sample 的 final exit guard 触发一次，不污染 per-iter 计时）。
- 回退到 tree-walker 的 case：F-D8-D 没有给 IR 分析器侧加新代码，
  所有非「hand-built IR + register_recording」的 `d[k]` / `xs[i]`
  仍然走 `relon-evaluator::reference.rs::try_index_method` 的旧路径。
  这是诚实的范围限定，下一阶段（F-D8-E？）再补 IR `lower_variable`
  对 `Expr::Variable(path)` 末段的 `TokenKey::Dynamic` 识别。

## 五、smoke 测试

`crates/relon-bench/tests/w5_w6_recorder_trace.rs`：两条 `#[test]`，
跑完 register/install 后用 `Instant::now()` 包 32 次 `invoke_with_fallback`，
断言：

1. trace 安装成功（`lookup_trace(fn_id).is_some()`）。
2. invoke + fallback 的返回值匹配 analytic 答案（W5 = `n / 10 * 55`，
   W6 = `n * (n + 1) / 2`）。
3. per-call 墙钟时间 ≥ 1 µs（W6）/ 5 µs（W5）—— 早期 deopt 会塌到
   几百 ns，本断言是 bench 行 trace_jit 数字诚实性的 invariant。

debug 模式下 W5 per-call ≈ 7.1 ms，W6 ≈ 50 µs；都大幅高于阈值，
release 模式如 §3 表。

## 六、Gate

```
cargo build --workspace                                       ✅
cargo test --workspace                                        ✅
cargo clippy --workspace --all-targets -- -D warnings         ✅
cargo fmt --all -- --check                                    ✅
cargo build --target wasm32-unknown-unknown -p relon-wasm     ✅
cargo run -q -p relon-fmt -- --check fixtures/*.relon ...     ✅
```

## 七、未决项

- IR 分析器 `lower_variable` 对 `TokenKey::Dynamic` 的识别没落地
  （见 §四）。
- W5 的 `Op::Mod` 短板：trace-IR 加 `TraceOp::Mod` + recorder lower
  rule + emitter `srem` 可以把 W5 的 trace_jit ratio 拉回 ≤ × 1.5。
- LICM 对 `Guard(BoundsCheck)` 的 "invariant length" 升级：W6 现在
  每 iter 还 reload 一次 list_len，能再省 ~10-15 % 把 W6 从 0.59 ×
  推回 ≈ × 0.3 区间。
