# W-series perf panel — canonical fixation (2026-06-10)

W7 / W12 / W16 / W17 / W18 / W19 六个 W 内核在 s90 上的正式对照表：全行实测、
每行诚实三问、kernel-vs-row 分离测量、测量协议、与 2026-05-30 数据的对照。
本文档收编三份点开头草稿（`.perf-w18-w17-aot-gap-2026-05-30.md`、
`.s90-w17-w18-2026-05-30.md`、`.s90-host-cpu-fix-2026-05-30.md`）的关键结论，
此后以本文档为权威记录。

## TL;DR

- 双口径分列、禁止互比：criterion **row** 口径（全表节）与 `w_kernel_loop`
  + perf-stat **kernel** 口径（kernel-vs-row 节）各自成表；22 配置 × 3
  runs checksum 全 true。
- µs 级 workload 与 native Rust 平价：row 口径 W7 1.001× / W17 0.986× /
  W18 1.000×（fast 行）、W19 1.017×，均 ±2% 内；kernel 口径 W17/W18 复证
  平价，W7/W19 取保守口径计平价（harness 敏感性见 kernel 节）。
- W16 公平基线修正后 relon 仍 **0.707×（快 1.41×）**，kernel 口径 0.705×
  复证；归因 scratch-arena vs malloc 的分配器策略差异，非算法差异。
- W12 fast 入口真回退：2.8915 → 7.7313 ns（2.67×），bisect culprit =
  `7a908521`；本轮只定位不修，影响面仅 ns 级 per-call 固定开销。
- matched target-cpu（`w_kernel_loop_bdw`）对照列已补：仅 W17 的 rust-bdw
  快默认 rust 3.2%（2214 vs 2288 ns），其余 ≤1%。

## 快照指纹

| 项 | 值 |
|---|---|
| Commit | `e6eeb0ad`（bench 修正：W16 公平基线 + W12 口径注释 + w_kernel_loop）— 基线 `d0eb6669` |
| cmp_lua bench binary md5 | `1e4c6dfee93334d9bbec91ddd2c2449b` |
| w_kernel_loop md5 (默认 flags) | `4619a021c97d86f6e89fd1c69b311ae1` |
| w_kernel_loop_bdw md5 (`-C target-cpu=broadwell`) | `d04bf3ddaba92b8d548a4261168b4359` |
| Host | s90-bench (192.168.213.90, Xeon E5-2620 v4, Broadwell-EP) |
| 日期 | 2026-06-10 |

## 测量协议

- 隔离：`taskset -c 2`；开跑前确认 load1 ≈ 0.00（实测开跑时 0.00–0.09）。
- **Row 测量**（criterion）：sample-size 100 × measurement-time 5 s（编译进
  bench binary 的 group 设置）；按 `W7_fib|W12_p99_tail|W16_quicksort|
  W17_binary_search|W18_prime_count_trial_div|W19_matrix_multiply` 过滤跑全行。
- **Kernel 测量**（`w_kernel_loop`，本次新增的 bench 基建，
  `crates/relon-bench/src/bin/w_kernel_loop.rs`）：单 workload × 单路径裸循环
  （warmup 1 次 + N 次计时），每配置 3 runs，另附 `perf stat`
  cycles/instructions。三条路径：
  - `llvm` = `LlvmAotEvaluator::run_main`，每次调用现做 HashMap 参数包 +
    buffer-protocol 搬运（**row 等价口径**，含搬运、不含 criterion harness）；
  - `llvm_fast` = `run_main_legacy_i64_fast`，标量参数提到循环外
    （**kernel 口径**，不含搬运）；
  - `rust` = 手写 rust_native 内核（kernel 口径，本路径不存在搬运）。
