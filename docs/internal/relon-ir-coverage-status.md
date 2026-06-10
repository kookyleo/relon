# relon-IR 编译覆盖状态 / 未展开工作记录

> 值模型 = 向 Rust 靠拢、尽可能静态(实测修订,推翻早先的 C/Dyn 盒子地基)。
> 本文是人读记录,与机器化的覆盖账本互补:
> - cap-site 注册表 `crates/relon-ir/src/lowering/cap.rs::LOWERING_CAP_IDS`
> - 覆盖账本 `crates/relon-test-harness/src/ledger.rs`(`LEDGER` 拒绝点 + `SUPPORTED_SURFACE` 已覆盖面)
> - no-fallback 证明 `crates/relon-test-harness/tests/no_fallback_supported.rs`
> 截至 main `790d25da`(2026-06-09)。

## 已覆盖(编译四方 bit-equal,no-fallback 门已绿)
`SUPPORTED_SURFACE`(67 构造,全 `Status::Covered`):算术/比较/三元/where、算术 trap parity、标量 stdlib(abs/min/max/length/is_empty)、上下文类型推断后的闭包(经 HOF)、comprehension(`List<Int>`/`List<Float>`/`List<String>`-map + 元素变型 map)、pipe、f-string、range/map/filter/reduce(`List<Int>`/`List<Float>` + 跨类型数值 map + String 结果 map)、`type()` 常量折叠、严格 match 静态选臂 + no-match trap(`TrapKind::NoMatch`→TypeMismatch 四方)、标量 Float math(abs/floor/ceil/round 半偶/sqrt + Int-widen)、标量 string(len/ends_with/replace)、str concat 折叠(StrConcatN)、is_uuid、JSON-Schema 谓词(multiple_of Int 形 / in_range / size_in_range List 形)、字符串件(trim/trim_start/trim_end / is_email / is_uri / split→List<String>)、schema-rooted dict 返回、tuple(标量 return / `.N` 位置访问 / 参数投影)、backward `&sibling`/`&root` 标量字段引用(R10/R10b 严格)、字段 decorator desugar(+纯-String 结果体)、spread(list 字面量源 / 单运行期列表源 / dict schema 源)、**值→String 统一分发**(f-string 插值与 `String + 非String` 拼接共用一个按类型分发器:String 恒等 / Int 走 IntToStr / Bool 选 "true"/"false" 常量 / **Float 走 `Op::FloatToStr` 共享 Display shim**,复合类型响亮 cap)、Float 值 decorator 拼接旗舰(`@currency("USD") display: price` 四方 `"USD 567.34"`)、**stdlib 尾巴 ST 波**(pow / count / every / some / unique,见进展更新)。
> enum/variant(Option/Result/custom enum)的编译链路是真四方,但用专门的 codegen 测试(`relon-codegen-llvm` 系列)证而非进 `SUPPORTED_SURFACE` 表,故不计入上述 67 构造数,见下「类别 C」。
`no_fallback_over_supported_surface` 用 `AutoEvaluator::last_dispatch_route()` 钩子断言:每个 supported case 走编译路(Aot),绝不静默回退(trivial-scalar-main perf 路豁免、改断值相等)。`RELON_AUTO_FALLBACK_PANIC=1` 调试守卫令回退响亮。

