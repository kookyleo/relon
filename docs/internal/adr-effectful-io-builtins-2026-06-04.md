# ADR:Relon 是纯函数 —— 不暴露任何 effectful 语言级 builtin

- **状态**:**已决定(2026-06-04)**。
- **关联**:[`capability-and-trust-model.md`](./capability-and-trust-model.md)(信任模型 / 6 个 CapabilityBit)、[`../zh/guide/stdlib.md`](../zh/guide/stdlib.md)(能力门控 builtin —— 待按本 ADR 修订)、[`phase1-execution-plan.md`](./phase1-execution-plan.md) §8(P-fs staging —— 撤销)
- **决策者**:项目所有者
- **一句话**:**Relon 语言本身是一个纯函数 `f(inputs) -> output`。** 它永不向外伸手取数据;一切 effectful 的东西(文件内容、目录、文件元信息、时间、随机数、环境)都是**外部 host 喂进来的 input**。

---

## 1. 核心立场

> **Relon = 纯函数。** 求值是 `inputs → output` 的确定性变换,**无任何 ambient 副作用、无任何向外取数据的语言级能力**。
>
> 凡 effectful 的值,都由 **host 在求值之外取好、作为 input 喂进来**:host 采时钟、读文件、列目录、生成随机数、读环境,把结果作为**输入数据**绑定给这次求值。Relon 拿到的永远是已经定好的输入。

这一刀切下去,三条北极星(决定性 / 可审计 / 零信任沙箱)**全部无条件成立**,因为求值本身根本不碰外部世界。

---

## 2. 两类「向外」必须分清(关键)

容易混淆、但本质不同的两件事:

| | 性质 | 时机 | 裁决 |
|---|---|---|---|
| **`#import`(代码 / 远程 module)** | **程序构造** —— 决定「哪些 module 组成这个程序」 | **编译期**(load / analyze,选后端之前) | **保留**。类比编译 / 链接 / 取依赖,不是运行时数据 IO。可 sandboxed/trusted resolver + 内容固定。 |
| **`read_file` / `read_dir` / `stat` / `clock` / `random`** | **运行时取数据** —— 语言在求值中向外伸手够外部状态 | 求值期 | **撤销**。这些值应是 host 喂进来的 input,不是语言的能力。 |

`#import` 拉进来的是**程序的一部分**(代码),发生在程序还在被构造的阶段;这与「一个已经构造好的纯函数在运行时去读一个数据文件」是两回事。前者像 `#include` / `import`,后者像 `open()` syscall —— 一门纯函数式配置语言可以有前者,不该有后者。

---

## 3. 为什么这些都是 input,不是能力

- **时间 / 随机数**:经典的非确定来源。`clock()` / `random()` 同输入不同时刻不同果——它们**就是外部输入**。需要「当前时间」就让 host 把时间戳作为 input 传进来;需要 nonce 就让 host 生成好传进来。求值对一个**给定的**时间戳 / nonce 是纯的。
- **文件内容 / 目录 / 元信息**:同样会变,且 `read_file(computed_path)` 让「依赖哪些文件」无法静态枚举,戳穿可审计。需要某文件内容就让 host 读好、作为 input 喂进来(host 知道读了什么、可审计、可固定)。
- **环境变量**:同理,host 读好喂进来。

共同点:**「取」这个动作属于 host,「算」这个动作属于 Relon。** 把取和算分开,Relon 就是纯的。

> 之前沿 P-fs 线把这些做成语言级 builtin(clock/random/read_file/read_dir/stat,各后端 lower + CheckCap 门 + 四方/三方 bit-equal),是**方向性错误**:把本该是 input 的东西做成了语言向外伸手的能力。**撤销。**

---

## 4. 那 6 个 CapabilityBit / `#native` 还有什么用?

`enum CapabilityBit`(ReadsFs/WritesFs/Network/ReadsClock/ReadsEnv/UsesRng)与 `NativeFnGate` **保留**,但归属收窄到一处:**只治理 host 注册的 `#native` fn**。

- host 若**选择**给某次嵌入暴露一个 effectful 原生函数(它自己的决定、自己担责),用 `#native` + 声明 gate。这是显式逃生舱,host 拥有它、审计它。
- **语言自带的 builtin 一个 effectful 的都没有。** capability 门是给「host 主动暴露的 `#native`」用的,不是给语言 builtin 用的。

信任模型文档 §9.2 已铺好的 host-fn registry + gate-driven dispatch 正是为此服务。

---

## 5. 后果(要做的事)

**撤销**以下语言级 effectful builtin 及其支撑(回退已落地、已推送的实现):

- `clock()` / `random()`(门 ReadsClock / UsesRng)
- `read_file()` / `read_dir()` / `stat()`(门 ReadsFs)
- 对应 `Op::*`、lowering recognizer、stdlib 签名、cranelift `VtableSlot`(RelonReadFile/RelonReadDir/RelonStat 等,COUNT 回退)、llvm `wasi_cap.rs` 的 WASI 降级、wasm preview1 import、`relon-util` 的 fs sandbox、四方/三方测试、`docs/*/guide/stdlib.md` 的「能力门控 builtin」节。

**保留**(与 effectful builtin 无关、本身是纯基础设施或属另一类):

- `enum CapabilityBit`、`NativeFnGate`、`CheckCap` / `CallNative`、`#native` 机制(§4:治理 host 注册的 effectful fn)。
- `#import` / resolver(§2:编译期程序构造)。
- **`List<String>` top-level 返回的 codegen 重定位**(ListString-return fix):它是**纯**返回机制,任何返回 `List<String>` 的纯 `#main` 都用得上,与 read_dir builtin 解耦,**留**。

**构造侧(input 通道)**:effectful 值改由 host 作为 input 喂入。需要核实 / 可能补齐 host→求值的输入绑定通道(entry args 已有;若要喂结构化数据 / 文件内容 blob,确认现有 external-input 通道是否够用,不够再设计——属 host 侧接线,非语言 builtin)。

---

## 6. 一句话心智模型

**取数据是 host 的事,算数据是 Relon 的事。** Relon 永远只看见已经定好的 input,所以它永远是纯函数。