- 构建：本地 `cargo build --release -p relon-bench --features llvm-aot`，
  **默认 rustc flags**（与 5/30 协议一致、binary-portable），scp 到 s90 执行。
  另出一份 `RUSTFLAGS="-C target-cpu=broadwell"` 的 `w_kernel_loop_bdw` 作
  matched-target-cpu 诚实对照列（relon 的 MCJIT 在运行时按 host CPU
  （= broadwell）发指令，而默认 rustc 基线是 generic x86-64 —— 不匹配方向
  对 relon 有利，故必须补这一列；见 2026-06-06「AOT vs Rust 实测」教训）。
- 结果一致性：`w_kernel_loop` 每条路径先跑一次与 rust 内核 oracle 比对，
  循环内 checksum 校验每次调用返回值（`checksum_ok=true` 才计数）。

## 全表 — 6 workload × 各 tier（row 口径，criterion，s90 实测 2026-06-10）

每行为 criterion point estimate（100 samples × 5 s）。`(N×)` = 本行时间 ÷
同 workload `rust_native` 时间（<1 表示 relon 更快）。W12 各行口径不同，
见「W12 口径标注」。

| Workload | relon_llvm_aot | relon_llvm_aot_fast | rust_native | relon_aot (cranelift) | luajit | relon_tree_walk |
|---|---|---|---|---|---|---|
| W7_fib (n=22) | 85.872 µs (1.010×) | 85.075 µs (1.001×) | 84.997 µs | — | 898.10 µs (10.6×) | 123.94 ms |
| W12_p99_tail | 192.41 ns † | 7.7313 ns (1.46×) ‡ | 5.3016 ns | 705.94 ns † | 91.225 ns (17.2×) | 1.2890 µs |
| W16_quicksort (n=1000) | 78.130 µs (**0.707×**) | n/a | 110.48 µs | 681.76 µs (6.17×) | 972.19 µs (8.80×) | 105.67 ms |
| W17_binary_search (n=100) | 2.4508 µs (1.073×) | 2.2515 µs (0.986×) | 2.2840 µs | 13.287 µs (5.82×) | 6.0968 µs (2.67×) | 3.6072 ms |
| W18_prime_count (n=10000) | 752.28 µs (1.001×) | 751.86 µs (1.000×) | 751.60 µs | 3.4018 ms (4.53×) | 2.7049 ms (3.60×) | 491.87 ms |
| W19_matrix_multiply (n=16) | 9.5944 µs (1.017×) | n/a | 9.4295 µs | 41.535 µs (4.40×) | 43.299 µs (4.59×) | 26.211 ms |

† 含 per-call 参数包构造 + buffer-protocol 搬运（row 口径），与 rust_native
（kernel 口径）不同口径，不标比值（见「W12 口径标注」）。
‡ **回退**：5/30 实测 2.8915 ns（彼时快于 rust_native 4.8189 ns，0.60×）；
本次 7.7313 ns，慢于 rust（1.46×）。culprit 见「与 2026-05-30 数据对照」。

速读：µs 级 kernel（W7/W17/W18/W19）LLVM-AOT 与 native Rust 在 ±2% 内平价；
W16 在公平基线修正后 relon 仍快 1.41×（来源是 allocator 策略差异而非算法，
见 W16 一节）；六个 workload 全部快于 LuaJIT（2.7×–17×）。

## Kernel vs row 分离（w_kernel_loop，s90 实测 2026-06-10）

`~/wpanel-ab3e/run_kernels.sh` 全程 22 配置 × 3 runs，**66 行
`checksum_ok=true`、0 false**，oracle 比对全过（kernels.log，跑完于
09:34:39 UTC）。中位数取 3 runs 的 per_call 中位；离散 = (max−min)/min；
IPC / instructions 来自每配置附加的单次 `perf stat`。`binary=bdw` 行即
`w_kernel_loop_bdw`（`-C target-cpu=broadwell` matched 对照，仅 rust 路径）。

