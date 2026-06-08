# TODO 依赖层次与优先级清单

更新时间：2026-06-08。本文只整理当前脏工作区中的待办事实，不改变代码语义。

## 读法

- **当前代码事实**：以当前源码、测试、public docs 为准。尤其 tuple 已是 `Value::Tuple`，不是旧的 `Value::List` 承载。
- **ledger cap-site**：`LEDGER` 表示“仍会拒绝/降级的 cap-site”，正常应为 `Status::Capped`；`SUPPORTED_SURFACE` 才表示已覆盖能力。
- **历史 internal docs 噪声**：`docs/internal` 中旧 wave、旧 coverage、隐藏 scratch 可能记录过时设计；只有与当前代码/ledger/public docs 互相印证时才作为待办依据。

优先级含义：

- **P0**：会误导当前 tuple/ledger 判断，或直接破坏支持面诚实性的事项。
- **P1**：tuple 与 host/编译链路下一步最可能阻塞用户可用性的事项。
- **P2**：重要设计/实现缺口，但已有明确 cap 或不阻塞当前 tuple 第一批支持面。
- **P3**：低风险清理、远期 target、形式化占位或平台扩展。

## 当前事实快照

- tuple 当前设计事实：`[]` 是同构 `List<T>`，`()` 是定长异构 `Tuple<...>`，运行时有独立 `Value::Tuple`；JSON 投影时 list/tuple 都是 JSON array。
- `from_json` 已从 stdlib 删除：当前注册表只有 `to_json`，并有 `from_json_is_not_registered` 测试保护。
- tuple 第一批已覆盖面：tree-walk / LLVM native / LLVM wasm / Cranelift 均有标量异构 tuple return/input 覆盖；host buffer 已能用 tuple schema 解码为 `Value::Tuple`。
- tuple 当前剩余主线：IR/lowering 的 tuple canonical/return 仍只接收 `Int/Float/Bool/String` 标量元素；嵌套 tuple、List、Null/Option/Result 等仍是 cap。
- CLI 当前 `--args <json>` 是 targetless `serde_json -> Value`；JSON array 会先落到 `Value::List`，不能自动按 `#main` 签名变成 `Value::Tuple`。
- ledger 当前有一个明显异常：cap-site 表 `LEDGER` 中混入了 `Status::Covered` 行，与该表模块说明冲突。

## 依赖层次总览

1. 语言 / 类型系统：先冻结 list/tuple/dict 的语义边界，再决定 spread、Dict、Variant、动态引用等是否进入编译支持面。
2. Host Value / serde / 输入边界：tuple 参数必须经过目标类型感知解码，否则 CLI/WASM/JS JSON 参数会把 tuple 数组误解成 list。
3. IR / lowering：把语言事实转成 canonical schema 与可编译 IR；cap-site 必须和 ledger 保持一一对应且语义诚实。
4. LLVM / Cranelift / WASM：在 lowering 支持面扩展后补四路后端验证；wasm32 pointer width 是独立远期修正。
5. stdlib / API：清理 `from_json` 遗留、决定是否新增 `len/is_empty` 的 tuple 语义，并区分 tree-walker-only API 和跨后端 API。
6. docs / 测试 / 清理：公开文档跟随当前事实；旧 internal 文档只作历史，不再反向驱动实现。

## 1. 语言 / 类型系统

