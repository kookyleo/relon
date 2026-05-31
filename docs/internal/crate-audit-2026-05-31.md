# 全 crate /review + /simplify 审计 — 2026-05-31

对整个 workspace(27 crate,~180k LOC)逐 crate 做了一轮只读审计(/review + /simplify 方法论),
再把 **safe + high-confidence** 的机械化简化落地,**correctness/security 类只报告不自动改**。

## 概览

- **审计**:27 crate 并行只读,产出 121 条 finding(reuse 22 / efficiency 48 / quality 39 /
  comments 12 / correctness 15 / deadcode 7;risk: safe 105 / moderate 31 / risky 7)。
  `relon-rs-demo` 干净无 finding。
- **已落地**:safe+high 的 ~36 条机械简化,分 5 个 worktree lane 应用 + 对抗审查双 accept,
  每条均 behavior-preserving、per-crate `cargo test` 绿。另修一处审计带出的预存 clippy 缺陷。
- **保守跳过**:约 25 条(cross-crate 重构、需新建共享 crate、或经检查并非真正 safe 的),
  逐条记录原因,留作后续。
- **只报告**:15 条 correctness/security(下方),**未自动改** —— 改动这些有行为风险,需人工判断。

### 已落地的 commit(均 kookyleo,零 AI 痕迹)

| commit | 范围 | 内容摘要 |
|---|---|---|
| `cfec6dd8` | trace-emitter / codegen-wasm / codegen-llvm / wasm-evaluator / rs-build | 去冗余 clone、删死字段/死代码、抽 `anyhow_to_wasmtime` 复用、修陈旧注释 |
| `31eb36c1` | trace-jit / evaluator / eval-api / object-cache / lsp | const_fold 借用代替 per-op clone、`eval_closure` 深拷 AST→`Arc::clone`、`hex::encode` 复用、抽 lsp 诊断 helper |
| `ac5ee609` | trace-recorder / object-link | recorder guard 化简、linker ELF/输出处理化简 |
| `2af35b0a` | parser / bench / cli / rs-macro / wasm-bindings | parser `cast()` 去冗余 clone、bench ctx shadowing 拍平、cli 死 `_caps` 清理等 |
| `028a4789` | fmt / analyzer / codegen-cranelift / test-harness / trace-abi | 抽共享 helper、删死参数、合并重复 borrow、消冗余 alloc |
| `2a791404` | relon | jit.rs 无条件 `Mutex` import 在 `--no-default-features` 下 unused → 改全限定 `std::sync::Mutex`,只导 `Arc`(修复 lean build 的 `clippy -D warnings` 失败) |

> 关键纪律:每个 lane 只应用审计列出的 safe-high finding(不自由发挥),逐条先检查再应用,
> 破坏测试即 revert 并记 skipped;对抗审查逐 hunk 读 diff + 重跑测试确认 behavior-preserving。

---

## correctness / security findings(只报告,待人工 triage)

> 这些**未自动修改**——改动涉及行为/语义判断。按风险排序;前两组是真实 bug 嫌疑,建议优先。

### 🔴 真实 bug 嫌疑(建议优先)

1. **`relon-cli` `--trust` capabilities 对 AOT/cranelift/bytecode 后端从未生效**
   (`crates/relon-cli/src/main.rs:782 / 853 / 872`)—— 三处计算出的 `_caps` 都被丢弃
   (`_` 前缀、从未读取)。tree-walk 路径会装 capabilities,但这三个后端构造后没调
   `install_capabilities_mut()`,所以 `--trust` 标志对它们无效。要么构造后配置 capabilities,
   要么 evaluator API 改成构造时接收。**这是 L4 lane 删 `_caps` 死变量时确认的真实功能缺失,
   不是误报**(删死变量是 cosmetic,但底层 capabilities 未配置是 bug)。

2. **`relon-codegen-cranelift` `sandbox_matches()` 漏比 `trace_jit_fn_id`**
   (`crates/relon-codegen-cranelift/src/evaluator.rs:1627`)—— `SandboxConfig` 有 5 个字段
   (`sandbox.rs:99`),但 `sandbox_matches` 只比 4 个,漏了 `trace_jit_fn_id`,而其注释(1624)
   声称比对「每个 flag 字段」。要么补 `&& a.trace_jit_fn_id == b.trace_jit_fn_id`,要么改注释。
   关系到 object-cache 的 sandbox-drift 失效判定正确性。