| Kernel | 路径（口径） | binary | iters | per_call 中位 | 离散 | IPC | instructions |
|---|---|---|---|---|---|---|---|
| W7 fib | llvm（含搬运） | native | 120 000 | 85.936 µs | 0.09% | 3.25 | 70.04 G |
| W7 fib | llvm_fast（kernel） | native | 120 000 | 85.096 µs | 0.02% | 3.27 | 69.72 G |
| W7 fib | rust（kernel） | native | 120 000 | 96.009 µs | 0.20% | 2.81 | 67.56 G |
| W7 fib | rust（kernel） | bdw | 120 000 | 96.029 µs | 0.03% | 2.81 | 67.56 G |
| W12 x+1 | llvm（含搬运） | native | 30 000 000 | 171.87 ns | 0.09% | 2.97 | 32.00 G |
| W12 x+1 | llvm_fast（kernel） | native | 1 000 000 000 | 7.72 ns | 0.0% | 2.80 | 45.10 G |
| W12 x+1 | rust（kernel） | native | 1 000 000 000 | 3.38 ns | 0.0% | 1.71 | 12.04 G |
| W12 x+1 | rust（kernel） | bdw | 1 000 000 000 | 3.38 ns | 0.3% | 1.71 | 12.04 G |
| W16 quicksort | llvm（含搬运） | native | 120 000 | 77.894 µs | 0.52% | 3.50 | 68.71 G |
| W16 quicksort | rust（kernel） | native | 120 000 | 110.499 µs | 0.23% | 3.20 | 88.50 G |
| W16 quicksort | rust（kernel） | bdw | 120 000 | 109.967 µs | 0.16% | 3.17 | 87.69 G |
| W17 binsearch | llvm（含搬运） | native | 3 000 000 | 2421.63 ns | 0.05% | 2.10 | 31.92 G |
| W17 binsearch | llvm_fast（kernel） | native | 3 000 000 | 2250.63 ns | 0.19% | 2.04 | 28.86 G |
| W17 binsearch | rust（kernel） | native | 3 000 000 | 2287.56 ns | 0.02% | 2.18 | 31.12 G |
| W17 binsearch | rust（kernel） | bdw | 3 000 000 | 2214.49 ns | 0.02% | 2.21 | 30.69 G |
| W18 primes | llvm（含搬运） | native | 15 000 | 753.356 µs | 0.14% | 1.22 | 28.81 G |
| W18 primes | llvm_fast（kernel） | native | 15 000 | 751.345 µs | 0.30% | 1.22 | 28.80 G |
| W18 primes | rust（kernel） | native | 15 000 | 751.089 µs | 0.25% | 1.22 | 28.73 G |
| W18 primes | rust（kernel） | bdw | 15 000 | 751.955 µs | 0.17% | 1.22 | 28.73 G |
| W19 matmul | llvm（含搬运） | native | 800 000 | 9.544 µs | 0.39% | 3.34 | 53.98 G |
| W19 matmul | rust（kernel） | native | 800 000 | 11.263 µs | 0.13% | 3.48 | 65.39 G |
| W19 matmul | rust（kernel） | bdw | 800 000 | 11.376 µs | 0.13% | 3.47 | 65.80 G |

读法与披露：

- **同口径比值（kernel，无搬运）**：W18 fast/rust = 1.000×；W17 fast/rust =
  0.984×（vs matched bdw = 1.016×，即 matched 基线下 rust 略快 1.6%）；
  W12 fast/rust = 2.28×（fast 入口回退态，见对照节；criterion row 同方向
  1.46×）；W7 fast/rust = 0.886×。
- **llvm（含搬运）行**：per-call 搬运实测 ≈ 0.16 µs（W12 kernel：171.87 −
  7.72 ns），对 µs 级 kernel 占比 ≤2% 且方向对 relon 不利（保守）。
  W16 llvm/rust = 0.705×，与 row 口径 0.707× 一致 —— 1.41× 优势在
  kernel 口径复证；W19 llvm/rust = 0.847×（但见下条）。