## 进展更新(2026-06-07,loop 驱动)
- ✅ **R10 `a57bb7e2`**:类别 A 的 **references `&sibling.<name>` + 入口级 `&root.<name>`(向后静态字段依赖)** 已编译四方(reuse 源序 field-let 图 → `LetGet`)。仍 cap:forward ref、dynamic key、multi-segment、`&uncle/&prev/&next/&this/&index`、`#internal` 隐私歧义。注:目前 references-in-dict-field 是 `#relaxed`(严格模式 analyzer 不推 reference 类型,`diagnostic.rs:462`,需类似 R1 的 analyzer 扩展=潜在后续 R10b)。GENERATOR_VERSION→14(scalar 字段改走 LetSet/LetGet,动了既有 anon-dict-return 字节)。
- ✅ **R11 `e9f2d7c3`**:类别 A 的 **field decorator**(`@deco(v, args)` 值优先,实测自 evaluator 非文档注释;stacked 自底向上)已编译四方(scalar-Int)。**顺带修了一个静默 miscompile**(decorated 字段原先 lower 时无视 decorator → cranelift 返原值)。仍 cap:builtin `@`-decorator、named arg、multi-segment、String 结果(closure-field 参数类型信封)。
- ✅ **R10b `465b0065`**:严格模式 references 类型推断(analyzer 从目标字段静态类型推 `&sibling.x`/`&root.x`,backward/单段守卫;`#relaxed` 不再是前提)。`&sibling.x` 现严格模式四方编译。GENERATOR_VERSION 不变(analyzer-only)。
- **R12 spread `ddf4ee8f`**:全 cap(诚实)——当前 list/tuple 已分开：`[]` 是同构 `List<T>`，`()` 是定长 tuple。list spread 的剩余问题在 IR 转换层没有完整 list-producing expression 路径；dict spread 仍撞 Dict-by-design。记 TW_ONLY，非 lowering cap site（故不进 LOWERING_CAP_IDS，双射不破）。
- **类别 A 实质收尾**:references(R10/R10b)✓ · decorator(R11)✓ · spread = analyzer/类型系统挡(需决策)· VariantCtor = 大概率 Dict-by-design 挡(变体值=branded Dict)。
- **剩余皆需决策或大改(已到 surface 点)**:① spread/list-producing expression 的 IR 转换范围(R12 已编 list 字面量源 + dict schema 源两形,见 R12 条)② Unicode-seam string ops(upper/title/trim/nfd)移植 LLVM/wasm UTF-8 解码路径——**已完成,见 R14** ③ List<String> 返回的 wasm in-place 解码器——**已完成,见 R13** ④ Dict 支持(当前 by-design cap,要不要改)⑤ #relaxed 真动态 / &prev/&next / regex / net-parse(静态优先下倾向诚实 cap)。

- ✅ **R13 `371fa947`**:List<String> 就地返回的 **wasm 腿四方证实**(发现生产路 `wasm_buffer_decode` 本就走共享 verifier+reader,原"gap"只是测试 harness 窄;补了真四方 parity)。R3c 的 String-结果 map 现真四方。GENERATOR_VERSION 不变。
- ✅ **R14 `5574b1bc`**:**Unicode-seam 移植 LLVM/wasm**——upper/lower/title/nfd 从两方升到**四方**。根因:`*TableAddr` 在每个 codepoint 循环里把整张表 memcpy 进 scratch→arena 溢出 SEGV/overflow;改成像 cranelift ConstPool 那样把表放 const-data 前缀一次性(`*TableAddr`=固定偏移);并修 `emit_if` 缺 label frame 的 Br 深度 bug。共用 `relon-unicode` 表,逐字节四方。(大输入 cranelift 因既存 cross-region 返回 scratch 窗口限制排除在 large 探针外=三方,small 全四方;trim 非注册 stdlib 方法、不在范围。)GENERATOR_VERSION 不变(只动 llvm 后端)。
- **用户明确 greenlight 的两项工程已完成**(List<String> wasm 解码器 + Unicode-seam 移植)。
- **剩余仍需决策/受设计边界限**:dict.keys(撞 Dict-by-design)·spread/list-producing expression 的 IR 转换范围·Dict 支持(by-design)·#relaxed 真动态/&prev/&next/regex/net-parse(静态优先诚实 cap)。

- ✅ **R15 `79fa4e62`**:`str.split` → 变长 `List<String>` 四方(两遍:数段→发子串记录+偏移槽,复用 R8 子串匹配器,无新 Op,就地返回)。匹配 Rust `str::split`。cap:空分隔符(oracle 报错非值)+ 不可证非空的运行期分隔符。无 GENERATOR_VERSION bump。

