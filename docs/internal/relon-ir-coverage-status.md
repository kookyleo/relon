# relon-IR 编译覆盖状态 / 未展开工作记录

> 值模型 = 向 Rust 靠拢、尽可能静态(实测修订,推翻早先的 C/Dyn 盒子地基)。
> 本文是人读记录,与机器化的覆盖账本互补:
> - cap-site 注册表 `crates/relon-ir/src/lowering/cap.rs::LOWERING_CAP_IDS`
> - 覆盖账本 `crates/relon-test-harness/src/ledger.rs`(`LEDGER` 拒绝点 + `SUPPORTED_SURFACE` 已覆盖面)
> - no-fallback 证明 `crates/relon-test-harness/tests/no_fallback_supported.rs`
> 截至 main `861d9f83`(2026-06-07)。

## 已覆盖(编译四方 bit-equal,no-fallback 门已绿)
`SUPPORTED_SURFACE`(30 构造):算术/比较/三元/where、标量 stdlib、上下文类型推断后的闭包(经 HOF)、comprehension、pipe、f-string、range/map/filter/reduce(`List<Int>`/`List<Float>` + 跨类型数值 map + String 结果 map)、`type()` 常量折叠、严格 match 静态选臂、标量 math(abs/floor/ceil/round 半偶/sqrt)、标量 string(len/ends_with/replace)、is_uuid、schema-rooted dict 返回。
`no_fallback_over_supported_surface` 用 `AutoEvaluator::last_dispatch_route()` 钩子断言:每个 supported case 走编译路(Aot),绝不静默回退(trivial-scalar-main perf 路豁免、改断值相等)。`RELON_AUTO_FALLBACK_PANIC=1` 调试守卫令回退响亮。

## 进展更新(2026-06-07,loop 驱动)
- ✅ **R10 `a57bb7e2`**:类别 A 的 **references `&sibling.<name>` + 入口级 `&root.<name>`(向后静态字段依赖)** 已编译四方(reuse 源序 field-let 图 → `LetGet`)。仍 cap:forward ref、dynamic key、multi-segment、`&uncle/&prev/&next/&this/&index`、`#internal` 隐私歧义。注:目前 references-in-dict-field 是 `#relaxed`(严格模式 analyzer 不推 reference 类型,`diagnostic.rs:462`,需类似 R1 的 analyzer 扩展=潜在后续 R10b)。GENERATOR_VERSION→14(scalar 字段改走 LetSet/LetGet,动了既有 anon-dict-return 字节)。
- ✅ **R11 `e9f2d7c3`**:类别 A 的 **field decorator**(`@deco(v, args)` 值优先,实测自 evaluator 非文档注释;stacked 自底向上)已编译四方(scalar-Int)。**顺带修了一个静默 miscompile**(decorated 字段原先 lower 时无视 decorator → cranelift 返原值)。仍 cap:builtin `@`-decorator、named arg、multi-segment、String 结果(closure-field 参数类型信封)。
- ✅ **R10b `465b0065`**:严格模式 references 类型推断(analyzer 从目标字段静态类型推 `&sibling.x`/`&root.x`,backward/单段守卫;`#relaxed` 不再是前提)。`&sibling.x` 现严格模式四方编译。GENERATOR_VERSION 不变(analyzer-only)。
- **R12 spread `ddf4ee8f`**:全 cap(诚实)——当前 list/tuple 已分开：`[]` 是同构 `List<T>`，`()` 是定长 tuple。list spread 的剩余问题在 IR 转换层没有完整 list-producing expression 路径；dict spread 仍撞 Dict-by-design。记 TW_ONLY，非 lowering cap site（故不进 LOWERING_CAP_IDS，双射不破）。
- **类别 A 实质收尾**:references(R10/R10b)✓ · decorator(R11)✓ · spread = analyzer/类型系统挡(需决策)· VariantCtor = 大概率 Dict-by-design 挡(变体值=branded Dict)。
- **剩余皆需决策或大改(已到 surface 点)**:① spread/list-producing expression 的 IR 转换范围 ② Unicode-seam string ops(upper/title/trim/nfd)移植 LLVM/wasm UTF-8 解码路径（后续已完成，见 R14）③ List<String> 返回的 wasm in-place 解码器（后续已完成，见 R13）④ Dict 支持(当前 by-design cap,要不要改)⑤ #relaxed 真动态 / &prev/&next / regex / net-parse(静态优先下倾向诚实 cap)。