- **harness 敏感性（如实披露，不择优）**：同一手写 rust 内核在
  w_kernel_loop 与 criterion row 下数字不同 —— W7 96.0 vs 85.0 µs、
  W19 11.26 vs 9.43 µs（kernel-loop 更慢）、W12 3.38 vs 5.30 ns
  （kernel-loop 更快），方向不一，ns–µs 级内核对代码布局/黑盒方式敏感。
  因此两口径只各自内部对比；W7/W19 的"relon 快于 rust"仅 kernel-loop
  口径成立，row 口径为平价 —— **结论取保守口径：W7/W19 平价**。
- bdw 对照列：除 W17（rust-bdw 2214 ns，较默认 rust 快 3.2%）外，各
  workload bdw 与默认 rust 差 ≤1% —— target-cpu 不匹配对本组 kernel
  影响有限，但 W17 证明该列必须保留。
- W18 三路径 IPC 均 1.22：瓶颈在 64 位整除链（见草稿收编 #1），三方
  生成码等效，平价是结构性的。

## 诚实三问（每行：同算法？同执行路径？同数据形状？）

按 HONESTY_POLICY 逐行回答（✔ = 是；标注 = 有差异且已披露）：

| 行 | 同算法 | 同执行路径 | 同数据形状 | 备注 |
|---|---|---|---|---|
| W7 relon_llvm_aot / fast / rust | ✔ | ✔（均为编译后机器码热循环） | ✔（dict-bodied fib，rust 用同构 struct） | fast 与默认行差异仅入口搬运 |
| W12 relon_llvm_aot / cranelift | ✔ | **路径含 per-call 搬运**（row 口径） | ✔ | 与 rust 行不同口径，不比 |
| W12 relon_llvm_aot_fast / rust | ✔ | ✔（kernel 口径，无搬运） | ✔（单标量 in/out） | 当前 fast 有回退（见对照节） |
| W16 relon / rust（新基线） | ✔（三遍 filter partition 快排） | ✔ | ✔（每遍物化 worst-case 容量 list） | 残余差异 = relon scratch-arena vs Rust malloc（runtime 属性，披露保留） |
| W16 luajit | **算法有利于 Lua**（单遍 partition） | ✔ | table 单遍构造 | 方向对 relon-beats-LuaJIT 结论保守，故保留并披露 |
| W17 全行 | ✔（迭代二分） | ✔ | ✔（同一 100 元素有序数组） | |
| W18 全行 | ✔（trial-division，filter+len） | ✔ | ✔（range 物化 + filter 物化） | |
| W19 全行 | ✔（三重循环 matmul） | ✔ | ✔（行主序物化矩阵） | |
| tree_walk 行 | ✔ | 解释器路径（仅作量级参照） | ✔ | 不参与 vs-rust 论断 |

红线自查：本轮无 algorithm substitution、无 harness 摊薄、无 inline-stdlib
类 paper win；W16 旧基线（不同算法形状）已作废重测；W12 fast 回退如实
记录且不以旧数充新数。

## W16 公平 Rust 基线（本次修正）

**旧基线作废**：5/30 的 `rust_native_w16`（149.82 µs，由此得出 relon 0.515×
即"快 1.94×"）是**单遍** partition + `Vec::new()` 增长式 push（多次 realloc），
与 relon 实际执行的算法形状不同 —— 该 0.515× 比值作废，不得再引用。

**relon 实际算法**（读 `crates/relon-ir/src/stdlib/defs.rs` `list_int_filter_body`
与 W16 relon 源确认）：快排按 `lt / eq / gt` 跑**三遍独立 `_list_filter`**，
每遍物化一个新 list —— worst-case 容量一次性分配（`8 + 8n + 8` bytes，
scratch bump arena `AllocScratchDyn`）、单遍写入、无 realloc。

**新基线**（`cmp_lua.rs` `rust_native_w16`，本次重写）：同样三遍
`filter_to_vec`（`Vec::with_capacity(xs.len())` worst-case 一次分配、单遍
push、无 realloc），`eq` 列表同样物化后求和。算法、遍数、物化形状逐项对齐。

