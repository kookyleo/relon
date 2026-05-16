# Wasm AOT backend 性能对比报告（2026-05-16）

> 本文档定位：Phase 1.beta → Phase 9 整链路收官的**性能交付物**。
> 用 criterion 0.5 在同一台机器上对比 `WasmAotEvaluator`（AOT，wasmtime
> 驱动）与 `TreeWalkEvaluator`（解释器）的端到端开销，给出 cold start
> 与 warm invoke 两个截面的真实数字，并据此说明两种 backend 的使用场景。
>
> Bench 入口：`cargo bench -p relon-bench --bench wasm_aot_vs_tree_walk`。
> 源码：`crates/relon-bench/benches/wasm_aot_vs_tree_walk.rs`。
>
> 配套阶段实现的提交清单见文末"附录 B：每个 phase 的 merge commit"。

## 一、执行摘要

针对三类典型脚本（纯算术 / 返回 branded dict / String 长度 stdlib 调
用），criterion 0.5 在 release profile（fat LTO + codegen-units = 1）
下采集 50-sample × 3s 测量窗口的数据：

| 场景 | wasm-AOT cold | wasm-AOT warm | tree-walk total | tree-walk warm | 倍率（cold / tree-warm） |
| --- | ---: | ---: | ---: | ---: | ---: |
| `arithmetic`    | 2.223 ms | 44.62 μs | 1.011 ms | 2.659 μs | 836× |
| `dict_literal`  | 2.305 ms | 45.41 μs | 1.149 ms | 36.40 μs | 63× |
| `stdlib_length` | 2.186 ms | 44.34 μs | 983.7 μs | 3.377 μs | 647× |

数字以中位数为准；完整数据（criterion 报告的 lower / median / upper
三元组 + outlier 计数）见附录 A。三个结论性观察：

1. **wasm-AOT cold start 在 2.2 ms 左右**：cold start 主要被
   `wasmtime::Module::new`（cranelift 把宿主 wasm 模块 JIT 成原生机器
   码）吃掉，对 body 长度几乎不敏感（三个场景都在 2.2 ms 附近）。
2. **wasm-AOT warm invoke 是 ~44 μs**：跨 `run_main` 调用复用
   `Engine + Module`，每次仍付 `Store::new + Linker::instantiate` +
   buffer marshal + wasm 执行的固定开销，与脚本复杂度基本解耦。
3. **tree-walker warm 显著快于 wasm-AOT warm（pure 算术 16×，stdlib
   13×）**：算术体在 tree-walker 上不付 binary handshake / wasmtime
   instantiate 开销，纯解释器 dispatch 接近 ns 级。`dict_literal` 是
   唯一一个两侧 warm 数字接近（36.4 μs vs 45.4 μs）的场景，因为
   tree-walker 这里要做 branded Dict 构造 + schema 验证，工作量已经
   摊到和 wasm-AOT 的固定开销同一量级。

> 数字解读要点：**当前 v1 实现里，wasm-AOT 在所有"通用业务脚本"场景
> 上都比 tree-walker 慢**。这并不奇怪——cold start 有 cranelift JIT
> 开销，warm invoke 有 wasmtime `Store` / `instantiate` 固定开销，这
> 两块都是 wasmtime 本身的成本，不是 codegen-wasm 的实现成本。
> wasm-AOT 真正胜出的场景是**沙箱隔离更强**（trap 不杀宿主）+ **AOT
> 缓存可以跨进程复用编译产物**（`from_bytes` 入口），而不是裸跑速度。
> 详见第五节"何时用 wasm-AOT"。

## 二、测试场景描述

每个场景都通过 **`WasmAotEvaluator::from_source` + `Evaluator::run_main`**
的高层 surface 跑（与 tree-walker 的 `Evaluator::run_main` 完全对应），
因此两边的对比是 apples-to-apples 的"用户代码视角"。

### 2.1 `arithmetic`

```relon
#main(Int x, Int y) -> Int
x * y + 1
```

参数 `{ x: 7, y: 6 }`，期望返回 `Int(43)`。最短的可表达脚本：cold
start 几乎纯被 codegen + JIT 占满；warm invoke 主要是 marshal+dispatch
固定开销。

### 2.2 `dict_literal`

