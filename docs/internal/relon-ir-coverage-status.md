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
- 类别 A 剩余:**spread**、**VariantCtor**(+ 上述 references 的 cap 子集、decorator 的 cap 子集、R10b 严格 ref analyzer)。

## 未展开工作(诚实记录,分三类)

### 类别 A:解释器有、relon-IR 编译路未 lower(= 真"尚未在 relon-ir 实现",auto 回退解释器)
| 构造 | 解释器证据 | 编译路现状 | 备注 |
|---|---|---|---|
| references `&root/&sibling/&prev/&next` | `eval.rs` `Expr::Reference`(~755);pricing.relon 用 16 处 | cranelift "unsupported expression" | **最高价值**(配置高频),设计评估点名"最难"。攻前应先派只读 agent 核**静态可解性**:结构在 lowering 期已知 → 多少能解成直接字段访问 vs 必须运行期(可能部分 cap) |
| spread `...x` | `eval.rs` `Expr::Spread`(~681) | cranelift CAP | dict/list 展开;需确认静态形状 |
| VariantCtor(构造 enum 变体) | `eval.rs` `Expr::VariantCtor`(~956) | 未 lower | 运行期值 = 带 brand 的 `Dict`(无独立 Variant Value),可搭现有 Dict-return + match(R5) |
| decorator `@foo(...)` | tree-walk(pricing 用 `@currency`) | cranelift CAP | desugar 成调用(`@f("x") k: v` ≡ `f("x", v)`);静态可解时应可 lower |

### 类别 B:relon-IR + cranelift 有、但 LLVM-native/wasm 后端 codegen 崩(= 不是"未实现",是后端缺一段)
| 构造 | cranelift | llvm/wasm | 缺口 |
|---|---|---|---|
| Unicode-seam string ops:upper/title/trim/nfd | OK(实测 `s.upper()` cr=OK) | 段错 / 栈溢出 | LLVM/wasm 上的 UTF-8 解码 + `is_whitespace`/case-fold codegen seam(见 `relon-codegen-llvm/tests/phase0b_unicode.rs`)。补它 = 给两后端补 Unicode 解码,非"在 relon-ir 实现" |

### 类别 C:不是运行期"值"构造,先核清再决定要不要
运行期 `Value` 仅 12 变体(Null/Bool/Int/Float/String/List/Dict/Closure/Schema/EnumSchema/Type/Wildcard)——**无 Tuple/Result/Option/Variant 值**(实测 0 处)。故:
- **tuple**:无 `Expr::Tuple`、无 `Value::Tuple`。relon 很可能没有元组值(list/dict 已够)。= "要不要加这个语言特性"的问题,不是"实现"问题。
- **Result / Option**:类型层/sugar,无独立运行期 Value。一个 Result/Option 值落地多半是带 brand 的 Dict 或 EnumSchema 实例 → 归 VariantCtor(类别 A)。
- **enum**:类型 = `Enum<...>` / `EnumSchema`;变体值 = 带 brand 的 Dict;构造 = VariantCtor(A);匹配 = match(R5 静态片已做)。

### 其它已知 Capped(按设计或既存 gap,账本已记)
- **Dict 入参/返回/字面量**:按既定设计 cap(analyzer 走 schema 不走 Dict)——**正确边界,非缺陷**,别 naively 重试。
- **`List<String>` 返回的 wasm 验证**:`aot_wasm_parity` 缺 List<String> in-place 解码器,故 R3c 的 String 结果列表是**三方**(tree-walk==cranelift==llvm-native),wasm 腿未验(codegen 发同样哨兵,是测试解码器缺)。补它 = 写 wasm 侧 verifier+reader over scratch。
- **`#relaxed` 真无类型位 / 运行期 brand 分发 match / 动态 `type()`**:动态残留。静态优先决策下**倾向诚实 cap**(非默认 escape hatch);若要编译需窄动态机制(tag+payload 盒子),代价见值模型记录。
- **no-match `match` 的 `TypeMismatch` trap**:跨后端无对应 trap kind,需新加;未强上(R5 留 Capped)。
- regex `matches`、net-parse `is_ipv4/6`、UTF-8-seam `is_email/is_uri`:同类别 B 的 seam / 外部引擎,llvm/wasm 不可移植或需引擎。

## 红线(任何后续波不变)
真 codegen 非嵌入解释器;逐字节四方 bit-equal(tree-walk oracle);免分配/单态化是优化、闭式替换=算法替换红线禁;证不了就 cap 记账不硬上;verifier 必过再 decode。编码交 subagent、主线只裁决 green-gate。
