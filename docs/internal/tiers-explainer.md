# Relon 执行层:3 支柱 / 4 后端

> 新人 onboarding 向导。用**乐谱与乐团**的比喻,讲清 Relon 怎么把一份源码跑起来:**4 个执行后端**,归成 **3 个架构支柱**。读完你就分得清「支柱」和「后端」这两个常被混淆的词。
>
> 配套架构决策:[`adr-execution-tiers.md`](./adr-execution-tiers.md)。

---

## 先看大局:从源码到执行的流水线

你写的 `.relon`,要变成「跑起来的结果」,中间经过一条流水线。用**音乐**打比方:

| 阶段 | 比喻 | 干什么 |
|---|---|---|
| 源码 | 作曲家心里的旋律 | 你写的 `.relon` |
| parser | 速记员 | 记成草谱(语法树) |
| analyzer | 校对 | 查错音、查权限(类型 / 能力检查) |
| **IR** | **标准五线谱** | 统一的「中间表示」,由 **94 种音符(`Op`)** 组成 |
| 后端 | **照谱演奏的乐手** | 都吃同一份谱,各演各的 |

**前半段(到 IR / 五线谱为止)只有一份**;真正「演奏」的是后面的后端。为什么要好几个后端?因为「启动快 / 跑得快 / 什么曲子都会 / 哪都能放」**没法全占**,于是按场合分了几路。

> 📌 黑话速记:
> - **IR**:标准五线谱(后端都吃它) · **Op**:谱上一种音符(94 种)
> - **AOT(提前编译)**:演出前先排练成定版机器码
> - **解释(interpret)**:拿谱当场一个音一个音读着演,不预编

---

## 两个层级:3 支柱(乐团)/ 4 后端(乐手)

这是最容易混的地方,先一张表钉死:

| 支柱(架构分组 = 3 个「乐团」) | 含的后端(乐手 = 4 个) | crate | 场合 |
|---|---|---|---|
| **P1 · 自适应核心** | 🎻 tree-walk(视奏手)**+** 🎺 cranelift(交响手) | `relon-evaluator` + `relon-codegen-cranelift` | 通用运行时:随到随演 |
| **P2 · 静态 AOT** | 🏎️ llvm | `relon-codegen-llvm`(+ `relon-rs-*` 构建期工具链) | 提前把曲子刻成「唱片」(原生二进制) |
| **P3 · 动态可移植** | 📻 wasm | `relon-codegen-wasm` + `relon-wasm-evaluator` | 装进「便携音乐盒」到处放(浏览器/沙箱) |

**关键:`P1` 这一个支柱里有两位乐手**——视奏手(解释)和交响手(直连 AOT)。它「冷场/简单曲子让视奏手上、正经大曲子让交响手上」**自动换人**。所以是 **3 支柱、4 后端**:支柱是「按场合分的编制」,后端是「具体哪位乐手」,P1 编制里坐了 2 位。

> 统一门面:`relon` crate 里的 **`AutoEvaluator`(`Backend::Auto`,默认)** 就是 P1 这位「乐团指挥」——对外只是一个 `Evaluator`,内部按曲子难易在视奏手 / 交响手之间调度。

---

## 4 位乐手,逐个认识

### 🎻 tree-walk 视奏手 —— `relon-evaluator`(P1)
拿谱**当场逐音读着演**(逐 AST 节点解释执行)。
- **慢**,但**什么谱都会、绝不卡壳**——它是语义的「标准答案」(差分测试的标尺 / oracle)。
- 很多「非整曲演奏」的活**只有他会**:片段求值(LSP 悬停只算一个表达式)、闭包回调、惰性求值——`run_main` 之外的方法全靠他。
- **零 codegen 依赖**,能在 wasm32 / 无工具链环境单独构建,被 `relon-lsp`、`relon-wasm-bindings` 直接复用。
- 📌 **顶梁柱,不可少**。

### 🎺 cranelift 交响手 —— `relon-codegen-cranelift`(P1)
把整首谱**提前编成精排定版机器码(直连 AOT)**,单次快编、跑得快。
- **94 种音符全会**,四道安保(越界 / 陷阱 / 能力门 / 超时)齐全。
- 是 `Backend::Auto` 下**非平凡 `run_main` 的默认主力**;纯 Rust、零外部工具链,所以哪都能编。
- 冷编实测 ~300µs–3ms,故**平凡/单发**的曲子 `AutoEvaluator` 会路由回视奏手(冷编不值)。
- 📌 **性能主力,不可少**。

