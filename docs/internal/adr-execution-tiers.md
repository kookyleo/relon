# ADR:执行层(execution tiers)收敛为三支柱

- **状态**:**部分实施(2026-06-03)**——trace-JIT + bytecode 已退役(选项 C,见 §0.1);P2/P3 长大待排期。原决策推导见 §1–§8(保留作记录),**以 §0.1 实施结果为现状准绳**。
- **关联**:[`tiers-explainer.md`](./tiers-explainer.md)(新人向导)、[`capability-and-trust-model.md`](./capability-and-trust-model.md)(信任模型)
- **北极星**:`crates/relon-bench` 的 `cmp_lua` —— 以 **LuaJIT** 为性能对标(注:打 LuaJIT 的关键路径是 **wasm_fast / llvm_aot 的 AOT**,**不是** trace-JIT)

---

## 0. 施工前核对(2026-06-03):与代码对齐的修正

通盘核对代码后修正了几处与现状不符的表述,**施工据本节为准**:

| # | 原表述 | 实际(代码证据) | 影响 |
|---|---|---|---|
| C1 | trace-JIT 解决「数据依赖闭包分发」,是 P1 热路径支柱 | recorder 对 `Op::If`/`CallClosure`/`CallNative` **直接 abort**(`relon-trace-recorder/src/lowering.rs:505/543/544`,后两者标 `UnrecoverableEffect`);trace-JIT **不在打 LuaJIT 的关键路径** | **trace-JIT 降为「窄、可选、实验性」**:只能 trace 无分支直线数值循环。「数据依赖闭包」**今天由 cranelift 直连 AOT 的闭包表间接调用承接**(全覆盖、可用,不内联);trace-JIT 内联是**未来**且先要补 recorder。P1 真正的核心 = **解释 + 直连 AOT** |
| C2 | 冷编 ≤15µs,单发问题闭合 | 实测冷启动 **~300µs–3ms**(W11 2.93ms / CLI ~300µs / schema ~567µs);≤15µs 只是某 object-cache 里程碑**目标** | 「非平凡单发是否解释优先」**仍未决**;`is_trivial_main→解释` 正因冷编贵 |
| C3 | 删 bytecode = 退役 scaffold | cranelift trace-JIT 协议类型(`HotTraceTrigger`/`RecordingRegistrationData`/`InstalledTraceLookup`/`TraceInvokeOutcome`/`VmValue`)**住在 bytecode crate**,cranelift 依赖之 | 删 bytecode **前置**:先把这些类型搬出(→ trace-abi 或新家) |
| C4 | — | llvm **无能力门**(Phase B;cap vtable / sandbox traps 是 Phase C,`codegen-llvm/lib.rs:41`) | P2 要可信强制,**Phase C 是信任模型前置**;本会话能力门只覆盖了 bytecode+cranelift |
| C5 | llvm/wasm 各从 ~10-20% 长到 100% | P1(tree-walk+cranelift)**今天即全覆盖**;llvm/wasm 只需覆盖各自部署 niche | 工作量大幅下修;**P1 无需生长**;tree-walk 留着 → oracle **不迁移** |

---

## 0.1 实施结果(2026-06-03):trace-JIT + bytecode 已退役(选项 C)

基准实测显示 `relon_jit`(trace-JIT)列在 28 个 workload 里**无一真加速**——全 fallthrough 到解释器/bytecode 速度,且被 AOT 列碾压 100–1000×(性能由 AOT 担,非 trace-JIT)。又因 trace-JIT 与 bytecode VM **深度耦合**(jit.rs 的 `JitEvaluator` 以 bytecode VM 为基线 + 录制源 + deopt 落点),二者是「一个耦合子系统」,故**一并退役**:

- **已删 5 个 crate**:`relon-bytecode` · `relon-trace-abi` · `relon-trace-jit` · `relon-trace-recorder` · `relon-trace-emitter`(净 **−62,787 行**;`EffectClass`/`fx_hash_bytes` 迁入 `relon-ir`)。
- **已剥用法**:cranelift 的 trace 机器(bytecode_bridge / trace_install/ic/recording/inline / hot-counter codegen)、relon 的 `jit.rs` + `Backend::Bytecode`、cli `--backend bytecode`、bench 的 `relon_jit`/`bytecode` 列、test-harness 的 bytecode 臂。
- **零性能损失**(删的是从未加速的列)、全验绿、24 crate。