**新结果（s90 实测）**：relon_llvm_aot 78.130 µs vs rust_native 110.48 µs →
relon **0.707×（快 1.41×）**。残余差异来源：relon 的 filter 输出从 scratch
bump arena 划拨（指针碰撞，几乎零成本），Rust 基线每遍 `Vec::with_capacity`
走系统 malloc/free。这是 runtime/allocator 策略差异，不是算法差异 ——
按诚实纪律披露并保留（替 Rust 换 arena allocator 就不再是"惯用 Rust"基线）。

结论修正：W16 的 relon 优势从夸大的 1.94× 收敛为诚实的 **1.41×**，方向不变。

## W12 口径标注

W12 内核是 `#main(Int x) -> Int` 的 `x + 1` —— 计算本体约 1 ns 量级，因此
**入口搬运是否计入**主导一切数字。两条 LLVM 行口径不同，分行各自标注，
不平均、不互替（cmp_lua.rs 源码处已加 "Measurement basis" 注释钉死）：

| 行 | 口径 | s90 实测 | 含义 |
|---|---|---|---|
| relon_llvm_aot | **row 口径（含搬运）** | 192.41 ns | 每次调用：HashMap 参数包构造 + buffer-protocol 编解码 + JIT 调用。代表"宿主每次喂参数调一次"的真实端到端成本 |
| relon_llvm_aot_fast | **kernel 口径（不含搬运）** | 7.7313 ns | legacy-i64 fast 入口，标量直传。代表 JIT 代码本体成本。对比对象是同口径 rust_native（5.3016 ns） |
| relon_aot (cranelift) | row 口径（含搬运） | 705.94 ns | 同默认行口径，cranelift tier |
| rust_native | kernel 口径 | 5.3016 ns | 手写 `wrapping_add(1)`，无任何搬运层 |

默认行 192 ns 与 fast 行 7.7 ns 的差（~185 ns）即 per-call 搬运成本本身，
不是 JIT 代码质量差距。把 192 ns 拿去和 5.3 ns 对比、或用循环摊薄搬运成本，
都是口径混用，禁止。

## 与 2026-05-30 数据对照

5/30 数字取自 `.s90-host-cpu-fix-2026-05-30.md`（MCJIT host-cpu 修复
`2d08ca20` 后的全表）。Δ = (本次 − 5/30) / 5/30。

| 行 | 5/30 | 本次 (06-10) | Δ | 判定 |
|---|---|---|---|---|
| W7 llvm / fast / rust | 85.833 / 84.980 / 85.025 µs | 85.872 / 85.075 / 84.997 µs | ≤ +0.1% | 平价 |
| W7 luajit | 901.09 µs | 898.10 µs | −0.3% | 平价 |
| W12 llvm_aot | 197.67 ns | 192.41 ns | −2.7% | 噪声内 |
| **W12 llvm_aot_fast** | **2.8915 ns** | **7.7313 ns** | **+167%** | **真回退（见下）** |
| W12 rust_native | 4.8189 ns | 5.3016 ns | +10.0% | 边界；rust 源未变，bench binary 因 W16 重写而代码布局变化，ns 级内核对对齐敏感 |
| W12 luajit | 88.464 ns | 91.225 ns | +3.1% | 噪声内 |
| W16 llvm_aot | 77.096 µs | 78.130 µs | +1.3% | 平价 |
| W16 rust_native | 149.82 µs | 110.48 µs | 不可比 | 基线算法已更换（旧基线作废，见 W16 节） |
| W16 luajit | 1.3293 ms | 972.19 µs | −26.9% | Lua 源未变；LuaJIT trace 编译决策方差（变快方向，非回退），记录待复测 |
| W17 llvm / fast / rust | 2.4512 / 2.2687 / 2.2815 µs | 2.4508 / 2.2515 / 2.2840 µs | ≤ 0.8% | 平价 |
| W17 luajit | 6.1816 µs | 6.0968 µs | −1.4% | 平价 |
| W18 llvm / fast / rust | 751.67 / 750.60 / 749.95 µs | 752.28 / 751.86 / 751.60 µs | ≤ +0.3% | 平价 |
| W18 luajit | 2.7094 ms | 2.7049 ms | −0.2% | 平价 |
| W19 llvm / rust | 9.5081 / 9.9299 µs | 9.5944 / 9.4295 µs | +0.9% / −5.0% | 噪声内 |
| W19 luajit | 45.163 µs | 43.299 µs | −4.1% | 噪声内 |