### 🟡 unwrap / panic 面(健壮性,多数有上游 guard 但脆弱)

3. **`relon-object-cache` storage.rs:302/326/336** 三处 `.try_into().unwrap()` 假设 slice 恰好 4 字节;
   总长检查在,但单个 slice 长度未在 decode 路径上保证 → 建议改 `map_err(|_| CacheError::Truncated)`。
4. **`relon-codegen-llvm` emitter.rs:3562/3578/5346/5417** `get_insert_block().unwrap()` —— 周围代码
   用 `.map_err()`,这里却 unwrap;建议改 `.ok_or_else(|| LlvmError::Codegen(...))?` 统一错误处理。
5. **`relon-parser` lower.rs:956** `integrity_algo.take().unwrap()` —— guard 只查 `is_some() && saw_colon`,
   状态机顺序异常时会 panic;建议 `.ok()?` 传播。
6. **`relon-bench` bench_stats.rs:128** div-by-zero 赋 `f64::INFINITY` 后 `sort_by(partial_cmp).expect()`
   遇 NaN 会 panic(与「times 有限」的注释矛盾)—— 仅影响 bench 工具,建议过滤非有限值。
7. **`relon-trace-emitter` op_lower.rs:355 / emitter.rs:1452 / 1587** —— LocalGet offset 在 i64→i32
   溢出时静默归 0、DictLookup(`record_len`)在 hint 为 None 时静默归 0;极端输入下可能错算,
   建议显式 error 而非静默默认。
8. 其余低风险 unwrap(`relon-parser` lower.rs:1581/2812、`relon-ir` lowering.rs:2547)经检查实际安全
   (上游已 guard),仅建议改写得更显式;**非 bug**。
9. **`relon-trace-jit` dict_list.rs:858**(测试代码)`from_utf8(key).unwrap()` 于不可信字节 —— 仅测试。

---

## 保守跳过的简化(留作后续)

主要是**跨 crate / 需新建共享 crate**的去重(无法在单 crate behavior-preserving 范围内安全完成):
- `is_remote_url` / URL 检测在 `relon` / `relon-evaluator` / `relon-analyzer` 三处重复 → 宜抽到
  共享 util(如 `relon-eval-api`)。
- `align_up`(`relon-codegen-llvm` ↔ 其他)、`is_valid_rust_ident`(`relon-rs-build` ↔ `relon-rs-macro`)
  去重 → 需共享 crate。
- 若干 wasm/lsp 的多站点 helper 抽取「太 invasive」被跳过。
- 经检查并非真 win 的(如某些 clone 在 success 路径本就只一次)被正确保留。

这些都需要决定共享 util 的归属 crate;不属本轮「单 crate 安全简化」范围,故留下。

---

## correctness 修复(DONE,2026-05-31)— verify-then-fix

对上面 15 项 correctness/security 逐项 verify-then-fix(4 个 worktree lane + 对抗审查全 accept,
每个真修都带「去掉就 fail」的行为证明测试)。**关键:先核实真伪,真 bug 才改,非 bug 诚实保留。**

### 真 bug,已修(4 个 commit,均 kookyleo,零 AI)

