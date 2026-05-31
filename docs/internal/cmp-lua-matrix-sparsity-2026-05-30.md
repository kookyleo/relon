# cmp_lua 结果矩阵稀疏性解读 — 2026-05-30

> 配套 `cmp-lua-panel-2026-05-30.md`(实测数值)。本文回答一个反复出现的疑问:
> **为什么 panel 这张 28×10 的表不是稠密的(很多格子是空的)?哪些 (workload × backend)
> 交叉点没有参与比对,为什么?**
>
> 一句话结论:**这张表本就该是稀疏的,稠密化反而会破坏诚实性。** 空格不是「没测」,
> 而是「这个后端的能力 envelope 容不下这个 workload」或「闸门主动拒绝以避免 paper win」。

来源:对 `crates/relon-bench/benches/cmp_lua.rs`(约 8267 行)逐列回源核对 gating 逻辑得出
(2026-05-30 审计)。审计读的是**当前代码**;数值取自最近一次 s90 实测快照
(md5 `9738c99a` + 数值-kernel 行 `a907c717`),两者有轻微漂移,见文末「快照漂移」。

---

## 1. 矩阵的本质:2 列恒满,其余列能力闸门

- **恒满列(锚)**:`luajit`(外部脚本基准)、`tree_walk`(relon 参考解释器)。每个 workload
  都跑这两条,所以这两列 28/28 全有值——所有比对都以它们为锚。
- **其余每一列**都是 capability-gated:某后端只有在能真正**编译/运行该 workload 的生产源**
  时才 emit `group.bench_function(BenchmarkId::new(label, backend_id))`。不能编译 → 不出行 → 空格。

因此:**空格 = envelope 外(结构性不参与);`n/a` = 闸门主动拒绝(诚实性);`(fx)` = 夹具非
生产路径。** 没有任何一格是「本可以测却漏测了」。

---

## 2. 完整矩阵(28×10,旧快照 md5 `9738c99a` / `a907c717`)

> ⚠️ **本表为 2026-05-30 之前的混合快照,部分数值/参与情况已被 §6 的当前代码单二进制
> 重测(md5 `bf4a3bc7`)取代。** §6 是权威的「当前代码」表;本表保留作审计轨迹。§6
> 相对本表纠正了 6 处(W1/W2 wasm_fast、W17/W18 wasm、W19 cranelift、W16 cranelift 失败
> 形态、W28 rust_native、aot_fast 计数),详见 §6 开头的「纠正清单」。

| Workload | luajit | relon_jit¹ | wasm | wasm_fast | llvm_aot | aot_fast | cranelift | bytecode | tree_walk | rust_native |
|---|---|---|---|---|---|---|---|---|---|---|
| W1_int_sum | 14.52µs | — | 6.27µs | 6.11µs ² | — | — | — | 1.236ms | 16.90ms | — |
| W2_f64_dot | 12.58µs | — | 1.265µs | 1.096µs ² | — | — | — | 244.0µs | 3.449ms | — |
| W3_string_concat | 1.154ms | — | 2.354µs | — | — | — | — | 2.501ms | 5.817ms | — |
| W4_string_contains | 14.55µs | (fx) | 5.076µs | 4.902µs | — | — | — | 5.203ms | 36.50ms | — |
| W4_long_haystack | 14.56µs | (fx) | 5.309µs | 5.133µs | — | — | — | — | 36.21ms | — |
| W5_dict_str_key | 99.37µs | 51.21ms | — | — | — | — | — | — | 52.08ms | n/a ᶜ |
| W6_list_int_sum+1 | 53.08µs | 2.106ms | 14.67µs | 14.50µs | — | — | — | 2.038ms | 30.71ms | n/a ᶠ |
| W7_fib | 909.8µs | 20.30ms | 229.2µs | 228.9µs | 85.85µs | **84.99µs** | — | 20.29ms | 132.1ms | 84.96µs |
| W8_poly_callsite | 105.4µs | 51.20ms | — | — | — | — | — | — | 51.60ms | n/a ᶜ |
| W9_nested_matrix | 44.62µs | 6.449ms | — | — | — | — | — | — | 6.538ms | n/a ᶜ |
| W10_config_eval | 17.58µs | 4.544ms | — | — | — | — | — | — | 4.600ms | n/a ᶜ |
| W12_p99_tail | 89.10ns | 559.3ns | 229.3ns | 62.86ns | 196.2ns | **2.89ns** | 683.7ns | 105.1ns | 1.291µs | 4.82ns |
| W13_deep_dict_access | 3.993µs | 3.989ms | — | — | — | — | — | — | 4.086ms | n/a ᶜ |
| W14_schema_validate | 9.111µs | 567.96µs | 4.159µs | 3.977µs | — | — | 6.990µs | 575.4µs | 3.645ms | n/a ᶠ |
| W15_conditional_field | 4.525µs | 259.3µs | 1.951µs | 1.755µs | — | — | 4.589µs | 258.7µs | 2.156ms | n/a ᶠ |
| W16_quicksort | 1.336ms | 119.4ms | — | — | **77.10µs** | — | **n/a ˣ** | — | 119.0ms | 149.8µs |
| W17_binary_search | 6.241µs | 483.0µs | — | — | 2.451µs | **2.269µs** | — | — | 3.863ms | 2.281µs |
| W18_prime_count | 2.732ms | 536.7ms | — | — | 751.7µs | **750.6µs** | — | — | 533.0ms | 749.95µs |
| W19_matrix_multiply | 46.64µs | 28.69ms | — | — | **9.508µs** | — | — | — | 28.55ms | 9.930µs |
| W20_n_body | 211.9µs | 242.7ms | — | — | — | — | — | — | 241.1ms | 25.14µs |
| W21_match_dispatch | 133.8µs | 43.88ms | — | — | — | — | — | — | 45.04ms | n/a ᵇ |
| W23_dict_spread | 2.860ms | — | — | — | — | — | — | — | 86.13ms | n/a ˢ |
| W24_list_comprehension | 77.40µs | — | — | — | — | — | — | — | 10.76ms | n/a ˢ |
| W25_pipe_chain | 45.01µs | — | — | — | — | — | — | — | 35.75ms | n/a ˢ |
| W26_fstring_interp | 63.38µs | 2.582ms | — | — | — | — | — | — | 2.633ms | n/a ᶠˢ |
| W27_stdlib_dict | 10.12ms | — | — | — | — | — | — | — | 151.3ms | n/a ˢ |
| W28_float_mixed_ops | 72.64µs | — | — | — | — | — | — | — | 20.85ms | 25.14µs* |
| W30_strict_mode | 52.92µs | — | — | — | — | — | — | 2.037ms | 30.50ms | n/a ᶠ |
| **参与数 /28** | **28** | **26**(全 fallthrough) | **9** | **7** | **6** | **3** | **3** | **11** | **28** | **8** |