- ✅ **R13 `371fa947`**:List<String> 就地返回的 **wasm 腿四方证实**(发现生产路 `wasm_buffer_decode` 本就走共享 verifier+reader,原"gap"只是测试 harness 窄;补了真四方 parity)。R3c 的 String-结果 map 现真四方。GENERATOR_VERSION 不变。
- ✅ **R14 `5574b1bc`**:**Unicode-seam 移植 LLVM/wasm**——upper/lower/title/nfd 从两方升到**四方**。根因:`*TableAddr` 在每个 codepoint 循环里把整张表 memcpy 进 scratch→arena 溢出 SEGV/overflow;改成像 cranelift ConstPool 那样把表放 const-data 前缀一次性(`*TableAddr`=固定偏移);并修 `emit_if` 缺 label frame 的 Br 深度 bug。共用 `relon-unicode` 表,逐字节四方。(大输入 cranelift 因既存 cross-region 返回 scratch 窗口限制排除在 large 探针外=三方,small 全四方;trim 非注册 stdlib 方法、不在范围。)GENERATOR_VERSION 不变(只动 llvm 后端)。
- **用户明确 greenlight 的两项工程已完成**(List<String> wasm 解码器 + Unicode-seam 移植)。
- **剩余仍需决策/受设计边界限**:dict.keys(撞 Dict-by-design)·spread/list-producing expression 的 IR 转换范围·Dict 支持(by-design)·#relaxed 真动态/&prev/&next/regex/net-parse(静态优先诚实 cap)。

- ✅ **R15 `79fa4e62`**:`str.split` → 变长 `List<String>` 四方(两遍:数段→发子串记录+偏移槽,复用 R8 子串匹配器,无新 Op,就地返回)。匹配 Rust `str::split`。cap:空分隔符(oracle 报错非值)+ 不可证非空的运行期分隔符。无 GENERATOR_VERSION bump。
- **design-free 工程件已基本做尽**。**剩余=两个真决策 + 设计边界内的诚实 cap**(下方"决策点")。