### W12 llvm_aot_fast 回退定位（唯一 >10% 回退）

- 现象：fast 入口 2.8915 ns → 7.7313 ns（2.67×）。criterion row 与独立
  `w_kernel_loop` bin 双确认，本地复现（`2d08ca20` = 3.55 ns vs `d0eb6669`
  = 9.46 ns），非噪声。
- `git bisect run`（自动二分 `2d08ca20..d0eb6669`，阈值 6.0 ns）收敛：
  **culprit = `7a908521`**（2026-05-31，"refactor(bytecode,llvm): drop
  redundant clone in with_fn_id and clarify fast-path arity invariant"，
  改动 `crates/relon-codegen-llvm/src/evaluator.rs` +15/−2 —— 正是 fast
  入口路径上加的 per-call 工作）。
- 影响面：仅 legacy-i64 fast 入口的 per-call 固定开销（ns 量级）；µs 级
  workload 的 fast 行（W7/W17/W18）不受可见影响。默认 buffer-protocol
  入口不受影响。
- 按本轮纪律**只报告不修**；修复方向显然是把该 commit 引入的 per-call
  检查移出热路径（一次性校验或编译期保证）。

## 不收录的行（无法构造公平基线）

「加不进 honest row 就不加」—— 以下行存在于 bench 套件但**不进本 panel
结论**：

- `W25_pipe_chain/rust_native`、`W30_strict_mode_baseline/relon_llvm_aot`、
  `W30/rust_native`：LLVM/rustc 把等差数列求和折叠成闭式多项式（算法被
  编译器替换），bench 内已标 n/a（见 cmp_lua 运行输出与 audit #332）。
- `W11` Relon 冷启动行：依赖 relon-cli 安装路径，s90 上未部署，跳过。
- W12 `relon_llvm_aot` / `relon_aot`（cranelift）行不与 rust_native 行比值：
  口径不同（含搬运 vs 不含），见「W12 口径标注」。
- `relon_tree_walk` 行仅作量级参照（解释器 tier），不参与 vs-rust 论断。

## 草稿收编：2026-05-30 调查的关键结论

来自三份点开头草稿（原件保留，不再维护）：

1. **W17/W18 AOT gap 的 root cause（`.perf-w18-w17-aot-gap-2026-05-30.md`）**：
   修复前 LLVM MCJIT 以 generic x86-64 发指令，64 位整除被 narrowing 成
   div32 序列等劣化；W18 kernel 6.65 s → 3.03 s（2.20×）。修复 = MCJIT 按
   host CPU 配置（commit `2d08ca20`）。同时记录方法论 caveat：**W18 的
   criterion row 中 JIT 循环只占 ~21% profiled cycles**，row 数字不能当
   kernel 数字用 —— 这是本 panel 强制 kernel-vs-row 分离测量的直接动因。
2. **修复前基线（`.s90-w17-w18-2026-05-30.md`）**：W17 1.42×、W18 2.25×
   慢于 rust —— 作为修复幅度的历史参照。
3. **修复后全表（`.s90-host-cpu-fix-2026-05-30.md`）**：即上节对照表的
   5/30 列；其中 W16 rust 基线（149.82 µs）本轮已查明不公平并作废。
4. **target-cpu 公平性（2026-06-06 教训，AOT vs Rust 实测）**：MCJIT 按
   host CPU（s90 = Broadwell）发指令而默认 rustc 基线是 generic x86-64，
   不匹配方向**对 relon 有利**；本 panel 因此附 `w_kernel_loop_bdw`
   （`-C target-cpu=broadwell`）matched 对照列。