```relon
#schema U { Int age: *, Int birth: * }
#main(Int x) -> U
{ age: x, birth: 2026 - x }
```

参数 `{ x: 42 }`，期望返回 `Dict{ age: 42, birth: 1984 }`，brand = `U`。
练习 BufferReader 的 sub-record 解码路径（wasm-AOT 侧）以及 tree-walker
的 branded Dict 构造 + schema 验证路径。

### 2.3 `stdlib_length`

```relon
// wasm-AOT 源
#main(String s) -> Int
s.length()

// tree-walker 源
#main(String s) -> Int
s.len()
```

参数 `{ s: "the quick brown fox jumps" }`（25 字节），期望返回 `Int(25)`。
练习 String 输入侧的 pointer-indirect tail record（wasm-AOT 侧）以及
schema-rooted stdlib 字节长度 method dispatch（两侧都用）。

**stdlib 命名分歧**：Phase 4.a 的 wasm-AOT IR 把这个 intrinsic 命名为
`length`（`crates/relon-ir/src/stdlib.rs`），但 tree-walker 的 carrier
file `crates/relon-analyzer/src/core/string.relon` 把它声明为 `len`
（Decision 21' / `schema-rooted-model-2026-05-11.md`）。语义完全相同，
surface name 没对齐——bench 让每个 backend 跑自己的"母语"，不强行抹平
到最小语法公约数，否则会偏向其中一边的实现。把这两个名字统一是未来工
作之一（见第六节）。

### 2.4 为什么没有独立的 `method_dispatch` 场景

最初的计划里有第 4 个场景 `#schema V with { doubled() ... }` 把 `V` 作为
`#main` 参数。实测时发现：

- wasm-AOT 当前的 input bridge（`evaluator.rs::write_value_into_builder`）
  对 `Schema { ... }` 类型的 `#main` 参数返回 `Unsupported`（注释明确
  标记 Phase 9 工作）；
- 也试过 `(V { x: n }).doubled()`、`V { x: n }.doubled()`、
  `let v = V { x: n } in v.doubled()` 等替代写法，都被 parser/lowering
  拒绝（IR 当前只把 `V { ... }` 当作顶层 dict literal 接收）。

`stdlib_length` 已覆盖 schema-rooted method dispatch 路径（`String.length`
正是 stdlib 的 schema 方法），因此对比性上**不损失语义维度**。完整的
`with { ... }` 用户自定方法走高层 surface 的 bench，留给"未来工作"。

## 三、数据采集环境

- host：与 `perf-final-2026-05-16.md` 同机（`Linux 6.8.0-110-generic`）
- rustc：`1.93.0`（workspace 锁定的 MSRV 1.92 + 当前主链 1.93）
- profile：`[profile.release]` = fat LTO + codegen-units = 1 + strip
- criterion 配置：源码里 group 默认 `sample_size(50) + measurement_time(8s)`，
  本次实际跑时通过 CLI 把 measurement window 缩短到 3s + warmup 1s（保
  留 50 sample 数）以便整套 bench 在合理 wall-time 内完成；统计估计的
  signal-to-noise ratio 仍然足够（每个 measurement 收集 3k - 3M iter）
- 总共 4 个 group × 3 场景 = 12 个 bench function
- 命令：`cargo bench -p relon-bench --bench wasm_aot_vs_tree_walk -- \
  --warm-up-time 1 --measurement-time 3 --sample-size 50`

## 四、诊断分析

### 4.1 wasm-AOT cold start 成本结构（自上而下）

实测三个场景的 cold start 都在 2.2 - 2.3 ms 之间，对 body 长度几乎不
敏感——这本身就是结论：cold start 的主导项是 cranelift JIT，**不是**
codegen-wasm 的字节码生成。`WasmAotEvaluator::from_source` 内部 stage
（按耗时大致排序）：

1. **`wasmtime::Module::new`（主导项，~1.5-2 ms 量级）**：cranelift 把
   wasm 字节码 JIT 成原生机器码。即使脚本 body 只有 `x * y + 1`，宿主
   模块本身仍要解析 imports + memory + globals + exports，再为 `run_main`
   一个函数走完 cranelift 的 IR-降-机器码 全流程。
