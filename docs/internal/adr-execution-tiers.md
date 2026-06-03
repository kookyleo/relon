# ADR:执行层(execution tiers)收敛为三支柱

- **状态**:方向已认可,实施待排期(2026-06-03)
- **关联**:[`tiers-explainer.md`](./tiers-explainer.md)(新人向导)、[`capability-and-trust-model.md`](./capability-and-trust-model.md)(信任模型)
- **北极星**:`crates/relon-bench` 的 `cmp_lua` —— 以 **LuaJIT** 为性能对标

---

## 1. 背景

当前有 **5 条执行路径**(后端 / tier),都消费同一份 `relon-ir`:

| tier | crate | 现状 | 完成度 |
|---|---|---|---|
| ① tree-walk | `relon-evaluator` | 默认兜底 + 全语言 + 非 `run_main` 方法独占 | 全 |
| ② bytecode | `relon-bytecode` | M2-A scaffold,仅手动 opt-in,deopt 落点未真正接通 | 标量信封 |
| ③ cranelift | `relon-codegen-cranelift` | 默认 `Backend::Auto`,全覆盖,**宿主 trace-JIT** | 全 |
| ④ llvm | `relon-codegen-llvm` | Phase B(W1/W2),feature-gated,需 LLVM 工具链 | 极少 |
| ⑤ wasm | `relon-codegen-wasm` | Phase Z POC,3 workload,仅浏览器,未进 CLI/Backend | 极少 |