→ **执行 tier 从 5 收为 4:`tree-walk`(解释 + oracle)+ `cranelift`(直连 AOT,默认主力)+ `llvm`(P2)+ `wasm`(P3)。** **P1 = 解释 + cranelift 直连 AOT 两挡**(原文 §3 的「可选第三挡 trace-JIT」已不存在);cranelift 现**只剩直连 AOT 一职**(trace 发射器随 trace-JIT 一并删除)。

> 下文 §1–§8 是退役**之前**的决策推导,措辞仍含 trace-JIT(标注「可选第三挡」「trace 发射器」等)——保留作历史记录,凡与本节冲突处**以本节为准**。

---

## 0.2 语义双路径:tree-walk(AST)是 oracle,IR 是编译后端的腰(2026-06-06 架构 review 校准)

防后人误解"relon-IR 是全系统唯一语义腰",在此钉死真实结构:

- **relon-IR 是「编译后端」的唯一腰**:cranelift / llvm / wasm 三者都只 lower 它,语义在 IR 层定义一次、各后端只做 codegen(不重定义语义)。这条对编译后端**完全成立**。
- **但 tree-walk 解释器直接走 AST**(`relon-evaluator` 接 `&Node`,核心求值**不消费 IR**;它依赖 `relon-ir` 仅为复用 Unicode 数据表,该耦合已于 2026-06-06 抽出 `relon-unicode` leaf crate 消除)。所以"IR 是全系统唯一语义腰"**字面不成立**:语义落在**两条代码路径**——tree-walk 的 AST 求值,与 IR-lowering→codegen。
- **这是有意设计,不是裂缝**:**tree-walk = 语义基准 oracle**(最权威、最全那一份),编译后端是必须与它**逐字节一致**的性能实现。"解释器走 AST、编译器走 IR"是正常分层——IR 是给编译后端的腰,**不是**给解释器的。
- **两条路靠差分测试对齐**(`relon-test-harness`,tree-walk 当 oracle,bit-equal 深比对),而非靠共享同一份表示。
- **代价(留痕)**:同一语义要实现两遍、只靠测试对齐——**差分覆盖漏了某形状,两条路就可能悄悄分叉**。这是全系统唯一一处根本性结构二元性,被强差分 harness 缓解但未消除。**因此差分覆盖是这条二元性的唯一保险:扩语言面 / 新增 Op / 改求值语义时,必须同步扩差分用例,否则 oracle 名存实亡。**
- peephole 等优化只在 IR-lowering(如 `list.sum(range())` 融合成单 Loop),tree-walk 无对应路径——这是**性能差异、非语义差异**,不影响正确性(差分比的是结果值)。

---

## 1. 背景(退役前的 5-tier 历史快照)

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
2. **数据依赖的闭包分发**:`ops[name](v)` —— callee 是数据选出的 `Value::Closure`,编译期证不出调哪个 lambda。只能 (a) 间接调用(不内联),或 (b) 运行时投机内联 + 守卫 + **deopt**(= 带去优化的 JIT = trace-JIT)。**现实(C1):今天走 (a)——cranelift 直连 AOT 的闭包表间接调用,全覆盖、可用;(b) trace-JIT 内联是未来,且当前 recorder 对 `CallClosure` 直接 abort,要先补 recorder。**
3. (附)**语义 oracle**:差分测试的标尺,今天是 tree-walk(本方案保留 tree-walk,故 oracle 不动)。

→ **交互式片段求值必须有解释器**(结构性,无替代)。数据依赖闭包**间接调用即可用**(直连 AOT);要把它**内联级提速**才需要 trace-JIT——而那是窄、可选、未来的优化(见 §0 C1)。

> 代码级示例见 [`tiers-explainer.md`] 的延伸,以及本次讨论的 `eval(node,scope)` / `ops[name](v)` 两例。

---

## 3. 决策:收敛为三支柱

| 支柱 | 角色 | 由谁演化 | 类比 |
|---|---|---|---|
| **P1. 自适应核心** | 通用运行时核心:**搞定所有语义、且今天即全覆盖**。两挡——**解释**(冷路径 / 片段求值 / 数据依赖分发 / 作 oracle)· **cranelift 直连 AOT**(非平凡 run_main:全覆盖,含闭包表/native 调用)。〔原文这里还有「可选第三挡 trace-JIT」,**已退役**,见 §0.1〕 | tier ① + tier ③ | 核心 = 解释 + AOT |
| **P2. AOT @ llvm** | 构建期、闭世界:把 `.relon` + 宿主 Rust **共址内联**编成**原生二进制** | tier ④ 长成全覆盖 | GraalVM Native Image / 普通 AOT |
| **P3. JIT @ wasm** | 运行时、开世界:编成 wasm 交给 **wasmtime** JIT;可移植 / 硬隔离沙箱 | tier ⑤ 长成全覆盖 | 浏览器 / WASI 部署 |