| 优先级 | 项 | 为什么优先 | 依赖什么 | 代表性文件/行 |
| --- | --- | --- | --- | --- |
| P1 | 冻结 tuple/list 语义：tuple 只能由 `()` 构造并以 `Value::Tuple` 表示，list 只能由 `[]` 构造并保持同构 | 这是所有后续 lowering、host decode、docs 判断的前提；旧“tuple 用 List 承载”会导致错误待办 | 当前 parser/infer/typecheck/evaluator 已落地，需要 public/internal docs 统一 | `docs/zh/guide/spec.md:525-551`, `docs/zh/guide/spec.md:664-672`, `docs/zh/guide/syntax.md:29-49`, `crates/relon-eval-api/src/value.rs:145-178`, `crates/relon-evaluator/src/eval.rs:601-615`, `crates/relon-analyzer/src/infer/mod.rs:830-858` |
| P1 | tuple 非标量元素策略：嵌套 tuple、List、Null/Option/Result 在 `#main -> Tuple<...>` 中是继续 loud cap，还是扩展为可编译支持面 | 这是 tuple 当前最主要剩余项；host buffer 已有一部分表达能力，但 IR canonical/return lowering 只放行标量 | 需要先定 schema canonical 表达、buffer 编码、跨后端布局和失败语义 | `crates/relon-ir/src/lowering/mod.rs:2645-2710`, `crates/relon-ir/src/lowering/mod.rs:908-940`, `crates/relon-ir/src/lowering/mod.rs:8020-8105`, `crates/relon-test-harness/src/ledger.rs:219-242` |
| P2 | list spread / dict spread 的编译支持面决策 | analyzer 已能让 list spread 贡献元素类型，但 lowering 层仍没有完整可编译路径；dict spread 又依赖 Dict compiled value | list spread 依赖 list lowering；dict spread 依赖 Dict value model | `crates/relon-analyzer/src/infer/mod.rs:1105-1118`, `crates/relon-analyzer/src/typecheck/spread.rs:27-120`, `crates/relon-analyzer/src/typecheck/spread.rs:280-315`, `docs/internal/relon-ir-coverage-status.md:30-33` |
| P2 | Dict compiled value 是否进入当前编译支持面 | 影响 dict literal/spread、`dict.keys/values/merge`、VariantCtor；旧 internal docs 将其列为 by-design cap，需要重新按当前目标确认 | 需要 Dict 的 IR 表示、内存布局、host schema 和后端支持 | `docs/internal/.relon-ir-remaining-decisions.md:33-45`, `crates/relon-ir/src/lowering/mod.rs:1395-1399`, `crates/relon-ir/src/lowering/mod.rs:5192-5253` |
| P2 | 动态引用、positional base、多段 path、forward reference 的支持边界 | 当前只可靠覆盖静态 backward `&sibling` / `&root` 单段路径；继续扩展会影响类型系统、lowering 和错误模型 | 需要引用解析顺序、运行时 path 表示、隐私规则和跨后端 trap 语义 | `crates/relon-ir/src/lowering/mod.rs:8340-8385`, `crates/relon-test-harness/src/ledger.rs:1263-1279` |
| P2 | schema method operator lowering 待办 | analyzer 测试只检查 constraint registry entry 的形状，`for / a[i] / arithmetic` 等 operator lowering 仍未覆盖 | 需要先明确 schema method 中表达式到约束/IR 的映射 | `crates/relon-analyzer/tests/schema_methods.rs:228` |

## 2. Host Value / serde / 输入边界

| 优先级 | 项 | 为什么优先 | 依赖什么 | 代表性文件/行 |
| --- | --- | --- | --- | --- |
| P0 | CLI `--args <json>` 需要目标类型感知解码 tuple 参数 | 当前 CLI 用 `serde_json::from_str::<HashMap<String, Value>>`，JSON array 会 targetless 解成 `Value::List`；这会让 `#main(Tuple<...>)` 参数无法从 CLI JSON 正确进入 | 需要读取 `#main` 参数类型或 canonical schema，再把 JSON array 按目标类型映射到 `Value::Tuple` / `Value::List` | `crates/relon-cli/src/main.rs:90-96`, `crates/relon-cli/src/main.rs:713-736`, `crates/relon-eval-api/src/value.rs:145-178`, `docs/zh/guide/host-integration.md:41-44` |
| P1 | WASM/JS 入口若支持 `#main(args)`，必须避免复用 targetless JSON -> `Value` | 公开 host 边界已经说明 targetless JSON array 默认是 list；WASM 侧一旦开放参数入口，会遇到同一 tuple/list 歧义 | 依赖 CLI 同类目标类型感知解码方案，以及 wasm binding 的函数签名设计 | `docs/zh/guide/host-integration.md:41-44`, `docs/en/guide/host-integration.md:47-50`, `crates/relon-wasm-bindings/src/lib.rs:1-220` |
| P1 | 为 tuple host 输入补边界测试：CLI JSON tuple args、host `Value::tuple` args、四路后端一致性 | 现有 tuple return/input 已有 corpus 和四路验证，但 targetless CLI JSON 是单独输入边界风险 | 依赖 P0 解码策略；测试应同时证明 JSON list 参数仍为 `Value::List` | `crates/relon-test-harness/src/corpus.rs:199-215`, `crates/relon-test-harness/src/corpus.rs:1207-1245`, `crates/relon-codegen-llvm/tests/tuple_return_four_way.rs:1-23` |
| P2 | `from_json` 遗留清理：确认所有 public docs、生成器、旧脚本不再把它列为 stdlib | 当前代码已删除且有测试；风险在旧 internal scratch 或外部文档继续误导 | 不依赖实现，只依赖文档/脚本扫描和必要清理 | `crates/relon-evaluator/src/stdlib.rs:122-148`, `crates/relon-evaluator/src/stdlib_drift_tests.rs:237-245`, `docs/internal/.wf-wave-a.js:73-89` |