标记图例:
- `—` envelope 外(后端结构性无法编译此 workload,不出行)
- `n/a` 闸门主动拒绝(下标标原因;代码打印 `eprintln "... row n/a"` 而非 emit bench)
- `(fx)` `relon_trace_jit_fixture` 夹具行,非生产 trace-JIT 路径
- `*` 当前代码已接但该次快照尚未重测到(漂移,见 §5)
- 数值列加粗 = 该 workload 该后端的最佳行

`rust_native` 的 `n/a` 下标(= 5 个 paper-win 闸门 + 坍缩守卫):
- `ᶠ` 闭式折叠 —— 等差/等比/Faulhaber 求和被 rustc/LLVM -O3 折成 O(1) 多项式(audit #332)
- `ᶜ` 代数坍缩 stand-in —— kernel 用标量替身((i%10)+1 等)代替生产的 dict-probe / closure-dispatch(audit #318)
- `ᵇ` brand-dispatch —— Rust enum+match 把运行时 brand 字符串比较折成编译期 variant tag
- `ˢ` 容器语法糖 —— spread / comprehension / pipe / stdlib-dict,落到 host 分配器或被折叠
- `ˣ` cranelift frontend 崩溃(见 §4 cranelift)

---

## 3. 各列 envelope(为什么这么多空格),附代码锚点

### luajit / tree_walk —— 28/28 恒满
外部脚本基准 + relon 参考解释器,无条件对每个 workload 运行,是全表两根锚。

### rust_native —— 8/28(诚实闸门,不是没写基准)
手写 Rust 平价基准,**唯一 emit 点是 canonical_panel 循环**(约 7849–8012 行),按有序
if/else-if 链分派。只对「手写 Rust 是合法下界」的 workload 出行:
**W7 / W12 / W16 / W17 / W18 / W19**(i64,经 `rust_native_dispatch`)+ **W20 / W28**(f64,
专用 `rust_native_w20/w28`)。其余被 5 个 paper-win helper 拦掉:
- `paper_win_closed_form_fold_label`(~411,W1|W2|W6|W13|W14|W15|W30)→ `ᶠ`
- `paper_win_brand_dispatch_label`(~453,W21)→ `ᵇ`
- `paper_win_container_sugar_label`(~489,W23|W24|W25)→ `ˢ`
- `paper_win_fstring_interp_label`(~522,W26)→ `ᶠˢ`
- `paper_win_stdlib_dict_label`(~548,W27)→ `ˢ`
- `paper_win_collapsed_variant_label`(~340,W5|W8|W9|W10)→ 最终 else `ᶜ`

> 关键:W1/W2/W3/W4/W4_long **根本不在** canonical_panel 数组里(该数组从 W5 起),它们走
> ~4754–5600 的独立 bench block,从未长出 rust_native arm。`rust_native_dispatch`(~4630)
> 里保留它们的 arm 只为 grep 可见性 + 防止重新引入时 panic。

### llvm_aot / llvm_aot_fast —— 6/28 ÷ 3/28
`LlvmAotEvaluator::from_source` 的 "Phase E + AOT-3/AOT-4" typed 面:Int-only `#main` 参数、
Int 或单 Int 字段 anon-Dict 返回、标量 Int 算术 / 三元 / bool、range().map()/reduce()→loop、
字符串 concat+contains(Phase E.1)、自/where 递归闭包提升(W7/W16/W17)、运行时 `List<Int>`
物化 + `_list_filter`/`_len`/`list.sum`(AOT-4,W16/W18)、2D `List<List<Int>>` 物化 + N-D 内联
索引(AOT-4,W19)。
- **OUT**:Float 返回(W20/W28)、f-string(W26)、bare-Dict/brand/spread/stdlib(W21/W23–W27)、
  非 Int-面 dict/字符串坍缩(W5/W8/W9/W10/W13)。判定 = `llvm_aot_source_for(label)`(~4338)
  返回 `None`。
- **`aot_fast`** = marshalling-bypass 快速入口,仅 **Int 标量返回**(W7/W12/W17)有;list-return
  形状(W16/W18/W19)无快速入口,`aot` 行已含 buffer marshalling(在这些 per-call 量级下可忽略)。

### wasm / wasm_fast —— 9/28 ÷ 7/28
只有当源文本 classify 命中 `relon-wasm-evaluator/src/classify.rs::classify_main` 的某个 Z.1
`WasmProgram` 形状时才参与。**该分类器是文本匹配的**(逐字节匹配归一化后的源),所以真实
envelope = 标量 Int-返回的 loop/recursion/字符串 kernel,其源文本恰好匹配某个已知 program:
W1 / W2 / W3 / W4 / W4_long / W6 / W7 / W12。其余一律 `ScopeCut("unknown-shape")` → 回落
TreeWalker → 无 wasm 行:
- Float 返回(W20/W28,且在 ~8039 行 canonical 循环里被显式 guard 提前跳过,Z.1 无 Float lowering)
- bare-Dict / 首类闭包 / 2D / list-递归(W5/W8/W9/W10/W13/W16/W17/W18/W19)
- brand / spread / comprehension / pipe / f-string / stdlib(W21/W23–W27)
- **W30 仅因 `(Int i)` 带类型注解** → 文本与 classify.rs:62 的无类型版不匹配 → ScopeCut
`wasm_fast` = wasm 行里 `program_returns_scalar_int()==true` 的子集(W3 因 `(i64,i64)` 非标量被排除)。

### cranelift —— 3/28(最窄)
列 id `relon_aot`,**唯一 emit 点 = canonical_panel 循环 ~7665 行**,由 `try_build_aot(src,label)`
(~7661,`catch_unwind` 包 `AotEvaluator::from_source`)守卫。其 codegen 只接 legacy-i64 入口
`((I64,..)->I64)` 或 buffer-protocol 形状,op_visitor 只 lower 标量 Int/Float 算术 + 比较 +
控制流(`Op::If` / range.reduce loop):**W12 / W14 / W15**。
- 所有 list/dict/string 物化形状 → 编译失败 → 不出行(`—`)。
- **W16 = `n/a` 崩溃(`ˣ`)**:cranelift frontend panic `declared type of variable var3 does not
  match type of value v31`(list-materialize 形状),被 `catch_unwind` 捕获 → n/a。这是 **#355**,
  尚未修;与 2026-05-30 已修的 **#357**(选择性 `_list_filter` 闭包 ABI double-captures bug)是
  **两个不同的 cranelift bug**。LLVM AOT 路径不受影响(W16 = 77.10µs)。

### bytecode —— 11/28
`RelonBackend::Bytecode`(M2-A VM)的标量 envelope:标量 Int `#main` 返回、Int 算术、Phase D 递归
闭包(fib 经 MakeClosure/CallClosure)、range-pipeline peephole(把 `list.sum(range.map(...))` /
`range.map.filter.len` / `range.reduce` 的纯 Int-arith/compare/ternary body 折成累加 loop)——
覆盖 **W1/W2/W3/W4/W6/W7/W12/W14/W15/W30**。拒绝一切需要堆/聚合或非 Int 面的:dict 字面量、
list 字面量、bare-Dict 返回、首类闭包 VALUE。

### relon_jit —— 26/28 但全是 fallthrough¹(唯一「列满却不算数」的列)
列由两条路喂:
1. canonical_panel 循环(~7563–7658)对每个 panel label emit 一个 **`relon_jit_fallthrough`** 行,
   经 `build_jit = JitEvaluator::new`(永不失败,回落 tree-walker/bytecode)。**全部** panel label 的
   trace recorder 都在 `Op::If` / `CallClosure` / `CallNative` / dict+list 物化处 abort,
   `active_tier ≠ Trace` —— 所以这些行尽管名字带 jit,实测的是 **tree-walker/bytecode 回落**。
2. 仅当 `trace_jit_production_label_eligible(label)` 为真(硬编码 `W1|W2|...`)才 emit 真
   `relon_trace_jit` 行;W4/W4_long/W10 显示的 `(fx)` 是 `relon_trace_jit_fixture` 夹具,非生产路径。

> 因此 **relon_jit 列不计入任何 "JIT 超 LuaJIT" 的 beats 统计**。trace-JIT 不在
> JIT>LuaJIT 关键路径上——真正打赢 LuaJIT 的是 wasm/wasm_fast/llvm_aot 编译层。

---

## 4. 两个目标只看「比对真实成立」的格子

- **AOT 比肩 Rust** —— 只看 6 个数值 kernel(`llvm_aot/aot_fast` × `rust_native` 那 6 行):
  W7 1.00× / W12 0.60× / W16 0.515× / W17 0.99× / W18 1.00× / W19 0.96×。**已达标**(全部平价或超 Rust)。
- **JIT 超 LuaJIT** —— 只看真实编译层(wasm/wasm_fast/llvm_aot/llvm_aot_fast,**排除** relon_jit
  与 `_fixture`)对 luajit 的胜场:13/28(含 4 个新数值 kernel)。**已达标,且面广**。

把空格强行填满 = 要么逼后端 fallthrough(像 relon_jit 整列那样看似满、实为解释器,毫无意义),
要么给会折叠/坍缩的 kernel 补 rust_native(paper win)。**稀疏 = 诚实的代价,也是诚实的证据。**

---

## 5. 快照与当前代码的漂移(诚实补充)

committed 快照(md5 `9738c99a`)略落后于当前 bench 代码,审计读当前代码,故两处对不上:
1. **W1/W2 `wasm_fast`**:快照显示 6.11µs / 1.096µs(脚注²),当前代码已 fold-gate 掉
   (audit #346,`paper_win_closed_form_fold_label` → 打印 n/a 不出行)。整盘重测后这两格 → `n/a ᶠ`。
2. **W28 `rust_native`**:当前代码已接(`rust_native_w28`),该次快照未测到 → 表中标 `*`。

`cmp-lua-panel-2026-05-30.md` 自身亦标注:「23 个非数值-kernel 行沿用 05-29 快照,待 s90 整盘
重测后补 28 行单二进制刷新」。结构性 envelope 故事不受此漂移影响;如需一张消除漂移的「当前
代码」单二进制表,触发 s90 整盘重测即可。

---

## 附:参与数速览

| 列 | 参与 /28 | 性质 |
|---|---|---|
| luajit | 28 | 外部脚本锚 |
| tree_walk | 28 | relon 参考解释器锚 |
| bytecode | 11 | M2-A 标量 VM |
| wasm | 9 | 文本-classify 编译层 |
| wasm_fast | 7 | wasm 标量-Int 子集 |
| rust_native | 8 | 手写 Rust 下界(经 5 闸门去 paper-win) |
| llvm_aot | 6 | typed 标量 Int AOT(AOT-3/4 envelope) |
| llvm_aot_fast | 3 | AOT Int 标量返回快速入口 |
| cranelift | 3 | 最窄;W16 崩溃待修(#355) |
| relon_jit | 26 | **全 fallthrough,不计 beats** |

> 注:本速览的部分计数(cranelift、wasm、wasm_fast、rust_native、aot_fast)取自旧快照,
> 已被 §6「当前代码」表纠正。以 §6 为准。

---

## 6. 当前代码单二进制实测(消除漂移)— 2026-05-30

一次性单二进制全 panel 重跑,消除 §2 的混合快照漂移。

- **Host**:s90-bench(192.168.213.90)· `taskset -c 2` · load1≈0.0 · quiescence 放行
  (`governors=0/4 perf`,无 cpufreq/no_turbo 节点 → 容忍;未用 FORCE_RUN)
- **Binary**:md5 `bf4a3bc79e4fcd2ee3fd3d492fb26a54`(当前 HEAD,含 #357 cranelift 修复 +
  #346 fold-gate;两端 md5 校验一致)
- **criterion**:100 samples × 5s measure × 3s warmup,`--noplot`,filter `v6_lambda_cmp_lua/`
- **数值取点估计中位**(criterion `time:` 区间中值)

### 纠正清单(§6 相对 §2 / 审计)

| # | cell | §2/审计 | §6 实测(真相) | 性质 |
|---|---|---|---|---|
| 1 | W1/W2 `wasm_fast` | 有值(6.11/1.096µs) | **`—`** | fold-gate #346 生效 → 不出行(用户问的核心 drift,已消) |
| 2 | W17 `wasm`/`wasm_fast` | 审计判 absent / §2 误写 `—` | **13.18 / 12.97µs(有)** | 审计 ScopeCut 误判,实为 present |
| 3 | W18 `wasm`/`wasm_fast` | `—` | **2.812 / 2.820ms(有)** | wasm envelope 实际覆盖 |
| 4 | W19 `cranelift` | `—` | **41.07µs(有)** | cranelift 现可编译 2D matmul |
| 5 | W16 `cranelift` | `n/a` 崩溃(#355 var3 panic) | **graceful `UnsupportedShape`(LetGet)** | 本会话 cranelift 加固后不再 panic,改为干净拒绝 |
| 6 | W28 `rust_native` | 审计判有 / §2 标 `*` | **`—`(无)** | 专用 block 不 emit、canonical 循环未覆盖 → 当前代码确实没有 |

### 完整矩阵(md5 `bf4a3bc7`,当前代码)

| Workload | luajit | relon_jit¹ | wasm | wasm_fast | llvm_aot | aot_fast | cranelift | bytecode | tree_walk | rust_native |
|---|---|---|---|---|---|---|---|---|---|---|
| W1_int_sum | 14.525µs | — | 6.284µs | — ᶠ | — | — | — | 1.239ms | 17.412ms | — ᶠ |
| W2_f64_dot | 12.944µs | — | 1.274µs | — ᶠ | — | — | — | 243.600µs | 3.475ms | — ᶠ |
| W3_string_concat | 1.163ms | — | 2.364µs | — | — | — | — | 2.535ms | 5.787ms | — |
| W4_string_contains | 14.559µs | (fx) | 5.074µs | 4.897µs | — | — | — | 5.193ms | 35.502ms | — |
| W4_long_haystack | 14.561µs | (fx) | 5.307µs | 5.124µs | — | — | — | — | 35.906ms | — |
| W5_dict_str_key | 99.425µs | 50.595ms | — | — | — | — | — | — | 50.581ms | — ᶜ |
| W6_list_int_sum+1 | 52.121µs | 2.069ms | 14.692µs | 14.504µs | — ᶠ | — | — | 2.061ms | 31.048ms | — ᶠ |
| W7_fib | 913.550µs | 20.233ms | 230.110µs | 229.020µs | 85.927µs | **85.034µs** | — | 20.314ms | 132.590ms | 84.952µs |
| W8_poly_callsite | 105.410µs | 51.106ms | — | — | — ᶜ | — | — | — | 50.926ms | — ᶜ |
| W9_nested_matrix | 44.090µs | 6.507ms | — | — | — ᶜ | — | — | — | 6.447ms | — ᶜ |
| W10_config_eval | 17.144µs | 4.507ms | — | — | — ᶜ | — | — | — | 4.519ms | — ᶜ |
| W12_p99_tail | 86.81ns | 565.78ns | 235.10ns | 63.18ns | 197.13ns | **2.89ns** | 679.33ns | 104.71ns | 1.275µs | 4.82ns |
| W13_deep_dict_access | 3.997µs | 4.153ms | — | — | — ᶠ | — | — | — | 4.073ms | — ᶠ |
| W14_schema_validate | 9.285µs | 568.270µs | 4.166µs | 3.981µs | — ᶠ | — | 6.981µs | 567.880µs | 3.669ms | — ᶠ |
| W15_conditional_field | 4.456µs | 258.390µs | 2.024µs | 1.644µs | — ᶠ | — | 4.594µs | 258.050µs | 2.138ms | — ᶠ |
| W16_quicksort | 1.335ms | 119.120ms | — | — | **76.885µs** | — | 688.10µs ⁷ | — | 119.280ms | 147.680µs |
| W17_binary_search | 6.099µs | 466.460µs | 13.176µs | 12.972µs | 2.449µs | **2.244µs** | — ˣ | — | 3.836ms | 2.285µs |
| W18_prime_count | 2.728ms | 64.236ms | 2.812ms | 2.820ms | 752.390µs | **751.120µs** | — ˣ | — | 536.990ms | 751.100µs |
| W19_matrix_multiply | 44.606µs | 28.615ms | — | — | **9.517µs** | — | 41.066µs | — | 28.407ms | 10.395µs |
| W20_n_body | 212.040µs | 240.080ms | — | — | **53.877µs**⁸ | — | 332.10µs¹⁰ | — | 239.690ms | 25.156µs |
| W21_match_dispatch | 133.830µs | 44.722ms | — | — | — ᵇ | — | — | — | 44.322ms | — ᵇ |
| W23_dict_spread | 2.908ms | — | — | — | — ˢ | — | — | — | 82.441ms | — ˢ |
| W24_list_comprehension | 90.300µs | — | — | — | — ˢ | — | — | — | 10.959ms | — ˢ |
| W25_pipe_chain | 44.991µs | — | — | — | — ˢ | — | — | — | 38.462ms | — ˢ |
| W26_fstring_interp | 64.869µs | 2.626ms | — | — | — ˢ | — | — | — | 2.641ms | — ᶠˢ |
| W27_stdlib_dict | 10.131ms | — | — | — | — ˢ | — | — | — | 145.800ms | — ˢ |
| W28_float_mixed_ops | 72.604µs | — | — | — | **31.135µs**⁸ | — | — | — | 20.736ms | 28.945µs⁹ |
| W30_strict_mode | 52.493µs | — | — | — | — ᶠ | — | — | 2.033ms | 30.971ms | — ᶠ |
| **参与数 /28** | **28** | 17² | **12** | **9** | **8**⁸ | **4** | **6**⁷¹⁰ | **10** | **28** | **8**⁹ |

下标(空格原因,实测确认):`ᶠ`=闭式折叠(#332) · `ᶜ`=代数坍缩 stand-in(#318) · `ᵇ`=brand→编译期 tag ·
`ˢ`=容器语法糖(spread/comprehension/pipe/stdlib) · `ˡ`=Float `#main`(仅 cranelift 仍拒:`If result_ty F64`;
**LLVM AOT 现已支持 Float,#359 Part A/B**) · `ˣ`=cranelift codegen 拒绝(`UnsupportedShape: LetGet`,graceful 非 panic) ·
`⁷`=W16 cranelift n/a→688µs(#358) · `⁸`=W20/W28 LLVM AOT 新行(#359;llvm_aot 6→8) ·
`⁹`=W28 rust_native 首轮 md5 `bf4a3bc7` 标 `—` 系 SIGKILL 在 W27 中断 canonical 循环、未跑到 W28 的伪缺失,本轮干净测出 28.945µs(rust_native 7→8)。
`¹` relon_jit 全 fallthrough(tree-walker 速度,不计 beats);`²` 本次该列尾部 11 个慢 fallthrough 行
在 W27 处被 SIGKILL(`EXIT=137`,W27 stdlib_dict 的 tree-walker 行需 26s/100 样本)——**不影响任一目标**
(fallthrough 不计入 beats,两根锚 luajit/tree_walk 28/28 完整,所有编译层/rust_native cell 齐全)。

### 目标判定(消除漂移后,仍达标)

**AOT 比肩 Rust(best `llvm_aot/aot_fast` ÷ `rust_native`)— 8 kernel(6 Int 平价/超越 + 2 Float 新增,#359):**

| Kernel | best AOT | rust_native | 比值 | 判定 |
|---|---|---|---|---|
| W7_fib | 85.034µs | 84.952µs | **1.001×** | parity |
| W12_p99_tail | 2.89ns | 4.82ns | **0.600×** | beats |
| W16_quicksort | 76.885µs | 147.680µs | **0.521×** | beats(⚠ naive `Vec::new` 基线) |
| W17_binary_search | 2.244µs | 2.285µs | **0.982×** | parity |
| W18_prime_count | 751.120µs | 751.100µs | **1.000×** | parity |
| W19_matrix_multiply | 9.517µs | 10.395µs | **0.916×** | beats |
| **W28_float_mixed**(#359) | 31.135µs | 28.945µs | **1.076×** | 近平价 |
| **W20_n_body**(#359) | 53.877µs | 25.145µs | **2.143×** | 慢于 Rust(见下) |

> W28 近平价(1.076×,差 8%)。W20 2.14×:n-body 每步用**不可变列表**新建 72 字节状态记录(与
> tree-walker 语义一致),而 Rust 用**可变 in-place** 状态 —— 是语义不对称,不是 codegen 缺陷;
> 二者皆 oracle bit-exact(f64::to_bits,n=0..1000)。W20 cranelift 仍 n/a(`If result_ty F64`,留作 followup)。

**JIT 超 LuaJIT(best 真实编译层 ÷ luajit)— 16/16 全胜:**
W1 0.433× · W2 0.098× · W3 0.002×ᵃ · W4 0.336× · W4_long 0.352× · W6 0.278× · W7 0.093× ·
W12 0.033× · W14 0.429× · W15 0.369× · W16 0.058× · W17 0.368× · W18 0.275× · W19 0.213× ·
**W28 0.402×(31.14/77.42µs)· W20 0.244×(53.88/220.5µs)**。
(ᵃ W3 是复杂度级差:LuaJIT O(n²) `..` 拼接 vs relon O(n) arena;记录非 headline。真实编译层 =
wasm/wasm_fast/llvm_aot/aot_fast/cranelift,**排除** relon_jit fallthrough。)

> 结论:消除快照漂移后两个目标干净成立;#359 再添 2 个 Float AOT kernel(W28 近平价、W20 胜 luajit
> 4×)。数据源:`panel_rerun.log`(主表骨架,md5 `bf4a3bc7`)+ §7 的 #358/#359 行(md5 `6bab980d` / `6144b22b`,
> 同 host 同协议,Int kernel 回归实测零变化)。

---

## 7. 后续引擎改动(2026-05-30/31,#358 done / #359 done)

### #358 — cranelift 现在编译 W16 quicksort(`⁷`,真实新行)
两处真实修复(均 oracle bit-exact,全 cranelift 套件 + 全 workspace 绿):
1. `fix(cranelift): handle self-recursive closure captures in MakeClosure`(`2026cc6a`)——
   `emit_make_closure`(`closure.rs`)对自递归闭包(`sum_qs` where-绑定捕获自身,`Op::MakeClosure`
   在匹配 `LetSet` 之前发出)无条件 `get_let` → `LetGet read before LetSet`。镜像 LLVM
   `emitter.rs:4273-4357`:slot 未绑定时校验 `cap.ty==Closure` 并盖入刚分配的 closure handle
   (i32 arena offset,值循环安全)。新增 `let_is_bound` helper。
2. `fix(cranelift): raise AOT scratch arena to 1 MiB (parity with LLVM AOT)`(`e37624da`)——
   cranelift scratch arena 原 64 KiB,LLVM AOT 早已 1 MiB;W16 的 O(n log n) 分区子表在 n>256
   撑爆 64 KiB(graceful `WasmIndexOutOfBounds`,非误算)。提到对等 1 MiB,W16 在 bench N=1000
   跑通。

**s90 实测**(md5 `6bab980d`,`taskset -c 2`):`W16_quicksort/relon_aot` = **688.10µs**(原 n/a)。
回归确认无变化:W16 llvm_aot 77.19µs、W19 cranelift 41.04µs、W19 llvm_aot 9.51µs。
诚实定位:cranelift W16 688µs **胜 luajit 1.94×**,但慢于 LLVM AOT(77µs)/ Rust(148µs)——
cranelift 是快速-低优化后端,关键是从 n/a 变成真实可跑的 oracle 验证行,不是比肩 Rust。

> `⁷` W16 cranelift 从 `n/a ˣ` 变为 688.10µs(#358);cranelift 参与数 4 → 5。

### #359 — Float #main AOT(W20/W28):DONE(两阶段,均 oracle bit-exact)
> 历史:首轮 agent 诚实判定 blocked —— 实验推翻了 orchestrator 的 envelope 假设(`#main(Int n)->Float`
> 走 buffer 协议、纯 Float 体已能编译,改 envelope 是 dead code),拒绝提交 no-op 充数。真实卡点是
> relon-ir 缺 Int→Float 转换 op + list-valued reduce。据此重新立项,分两阶段真实落地。

**Part A — `feat(ir): add Int->Float promotion op (ConvertI64ToF64) across backends`(`803adf0f`)**
新增共享 IR op `Op::ConvertI64ToF64`(sitofp);`OpVisitor` trait 无默认实现,强制每个后端实现(编译期
exhaustive-match 作安全网)。lowering 算术对 {I64,F64} 混合 Add/Sub/Mul/Div 在 I64 操作数上插入转换、
结果 F64,**精确镜像 tree-walker `as_f64()`**(同操作数提升、同结果类型、保留 f64 div-by-zero trap;
mixed Mod 保守仍拒,W28 的 `i%7` 是 Int%Int 不触发)。后端实现:cranelift `fcvt_from_sint`、wasm
`f64.convert_i64_s`、bytecode `as f64`、LLVM `build_signed_int_to_float`+bitcast、trace-JIT clean-abort。
翻转 `f64_mixed_int_float_rejected`→断言提升==tree-walker。W28 gate None→Some。

**Part B — `feat(ir): compile W20 n-body kernel through LLVM AOT (List<Float> reduce)`(`0ecdd345`)**
**无新增 Op**,全部由既有 op 组合(`AllocScratchDyn`/`StoreF64AtAbsolute`/`LoadF64AtAbsolute`/`ConvertI64ToF64`)。
五块:F64 闭包 ABI(f64 跨调用走 i64-bits,放开 `Op::Return` F64 拒绝)+ ctx 感知闭包返回类型推断
(pair_force/accel→F64、step→ListFloat)+ 闭包参数推断 + 计算/where-绑定 List<Float> 字面量物化
(`emit_list_float_literal_materialize`,布局同 List<Int>)+ 1D List<Float> 索引→f64
(`lower_list_index_typed`)。list-valued reduce accumulator 由既有 range-chain reduce 按 init 的
ListFloat handle 类型自动 carry。

**s90 实测**(md5 `6144b22b`,`taskset -c 2`,oracle f64::to_bits 全对):
- **W28 llvm_aot = 31.135µs** | rust 28.945µs → **1.076× 近平价** | luajit 77.42µs → 胜 2.49×
- **W20 llvm_aot = 53.877µs** | rust 25.145µs → **2.143×** | luajit 220.5µs → 胜 4.09×
- Int kernel 回归零变化:W7 85.97/84.98µs、W12 197.7/2.89ns、W16 77.18µs、W17 2.446/2.241µs、W19 9.50µs。
两 lane 均对抗审查 accept(reviewer 独立重跑测试 + 通读 diff:真实 lowering、无作弊、无回归、main 干净)。

诚实定位:W28 近平价(1.076×);W20 llvm_aot 2.14× 源于每步不可变列表分配(镜像 tree-walker 语义,Rust 用可变
in-place 状态)—— 语义不对称非 codegen 缺陷;两者皆 bit-exact、皆胜 luajit。

### #361 — W20 cranelift(`¹⁰`,真实新行,能力达成非性能胜出)
`feat(cranelift): compile W20 n-body kernel via F64 If-join widening`(`0cd267c3`)。唯一 cranelift-codegen
缺口是 `emit_if`(`control_flow.rs ~446`)的 result_ty 内联 match 拒非标量(`If result_ty F64 unsupported`)——
W20 的 pair_force/accel 三元返回原生 F64、list-reduce body join 一个 ListFloat i32 handle。修复:把内联 match
换成共享的 `ir_ty_to_cl` helper(`emit_block` 早已用的单一真源,F64→原生 `types::F64`,String/List*/Closure→I32)。
**其余预想缺口全部已被既有 cranelift codegen + Part A/B IR 覆盖**(F64 闭包 ABI #357/#358、computed List<Float>
物化、f64 索引、ListFloat reduce-accumulator carry)—— probe-fix-repeat 中再未冒出新缺口。无新增 IR op,
未改 cmp_lua.rs(`try_build_aot` 自动 emit W20 relon_aot 行)。reviewer 做了 **load-bearing 对抗检查**:还原修复
→ 测试以确切错误失败 → 恢复,证明修复承重。

**s90 实测**(md5 `0ea8d43c`):`W20/relon_aot` = **332.10µs**(原 n/a),oracle bit-exact(n=0..1000)。
回归零变化:W16 cranelift 686.14µs、W19 cranelift 41.32µs。
诚实定位:W20 cranelift 332µs **慢于 LLVM AOT(54µs)且慢于 luajit(220µs)**——cranelift 低优化层跑闭包密集 +
每步列表物化即如此。这是**能力达成**(cranelift 现能编译 + 正确运行 W20),**不是性能胜出**;不计入 JIT>LuaJIT
(W20 的最佳编译层仍是 llvm_aot 54µs,已计)。cranelift 参与数 5 → 6。

`¹⁰` W20 cranelift n/a→332.10µs(#361)。followup(未做):computed ListInt 字面量、
n>>14k 的 per-iter scratch reset。

### #362 — mixed-Mod Int→Float 提升(tree-walker parity,无 panel 行变化)
`feat(ir): extend Int->Float promotion to Mod for tree-walker parity`(`ec13f9b5`)。把 #359 Part A 的
Int→Float 提升扩到 `%`:lowering 把混合 {I64,F64} Mod 的 I64 操作数经 `ConvertI64ToF64` 提升、并移除
`lhs_ty==F64 && Mod` 拒绝,使混合 Int%Float 与 F64%F64 都 lower 成 `Op::Mod(F64)`。后端:LLVM AOT
`build_float_rem`(frem=fmod,截断取余、符号随被除数,**bit-identical Rust f64 `%`**);bytecode `BcOp::ModF64`
(早已接);cranelift/wasm **无原生 frem → 优雅拒绝**(clean error,非 panic、非误算);trace-JIT clean abort。
oracle bit-exact 测试 4 kernel(mixed / F64%F64 / 负被除数 / 运行时除数)× 9 个 n。

> **诚实更正**:原 spec 误以为 f64 mod-by-zero 返回 NaN(不 trap)。实测 tree-walker(`arithmetic.rs:603-605`
> 在 match 前对 Div+Mod 都查 zero-divisor)**会 trap `DivisionByZero`**。故 F64 Mod 与 F64 Div 一样 **trap**
> (OEQ-against-0.0 guard),这才是真 parity —— 返回 NaN 才是 bug。reviewer 跑 probe(`5.0 % 0.0`→Err)独立确认。
> 无 panel workload 用 mixed Mod(W28 的 `i%7` 是 Int%Int),故**无 panel 行变化、无需 s90**;纯完整性/parity。

### bytecode F64 zero-trap parity(#362 followup,DONE)
`fix(bytecode): trap DivisionByZero on f64 divide/mod by zero (tree-walker parity)`。bytecode VM 的
`BcOp::DivF64`/`ModF64`(`vm.rs`)此前算原始 IEEE(NaN/inf),不像 tree-walker(`arithmetic.rs:603-605`
对 Div+Mod 都 `right.as_f64()==0.0` trap 前置)和 LLVM AOT(OEQ guard)那样 trap —— #359 起既存、对称偏差。
给两个 arm 各加 `if rhs == 0.0 { return Err(BcVmError::DivisionByZero) }`(`rhs == 0.0` 同时捕获 ±0.0、不捕获
NaN,与 tree-walker 的 `== 0.0` 一致),镜像既有 `DivI64`/`ModI64` 形态。新增 oracle parity 测试
`tests/f64_div_mod_zero_trap.rs`:bytecode vs TreeWalkEvaluator,非零除 bit-exact(f64::to_bits)、±0.0 两引擎
都 `DivisionByZero`。无 panel 行变化(bytecode f64 行不在 panel);纯完整性。fmt/clippy/全 workspace 绿。