> ⚠️ 历史注意:cranelift 曾经还兼「现场即兴(trace-JIT)」一职——边演边把热段升级成炫技版。**该机制连同 bytecode 已退役**(见下「已退役」),现在 cranelift **只剩直连 AOT 一职**。

### 🏎️ llvm 录音棚 —— `relon-codegen-llvm`(P2)
为**构建期**把 `.relon` + 宿主 Rust 一起**刻成原生二进制唱片**(`relon-rs-*` 工具链就是干这个)。
- 编译慢、要 **LLVM 18 工具链**,默认 feature-gated。其归宿是「提前编好、零运行时编译」的部署。
- 现状 **Phase B**(覆盖面窄),且**还没接能力门**(Phase C 待补——见 ADR §0 C4)。
- 📌 **待长大**(只需长到「要刻成原生的子集」,非全 IR)。

### 📻 wasm 便携音乐盒 —— `relon-codegen-wasm` + `relon-wasm-evaluator`(P3)
把谱灌进**哪都能放的密封盒**:`codegen-wasm` 出 wasm 字节,`wasm-evaluator` 用 **wasmtime** 跑(秒启动、硬隔离沙箱)。
- 现状 **Phase Z POC**(只灌了几首),**还没接进 `Backend` 枚举**,主要在浏览器 playground。
- 📌 **待长大**(只需长到「浏览器/沙箱部署所需」,非全 IR)。

---

## 已退役:bytecode + trace-JIT

原来还有第 5 位乐手 🥁 **bytecode**(`relon-bytecode`)和一套 **trace-JIT**(现场即兴 + 录制 + 守卫回退,4 个 `relon-trace-*` crate)。**它们已被删除**,原因诚实记录:

- **trace-JIT 实测从未加速**:基准里它那一列(`relon_jit`)在 28 个 workload 全部 fallthrough 到解释器/bytecode 速度,被 AOT 列碾压 100–1000×。**性能从来是 AOT 在担,不是它。**
- **bytecode 与 trace-JIT 深度耦合**:trace-JIT 以 bytecode VM 为基线 + 录制源 + 回退落点,二者是「一个耦合子系统」,要么一起留要么一起删。
- 结论:**一并退役**,删 5 个 crate(净 −62,787 行),零性能损失。`EffectClass` 等仍有用的小件迁进了 `relon-ir`。

> 这不是「冷冻」,是「删除」——删的是从未加速、且拖着复杂度的东西。

---

## 速查表 + 一句话记忆

| 后端 | 支柱 | 启动 | 跑速 | 会几首 | 现状 |
|---|---|---|---|---|---|
| 🎻 tree-walk | P1 | 即 | 慢 | **全部** | 顶梁柱 / oracle |
| 🎺 cranelift | P1 | 中(冷编 ~ms) | **快** | **全部** | **默认主力** |
| 🏎️ llvm | P2 | 慢(要工具链) | 极快 | 窄(Phase B) | 构建期 AOT,待长大 |
| 📻 wasm | P3 | 即 | 中 | 窄(Phase Z) | 浏览器/沙箱,待长大 |

**一句话记忆:**

> **3 个支柱按场合分**:P1 随到随演(解释 + 直连 AOT 自动换人)、P2 提前刻原生唱片、P3 装便携盒到处放。
> **4 个后端**:P1 坐了 2 位(tree-walk 🎻 + cranelift 🎺),P2/P3 各 1 位(llvm 🏎️ / wasm 📻)。
> **功能与正确性今天全靠 P1 托底**(tree-walk 全语言 + cranelift 全覆盖);P2/P3 是按部署形态特化的快路 / 可移植路,补不全就回落 P1。

---

## 延伸阅读

- 架构决策(为什么 3 支柱、trace-JIT 为何退役、P2/P3 怎么长大):[`adr-execution-tiers.md`](./adr-execution-tiers.md)
- 能力 / 信任模型:[`capability-and-trust-model.md`](./capability-and-trust-model.md)
- 各后端的 envelope / phase 边界:见各 crate `src/lib.rs` 顶部模块文档(搜 `Phase` / `envelope` / `Z.1`)。