## 3. IR / lowering

| 优先级 | 项 | 为什么优先 | 依赖什么 | 代表性文件/行 |
| --- | --- | --- | --- | --- |
| P0 | 修正 ledger cap-site 表中混入的 `Status::Covered`，并加测试保证 `LEDGER` 全部为 `Capped` | `LEDGER` 模块说明明确 cap-site 行应是真 cap；Covered 行混在其中会让支持面、cap coverage、no-fallback 审计失真 | 需要决定该行是移入 `SUPPORTED_SURFACE`，还是恢复为 `Capped` 并补对应证据；随后补 ledger invariant 测试 | `crates/relon-test-harness/src/ledger.rs:15-49`, `crates/relon-test-harness/src/ledger.rs:1281-1290`, `crates/relon-test-harness/tests/ledger_completeness.rs:1-83`, `crates/relon-test-harness/tests/no_fallback_supported.rs:75-96` |
| P1 | tuple return lowering 扩展或明确冻结第一批支持面 | 当前 tuple scalar return/input 已覆盖；非 tuple literal body、arity mismatch、非标量元素仍是 cap。需要决定下一批是否只解 cap，还是保持 loud rejection | 依赖语言层非标量 tuple 策略、host buffer 形状、后端布局 | `crates/relon-ir/src/lowering/cap.rs:57-74`, `crates/relon-ir/src/lowering/mod.rs:908-940`, `crates/relon-ir/src/lowering/mod.rs:2645-2710`, `crates/relon-ir/src/lowering/mod.rs:8020-8105` |
| P2 | reference cap-site 与 support surface 分表后再扩展 `&root/&sibling` 以外路径 | reference 当前既有 covered 行又有 capped 行，先修 ledger 语义，再做 forward/dynamic/multi-segment 扩展 | 依赖 P0 ledger 修复、引用解析设计、隐私规则 | `crates/relon-ir/src/lowering/mod.rs:8340-8385`, `crates/relon-test-harness/src/ledger.rs:1263-1290` |
| P2 | field decorator / branded schema decorator 的 lowering 缺口 | anon-Dict field decorator 已进 supported surface，但多段/dynamic path、builtin decorator、named args、branded schema 场景仍 cap | 依赖 Dict/field path 运行时表示和 decorator ABI | `crates/relon-ir/src/lowering/mod.rs:1520-1557`, `crates/relon-ir/src/lowering/mod.rs:7875-7889`, `crates/relon-test-harness/src/ledger.rs:2206-2215` |
| P2 | `match` 动态/undecidable arms 和 no-match TypeMismatch trap | 当前 lowering 对动态 arms、无法静态判定的 cases、no-match trap 有 cap；若进入编译支持面，需要统一跨后端 trap | 依赖类型系统可判定性、trap kind、后端错误映射 | `crates/relon-ir/src/lowering/mod.rs:3980-4048` |