**问题**:5 条路径维护面过大,且彼此重复(每加一个 IR `Op` 要在多处实现 + 测试)。一次结构审查(下文「#5 tier 分析」)量化:冻结边缘 tier 可省约 113 处 visitor 实现 + ~40% 每-op 测试。这促使我们重新审视**执行层的目标形态**。

---

## 2. 关键洞察(决策推导)

### 2.1 「宿主边界」是分离编译的产物,可被共址内联溶解
「局部/宿主驱动语义」(装饰器插件、native fn 回调 Relon 闭包)之所以难,只因 **编译后的 Relon 与宿主 Rust 处于不同世界、中间有边界**。若把宿主与 guest 编进**同一个编译单元**,让优化器跨边界内联,边界即溶解。这是成熟运行时的看家逻辑:

- **第一 Futamura 投影**:特化「解释器 w.r.t. 程序」= 编译后的程序。
- **GraalVM / Truffle**:解释器(Java)与 guest 同一世界,Graal 部分求值 → 跨内联,打平甚至超过手写 JIT。
- **Rust LTO**:`xs.iter().filter(|x| …)` 的闭包被内联进循环——同一回事。

推论:**任何能编到 LLVM 的宿主语言 → AOT 可共址内联;任何能编到 wasm 的宿主语言 → JIT 可共址内联。**

### 2.2 但「编译期已知」是前置;静态闭世界 vs 动态开世界
共址内联**只在编译期已知**「程序 + 宿主 fn 集」时完全成立:

- **闭世界**(程序 + 宿主集编译期固定)→ 完全内联。这正是**构建期 AOT**(把 `.relon` 编成原生二进制,即 `relon-rs-*`)的场景 → 归 **llvm**。
- **开世界**(运行时注册插件 / 来新 `.relon` / host fn 由运行时配置决定)→ 每次变更需**重编**。llvm 重编慢、要工具链 → 不适合;**wasm 重 JIT 便宜** → 归 **wasm**。

→ **静态 = llvm,动态 = wasm**,这条分界线是自洽的。

### 2.3 两处不可约的解释器职责(共址内联溶不掉)
1. **交互式片段求值**:LSP / REPL 要「在作用域里只算某个子表达式」(`Evaluator::eval(node, scope)`),且每次编辑重算。整程序编译产物是固定 `(args)->result`,没有「只算一个子树」的入口;compile-per-fragment-per-keystroke 延迟不可行。
2. **数据依赖的闭包分发**:`ops[name](v)` —— callee 是数据选出的 `Value::Closure`,编译期证不出调哪个 lambda。只能 (a) 间接调用(不内联),或 (b) 运行时投机内联 + 守卫 + **deopt**(= 带去优化的 JIT = **trace-JIT**)。
3. (附)**语义 oracle**:差分测试的标尺,今天是 tree-walk。

→ 要「数据依赖闭包也跑快」就**必须有 trace-JIT**(投机 + deopt 回退到解释器)。要交互式片段求值就**必须有解释器**。二者天然成对:**trace-JIT 的 deopt 落点就是解释器**。

> 代码级示例见 [`tiers-explainer.md`] 的延伸,以及本次讨论的 `eval(node,scope)` / `ops[name](v)` 两例。

---

## 3. 决策:收敛为三支柱

| 支柱 | 角色 | 由谁演化 | 类比 |
|---|---|---|---|
| **P1. 自适应核心(三挡)** | 通用运行时核心:**搞定所有语义**。内部三挡——**解释**(冷路径 / 片段求值 / 数据依赖分发 / 作 oracle)· **cranelift 直连 AOT**(运行时供给 + 单发要快)· **trace-JIT**(热循环投机,守卫失败 deopt 回解释器) | tier ① + tier ③ **全部**(cranelift 的直连 AOT + trace 发射两职都留) | **LuaJIT**(解释器 + trace-JIT)+ 直连 AOT 快冷启动 |
| **P2. AOT @ llvm** | 构建期、闭世界:把 `.relon` + 宿主 Rust **共址内联**编成**原生二进制** | tier ④ 长成全覆盖 | GraalVM Native Image / 普通 AOT |
| **P3. JIT @ wasm** | 运行时、开世界:编成 wasm 交给 **wasmtime** JIT;可移植 / 硬隔离沙箱 | tier ⑤ 长成全覆盖 | 浏览器 / WASI 部署 |

### 关键澄清:cranelift 不删,**留两职、降为 P1 内部模式**
cranelift **不再是独立的通用 AOT 支柱**(那个对外角色让位:静态→llvm,动态→wasm),但**整套 codegen 全保留在 P1 内部**,身兼两职:
- **直连 AOT 子模式**:`relon IR → 机器码` 单次快编,服务「运行时供给 + 一次性 + 立刻要快」剖面(v5-γ 抠到 ≤15µs 的那条冷启动路,投资不白费)。今天的 `AutoEvaluator` 基本就是这个形状。
- **trace-JIT 的码生成器**:录制器 → cranelift 发射 trace fn → 安装。trace-JIT 本就需要快速码生成器,cranelift 正合适(纯 Rust、零工具链、快编)。

于是 **P1 = { 解释 · cranelift 直连 AOT · trace-JIT } 三挡**,全部解释器 / cranelift 后端。对外它仍是「一个支柱」,只是内部按「冷/单发/热」自适应选挡。

- **bytecode(tier ②)删除**:它的「deopt 落点」角色由 P1 的解释器直接承担(现状 trace-JIT 本就 bottom-out 到 tree-walk,而非 bytecode)。

### 目标后端集
> **从 5 条 →「3 支柱 / 4 crate」**:
> - P1 = `relon-evaluator`(解释器)+ `relon-codegen-cranelift`(降为 P1 内部模式:**直连 AOT + trace 发射器**)+ `relon-trace-*`(录制/优化/发射)
> - P2 = `relon-codegen-llvm` + `relon-rs-*`(构建期工具链)
> - P3 = `relon-codegen-wasm` + `relon-wasm-evaluator`
> - **删除**:`relon-bytecode`

---

## 4. 各支柱适用场景

| 场景 | 支柱 |
|---|---|
| 桌面 / 服务端,运行时甩字符串、要正确 + 自适应快 | **P1**(解释冷启动 + trace-JIT 热路径) |
| 运行时供给 + **一次性、立刻要快**(CLI 单跑、serverless 冷启动) | **P1 的 cranelift 直连 AOT 挡**(单次快编→快跑,≤15µs 冷启动) |
| 交互式工具(LSP 行内求值、REPL) | **P1 的解释挡**(片段求值) |
| 数据依赖的闭包 / 多态派发要跑快 | **P1 的 trace-JIT 挡**(投机 + deopt) |
| 提前把配置编成**原生二进制**部署(可信、零运行时编译) | **P2**(llvm 共址内联,relon-rs) |
| 浏览器 / 边缘 / 不可信沙箱 / 可移植字节码 | **P3**(wasm + wasmtime) |

---

## 5. 后果

### 正面
- **维护面收敛**:删 bytecode;新增 `Op` 时,通用语义只需落在 P1 解释器(+ trace 录制器);llvm/wasm 各自按其闭世界/开世界需要跟进。
- **职责清晰**:P1 = 正确性 + 自适应;P2 = 静态极致(原生);P3 = 可移植 + 隔离。三者对应三种**部署形态**,而非三种「重复造的编译器」。
- **信任模型对齐**([`capability-and-trust-model.md`]):P1/P2 = 软件门(可信、快);P3 = VM 硬隔离(不可信)。三支柱天然映射两种信任姿态。
- **北极星一致**:P1 = LuaJIT 架构,正是 `cmp_lua` 对标对象。

### 代价 / 风险
- **P2 共址内联是 GraalVM 级投入**:要 Relon 全量 lower 到 LLVM + 宿主出 bitcode + LTO 共址 + 特化 pass。不是「加个 `#[inline]`」。
- **llvm / wasm 都要从 ~10–20% 覆盖长到 100%**:全 Op + sandbox + 闭包 + schema 方法。两份大工程。
- **effectful host fn**:共址内联省的是**调用开销**,但代码仍要在运行时跑、仍要**能力门**;wasm 侧还会落到 **WASI 边界**(syscall = import)。内联不消除 effect 本身。
- **oracle 迁移**:删 bytecode、未来若弱化 tree-walk 主导地位,差分测试标尺要改成 P1↔P2↔P3 互校或对规范校验。
- **重编/重 JIT 成本**:P3 动态路径每次变更要重 JIT(wasmtime 便宜但非零);P2 闭世界变更要重 LTO(慢,故定位构建期)。

---

## 6. 覆盖性论证 + 悬而未决

### 6.0 三支柱是否覆盖所有需求?——是,有保证
- **功能完备由 P1 单独托底**:P1 的解释挡覆盖**全语言**(它是语义参考实现),任何能用 Relon 表达的东西它都能正确跑。P2/P3 是**部署形态特化**,不是「另外两个要补全的语言实现」——补不全的回落到 P1(进程内)即可。
- **「运行时供给 + 单发 + 立刻要快」剖面**:由 **P1 的 cranelift 直连 AOT 挡**承接(见 §3 三挡),不靠 trace-JIT 预热曲线、不吃 P3 双重编译。这是显式保留 cranelift 直连 AOT 的理由——几乎零成本(codegen 本就在 P1 里给 trace 用)。
- **真正落在三支柱之外的小众 = 禁运行时 codegen 的平台**(iOS / 部分主机 / 强化沙箱):trace-JIT 与直连 AOT(都要运行时 codegen)在那儿用不了。对策:退到 **P1 纯解释挡**(慢但能跑)/ **P2 原生 AOT**(无运行时 codegen)/ **P3 让 wasmtime 跑解释模式**。**覆盖,只是那些平台少了 JIT/AOT 加速——平台限制,非架构缺口。**

→ 结论:**够。** 完备性 P1 托底;快冷启动单发由 P1 直连 AOT 挡保住;禁-JIT 平台靠 P1 解释挡 / P2 兜住。

### 6.1 实施前要答
1. **P2 的共址内联流水线**:rustc 宿主出 bitcode + Relon-LLVM 同 LTO 单元的工程路径?先做「静态 native fn 内联」最小可用,还是直接上通用特化?
2. **数据依赖闭包**:P1 里接受**间接调用**(简单,够用)还是要 trace-JIT **投机内联**(更快,更复杂)?二者可分阶段。
3. **P3 的宿主语义**:wasm 模块里 native fn / 装饰器以 host import 还是共址内联实现?effectful 的部分如何过 WASI?
4. **trace-JIT 边界**:P1 的 trace-JIT 自留(cranelift 发射),还是在 P3 里指望 wasmtime 的分层?(wasmtime 不懂 Relon 类型反馈,特化深度有限——倾向自留。)
5. **P1 三挡选挡:主干已落地,非未决**。`AutoEvaluator`(`crates/relon/src/auto_evaluator.rs`)+ cranelift 内部 hot-counter 今天就跑着默认策略:

   ```
   非 run_main(片段 / 惰性 / 宿主驱动:eval / eval_root / force_thunk / invoke_closure)
                                          → 解释               [结构性,定死——只有解释器能跑任意节点]
   run_main:
     trivial scalar 形状(单标量参 + 字面/算术体,is_trivial_main)
                                          → 解释               [冷编不值,解释器 µs 出结果]
     否则                                  → cranelift 直连 AOT  [v5-γ 把冷编压到 ≤15µs,几乎白送]
       其中单 fn 热循环越 hot 阈值        → trace-JIT 自动升挡  [cranelift hot-counter,~1000]
   ```

   **「非平凡单发是否该解释优先」**——唯一有张力的点——基本闭合:冷编从 ~4-5ms 降到 **≤15µs(v5-γ)**后,天平倒向「非平凡直接 AOT」,不值得为省 15µs 去解释。真正剩下的只是**三个可调/重构尾巴**:① trivial 分类边界(调参)· ② trace-JIT hot 阈值(调参 / 自适应)· ③ 把现散在两处的决策点(AutoEvaluator trivial 路由 + cranelift hot-counter)合成**一份显式三挡选择器**(重构,非研究问题)。
6. **迁移顺序**:见下。

---

## 7. 近期第一步(迈向目标,低风险)

不一刀切,分阶段:

1. **冻结边缘 tier(#5 建议)**:把 bytecode / llvm / wasm 标注 `maintenance-frozen` + 文档化 scope。这是迈向目标的第一步——**停止在「要么删(bytecode)、要么换轨(llvm/wasm 待长大)」的 tier 上做增量跟进**,立刻省维护面。(bytecode 的 freeze 即「删除前的冻结」。)
2. **删 bytecode**:确认 trace-JIT deopt 落点切到 tree-walk 后,退役 `relon-bytecode`。
3. **P3 长大**:wasm IR walker 补全 → 接入 `Backend` 枚举 → 成为动态开世界的正式路径。
4. **P2 长大**:llvm 全覆盖 + 共址内联流水线(借力已有的 `relon-rs-*`)。
5. **cranelift 降为 P1 内部模式**:当 llvm/wasm 接管**对外**的通用 AOT 后,cranelift 不再是独立支柱,但**整套 codegen 留在 P1 内部**——身兼**直连 AOT 子模式**(快冷启动单发,§6.0)+ **trace-JIT 发射器**。**不删 crate,只是收缩对外角色。**

> 顺序原则:**先冻结/删除(省维护),再让 P3/P2 长大(接管),最后 cranelift 降格(收尾)。** 任一阶段都保持全绿、可回退。

---

## 8. 一句话

> **3 支柱 = 自适应核心(P1:解释 / cranelift 直连 AOT / trace-JIT 三挡,LuaJIT 式)+ 静态原生 AOT(P2:llvm/relon-rs)+ 动态可移植 JIT(P3:wasm)。** 它们对应三种**部署形态与信任姿态**,而非三个重复的编译器。**功能完备由 P1 托底,覆盖性已论证(§6.0)**;cranelift 不死,降为 P1 内部的「直连 AOT + trace 发射器」两职;bytecode 退役。剩下的不是「能不能」,是 §6.1 那几个工程/产品决断。
