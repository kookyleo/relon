# Relon 性能对位(s90,main 8c73c6f9,2026-06-04)

> 架构正确的两轴对位:**llvm-AOT ↔ Rust**(静态原生天花板)· **wasm ↔ LuaJIT**(可移植/动态运行时)。
> 测量机 [`reference_s90_bench_host`]:Xeon E5-2620 v4,taskset 绑核,内建 quiescence gate,长 measurement + 多 reps 中位数,**编译期 vs 稳态分离**。
> 诚实纪律:`crates/relon-bench/benches/HONESTY_POLICY.md` —— 同算法/同 shape/同路径;fold-gated / paper-win 行如实标注、不算赢;信号面外的 workload 记 n/a 不替换。

## 0. TL;DR

- **llvm-AOT 在它能编的标量/递归/数值切片上贴着 native Rust**(≤1.2× 可信门成立);**容器密集递归(W16 quicksort)relon 反而快 1.79×**(arena-bump < Vec malloc);**唯一系统性输 Rust 的是 W20 softened n-body(2.14×)**,根因是 opaque arena 指针堵死 SROA/向量化(§5),**~53% 是可工程化掉的实现税、真 Value 语义固有税 ≈ 0**。
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
| **W16 quicksort** | llvm_aot 77.4 µs | **0.56×(relon 快 1.79×)** | **arena-bump < Rust Vec malloc** |
| **W20 n-body softened** | llvm_aot 53.9 µs | **2.14× 慢** | 唯一系统性容器短板(见下) |
| W12 call-edge(x+1) | fast 7.25 ns | 0.67×(Rust 4.87ns 地板) | run_main 调用边 cost |

> **修正(2026-06-04 深挖,见 §5)**:早期此表把 **W16 方向标反了** —— W16 实测 relon **1.79× 更快**(77.4µs vs Rust 138.9µs):sum-via-partition 每帧分配分区列表,Rust `Vec::new+push` 走真 malloc/realloc,relon 走 **arena bump**(改一个 i32 cursor),同算法下更便宜。**真正系统性输 Rust 的只剩 W20 一行**。

**读数**:标量/递归热循环 relon 的 LLVM-AOT 标量降级**贴着 rustc/LLVM**(同循环同 ISA)。**唯一**系统性劣势是 **W20**(softened n-body 内循环每步把 8 元素 state list 物化进 arena + 每访问从 arena 重载)——根因与可恢复性见 §5。

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
- **w7 递归 fib**:**已修(c917d132)** —— 曾因 object-emit fast-path 分支不发 lambda 体而编不出,非 IR 拒;现四后端 fib bit-equal。
- 唯一系统短板:**W20 softened n-body 2.14×**(W16 已澄清为 relon 1.79× 赢)。根因/可恢复性见 §5。
- 操作注:`llvm-aot` feature 默认 OFF;s90 无系统 wasm-ld 但 rustup 自带 `gcc-ld/wasm-ld`(LLVM-18 wasm object 前向兼容)。

---

## 5. W20 容器短板深挖(2026-06-04)+ 优化方案

**实测纠正**:W16 quicksort relon **1.79× 快**(arena-bump < Rust Vec malloc);**唯一系统性输 Rust 的是 W20**(softened n-body,2.14× 慢)。入口 arg/return 编组 ≈ 0(n=0 仅占 1.1%)—— gap 全在**内循环容器操作**。

**根因:opaque arena 指针堵死整条优化链**。`codegen/mod.rs` 的 `arena_base_ptr = inttoptr(load i64 from state)` → 所有容器 load/store 是 `getelementptr arena_base_ptr, <runtime i32 off>`;因基址是 state 里读出的整数再 inttoptr,LLVM alias 分析认为可能互相别名 → 无法 SROA 把 loop-carried state list 提进寄存器、无法 store-to-load forward、无法向量化内层 4×4 force loop、无法证 `r2*r2>0` 删 div-by-zero trap(`soft=0.1` 从 arena 读、LLVM 看不见)。

**28.7µs gap 分解**:① 内存 round-trip(state 走 arena,每访问真 load/store)+8.6µs(30%,可恢复)② div-by-zero trap(每 fdiv 一个零检查分支;tree-walk 真报 `DivisionByZero` 故 AOT 须匹配)+13.6µs(47%,半固有但可经非零证明/延迟检查恢复热路径分支)③ 残差(scratch-cursor 重载 + payload 重算 + zext/GEP)+6.5µs(23%,可恢复)。

**优化方案(ROI 序,均 codegen/IR,不替算法)**:
1. **消 div-by-zero trap**(~13.6µs):`arith.rs` —— 除数可证非零(`x*x+c`)时跳检查;需 where-bound 标量常量当 SSA 常量下发(非 arena load)让 LLVM value-range 折掉。
2. **fixed-arity-list-reduce 累加器寄存器化**(~8.6µs):`peephole.rs` reduce 路径 —— 固定小长度 List(W20 是 8)且不逃逸时降成 N 个 scalar loop-carried φ(alloca/SSA),解锁 SROA + 向量化。最结构性。
3. **where-bound 标量常量当 SSA 常量**(解锁 #1)。
4. **arena `noalias` 标注 + scratch-cursor 驻寄存器**(吃残差 ~6.5µs)。

**预期**:#1+#2 把 W20 从 2.14× 收到 ~1.2–1.3×(贴 ≤1.2× 可信门)。**诚实**:~53% 是可工程化掉的实现税(arena 表示选择,非 Value 语义要求);div-trap 47% 半固有但热路径大部可恢复;**真 Value 语义固有税 ≈ 0**(无 boxing/动态 tag/重复 marshal,入口编组 1.1%)。

### 落地实测(2026-06-04,commit f9b6fa19)
- **#3 where-bound 标量常量当 SSA 已 LANDED**(承重项):`soft/dt/m*` 折成 `Op::Const*`(含 lambda 体内,非 arena captures-struct load)。**s90 实测:W20 2.14× → 1.69×**(53.9µs→42.4µs,回收 ~11.4µs/~21%),值 bit-identical(`llvm_w20_n_body` oracle + cmp_lua 三方)。
- **#1 div-trap 消除 已建但 revert**:完整 sound 的 `FloatRange` lattice 实现,但对 W20 死代码——源里 `(s[j]-s[i])*(s[j]-s[i])` 是两个独立 SSA load,emit 时证不了相等(LLVM 仅 post-O3 CSE,trap fcmp 已发),需 **load-CSE / Dup 能力**才能让 #1 生效。按"不 ship 死码"revert,div÷0 语义保持 HEAD 行为。
- **#2 fixed-arity reduce 寄存器化 未做**(最结构性,bit-identical 风险高)。
- **剩余到 ≤1.2× 的路**:#1(需先上 load-CSE/Dup)+ #2(reduce 累加器 → scalar φ,解锁 SROA/向量化)。当前 **1.69×**,距 Rust 已收窄,但未贴可信门。

---

## 4. 关联
- 测量机:[`reference_s90_bench_host`](内部记忆) · 诚实纪律:`crates/relon-bench/benches/HONESTY_POLICY.md`
- 后端架构:[`adr-execution-tiers.md`](./adr-execution-tiers.md) · P2/P3 实施:[`phase1-execution-plan.md`](./phase1-execution-plan.md) · W5 epic:[`w5-epic-plan.md`](./w5-epic-plan.md)