## 4. LLVM / Cranelift / WASM

| 优先级 | 项 | 为什么优先 | 依赖什么 | 代表性文件/行 |
| --- | --- | --- | --- | --- |
| P1 | tuple 支持面扩展后补四路后端验证 | 现有标量 tuple 已有 tree-walk / LLVM native / LLVM wasm / Cranelift 证明；下一批非标量 tuple 必须同步验证，避免只在 tree-walk 可用 | 依赖 IR/lowering 先放行新的 tuple shape | `crates/relon-test-harness/src/ledger.rs:2151-2183`, `crates/relon-codegen-llvm/tests/tuple_return_four_way.rs:1-23`, `crates/relon-codegen-llvm/src/evaluator.rs:1719-1724`, `crates/relon-codegen-cranelift/src/evaluator.rs:1881-1885` |
| P3 | LLVM wasm32 pointer width TODO | 当前 LLVM backend 多处假设 supported host 为 64-bit；wasm32 应从 DataLayout 取 pointer width，避免未来 32-bit target 出错 | 依赖 target abstraction/DataLayout plumbing；不阻塞当前 x86_64/wasm64-ish 路径 | `crates/relon-codegen-llvm/src/codegen/mod.rs:1430-1431`, `crates/relon-codegen-llvm/src/codegen/mod.rs:1815-1821`, `crates/relon-codegen-llvm/src/codegen/mem.rs:40-53` |
| P3 | WASM targetless JSON args 与 playground/API 边界统一 | 目前 WASM binding 主要是 evaluate/format 边界；如果加入 `#main` 参数执行，必须沿用目标类型感知解码 | 依赖 Host Value 层 P0/P1 的统一方案 | `crates/relon-wasm-bindings/src/lib.rs:1-220`, `docs/zh/guide/host-integration.md:41-44` |

## 5. stdlib / API

| 优先级 | 项 | 为什么优先 | 依赖什么 | 代表性文件/行 |
| --- | --- | --- | --- | --- |
| P2 | 决定是否新增 `len(tuple)` / `is_empty(tuple)` | 当前实现和 public docs 都只把 `len` 定义在 String/List/Dict 上；tuple 是一等值后，是否提供长度/空判断需要单独 API 决策，不能由 JSON array 输出形态反推 | 若新增，需要实现 Tuple 支持、补 drift/stdlib 测试并同步 docs；若不新增，保持当前 TypeMismatch 语义 | `docs/zh/guide/spec.md:252`, `crates/relon-evaluator/src/stdlib.rs:463-481`, `crates/relon-evaluator/src/stdlib.rs:2158-2172`, `docs/zh/guide/stdlib.md:21`, `docs/en/guide/stdlib.md:26` |
| P2 | 区分 tree-walker-only stdlib 和跨后端 stdlib 支持面 | public docs 已标出部分 tree-walker-only API；后续新增 API 需要明确是否要求 LLVM/Cranelift/WASM | 依赖 ledger/support surface 是否为 stdlib 增加后端维度 | `docs/zh/guide/spec.md:638-657`, `crates/relon-evaluator/src/stdlib.rs:122-193` |
| P2 | stdlib roadmap 保持“未冻结”状态，不把未来模块误列为当前 TODO 必做 | `std/time`、`std/regex`、`std/path`、`std/base64` 等是 roadmap，不是当前 tuple 阻塞项 | 依赖 public docs wording 和 capability 注册方案 | `docs/zh/guide/stdlib.md:140-155` |
| P2 | 保持 `from_json` 删除事实，并把 JSON 输入能力放在 host 边界而非 stdlib | 避免把 tuple/list target-aware decode 问题错误地回退到语言内 `from_json` | 依赖 Host Value 层 P0 解码方案和文档同步 | `crates/relon-evaluator/src/stdlib.rs:122-148`, `crates/relon-evaluator/src/stdlib_drift_tests.rs:237-245` |

## 6. docs / 测试 / 清理

