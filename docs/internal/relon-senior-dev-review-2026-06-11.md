# Relon 资深开发者视角评审（2026-06-11）

> 立场设定：假设我是一名要把 relon 嵌进生产系统的资深开发者——配置求值、规则引擎、数据校验是典型用途。我先把语言特性逐项核实过一遍（四份核实报告：语法面 / 类型系统面 / 运行期语义与标准库面 / 工具链与文档面，全部以代码为唯一事实源），文档漂移已另行修复合入。本文是剩下的部分：**疑惑、看法、与建议**。客观陈述与主观判断分开标注。

> **处置记录（2026-06-11，同日落地）**：第 1 节 D1–D7 全部修复（merge `b27f3f9e` analyzer 六缺陷 / `b3a10f62` `++` 迁移诊断）；第 2.1 节 sum 三套语义已统一为 checked、第 3 节 List min / ends_with 方法对称已补齐（merge `96898012`，GENERATOR_VERSION → "v5-gamma 18"）。下文保留原始评审措辞作历史记录，现状以代码与 ledger 为准。仍开放：第 2 节 2–6 条（记录在案的刻意口径）、第 4 节工程面观察（trace-JIT 合成 leg 退役、Context pub 字段收口、playground 文档、dlopen 落地）。

## 0. 总体印象

值得先说的好话，因为它们是这门语言的真实卖点：

- **决定性是认真的**。Dict 用 BTreeMap 键序、浮点用 OrderedFloat 总序（NaN==NaN、-0.0==0.0）、整数算术全 checked、同一输入跨四个执行后端逐字节相等并有 no-fallback 测试钉死——这一套在小语言里罕见地完整。
- **沙箱默认全拒**，能力是 6 个显式 bit + 双预算（步数、值元素数），纯函数边界有 purity_guard 测试封死。嵌入方可以放心地跑不可信脚本。
- **「宁可响亮、不可静默」**作为总纪律在大部分地方贯彻得很好：编译覆盖不了的形状是显式 cap 加 ledger 记账，不是静默错值。
- 严格模式默认开、传染到全部可达 import，被导入模块自己声明 #relaxed 无效——这个设计比很多大语言的「严格性按文件自选」更可靠。

下面的批评都建立在这个底子上：**这门语言最大的风险不是设计，而是若干处对自己纪律的违反**。

## 1. 真缺陷候选（违反「响亮」纪律，建议立项修复）

这些不是口味问题，是「文档承诺的检查没发生」或「前后端对同一程序的判断不一致」，每一条都能让用户写出静默错误的代码。

| # | 问题 | 现场 | 为什么严重 |
|---|---|---|---|
| D1 | **含命名实参的调用跳过全部签名校验**：`f(x = 1)` 这种调用，参数个数、类型一概不查，静默放行 | analyzer fn_call.rs:101-103 | 类型检查的整面盲区。用户以为有静态保障，实际命名实参一出现保障就消失，且无任何提示 |
| D2 | **未知 #derive 名静默忽略**：`#derive(Comparble)`（拼错）不报错不警告，见证方法检查直接不发生 | analyzer constraints.rs:340-345 | 拼写错误 = 静默丢失约束。与「响亮」纪律正面冲突，修复成本极低（白名单外报错即可） |
| D3 | **DuplicateMainParam 文档有、实现无**：spec 承诺重复的 #main 参数名报错，全仓无此诊断变体，重名静默接受 | analyzer main_sig.rs:53-99 | 文档承诺的契约未兑现。裁决：补实现（而非删文档），与直觉一致 |
| D4 | **#internal 方法互调保护失效**：`in_method_block` 恒为 false，私有方法保护只对入口直调生效，方法体内互调不触发 PrivateMethodViolation | analyzer fn_call.rs:485-487 | 函数名与行为不符，访问控制有洞 |
| D5 | **#main 返回校验可被 Any 穿透**：体类型推断为 Any 时返回断言静默放行 | analyzer main_return.rs:65-67 | 推断缺口变成校验缺口，且用户面 Any 本是被禁的 |
| D6 | **Tuple 作 comprehension 源：analyzer 放行、evaluator 运行时拒绝** | analyzer mod.rs:580-584 vs evaluator | 前后端对同一程序判断相反——静态检查放过的代码运行时必炸。二选一：evaluator 补支持，或 analyzer 拒绝。倾向后者（Tuple 异构，迭代语义本就含糊） |
| D7 | **`++` 运算符是死表面**：lexer/parser 完整产出 Concat 节点（优先级 50），evaluator 对它统一 trap UnsupportedOperator | parser cst.rs:2605-2617 vs evaluator arithmetic | 「解析了但永远不能用」的语法。要么实现（String/List 拼接语义明确），要么 parser 层直接报「未支持的运算符」给出更好的错误信息。文档已刻意不写它 |

