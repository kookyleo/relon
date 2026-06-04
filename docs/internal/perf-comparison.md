# Relon 性能对位(s90,main 8c73c6f9,2026-06-04)

> 架构正确的两轴对位:**llvm-AOT ↔ Rust**(静态原生天花板)· **wasm ↔ LuaJIT**(可移植/动态运行时)。
> 测量机 [`reference_s90_bench_host`]:Xeon E5-2620 v4,taskset 绑核,内建 quiescence gate,长 measurement + 多 reps 中位数,**编译期 vs 稳态分离**。
> 诚实纪律:`crates/relon-bench/benches/HONESTY_POLICY.md` —— 同算法/同 shape/同路径;fold-gated / paper-win 行如实标注、不算赢;信号面外的 workload 记 n/a 不替换。

## 0. TL;DR

- **llvm-AOT 在它能编的标量/递归/数值切片上贴着 native Rust**(≤1.2× 可信门成立);内存/Value-容器编组重的场景落后 Rust ~2×(已知短板,量级随 W5 epic 收窄)。
- **wasm 在 fold-resistant 标量核上胜 LuaJIT 2–5×**,且仅付 **~10–18% 沙箱税**(相对 native llvm-AOT)。
- **架构序成立:native AOT(≈Rust)> wasm/wasmtime > LuaJIT trace JIT**。
- **信号面 caveat(最重要)**:canonical 面板 29 workload **仅 10 个有真 relon codegen 行**;其余生产源(dict 字面量经 W5 已编、但一等闭包/f-string/comprehension/pipe/stdlib 等)在 AOT 信号面外,记 n/a。别把 codegen 行读成整个语言面。

---

## 1. 轴一:llvm-AOT ↔ native Rust(AOT 天花板)

`cargo bench --features llvm-aot -p relon-bench --bench cmp_lua`(**`llvm-aot` feature 默认 OFF**)。

| workload | best relon codegen | vs native Rust | 备注 |
|---|---|---|---|
| W18 prime trial-div | fast 751 µs | **1.00×** | 持平 |
| W17 binary-search | fast 2.25 µs | 1.03× | 持平 |
| W7 fib | fast 84.97 µs | 1.13× | 持平 |
| W19 matrix-multiply | llvm_aot 9.50 µs | 1.19× | 持平 |
| W28 float-mixed | llvm_aot 31.0 µs | 0.94× | 持平(略快) |
| W16 quicksort | llvm_aot 76.9 µs | 1.79× 慢 | Value 容器编组 |
| W20 n-body softened | llvm_aot 53.8 µs | 2.1× 慢 | 同上,最大编组劣势 |
| W12 call-edge(x+1) | fast 7.25 ns | 0.67×(Rust 4.87ns 地板) | run_main 调用边 cost |

**读数**:标量/递归热循环 relon 的 LLVM-AOT 标量降级**贴着 rustc/LLVM**(同循环同 ISA)。唯一系统性劣势是 **Value/容器编组**(W16/W20)——入口/容器把 `&[i64]`/记录拷进 arena 的开销,不是内循环 codegen 弱。

---

## 2. 轴二:wasm ↔ LuaJIT(可移植/动态运行时)

relon wasm = relon-IR → LLVM → **wasm32 object** → `wasm-ld` → **wasmtime**(内部 cranelift 编 wasm→native)。即 AOT-to-wasm 再由 wasmtime 跑,**非 relon 自 JIT**;作为沙箱/可移植部署运行时,对位 LuaJIT 那条线。FastInt `(i64..)->i64` 标量核:

| workload | wasm→wasmtime | LuaJIT | native llvm-AOT | **wasm/LuaJIT** | **wasm/native(沙箱税)** |
|---|---:|---:|---:|---:|---:|
| w8 dispatch(i%4 三元) | 11.19 µs | 57.6 µs | 1.22 µs | **0.19(5.3× 胜)** | 9.2×* |
| w10 predicate(3-moduli) | 26.2 µs | 122.6 µs | 22.1 µs | **0.21(4.8× 胜)** | 1.18× |
| hash_fold(模哈希链) | 66.3 µs | 140.8 µs | 59.5 µs | **0.47(2.1× 胜)** | 1.11× |
| w1 listsum / w9 nested | — | — | <0.1ns/iter | — | **paper-win fold,不算赢** |

\* w8 的 9.2× 是 native 侧被 LLVM 部分 fold 的假象,非真沙箱税。**w10/hash_fold 是诚实沙箱税估计 ≈ 1.1–1.2×**(寄存器型整数核;bounds-check 在不碰数组 load 时几乎不体现)。
冷启动:wasmtime instantiate+compile ~1.0–1.2 ms/模块固定,稳态摊销掉。

**读数**:在真走循环的 fold-resistant 标量核上,**AOT-to-wasm-then-cranelift 比 LuaJIT trace JIT 出更紧的码(胜 2–5×)**,且相对 native llvm-AOT 天花板只付 ~10–18% 沙箱税。

---

## 3. 综合 + caveat

```
native AOT(≈Rust)  >  wasm / wasmtime  >  LuaJIT trace JIT  ≫  tree-walk 解释器
   轴一基准              轴一的 ~85-90%        轴二被 wasm 胜 2-5×     250-1000× 慢(correctness ref)
```

- **fold-gated / paper-win 行如实标注不算赢**(W1/W2/W6/W13… 算术级数被 LLVM 折成闭式;#318 等 gate)。
- **wasm 只测了 FastInt 标量核**;buffer-shape 返回(Float/String/List/Dict,经 parity 证能 emit+run)**本轮未计时**(follow-up)。
- **w7 递归 fib**:IR 层 `MakeClosure` 三后端同拒(信号面外,独立 epic)。
- 最大系统短板:**Value/容器编组**(轴一 W16/W20);量级已随 W5 epic(dict/list 编译)+ 退役旧 wasm 从早期 43.6× 收窄到 ~2×。
- 操作注:`llvm-aot` feature 默认 OFF;s90 无系统 wasm-ld 但 rustup 自带 `gcc-ld/wasm-ld`(LLVM-18 wasm object 前向兼容)。

---

## 4. 关联
- 测量机:[`reference_s90_bench_host`](内部记忆) · 诚实纪律:`crates/relon-bench/benches/HONESTY_POLICY.md`
- 后端架构:[`adr-execution-tiers.md`](./adr-execution-tiers.md) · P2/P3 实施:[`phase1-execution-plan.md`](./phase1-execution-plan.md) · W5 epic:[`w5-epic-plan.md`](./w5-epic-plan.md)