### 关键澄清:cranelift 不删,**留两职、降为 P1 内部模式**
cranelift **不再是独立的通用 AOT 支柱**(那个对外角色让位:静态→llvm,动态→wasm),但**整套 codegen 全保留在 P1 内部**,身兼两职:
- **直连 AOT(P1 的主力性能挡)**:`relon IR → 机器码` 单次快编、**全覆盖**(含闭包表间接调用、native 调用、控制流)。冷编实测 **~300µs–3ms**(非 ≤15µs,见 §0 C2),故对**平凡/单发**用 `is_trivial_main` 路由回解释。
- **trace-JIT 的码生成器(可选)**:录制器 → cranelift 发射 trace fn → 安装。**当前能力窄**——recorder 对 `If`/`CallClosure`/`CallNative` abort(§0 C1),只能 trace 无分支数值热循环,且不在打 LuaJIT 的关键路径上。

于是 **P1 = { 解释 · cranelift 直连 AOT } 核心两挡 + trace-JIT 可选第三挡**。对外它仍是「一个支柱」,内部按「冷/单发/热」选挡;**性能主力是直连 AOT,不是 trace-JIT**。

- **bytecode(tier ②)删除**:它的「deopt 落点」角色由 P1 的解释器直接承担(现状 trace-JIT 本就 bottom-out 到 tree-walk,而非 bytecode)。**前置(§0 C3)**:cranelift 的 trace-JIT 协议类型住在 bytecode crate,删除前须先迁出。

### 目标后端集(**已落地**,见 §0.1)
> **从 5 条 → 4 tier**:
> - P1 = `relon-evaluator`(解释器)+ `relon-codegen-cranelift`(**直连 AOT 一职**;trace 发射器随 trace-JIT 删除)
> - P2 = `relon-codegen-llvm` + `relon-rs-*`(构建期工具链)
> - P3 = `relon-codegen-wasm` + `relon-wasm-evaluator`
> - **已删除**:`relon-bytecode` + `relon-trace-abi` + `relon-trace-jit` + `relon-trace-recorder` + `relon-trace-emitter`

---

## 4. 各支柱适用场景

| 场景 | 支柱 |
|---|---|
| 桌面 / 服务端,运行时甩字符串、要正确 + 快 | **P1**(平凡→解释;非平凡→cranelift 直连 AOT) |
| 运行时供给 + **一次性、立刻要快** | **P1**:平凡走解释;非平凡付 ~300µs–3ms 冷编→快跑。**「单发是否解释优先」仍是开放调参(§0 C2)** |
| 交互式工具(LSP 行内求值、REPL) | **P1 的解释挡**(片段求值,结构性) |
| 数据依赖的闭包 / 多态派发 | **P1 直连 AOT 的闭包表间接调用**(可用,不内联);内联级提速→未来 trace-JIT(recorder 待补,§0 C1) |
| 无分支数值热循环要极致 | **P1 的 trace-JIT**(可选;当前唯一能 trace 的形状) |
| 提前把配置编成**原生二进制**部署(可信、零运行时编译) | **P2**(llvm;含 Phase C 能力门,§0 C4) |
| 浏览器 / 边缘 / 不可信沙箱 / 可移植字节码 | **P3**(wasm + wasmtime) |

---

## 5. 后果

### 正面
- **维护面收敛**:删 bytecode;新增 `Op` 时,通用语义只需落在 P1 解释器(+ trace 录制器);llvm/wasm 各自按其闭世界/开世界需要跟进。
- **职责清晰**:P1 = 正确性 + 自适应;P2 = 静态极致(原生);P3 = 可移植 + 隔离。三者对应三种**部署形态**,而非三种「重复造的编译器」。
- **信任模型对齐**([`capability-and-trust-model.md`]):P1/P2 = 软件门(可信、快);P3 = VM 硬隔离(不可信)。三支柱天然映射两种信任姿态。
- **北极星一致**:P1 = LuaJIT 架构,正是 `cmp_lua` 对标对象。