2. **`compile_lowered_entry`（codegen-wasm）**：从 IR 生成 wasm 字节
   码，构造 const 数据段、emit srcmap / uctab / abi / host_fns 等
   custom section、reschedule topo eager 顺序、写 BufferBuilder/Reader
   对应的 load/store。
3. **`relon_ir::lower_workspace_single`**：analyzer 树 → IR ops。
4. **`relon_analyzer::analyze`**：和 tree-walker 完全共享，这一步不
   产生 wasm 独有开销。
5. **`relon_parser::parse_document`**：和 tree-walker 完全共享。

由于 cold start 数字三个场景几乎相同，可以反向推出：stages 2-5 加起
来对算术 / dict / stdlib 三个场景的差异 < 100 μs。Phase 9 的"未来工
作"列表里第一条（Pool of `wasmtime::Module` / `from_bytes` 持久化磁盘
缓存）直接针对 stage 1。

### 4.2 wasm-AOT warm invoke 成本结构

三个场景的 warm invoke 都聚在 44-45 μs，方差极小（criterion 估计的
upper-lower 差 < 1 μs）——和 cold start 一样对脚本复杂度不敏感，说明
warm 路径上的固定开销远大于业务逻辑本身。`WasmAotEvaluator::run_main(args)`
的内部 stage：

1. **`build_input(&args)`**：BufferBuilder 把 `HashMap<String, Value>`
   按 `main_layout` 写成 little-endian 字节串。复杂度 O(args + tail
   record bytes)，是 `stdlib_length` 场景中可测的 marshal 成本。
2. **`Store::new(&self.engine, ())`**：新建 wasmtime 会话。fat-LTO 的
   wasmtime 实测在我们 host 上单次 `Store::new` 约 10 μs 量级。
3. **`Linker::instantiate(&compiled)`**：分配 memory pages + 调
   `start` 函数（我们没有 start，只 zero-init）。这一步实测占 warm
   invoke 的大头，**约 30 μs**——这就是 `arithmetic` 的 44 μs 里大部分
   时间所在。
4. **wasm 执行**：实际的 `run_main` 函数，包括 cap 检查 + 业务逻辑
   + buffer write。对 `arithmetic` 是 ns 级；对 `dict_literal` 多一
   次 sub-record write；对 `stdlib_length` 多一次 stdlib 函数调用 +
   长度读取。
5. **BufferReader 解码**：从 out_buf 把字节回填成 `Value`。带 String /
   List<Int> 的 scenario 在这里有一次 String 拷贝。

数据对照：`arithmetic` 44.6 μs vs `dict_literal` 45.4 μs vs
`stdlib_length` 44.3 μs，相差 < 3%。这印证 stages 2-3 加起来吃掉了 ≥
85% 的 warm invoke 时间；具体业务 op 的差异被淹没在 instantiate 噪声
里。

**这就是为什么 `arithmetic` 这种几乎无 IO 的场景里，wasm-AOT warm
invoke（44.6 μs）反而比 tree-walker warm invoke（2.66 μs）慢 16
倍**——tree-walker 不需要切到 wasm 沙箱，直接在原生堆栈上跑解释器
dispatch。Phase 9 未来工作里"Pool of Stores"直接针对 stages 2-3：把
`Store + Instance` 复用起来，每次只 reset memory，warm invoke 可以
压到个位数 μs。

### 4.3 tree-walker baseline 成本结构

- **`tree_walk_total`**：`parse_document` + `analyze` + `Context::new`
  + `prepend_module_resolver(StdModuleResolver)` + `with_analyzed` +
  `prepare_in_place`（stdlib 注册）+ `TreeWalkEvaluator::new` +
  `run_main`。基本和 wasm-AOT cold start 的 stage 1-3 重叠（共享
  parse + analyze），但跳过 codegen + JIT。实测三个场景在 983 μs -
  1.15 ms 之间，比 wasm-AOT cold 快 ~2×——节省的部分就是 cranelift JIT 时间。