| 优先级 | 项 | 为什么优先 | 依赖什么 | 代表性文件/行 |
| --- | --- | --- | --- | --- |
| P0 | 标记或清理旧 internal docs 中与当前 tuple 事实冲突的内容 | 旧文档仍写着“无 `Expr::Tuple` / `Value::Tuple`”“list literal inferred as Tuple”“tuple 用 `Value::List` 承载”等，会误导后续开发 | 依赖当前 tuple public docs 稳定；清理时不要改代码语义 | `docs/internal/relon-ir-coverage-status.md:50-53`, `docs/internal/.relon-ir-remaining-decisions.md:27-31`, `docs/internal/.wf-wave-a.js:87-93` |
| P1 | public docs 中 tuple/std/host 边界的 drift audit | 中文 public docs 已基本同步 tuple；仍需核对英文、stdlib 表、host integration warning 是否完全一致 | 依赖 P1 `len(tuple)` API 决策和 P0 CLI args 决策 | `docs/zh/guide/spec.md:525-551`, `docs/zh/guide/spec.md:664-672`, `docs/zh/guide/host-integration.md:41-44`, `docs/en/guide/host-integration.md:47-50` |
| P2 | 为 ledger 语义补测试：cap-site 全 Capped，supported-surface 全 Covered，二者不混表 | 当前已有 supported-surface 全 Covered 测试，但没有 cap-site 全 Capped 测试 | 依赖 IR/ledger P0 修复 | `crates/relon-test-harness/tests/ledger_completeness.rs:1-83`, `crates/relon-test-harness/tests/no_fallback_supported.rs:75-96` |
| P3 | TLA/形式化占位按触发条件推进 | capability/sandbox spec 中 INV1-INV4 仍是 TODO placeholder；这属于 cap RFC、新 cap variant、multi-tenant、第三方 backend 触发项 | 依赖 capability model 出现真实新需求 | `docs/internal/formalization-targets-2026-05-23.md:75-85`, `docs/internal/capability-sandbox-spec-2026-05-23.tla:140-159` |

## 噪声与不要误判

| 来源 | 旧说法 | 当前判断 | 处理建议 |
| --- | --- | --- | --- |
| `docs/internal/relon-ir-coverage-status.md:50-53` | 没有 `Expr::Tuple` / `Value::Tuple`，tuple 已通过 List covered | 已过时；当前有 `Expr::Tuple`、`Value::Tuple`、tuple schema 和四路测试 | P0 标记为历史或改写 |
| `docs/internal/.relon-ir-remaining-decisions.md:27-31` | list literal 会 inferred as Tuple，spread 因此 blocked | 已过时；当前 list 和 tuple 分开推断，list spread 至少在 analyzer 层贡献元素类型 | 不作为当前事实，只保留 spread lowering 待办 |
| `docs/internal/.wf-wave-a.js:73-93` | tier-2 stdlib 含 `from_json`；tuple 用 `Value::List` 表达 | 已过时；`from_json` 已删除，tuple 是 `Value::Tuple` | P0/P2 清理或归档 |
| 旧 wasm bench / v6 phase 文档 | 大量阶段性 TODO / DONE 混杂 | 多数是历史执行记录，必须用当前源码/ledger 复核后才能转为待办 | 不直接进入优先级队列 |

## 建议执行顺序

1. **P0**：先修 ledger `Covered` 混表，并补 invariant 测试；同时标记 tuple/from_json 旧 internal 噪声。
2. **P0/P1**：处理 CLI targetless JSON args，确定 tuple 参数从 JSON 进入 host 的目标类型感知解码路径。
3. **P1**：决定 tuple 非标量元素的第二批支持面；若放行，按 IR -> LLVM/Cranelift/WASM -> corpus/ledger 顺序推进。
4. **P2**：处理是否新增 `len(tuple)` / `is_empty(tuple)` 的 API 决策；若新增再同步实现、测试和 public docs。
5. **P2**：按需要推进 spread、Dict compiled value、reference/decorator/match 缺口。
6. **P3**：wasm32 pointer width、TLA 占位和远期 stdlib roadmap 单独排期。