## 决策点(需用户拍板,loop 已停在此)
1. **spread/list-producing expression**:当前 `[]` 与 `()` 已分开；要继续编译 spread，需要补 IR 中 list-producing expression 的构造与转发路径。要不要动?
2. **Dict 编译值支持**:VariantCtor(变体值=branded Dict)、dict.keys/values/merge、dict 字面量——全部 gated 在"Dict 作编译值"上,而这当前是**by-design cap**(analyzer 走 schema 不走 Dict)。要不要逆转这条边界、建 Dict 编译支持(大工程)?还是维持 schema-only、Dict 永久 cap?
3. 或:**宣告静态覆盖收尾**——no-fallback 门已绿,剩余皆设计边界内诚实 cap(#relaxed 真动态 / &prev/&next / regex / net-parse / Dict-by-design / spread-as-Tuple)。

## 未展开工作(诚实记录,分三类)

### 类别 A:解释器有、relon-IR 编译路未 lower(= 真"尚未在 relon-ir 实现",auto 回退解释器)
| 构造 | 解释器证据 | 编译路现状 | 备注 |
|---|---|---|---|
| references `&root/&sibling/&prev/&next` | `eval.rs` `Expr::Reference`(~755);pricing.relon 用 16 处 | cranelift "unsupported expression" | **最高价值**(配置高频),设计评估点名"最难"。攻前应先派只读 agent 核**静态可解性**:结构在 lowering 期已知 → 多少能解成直接字段访问 vs 必须运行期(可能部分 cap) |
| spread `...x` | `eval.rs` `Expr::Spread`(~681) | cranelift CAP | dict/list 展开;需确认静态形状 |
| enum variant 构造/返回/参数 | `Expr::VariantCtor` / `BuildVariantRecord` | 基础路径已接通；`List<Enum>` 参数原样返回、源码 `List<Enum>` 字面量返回、`map`/`filter`/comprehension 产出 `List<Enum>` / `List<Option>` / `List<Result>`、匿名 `Dict` 字段转发和源码字面量字段、参数 `match` 分派、payload 字段/索引访问、tuple/struct payload pattern 解构、泛型 custom enum，以及 tuple/list/Option/Result 嵌套 payload 已覆盖 | 剩余缺口主要是 spread、更多 list-producing source 形状和动态/no-match trap 场景 |
| decorator `@foo(...)` | tree-walk(pricing 用 `@currency`) | cranelift CAP | desugar 成调用(`@f("x") k: v` ≡ `f("x", v)`);静态可解时应可 lower |

### 类别 B:relon-IR + cranelift 有、但 LLVM-native/wasm 后端 codegen 崩(= 不是"未实现",是后端缺一段)
| 构造 | cranelift | llvm/wasm | 缺口 |
|---|---|---|---|
| Unicode-seam string ops:upper/title/trim/nfd | OK(实测 `s.upper()` cr=OK) | 段错 / 栈溢出 | LLVM/wasm 上的 UTF-8 解码 + `is_whitespace`/case-fold codegen seam(见 `relon-codegen-llvm/tests/phase0b_unicode.rs`)。补它 = 给两后端补 Unicode 解码,非"在 relon-ir 实现" |

### 类别 C:运行期值模型的当前事实
运行期 `Value` 已区分 `List` 与 `Tuple`；没有 `Null` 值。缺失值通过 Rust-like `None` 表达，输出到 JSON 时才投影为 `null`。
- **tuple**:`Expr::Tuple`、tuple schema 和 `Value::Tuple` 已落地；`[]` 是同构 list，`()` 是定长异构 tuple。list/tuple 输出到 JSON 都是 array。
- **Result / Option**:按 Rust-like enum 处理，运行期是带 brand 的 variant dict；编译端已有 binary layout、host buffer、verifier、LLVM/Cranelift native 测试。
- **enum**:公开语法 = Rust-like `#enum`。基础编译链路已支持 variant 构造、返回、参数 identity、`List<Enum>` 参数原样返回、源码 `List<Enum>` 字面量返回、`map`/`filter`/comprehension 产出 `List<Enum>` / `List<Option>` / `List<Result>`、`List<Enum>` 作为匿名 `Dict` 字段转发或源码字面量字段、custom enum 参数 `match` tag 分派、match arm 内 payload 字段/索引访问、tuple/struct payload pattern 解构、泛型 custom enum，以及 tuple/list/Option/Result 嵌套 payload；CLI/WASM typed JSON 输入已支持 payload variant 的 externally-tagged object/array，以及 `Option` / `Result` 的外部标签输入。剩余主要是 spread、更多 list-producing source 形状和动态/no-match trap 场景。

### 其它已知 Capped(按设计或既存 gap,账本已记)
- **Dict 入参/返回/字面量**:按既定设计 cap(analyzer 走 schema 不走 Dict)——**正确边界,非缺陷**,别 naively 重试。
- **`List<String>` 返回的 wasm 验证**:`aot_wasm_parity` 缺 List<String> in-place 解码器,故 R3c 的 String 结果列表是**三方**(tree-walk==cranelift==llvm-native),wasm 腿未验(codegen 发同样哨兵,是测试解码器缺)。补它 = 写 wasm 侧 verifier+reader over scratch。
- **`#relaxed` 真无类型位 / 非 enum 的运行期 brand 分发 match / 动态 `type()`**:动态残留。静态优先决策下**倾向诚实 cap**(非默认 escape hatch);若要编译需窄动态机制(tag+payload 盒子),代价见值模型记录。
- **no-match `match` 的 `TypeMismatch` trap**:跨后端无对应 trap kind,需新加;未强上(R5 留 Capped)。
- regex `matches`、net-parse `is_ipv4/6`、UTF-8-seam `is_email/is_uri`:同类别 B 的 seam / 外部引擎,llvm/wasm 不可移植或需引擎。

## 红线(任何后续波不变)
真 codegen 非嵌入解释器;逐字节四方 bit-equal(tree-walk oracle);免分配/单态化是优化、闭式替换=算法替换红线禁;证不了就 cap 记账不硬上;verifier 必过再 decode。编码交 subagent、主线只裁决 green-gate。