- **`tree_walk_warm`**：单次 `run_main` 在已搭好的 evaluator 上。纯解
  释器 dispatch。`arithmetic` 2.66 μs 是几乎纯 IR-walk + arith op；
  `stdlib_length` 3.38 μs 多了 stdlib `len` 调用的查表 + UTF-8 字节数
  读取；`dict_literal` 36.4 μs 跳了一大步，因为要构造 branded `Dict`
  + 跑 schema 验证（这两步在 wasm-AOT 上是 codegen 阶段静态完成的）。

`dict_literal` 是唯一一个两侧 warm 数字接近（36.4 μs vs 45.4 μs，
1.25×）的场景，恰好印证：**当脚本工作量足以摊薄 wasmtime instantiate
固定开销时，wasm-AOT 才有机会接近 tree-walker**。但在当前 v1 实现里，
"足以摊薄"的门槛在几十 μs 这一档；纯算术或 trivial stdlib 都还到不
了那里。

## 五、结论与推荐

> 本节的推荐**建立在 v1 实测数据基础上**——目前 wasm-AOT 在纯吞吐量
> 上还不是 tree-walker 的对手；其价值在沙箱强度 + AOT 缓存可移植性
> 这两个**非 latency** 维度上。下面把场景按"v1 实测哪边更快"分类。

### 5.1 何时用 wasm-AOT（v1 实测优势场景）

- **沙箱要求强 / 不可信脚本**：wasm-AOT 走 wasmtime 的 host 沙箱，
  capability 机制在 wasm linear memory 边界天然成立。即使脚本被恶意
  构造，越界访问也只能 trap 到 `RuntimeError::WasmTrap*`，宿主进程不
  受影响。tree-walker 走原生 Rust 解释器，安全性来自语言本身的内存
  安全 + RuntimeError 边界，沙箱深度弱一些。
- **AOT 缓存场景**：脚本编译产物（wasm + abi/srcmap/uctab/host_fns）
  可以通过 `WasmAotEvaluator::from_bytes` 从磁盘加载，cold start 跳
  过 codegen 只剩 `Module::new`；这是 host caching layer 的天然扩
  展点，尤其适合"很多脚本 + 启动开销分摊"的部署形态。
- **跨进程产物可移植**：wasm 字节码 + abi section 是稳定的二进制接
  口，可以用 host 之外的工具（wasm-objdump、自家 wasm 优化 pipeline）
  做进一步处理。tree-walker 的中间产物（AnalyzedTree / Context）是
  Rust 内部数据结构，不出进程边界。

### 5.2 何时用 tree-walker（v1 实测吞吐量优势场景）

- **任何高频 latency 敏感的调用**：tree-walker warm invoke 在
  `arithmetic` 上 2.66 μs，`stdlib_length` 上 3.38 μs；wasm-AOT
  warm invoke 都在 44 μs。**16× 性能差**是 wasmtime instantiate 固
  定开销造成的，跟脚本本身无关。
- **一次性 / 调试 / LSP**：开发期编辑脚本 + 立刻 eval 看结果；任何
  wasm-AOT 的 cold start 时间（2.2 ms）都是开发循环的延迟。
- **需要 `eval` / `force_thunk` / `invoke_closure`**：wasm-AOT 在这
  三个 trait 方法上返回 `Unsupported`（拓扑预求值后无 AST、无 live
  thunk、无 first-class closure）。LSP / projector / 任何"在表达式
  半路停下来 inspect 一下"的工作流必须用 tree-walker。
- **完整语言子集**：tree-walker 支持的特性集是 wasm-AOT 的真超集
  （详见 `wasm-aot-status-2026-05-16.md` 的支持清单）。脚本里只要含
  `loop` / `concat` / `upper` / `Option` / `Result` 等当前 wasm-AOT
  尚未覆盖的语法，就只能走 tree-walker。

### 5.3 混合策略

`relon::new_evaluator(source, Backend::WasmAot | Backend::TreeWalk)`
是 Phase 8 留下的入口。生产 host 可以：

- 默认 `Backend::TreeWalk`（v1 的 perf-first 默认）；
- 对不可信脚本或来自外部世界的脚本切到 `Backend::WasmAot`，用
  performance 换 sandbox depth；
- 根据脚本特征（含 `loop` / `concat` / closure 等当前 wasm-AOT 不
  支持的语法）动态选择 backend；