我的排序建议：D2（一行白名单，收益/成本比最高）→ D1（盲区面积最大）→ D6（前后端分歧）→ D3 → D5 → D4 → D7。

## 2. 语义一致性疑虑（已四方一致、但口径自相矛盾或违反直觉）

这些不是 bug——每条都有测试钉住且跨后端一致——但作为使用者我会被它们咬到。

1. **求和有三套溢出语义**。同样是「把 List<Int> 加起来」：方法形 `xs.sum()` 用 wrapping_add（溢出静默回绕，stdlib.rs:2230）；`std/list` 模块的 `sum`（reduce + `+`）溢出响亮 trap NumericOverflow；编译器的 RangeSum 融合按 checked 契约。**这是全语言唯一一处静默回绕**——整个算术面都是 checked，方法形 sum 是孤例。我强烈建议把方法形改成 checked（行为变更，需要 bump 并跑四方），「同名 surface 行为不一」比破坏兼容更伤。
2. **字符串长度双口径**：`len(s)` 数字节，`size_in_range(s, lo, hi)` 数 Unicode 码点。各自有理（len 对齐 Rust 的 `str::len`；size_in_range 是面向校验的「字符数」直觉），但文档此前完全没提醒，用户拿 `len` 写长度校验就会在非 ASCII 输入上错。至少要在 spec 用一个醒目框并排讲清两者。
3. **`min`/`max` 的 NaN 不对称**：`max(NaN, x) = x` 但 `max(x, NaN) = NaN`。这是 `.relon` 单源实现（`if a < b` 分支）的固有结果，注释已明示。Rust 的 `f64::max` 是「忽略 NaN」对称语义。既然值模型已选 OrderedFloat 总序，更自洽的做法是 min/max 也按总序（NaN 排最大），不过这是行为变更，收益不大，记录在案即可。
4. **Float 除零（含 -0.0）trap DivisionByZero**，不是 IEEE 的 ±inf。这是刻意的（校验语言宁可炸），spec 已补例外说明。但注意它与「浮点遵守 IEEE-754」的总句永远有张力，每个新来的人都会问一遍。
5. **`split` 空分隔符报 UnsupportedOperator** 而非 Rust 的空串切分行为。可接受（Rust 的 `"ab".split("")` 产生首尾空串是著名陷阱），spec 已补例外。
6. **`glob_match` 实际是「三方 + wasm 响亮拒」**：wasm 槽位 body 是 trap，cranelift 走 host helper。它被笼统归入四方表会误导，账面应单列。

## 3. 表面不规则（使用者会皱眉，但多数有刻意理由）