| commit | finding | 修复 |
|---|---|---|
| `3e711044` | cli `--trust` 对 **bytecode** 后端失效(#1 的一半) | 经 `BytecodeEvaluator::with_capability_gate(Arc<dyn CapabilityGate>)` 安装(`--trust`→all_granted,否则 zero-trust);2 测试:零信任下 CheckCap 入口被 `WasmCapabilityDenied` 拒、`--trust` 下放行 |
| `527f8c76` | cranelift `sandbox_matches` 漏 `trace_jit_fn_id`(#2) | **确认真 bug**:该字段在 `codegen/mod.rs:598` gate HotCounter prologue→改变 artifact;补 `&& a.trace_jit_fn_id == b.trace_jit_fn_id`,回归测试(去掉即 fail) |
| `b76289bc` | bench `bench_stats` INFINITY 哨兵 NaN sort panic(#6) | **真 bug**(div-by-zero 哨兵污染分布):改非有限过滤,全零样本返回 `Empty` 错误;测试零迭代不 panic + 全零报空 |
| `5d05e23b` | trace-emitter LocalGet slot offset 溢出静默归 0(#7a) | **真 silent 误编译**:slot≥2²⁸ 时 `i32::try_from(off).unwrap_or(0)` 静默 load slot-0;改 checked 转换→`Malformed` 错误(两条 emit 路径),4 测试(溢出拒 + 边界 in-range 通过) |

### 核实后判定为「非 bug」,诚实保留(未强改)

- **cli `--trust` 对 cranelift-aot 后端**(#1 另一半):cranelift 用 host-fn **存在性**(`CapabilityVtable` cap_bit→HostFnPtr)gate `#native`;唯一 install 路径需**已填充**的 vtable(grant==注册 host fn)。CLI 不带 host-fn registry,且其路由到 cranelift 的标量 `#main` 源 lower 后**无 CallNative/CheckCap op**——装个 all_granted-but-empty vtable 改变不了任何行为=**假修**。删了死计算 + 修了 Auto 路径的误导注释;真正支持需 host-fn registry + gate-driven vtable builder(跨 crate API,留作 followup)。
- **object-cache `try_into().unwrap()`**(#3):**provably unreachable**——每处对定宽 `bytes[a..a+4]` 窗口操作,上游长度已 guard;不会 panic。正确保留,另加防御性 malformed-blob 测试(确认只 Err 不 panic)。
- **llvm `get_insert_block().unwrap()`**(#4):非 bug——inkwell 仅在 builder 无插入块时返 None,而这些点紧跟控制流 lowering、不变量保证有块;内部 codegen 不变量,不可达。
- **trace-emitter dict `record_len` 默认 0**(#7b):非 bug——文档化的 benign fallback,下游 trace 干净处理。
- **parser `integrity_algo.take().unwrap()`**(#5):非 bug——`else if is_some() && saw_colon` 在同一栈帧查了 is_some(),不变量成立。

> followup(未做):cli cranelift-aot 的 `--trust`(需 host-fn registry + gate-driven vtable,或 `from_source_with_config` 放宽 `SandboxConfig.capability_check`)——跨 crate API,待立项。

---

## moderate/risky + cross-crate dedup 轮(DONE,2026-05-31)— verify-then-fix

对 26 项 moderate/risky 简化 finding + 跨 crate 去重逐项 verify-then-fix(5 lane + 对抗审查全 accept)。
**moderate-risk 多数被正确保守跳过**(跨 crate 结构改 / public API / 行为改变 / 推测性重构);只落地经核实
byte-identical / behavior-preserving 的 7 项。

### 已落地(4 commit;M2 lane 核实后零提交 = 正确)

| commit | 内容 |
|---|---|
| `7a908521` | bytecode `with_fn_id` 去 `BcFunction` 深拷(直接赋 `fn_id` 字段);llvm fast-path arity `unwrap_or(0)`→`.expect` 标注共populate 不变量 |
| `8012e3c8` | relon-fmt dict reorder tier 分桶 4 遍 flat_map→单遍 partition(依赖既有 `PairTier` discriminant,输出 byte-identical) |
| `19a75f46` | test-harness trap-reason magic string 抽 `pub(crate) const` 去重;trace-abi slot `byte_offset` i64-cast 链→`wrapping_mul`(bit-identical) |
| `2b81c70f` | `is_remote_url`(http(s) 前缀检测)收口到 **relon-parser**(relon/analyzer/evaluator 三调用方均已 dep,无新 dep 边、无 cycle),替换 3 处重复 |

### 核实后保守跳过(不强改,记录原因)

- **eval-api/evaluator 的 `Scope` Locals/Thunks `Mutex<HashMap>`→`Arc<Mutex>` CoW**(M2,4 项):core `Scope`
  类型的跨 crate 结构改;`child()`/`list_element_scope` 刻意建空 map 以隔离父子写 + 支撑 `&prev/&next`
  兄弟解析,Arc-share 会改 aliasing 语义 —— **非 behavior-preserving**。
- **eval-api `RuntimeError` 变体形状改**(risky):public API + 文档声明双surface 是有意的。
- **bytecode `compile.rs` stack-recipe/branch-stack 快照 clone**(M1,2 项):deopt/resume PC-对齐的承重数据,
  Cow/sparse 重写是推测性算法重构、corruption 风险高、仅编译期、收益不明。
- **codegen-wasm bench-corpus emit_w* 抽取**:那 6 个函数是 bench workload(W1/W2/W6/W8/W9/W10)带 honesty
  不变量,reuse 重构会改被测 bytecode —— 零收益。
- **object-link triple 校验收紧 / target-lexicon 升 prod dep**:刻意改 accept/reject 集 = 行为改变,
  现启发式是有意的 CI 政策。
- **align_up / is_valid_rust_ident 去重**:无 clean 共享 home(需新建 crate 或有 cycle 风险)——
  与下方 followup 同,留作结构决策。

### 仍未做(需架构/安全决策,非自动化范围)

1. **cli cranelift-aot `--trust`**:需 host-fn registry + gate-driven vtable(或 `from_source_with_config`
   放宽 `SandboxConfig.capability_check`);且对 CLI 当前标量 `#main`(无 native 调用)而言 moot —— 安全敏感,需设计决策。
2. **align_up / is_valid_rust_ident 跨 crate 去重**:需决定共享 util crate 的归属(可能新建 crate)。

这两项都需要架构/安全层面的人工决定,不宜 unattended 改动。

---

## 架构/安全决策项闭环(DONE,2026-05-31)

上一节「仍未做」的两项,经与用户确认后落地:

### 1. cli `--trust` footgun —— 不再静默无效(`fix(relon-cli)`)
原状:`--trust` 被 tree-walk + bytecode honor,但 cranelift-AOT 路径(无 host-fn registry 可授权)
**静默吃掉**该 flag —— 用户敲 `relon run --trust --backend cranelift-aot` 期待打开沙箱却被无声忽略,是 footgun。
**决策**:不删 flag(它对 tree-walk/bytecode 真有意义),而是让它在不 honor 的后端上**不再静默** ——
传 `--trust` 且解析到 cranelift-AOT 路径(显式 `--backend cranelift-aot` 或 auto 非平凡路由)时,
stderr 显式警告「--trust has no effect on the cranelift-AOT backend ...,用 tree-walk/bytecode」,run 仍成功。
测试:cranelift-aot+`--trust`→出警告且仍跑出正确值;tree-walk+`--trust`→不出警告(它真 honor)。
> 真正让 cranelift honor `--trust` 仍需 host-fn registry + gate-driven vtable(对当前 CLI 标量 #main moot,
> 无 native 调用可 gate);留待真有 native-call-on-cranelift 需求时随真实需求设计。

### 2. 跨 crate 去重 —— 新建 `relon-util` leaf crate(`refactor:`)
`align_up(u32,u32)->u32` 的 3 份字节相同副本(cranelift/llvm/rs-shims)+ `is_valid_rust_ident` 的 2 份
(rs-build/rs-macro)收口到新的 **leaf crate `relon-util`**(空 `[dependencies]`、无任何 relon-* 依赖 →
`cargo tree` 证无 cycle)。5 个消费方改依赖、删本地副本;6 个新单测(align 边界 + ident accept/reject 含非 ASCII)。
异语义同名函数刻意未碰:eval-api `align_up(usize)->Option`、analyzer `validate_identifier`、evaluator
`is_valid_identifier`(校验 RELON 标识符,规则不同)。proc-macro crate relon-rs-macro 依赖普通 lib crate,构建通过。

至此本轮审计带出的全部可落地项(safe 简化 / correctness / moderate / 跨 crate 去重 / trust footgun)均已处理或
诚实记录;剩余仅「真正 wire cranelift `--trust`」一项,明确为需真实 native-fn 需求驱动的未来工作。