### 代价 / 风险
- **P1 无需生长**(C5):tree-walk + cranelift 今天即全覆盖。这是最大的去风险点——通用语义不必重做。
- **P2 = llvm 覆盖 + Phase C 能力门 + (远期)共址内联**:
  - 覆盖只需长到「要编成原生二进制的子集」(relon-rs niche),**非 100% IR**(C5)。
  - **Phase C(cap vtable + sandbox traps)是信任模型前置**(C4):没它,P2 编出的原生二进制不强制 capability。本会话能力门只覆盖了 bytecode+cranelift。
  - 共址内联(Futamura/LTO)是 **GraalVM 级远期投入**,不是近期施工项。
- **P3 = wasm 覆盖**:只需长到「浏览器/沙箱部署所需」,非 100% IR(C5)。
- **trace-JIT 现状窄 + 不在关键路径**(C1):recorder 对 `If`/`CallClosure`/`CallNative` abort。要它担「数据依赖闭包内联」得先大改 recorder——**是否投入是独立战略决策**(见 §6.1)。
- **effectful host fn**:共址内联省**调用开销**,但代码仍运行时跑、仍要能力门;wasm 侧落到 **WASI 边界**。
- **oracle 不迁移**:本方案保留 tree-walk,差分标尺照旧。(仅当未来弃用 tree-walk 才需重设——本方案不弃。)
- **重编/重 JIT 成本**:P3 每次变更重 JIT(wasmtime 便宜但非零);P2 闭世界变更重 LTO(慢,故定位构建期)。

---

## 6. 覆盖性论证 + 悬而未决