## 进展更新(2026-06-09,自 `861d9f83` 以来落地)
- ✅ **tuple(T2/T3)**:`(...)` 语法 + `.N` 位置访问 + 标量 tuple 四方返回 + tuple 参数位置访问/投影。`[]` 同质 list 与 `()` 定长 tuple 已彻底分开。
- ✅ **enum/variant**:Option/Result/custom enum(带标签记录)的构造/返回/参数 identity、variant 模式匹配、match `_` 通配(原 `*`;schema 验证器仍用 `*`)、no-match trap(`TrapKind::NoMatch`→TypeMismatch 四方)已落地;砍掉了动态 brand-dispatch match(强制 `#enum`);`List<Option/Result/Enum>` 字段来源(匿名 Dict 转发 / 源码字面量 / map/filter/comprehension 产出)四方。
- ✅ **`T?` 已移除** → 一律 `Option<T>`(值模型向 Rust 靠拢)。
- ✅ **R14 Unicode 四方完成**:upper/lower/title/nfd 已 tw==cranelift==llvm-native==wasm 字节相等(原文档「②Unicode-seam 待移植」「③List<String> wasm 解码」两条已不再是待办,见下方 R13/R14 详条)。
- ✅ **P2 标准库(两波)全部四方**:
  - 数值件:`multiple_of`(Int 形)、`in_range`、`size_in_range`(List 形)。诚实 cap:`multiple_of` Float 形(`Op::Mod(F64)` cranelift/wasm 无原生取余)、`size_in_range` String 形(数 Unicode 码点,撞解码缝——cap 实际可解,当前未编 body)。两者均响亮回退,不静默编错值。
  - 字符串件:`trim`/`trim_start`/`trim_end`(复用 R14 解码缝 + `__is_whitespace` + memcpy)、`is_email`/`is_uri`(纯字节扫描)——全部四方,零 cap。
- ✅ **comprehension 元素类型(P3-b `099d55dd`)**:`List<Int>`/`List<Float>`/`List<String>`-map 四方,含元素变型 map(Int→Float/String、Float→Int/String,按闭包返回类型探测 `list_*_map_to_*` wrapper)。诚实 cap:`List<String>` filter(无四方 String→Bool 谓词 body)、`List<Bool>` 源(无注册 wrapper)。
- ✅ **forward reference(`d700bd3b`)**:`&sibling.<later>` / `&root.<later>`(引用源序更晚的兄弟/根标量字段)已四方——field-let 图改按引用边拓扑序发射(纯 backward 输入字节不变、GENERATOR_VERSION 未 bump);成环对齐 oracle `CircularReference` 响亮报错。诚实 cap:穿 `#main` 参数的 forward(oracle 丢 param scope 报 `variable_not_found`、编译路能算值→为避静默分歧响亮 cap)。
- ✅ **`is_iso_date`(`05a3bb40`)**:已四方编译——纯字节形状 + 整数日期算术(闰年 `year%4/%100/%400` 用 `Op::Mod(I32)`),无 UTF-8 seam,与 oracle 逐字节匹配(闰年边界 2024/2023/1900/2000 全对)。先前「body 未写」已补上。`parse_iso_date`(返回 Dict)仍按 Dict-by-design cap。