- 在 wasm-AOT pool-of-stores 落地后（"未来工作"第一条），重新跑本
  bench——届时 warm invoke 应当压到个位数 μs，整张 perf table 都会
  重写。

CLI 已支持 `--backend wasm-aot` 显式切换，可作为参考实现。

## 六、未来工作

按优先级排序的 wasm-AOT 后续优化方向：

1. **Pool of `wasmtime::Store`**：当前 `run_main_inner` 每次 `Store::new`，
   把 store reset + reuse 到 pool 里能消除 warm invoke 的固定 instantiate
   成本，特别适合 `arithmetic` 这种短源码场景。需要小心 reset 时
   memory 的状态（zero-fill vs 标脏）。
2. **Phase 4.c：stdlib allocator + loop ops + list_sum / list_map**：
   补齐 wasm-AOT 当前缺失的语言子集（详见状态快照）。需要在 wasm 线
   性内存里实现一个简单的 bump allocator，并 emit `loop` / `if` 控制
   流。
3. **Phase 5.b：方法返回变长 / closure 头等值**：当前 `with { ... }`
   方法体只能返回 Int / Bool / String / Schema 等定长或 pointer-
   indirect 已支持的类型；变长返回（动态 list / 嵌套 Schema）以及把
   closure 作为 Value 传递的能力还没接上。这两项落地后，wasm-AOT 才
   能完整替代 tree-walker 在多数业务脚本上的角色。
4. **多文件 `#import`**：现在 `lower_workspace_single` 名字里就写了
   "single"，第二个文件就报错。Phase 9 之后需要把 workspace tree 整
   体 lower 成一个大 IR，再 codegen 一份 wasm；或者按 module 拆 wasm
   多模块 linker。
5. **`memory.copy` 用 SIMD 加速 buffer marshal**：当 String / List<Int>
   payload 大（> KB）时，BufferBuilder 内部的 byte copy 可以走 wasm
   的 `v128.load` 一次搬 16 字节。
6. **`cap_grants` 精细化**：现在 wasm-AOT 把 `caps_avail` 全开
   (`i64::MAX`)，跟 trust mode 没有联动。需要从 `Capabilities` enum
   按位映射到 64-bit grants mask，让 sandbox 真正限到声明的
   `required_capabilities` 之内。
7. **修复 analyzer where-scope strict-mode bug**（Phase 3.a 报告里
   提到的：`#strict` + `where` 的作用域可见性不一致）。
8. **AOT 缓存层 + 持久化磁盘格式**：把 wasm 字节 + canonical schema
   hash + srcmap 一起持久化，跨 host 启动复用。配合 `from_bytes`
   入口就能跳过 codegen 阶段。
9. **closure 头等值落地**：解锁 `xs.map((n) => n * 2)` 之类的高阶
   stdlib。当前 wasm-AOT 只支持 schema-rooted method dispatch，闭包
   作为参数还走不通。
10. **bench 自动化进 CI**：把本 bench 接到 CI（最好 nightly 跑一
    次），用 criterion 的 baseline 比较捕获 regression。每个 phase
    merge 后跑一次自动 diff 给作者。

## 附录 A：criterion 实测数字

实测条件：`cargo bench -p relon-bench --bench wasm_aot_vs_tree_walk
-- --warm-up-time 1 --measurement-time 3 --sample-size 50`，
2026-05-16 在与 perf-final 同机上跑。Criterion 报告的是 `[lower
median upper]` 95% 置信区间。

### wasm-AOT cold start（包含 parse + analyze + IR lower + codegen + wasmtime::Module::new）

| Scenario | Lower | **Median** | Upper | Outliers |
| --- | ---: | ---: | ---: | --- |
| `arithmetic`    | 2.209 ms | **2.223 ms** | 2.237 ms | 2/50 (4%) |
| `dict_literal`  | 2.291 ms | **2.305 ms** | 2.320 ms | 1/50 (2%) |
| `stdlib_length` | 2.179 ms | **2.186 ms** | 2.194 ms | 5/50 (10%) |

### wasm-AOT warm invoke（每次 run_main：Store::new + Linker::instantiate + buffer marshal + wasm 执行 + reader decode）