- **List 有 `max` 方法没有 `min` 方法**；`count`/`pow` 只有自由函数形；`ends_with` 只有自由函数形而 `starts_with` 两形都有；`replace` 自由函数形不注册。index.rs 注明这是与 oracle 锁步的刻意设计——但「为什么 max 有 min 没有」对用户是无法自行推断的。建议：要么补齐对称（min 方法、ends_with 方法），要么在 stdlib 文档里给一张「形态矩阵」明示哪些有哪些没有。补齐对称是更好的解：这类不对称没有任何语义理由，纯属历史。
- **两套键值分隔符**：命名实参用 `=`（`f(x = 1)`），dict 字段用 `:`（`{x: 1}`）。和 Python 一样的选择，可辩护，但 spec 应明示这是刻意区分（实参绑定 vs 数据构造）。
- **`Infinity`/`NaN` 是标识符特判**，可被同名变量遮蔽。小坑，与 JavaScript 同病。锁成关键字更稳，但破坏性收益比低，记录即可。
- **match 表达力刻意收窄**（无守卫、无嵌套模式、无字面量模式），这是好裁决——但 spec 应显式声明「无守卫」是设计而非缺漏，否则每个 Rust 背景的用户都会当成 bug 报。
- **未知 `#指令` 宽容回落为 Value 形态**：`#shcema`（拼错）parser 层静默通过，靠后续分析才可能报错。与 D2 同根：标注性表面（指令、derive 名）应该是封闭集合，集合外响亮报错。
- **`null` 词法存在**（lower 成 Expr::Missing）而语言口径是「无 null」。parser 吃进来再由 analyzer 处置是合理的迁移诊断策略，但要确认 analyzer 对它的报错信息明确说「relon 没有 null，请用 Option」。
- **变体构造三形三 AST**（unit=Variable、tuple=FnCall、struct=VariantCtor），区分推迟到 analyzer。实现内伤，用户不可见，但它是 D1（命名实参盲区）这类问题滋生的土壤——调用形 AST 的歧义越多，签名检查的旁路越多。

## 4. 工程面观察

- **harness 的 "trace-JIT" leg 是字符串合成器**（three_way.rs:342 按 recipe 名匹配吐预期值），不是执行引擎。作为差分测试的一条腿它验证的只是 recipe 表自己，差分价值为零，且命名会让读代码的人以为 trace-JIT 后端还活着（crate 早已删除）。建议：直接退役该 leg，「四方」的第四腿现实中是 wasm + llvm-native，账面已照此改正。
- **`Context` 的 `module_resolvers`/`analyzed`/`capabilities` 是 pub 字段直暴**，与其余 register_* 方法封装风格不一。嵌入方可以绕过 builder 直接改 capabilities——对一门以沙箱为卖点的语言，这个面最好收成方法并审计调用方。
- **fast_path.rs 平凡标量 #main 双实现**有回退保护，但双实现就是双漂移面，每次语法变更要记得它的存在。
- **dlopen 执行路径标注 Deferred**而对象缓存（含 HMAC cache-key）已就绪——文档已改为如实表述。落地它之前，cranelift-AOT 的「缓存」只省编译不省启动，心智上别记成已完成。
- **playground.md 是 4 行占位**，而 wasm-bindings 已有 hover/rename/code_actions/signature_help 一大批 LSP 能力零文档。视产品意愿，这是现成功能白放着。
- **include_relon! 构建期 AOT 集成**此前零用户文档（已补小节）。这是嵌入方关心的能力，值得在 README 给一句话入口。

## 5. 给嵌入方的视角总结（如果我要选型）

会让我选它的理由：决定性 + 默认全拒沙箱 + 双预算 + 四后端逐字节差分。这四件事组合起来，在「跑不可信配置/规则」这个场景里我找不到现成替代品。

会让我犹豫的理由，按权重：
1. D1（命名实参盲区）——静态保障有整面缺口，在它修掉之前我会在团队规范里禁用命名实参；
2. sum 的 wrapping 孤例——审计数值代码时必须特判这一个方法；
3. len 字节口径——非 ASCII 输入的长度校验必须用 size_in_range，团队规范要写明；
4. stdlib 形态不对称——查文档频率被迫升高。

这四条没有一条是设计级的坏，全部可修，且修复路径都已明确。语言的地基（值模型、纯函数边界、严格传染、编译差分）是我见过的小语言里最扎实的一档。

---

*素材来源：/tmp/relon-survey/ 下四份核实报告（verify-parser / verify-analyzer / verify-evaluator / verify-tooling）+ 文档修正 lane 的两项实测发现（D6、D7）。文档漂移修复已于 2026-06-11 合入（merge cabdf0e8）。*