- ✅ **值→String 统一分发 Wave A(`ebe82085`)**:lowering 新增 `lower_value_to_string(ir_ty)` 单点分发器,f-string 插值与 `String + 非String` 拼接两个消费方统一接入——String 恒等、I64 走既有 `Op::IntToStr`(字节不变,GENERATOR_VERSION 未 bump)、**Bool 编译落地**(`Op::If` 选 "true"/"false" 常量串,无新 op),F64 与复合类型响亮 cap。配套 analyzer 修正:oracle 本就接受 `String + 任意`/`任意 + String` 为拼接(arithmetic.rs 双侧 arm),analyzer 此前把 `String + Bool` 静态拒掉属不自洽,现对齐。aot_wasm_parity 111(fstring_bool / string_plus_bool / bool_plus_string 四方)。**Wave B 已落地(`4e5cf3eb`)**:见下条。
- ✅ **Float→String 渲染 Wave B(`4e5cf3eb`)**:新 `Op::FloatToStr`,三后端共用同一 Rust leaf(`relon-ir::float_str::format_f64_display` = oracle 同一条 `format!("{v}")` Display 路径,构造性字节一致):cranelift 走 vtable 槽 5(COUNT 5→6,**GENERATOR_VERSION "v5-gamma 14"→"15"**,768B scratch);LLVM-native 走 `add_global_mapping` extern(rs-shims staticlib 自含副本);wasm 走 `(import "env" "relon_llvm_f64_to_str")` + parity linker `func_wrap` 同一函数。f64 跨 FFI 以 i64 位模式传递,无浮点 ABI 假设。边界电池四方:`1.0→"1"`、`-0.0→"-0"`、NaN/±inf、subnormal -5e-324(327 字符)、1e300。**旗舰封口**:pricing `@currency("USD") display: price` 四方 `"USD 567.34"`(concat-coercible 仅限推断 String 参数;显式 `(String s)` 标注仍拒标量实参,守卫测试在)。复合类型值→String 仍响亮 cap(Display 语义未定义)。
- ✅ **stdlib 尾巴 ST 波(`6fb7aed8`)**:pow / count / every / some / unique 四方编译,ledger +5(`st_pow_float`/`st_count_list`/`st_every_true`/`st_some_true`/`st_unique_dup`),bundled body 71→78,**GENERATOR_VERSION "v5-gamma 15"→"16"**(新 pow import + body 索引变更使旧缓存对象自失效)。各函数:**pow** 走新 `Op::F64Pow`(cranelift JIT shim / ELF libm libcall、LLVM `llvm.pow.f64`、wasm `env` import),Int 操作数 widen,IEEE-754 不 trap,cap=非数值操作数(不复现 oracle 的静默 `0.0`);**count** 走 record-header 长度读 peephole,任意元素类型,cap=count-empty 的 FastInt 入口形状在 wasm object emit 是**响亮编译期拒绝**(`AllocScratch outside buffer-protocol entry shape`,非 miscompile),已经 Buffer 等价源(`count([1, 2].filter(...))`)验证同语义、ledger 如实记账;**every/some** 为 `List<Int>`/`List<Float>` 短路谓词循环(短路停在会 trap 的谓词之前,空表 every=true/some=false);**unique** O(N²) i<j 扫描,F64 等值=OrderedFloat(NaN==NaN、-0.0==0.0 均判重)。仍 cap:`List<String>`/`List<Bool>`/指针数组的 every/some/unique。
- ✅ **多源运行期 spread SP 波(`abf2bc53`,merge `aaa7e8c3`)**:`[a, ...xs, b, ...ys, c]` 多运行期源四方编译——`classify_runtime_spread` 泛化为段序列(静态标量游程/运行时源交替,源个数编译期固定),直线代码无循环:逐源装载 base/count、total=静态个数+Σ源长度、一次 `AllocScratchDyn` 分配,标量段 cursor+静态偏移直写、源段 `MemcpyAtAbsolute` 整段拷贝。范围 `List<Int>`/`List<Float>`;cap:混型源 `mixed_source_ty`、非标量字面量元素(收窄后的 `unsupported_spread_source`)。SUPPORTED_SURFACE 新增 SP 行,corpus 10 case(空源/相邻/NaN/-0.0)+四方 5+wasm parity 4。**顺带修复既有 let-window bug(`9b21399d`)**:两后端 stdlib 内联窗口原只取「已声明 let 槽 max+1」,调用点之后才首次绑定的 caller let 会撞进 callee 窗口(LLVM 响亮 `let-slot aliased`,cranelift 有静默宽度截断风险,基线可复现非新引入);修复=新增 `body_let_watermark` 递归扫描函数体静态水位,窗口下限取 max(已声明+1, 水位)。cranelift **GENERATOR_VERSION "v5-gamma 16"→"17"**(旧窗口方案可能缓存过截断对象,bump 自失效)。
- **design-free 工程件已基本做尽**。**剩余=设计边界内的诚实 cap**(决策点已全部裁决,见下)。