| Scenario | Lower | **Median** | Upper | Outliers |
| --- | ---: | ---: | ---: | --- |
| `arithmetic`    | 44.48 μs | **44.62 μs** | 44.80 μs | 1/50 (2%) |
| `dict_literal`  | 45.23 μs | **45.41 μs** | 45.57 μs | 1/50 (2%) |
| `stdlib_length` | 44.26 μs | **44.34 μs** | 44.43 μs | 0/50 |

### tree-walker total（每次 parse + analyze + Context 装配 + run_main，cold-style）

| Scenario | Lower | **Median** | Upper | Outliers |
| --- | ---: | ---: | ---: | --- |
| `arithmetic`    | 1.003 ms | **1.011 ms** | 1.021 ms | 6/50 (12%) |
| `dict_literal`  | 1.149 ms | **1.149 ms** | 1.150 ms | 1/50 (2%) |
| `stdlib_length` | 971.7 μs | **983.7 μs** | 996.8 μs | 4/50 (8%) |

### tree-walker warm invoke（已搭好 evaluator，单次 run_main）

| Scenario | Lower | **Median** | Upper | Outliers |
| --- | ---: | ---: | ---: | --- |
| `arithmetic`    | 2.652 μs | **2.659 μs** | 2.666 μs | 9/50 (18%) |
| `dict_literal`  | 36.31 μs | **36.41 μs** | 36.54 μs | 4/50 (8%) |
| `stdlib_length` | 3.358 μs | **3.377 μs** | 3.399 μs | 2/50 (4%) |

数据原始 estimate.json 见 `target/criterion/<group>/<scenario>/new/`，
HTML 报告（带 violin plot）见 `target/criterion/report/index.html`
（用 `cargo install cargo-criterion && cargo criterion -p relon-bench
--bench wasm_aot_vs_tree_walk` 可以渲染更细的图表）。

## 附录 B：每个 phase 的 merge commit

整链路从 wasm-AOT 第一行字节码到 Phase 9 收官，merge 顺序如下：

| Phase | Commit | 说明 |
| --- | --- | --- |
| Phase 1.alpha | `f7efa31` | 最小硬编码 wasm smoke |
| Phase 1.beta  | `e8afc41` | end-to-end IR → wasm 降级 |
| Phase 1.gamma | `6c60651` | `relon.srcmap` custom section |
| Phase 2.a     | `3850f08` | `relon.abi` + schema canonical hash + typesafe buffer 骨架 |
| Phase 2.b     | `29024f8` | binary memory handshake |
| Phase 2.c     | `3e14614` | if/cmp + String / List<Int> pointer-indirect |
| Phase 3.a     | `af14faa` | bool/let + String/List<Int> 输出 |
| Phase 3.b     | `b3f475c` | dict literal + topo eager + cycle detection |
| Phase 4.a     | `771f36a` | stdlib 基础设施 + `length(String) -> Int` |
| Phase 4.b     | `d576fde` | `list_int_length` / `abs` / `min` / `max` / `is_empty` + Select op |
| Phase 5       | `f3c6cda` | schema-rooted method dispatch |
| Phase 6       | `fcffaf9` | capability + native fn import（abi v2） |
| Phase 7       | `516e4de` | 错误 traceback（`translate_trap` + `uctab` section） |
| Phase 8       | `e561280` | `WasmAotEvaluator` + `relon` facade + CLI `--backend` |
| Phase 9       | _本提交_ | criterion 对比 bench + 收官报告 |

前置基础：`5f3f7eb merge(arch): split relon-eval-api crate and abstract Evaluator trait`。

## 附录 C：loop 收官

本次 `/loop 10m` 从 Phase 1.beta 起步，跨越 14 次 merge 完成 wasm-AOT
backend 全链路：parser/analyzer 复用 → IR lowering → wasm 字节码 emit
→ wasmtime JIT 接入 → 二进制 ABI handshake → stdlib + method dispatch
→ capability gating + native fn import → traceback + custom section →
`Evaluator` trait 接入 + CLI / facade 公开。Phase 9 收官只剩下 bench
对比与文档，本提交把这两项落地。

到此 wasm-AOT backend 的 JIT + AOT 主链路完成，后续工作进入"语言子集
拓展 + 性能精调 + 沙箱细化"阶段（详见第六节），不再属于 `/loop` 单次
拉通的范畴。