### 6.0 三支柱是否覆盖所有需求?——是,有保证
- **功能完备由 P1 单独托底,且今天即全覆盖**:P1 = tree-walk 解释(全语言、语义参考)+ cranelift 直连 AOT(全覆盖),任何能用 Relon 表达的东西都能正确跑。P2/P3 是**部署形态特化**,不是「另外两个要补全的语言实现」——补不全的回落到 P1(进程内)即可。
- **「运行时供给 + 性能」剖面**:由 **P1 的 cranelift 直连 AOT** 承接(主力性能挡),不靠 trace-JIT(窄,§0 C1)。单发非平凡付 ~300µs–3ms 冷编(§0 C2)再快跑——是否值当 vs 解释,是工作量依赖的开放调参(§6.1#5),**但功能不缺**。
- **真正落在三支柱之外的小众 = 禁运行时 codegen 的平台**(iOS / 部分主机 / 强化沙箱):trace-JIT 与直连 AOT(都要运行时 codegen)在那儿用不了。对策:退到 **P1 纯解释挡**(慢但能跑)/ **P2 原生 AOT**(无运行时 codegen)/ **P3 让 wasmtime 跑解释模式**。**覆盖,只是那些平台少了 JIT/AOT 加速——平台限制,非架构缺口。**

→ 结论:**功能上够、且全覆盖今天就在**(P1 托底);性能由各路 AOT 担;禁-JIT 平台靠 P1 解释挡 / P2 兜住。**「单发非平凡的解释 vs AOT」是性能调参,不影响覆盖性。**

### 6.1 实施前要答
1. **P2 的共址内联流水线**:rustc 宿主出 bitcode + Relon-LLVM 同 LTO 单元的工程路径?先做「静态 native fn 内联」最小可用,还是直接上通用特化?
2. **数据依赖闭包**:P1 里接受**间接调用**(简单,够用)还是要 trace-JIT **投机内联**(更快,更复杂)?二者可分阶段。
3. **P3 的宿主语义**:wasm 模块里 native fn / 装饰器以 host import 还是共址内联实现?effectful 的部分如何过 WASI?
4. **trace-JIT 边界**:P1 的 trace-JIT 自留(cranelift 发射),还是在 P3 里指望 wasmtime 的分层?(wasmtime 不懂 Relon 类型反馈,特化深度有限——倾向自留。)
5. **P1 选挡:主干已落地,非未决**。`AutoEvaluator`(`crates/relon/src/auto_evaluator.rs`)+ cranelift 内部 hot-counter 今天就跑着默认策略:

   ```
   非 run_main(片段 / 惰性 / 宿主驱动:eval / eval_root / force_thunk / invoke_closure)
                                          → 解释               [结构性,定死——只有解释器能跑任意节点]
   run_main:
     trivial scalar 形状(单标量参 + 字面/算术体,is_trivial_main)
                                          → 解释               [冷编不值,解释器 µs 出结果]
     否则                                  → cranelift 直连 AOT  [冷编 ~300µs–3ms,非平凡即付]
       (无分支数值热循环越 hot 阈值       → trace-JIT 可选升挡,cranelift hot-counter ~1000)
   ```

   **「非平凡单发是否该解释优先」仍未决(C2 修正)**:冷编实测 **~300µs–3ms**(非 ≤15µs),对**单发**非平凡是真实成本——`is_trivial_main→解释` 正是为此。是否对「非平凡但廉价 + 单发」也解释优先(interpret-then-AOT-on-repeat),取决于真实负载「单发 vs 复用」占比 + 实测冷编成本曲线。其余三个调参/重构尾巴照旧:① trivial 边界 · ② trace-JIT 阈值 · ③ 两处决策点合一。
6. **trace-JIT:投入还是冻结?**(C1 引出)recorder 仅支持无分支数值循环(`If`/`CallClosure`/`CallNative` abort),且不在打 LuaJIT 的关键路径。**选项**:(a) 维持现状当窄优化;(b) 投入扩 recorder(`If`+`CallClosure`)以担「数据依赖闭包内联」——大工程;(c) 与 bytecode 一并冻结/精简(它本就和 bytecode 协议耦合,C3)。**默认建议 (a)**:核心性能靠直连 AOT,trace-JIT 不挡路也不优先。
7. **迁移顺序**:见下。

---

## 7. 近期第一步(迈向目标,低风险)

不一刀切,分阶段:

1. **冻结边缘 tier(#5 建议)**:把 bytecode / llvm / wasm 标注 `maintenance-frozen` + 文档化 scope。这是迈向目标的第一步——**停止在「要么删(bytecode)、要么换轨(llvm/wasm 待长大)」的 tier 上做增量跟进**,立刻省维护面。(bytecode 的 freeze 即「删除前的冻结」。)
2. **删 bytecode**(前置见 §0 C3):**先**把 trace-JIT 协议类型(`HotTraceTrigger`/`RecordingRegistrationData`/`InstalledTraceLookup`/`TraceInvokeOutcome`/`VmValue`-用法)从 `relon-bytecode` 迁出(→ `relon-trace-abi` 或新家),解开 cranelift→bytecode 依赖;**再**确认 trace-JIT deopt 落点切到 tree-walk;**最后**退役 `relon-bytecode`。依赖方还有 relon / relon-cli / relon-bench / relon-test-harness,一并清理。
3. **P3 长大**:wasm IR walker 补到**浏览器/沙箱 niche 所需**(非 100%)→ 接入 `Backend` 枚举 → 成为动态/隔离部署的正式路径。
4. **P2 长大**:llvm 覆盖到**原生二进制 niche 所需** + **Phase C 能力门**(C4,信任模型前置)+(远期)共址内联流水线(借力 `relon-rs-*`)。
5. **cranelift 降为 P1 内部模式**:当 llvm/wasm 接管**对外**的通用 AOT 后,cranelift 不再是独立支柱,但**整套 codegen 留在 P1 内部**——身兼**直连 AOT 子模式**(快冷启动单发,§6.0)+ **trace-JIT 发射器**。**不删 crate,只是收缩对外角色。**

> 顺序原则:**先冻结/删除(省维护),再让 P3/P2 长大(接管),最后 cranelift 降格(收尾)。** 任一阶段都保持全绿、可回退。

---

## 8. 一句话(现状,2026-06-03)

> **3 支柱 = 自适应核心(P1:解释 + cranelift 直连 AOT 两挡)+ 静态原生 AOT(P2:llvm/relon-rs,待长大 + Phase C 能力门)+ 动态可移植 JIT(P3:wasm,待长大)。** 它们对应三种**部署形态与信任姿态**,而非三个重复的编译器。**性能主力是 AOT(直连/llvm/wasm)**;**功能完备 + 全覆盖由 P1 今天就托底(无需生长)**。**trace-JIT + bytecode 已退役**(选项 C,§0.1——实测从未加速 + 与 bytecode 深耦合);执行 tier 从 5 收为 4。剩下的是 §6.1 里 P2/P3 长大的那几个工程决断 + §0 的前置(尤其 C4:llvm Phase C 能力门)。