## 决策点(已全部裁决,2026-06-10)
1. **spread/list-producing expression**:✅ 已做——list 字面量源 + dict schema 源(R12)+ 单运行期列表源(`e8787c29`)+ **多运行期源(`abf2bc53`,SP 波,2026-06-11)**。spread 轴收口。
2. **Dict 编译值支持**:❌ **维持 by-design cap 不立项**——真实语料匿名 Dict 作值=1、dict 操作调用=0;且大半早已建好(anon-Dict-return/`#brand Dict`),真剩硬 cap 只「Dict 作 #main 入参」(需求 0)。schema-only 是主路。
3. **Schema/EnumSchema/Type 作一等运行期值**:❌ **维持 cap 不立项**(2026-06-10 勘探)——语料 16 处全 showcase(workflow.relon 状态机 + brand.relon 演示)、真实业务 0;oracle 语义面本就极小(Schema==Schema 恒 false、`#[serde(skip)]` 禁序列化、不可跨函数传、Display 只吐 `<schema>`),它是类型检查的内部载体不是数据值;编译化 100% 耦合已否掉的 Dict 编译值 + closure-across-boundary,成本远超收益(收益≈0)。
4. **静态覆盖宣告收尾**:no-fallback 门已绿,SUPPORTED_SURFACE 67,剩余皆设计边界内诚实 cap(#relaxed 真动态 / &prev/&next 等 references 尾巴 / regex / net-parse / Dict-by-design / 复合类型→String)。stdlib 尾巴的 design-free 部分(pow/count/every/some/unique)已四方(ST 波 `6fb7aed8`);剩余 tree-walk-only stdlib 全部撞既有设计 cap:select_keys/omit_keys/parse_iso_date(返回 Dict=Dict-by-design)、to_json(复合类型→String)、is_ipv4/6 与 regex(引擎/seam)。perf 轴 W16-W19 收尾已固化于 `docs/internal/perf-panel-w-series.md`(2026-06-10 s90 实测:µs 级 W7/W17/W18/W19 与 native Rust ±2% 平价、W16 公平基线后仍 1.41× 快(分配器归因)、全部快于 LuaJIT 2.7×–17×;唯一回退 W12-fast 已修复 `1779a99d`,s90 复测 3.38ns 恢复与 rust 3.40ns 平价)。oracle 内部双源也已消除(`8d292bbf`):min/max/clamp/Int-abs/_list_contains 的 native 双胞退役、单一事实源=.relon body,38 条 NaN/±0.0/Int::MIN 边界金值钉测;`_string_join` 保留 native 并记账(其 Display 强转在 relon 无等价原语,faithful 退役不可行);HOF int 残留体已删(`e28e6cac`,与 typed builder I64 实例化字节恒等,常驻回归测试防分叉,audit ★ 跨家族 mega-模板裁决 NO-GO)。

## 未展开工作(诚实记录,分三类)

### 类别 A:解释器有、relon-IR 编译路未 lower(= 真"尚未在 relon-ir 实现",auto 回退解释器)
| 构造 | 解释器证据 | 编译路现状 | 备注 |
|---|---|---|---|
| references `&root/&sibling/&prev/&next` | `eval.rs` `Expr::Reference`(~755);pricing.relon 用 16 处 | **backward + forward `&sibling`/`&root` 标量已四方(R10/R10b 严格 + forward `d700bd3b`)** | 仍 cap:dynamic key、multi-segment、`&uncle/&prev/&next/&this/&index`、穿 #main 参数的 forward(oracle 丢 param scope→诚实 cap)。`&prev/&next/&index` 卡 list-context 循环 codegen、`&uncle`/multi-segment 卡嵌套 Dict 编译 |
| spread `...x` | `eval.rs` `Expr::Spread`(~681) | **已编:list 字面量源(R12)、dict schema 源(R12)、单运行期列表源(`e8787c29`)、多运行期源 `[a, ...xs, b, ...ys, c]`(`abf2bc53` SP 波,段序列+偏移累加,直线代码无循环)** | 仍 cap:混型源(`mixed_source_ty`)、非标量字面量元素、`List<String>`/`List<Schema>` 源、匿名 Dict 源(by-design) |
| enum variant 构造/返回/参数 | `Expr::VariantCtor` / `BuildVariantRecord` | 基础路径已接通；`List<Enum>` 参数原样返回、源码 `List<Enum>` 字面量返回、`map`/`filter`/comprehension 产出 `List<Enum>` / `List<Option>` / `List<Result>`、匿名 `Dict` 字段转发和源码字面量字段、参数 `match` 分派、payload 字段/索引访问、tuple/struct payload pattern 解构、泛型 custom enum，以及 tuple/list/Option/Result 嵌套 payload 已覆盖；no-match trap 已四方(`TrapKind::NoMatch`)；动态 brand-dispatch 已显式拒绝(强制 `#enum`) | 剩余缺口主要是 spread 与更多 list-producing source 形状 |
| decorator `@foo(...)` | tree-walk(pricing 用 `@currency`) | **已编 scalar-Int(R11) + 纯-String 结果(`1d62910d`,closure 字段 ret_ty 从类型系统取=标注优先/保守 body 推断,替掉写死 I64)**;desugar `@deco(args) k: v ≡ deco(v, args)`(value-first),stacked 自底向上 | 仍 cap:builtin `@`-decorator、named arg、multi-segment、branded `-> Schema` 字段;~~含 Float 操作数的 String 拼接~~——**已全封**(Wave A Int/Bool + Wave B `Op::FloatToStr`,pricing `@currency` 旗舰四方 `"USD 567.34"`) |

### 类别 B:relon-IR + cranelift 有、但 LLVM-native/wasm 后端 codegen 崩(= 不是"未实现",是后端缺一段)
> **已清空(R14)**:原列在此的 Unicode-seam string ops(upper/lower/title/nfd)已从两方升到四方——把 Unicode 表放 const-data 前缀一次性(原先每 codepoint 循环 memcpy 整表→arena 溢出 SEGV),并修 `emit_if` 缺 label frame 的 Br 深度 bug。trim/trim_start/trim_end 复用同一解码缝亦已四方(见进展更新)。当前类别 B 无遗留条目。

### 类别 C:运行期值模型的当前事实
运行期 `Value` 已区分 `List` 与 `Tuple`；没有 `Null` 值。缺失值通过 Rust-like `None` 表达，输出到 JSON 时才投影为 `null`。
- **tuple**:`Expr::Tuple`、tuple schema 和 `Value::Tuple` 已落地；`[]` 是同构 list，`()` 是定长异构 tuple。list/tuple 输出到 JSON 都是 array。
- **Result / Option**:按 Rust-like enum 处理，运行期是带 brand 的 variant dict；编译端已有 binary layout、host buffer、verifier、LLVM/Cranelift native 测试。
- **enum**:公开语法 = Rust-like `#enum`。基础编译链路已支持 variant 构造、返回、参数 identity、`List<Enum>` 参数原样返回、源码 `List<Enum>` 字面量返回、`map`/`filter`/comprehension 产出 `List<Enum>` / `List<Option>` / `List<Result>`、`List<Enum>` 作为匿名 `Dict` 字段转发或源码字面量字段、custom enum 参数 `match` tag 分派、match arm 内 payload 字段/索引访问、tuple/struct payload pattern 解构、泛型 custom enum，以及 tuple/list/Option/Result 嵌套 payload；CLI/WASM typed JSON 输入已支持 payload variant 的 externally-tagged object/array，以及 `Option` / `Result` 的外部标签输入；no-match trap 已四方(`TrapKind::NoMatch`→TypeMismatch)，动态 brand-dispatch match 已显式拒绝并强制 `#enum`。剩余主要是 spread、更多 list-producing source 形状。

### 其它已知 Capped(按设计或既存 gap,账本已记)
- **Dict 入参/返回/字面量**:按既定设计 cap(analyzer 走 schema 不走 Dict)——**正确边界,非缺陷**,别 naively 重试。
- **`List<String>` 返回的 wasm 验证**:**已修(R13)** —— `wasm_buffer_decode` 生产路本就走共享 verifier+reader,原"gap"只是测试 harness 窄;补了真四方 parity,R3c 的 String 结果列表现真四方。
- **`#relaxed` 真无类型位 / 非 enum 的运行期 brand 分发 match / 动态 `type()`**:动态残留。静态优先决策下**倾向诚实 cap**(非默认 escape hatch);若要编译需窄动态机制(tag+payload 盒子),代价见值模型记录。动态 brand-dispatch match 已显式拒绝、强制走 `#enum`。
- **no-match `match` 的 `TypeMismatch` trap**:**已完成** —— 新增 `TrapKind::NoMatch`,四方都 surface `RuntimeError::TypeMismatch`(R6,corpus `r5_match_no_arm_traps`)。
- regex `matches`、net-parse `is_ipv4/6`:外部引擎 / seam,llvm/wasm 不可移植或需引擎,诚实 cap。(`is_email`/`is_uri` 已四方,见进展更新;`is_iso_date` 属 body 未写,非 IR 无 op。)

## 红线(任何后续波不变)
真 codegen 非嵌入解释器;逐字节四方 bit-equal(tree-walk oracle);免分配/单态化是优化、闭式替换=算法替换红线禁;证不了就 cap 记账不硬上;verifier 必过再 decode。编码交 subagent、主线只裁决 green-gate。
