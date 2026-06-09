# TODO 依赖层次与优先级清单

更新时间：2026-06-09。本文只整理当前脏工作区中的待办事实，不改变代码语义。

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
- tuple 第一批已覆盖面：tree-walk / LLVM native / LLVM wasm / Cranelift 均已支持由标量元素组成的 tuple 输入和返回；主机传入 tuple schema 时会解码为 `Value::Tuple`。
- tuple 嵌套当前事实：IR 类型表示已允许 tuple 里继续放 tuple、`List`、`Option`、`Result`；tuple 返回的基础 IR 转换已能处理嵌套 tuple、`List`、`String`、`Option`、`Result` 等已有布局类型，并已有 focused 编译测试覆盖关键组合。下一步是继续扩展输入、访问和更多组合形状的四路回归。
- Relon 没有 `null` 值或 `Null` 类型。用户写 `Null` / `Unit` / 旧 generic `Enum` 类型写法都应报错；低层 `Unit` 只表示内部 void/unit 槽，不是语言 surface，enum 类型只由 `#enum Name { ... }` 定义。
- CLI `--args <json>` 和 WASM playground `#main(args)` 已按 `#main` 参数类型做目标类型感知解码：`Tuple<...>` / tuple schema 的 JSON array 进入 `Value::Tuple`，`List<T>` 仍进入 `Value::List` 并保留元素校验；内建标量目标会拒绝错误 JSON 形状；目标是 enum 时，JSON string 可进入同名 unit variant，externally-tagged JSON object 可进入带 payload 的 variant；`Option<T>` / `T?` 支持 JSON `null`、直接 payload 和 `Some/None` 外部标签，`Result<T, E>` 支持 `Ok/Err` 外部标签。
- Option / Result 已有编译端布局、host buffer、verifier、LLVM/Cranelift native 三路测试；Rust-like `None` / `Some(x)` / `Ok(x)` / `Err(e)`、参数 `match` 分派、payload 字段访问和 payload pattern 绑定都已进入该路径。
- 自定义 `#enum` 当前已支持 parser / analyzer / tree-walk / CLI / WASM 输入边界；编译端已有 canonical 类型、layout/buffer/verifier、IR variant record，以及 LLVM/Cranelift native 输入和返回三路对拍。WASM/four-way 已覆盖 unit/struct/tuple variant 返回、unit/tuple 参数 identity、`List<Enum>` 参数原样返回、源码 `List<Enum>` 字面量返回、`map`/`filter`/comprehension 产出 `List<Enum>` / `List<Option>` / `List<Result>`、`List<Enum>` 作为匿名 `Dict` 字段转发或源码字面量字段、参数 `match` 按 variant tag 分派，match arm 内 payload 字段/索引访问、tuple/struct payload pattern 解构、泛型 custom enum，以及 tuple/list/Option/Result 嵌套 payload 的输入和构造返回；CLI/WASM typed JSON 输入已支持 payload variant 的 externally-tagged object/array。剩余主要是 spread、更多 list-producing source 形状和动态/no-match trap 场景。
- ledger cap-site 表已恢复 invariant：`LEDGER` 行全部为 `Status::Capped`，covered construct 放在 `SUPPORTED_SURFACE`。

## 依赖层次总览

1. 语言 / 类型系统：tuple/list/dict 的语义边界已定；正常类型嵌套应支持；没有值用 `None`。
2. Host Value / serde / 输入边界：CLI 和 WASM 已做目标类型感知解码；下一步是抽共享实现，避免两份规则漂移。
3. IR 转换：把语言事实转成编译端能执行的数据结构；每个“不支持”的代码位置都必须在 ledger 里有对应记录。
4. LLVM / Cranelift / WASM：在 IR 转换支持面扩展后补四路后端验证；wasm32 pointer width 是独立远期修正。
5. stdlib / API：清理 `from_json` 遗留、决定是否新增 `len/is_empty` 的 tuple 语义，并区分 tree-walker-only API 和跨后端 API。
6. docs / 测试 / 清理：公开文档跟随当前事实；旧 internal 文档只作历史，不再反向驱动实现。

## 1. 语言 / 类型系统

| 优先级 | 项 | 为什么优先 | 依赖什么 | 代表性文件/行 |
| --- | --- | --- | --- | --- |
| P1 | 补齐嵌套 tuple 的四路验证 | 类型表示已能表达 `Tuple<Int, Tuple<String, Bool>>`，tuple 返回 IR 转换已能生成嵌套子记录；还需要把输入、访问、返回都纳入 tree-walk / LLVM native / LLVM wasm / Cranelift 的一致性测试 | 依赖主机入参/返回值编码、ledger 更新，以及各后端实际执行验证 | `crates/relon-ir/src/lowering/mod.rs:2640-2745`, `crates/relon-ir/src/lowering/mod.rs:8030-8155`, `crates/relon-test-harness/src/ledger.rs:219-242` |
| P1 | 补齐 tuple 里放 `List` 的四路验证 | 类型表示和 tuple 返回 IR 转换已能处理已有布局的 `List<T>` 元素；还需要覆盖参数输入、返回输出和跨后端一致性 | 依赖 `List` 入参/返回值编码规则、pointer-array list 的安全边界，以及 LLVM/Cranelift/WASM 验证 | `crates/relon-ir/src/lowering/mod.rs:908-940`, `crates/relon-ir/src/lowering/mod.rs:2640-2745`, `crates/relon-ir/src/lowering/mod.rs:8030-8290` |
| P2 | 自定义 `#enum` 的剩余表达式覆盖 | Rust-like enum 的核心输入、返回、构造、match 分派、payload 字段/索引访问、payload pattern 解构、泛型 custom enum、tuple/list/Option/Result 嵌套 payload，以及 `map`/`filter`/comprehension 产出 variant list 已有 focused 编译测试；剩余不是 enum payload 本身，而是 spread、更多 list-producing source 形状和动态/no-match trap 场景 | 依赖 list-producing expression 的 IR 规则、trap kind 和后端错误映射 | `crates/relon-codegen-llvm/tests/option_result_return_three_way.rs`, `crates/relon-codegen-llvm/tests/tuple_return_four_way.rs`, `crates/relon-ir/src/lowering/mod.rs` |
| P1 | 防止 `Unit` 被误当成用户类型 | 低层 `TypeRepr::Unit` / `IrType::Unit` 仍需要保留，用来表示 void/unit 槽；用户 surface 里写 `Unit` 应报错，空 tuple 写 `()` | 依赖 analyzer 诊断、docs 文案和 host/buffer 边界继续区分 internal unit 与 `None` | `crates/relon-analyzer/src/ban_unsafe_types.rs:43-75`, `crates/relon-eval-api/src/schema_canonical.rs:49`, `crates/relon-ir/src/ir.rs:48` |
| P2 | list spread / dict spread 的编译支持面决策 | analyzer 已能让 list spread 贡献元素类型，但 IR 转换层仍没有完整可编译路径；dict spread 又依赖 Dict compiled value | list spread 依赖 list IR 转换；dict spread 依赖 Dict value model | `crates/relon-analyzer/src/infer/mod.rs:1105-1118`, `crates/relon-analyzer/src/typecheck/spread.rs:27-120`, `crates/relon-analyzer/src/typecheck/spread.rs:280-315`, `docs/internal/relon-ir-coverage-status.md:30-33` |
| P2 | Dict compiled value 是否进入当前编译支持面 | 影响 dict literal/spread、`dict.keys/values/merge`、VariantCtor；旧 internal docs 将其列为 by-design cap，需要重新按当前目标确认 | 需要 Dict 的 IR 表示、内存布局、host schema 和后端支持 | `docs/internal/.relon-ir-remaining-decisions.md:33-45`, `crates/relon-ir/src/lowering/mod.rs:1395-1399`, `crates/relon-ir/src/lowering/mod.rs:5192-5253` |
| P2 | 动态引用、positional base、多段 path、forward reference 的支持边界 | 当前只可靠覆盖静态 backward `&sibling` / `&root` 单段路径；继续扩展会影响类型系统、IR 转换和错误模型 | 需要引用解析顺序、运行时 path 表示、隐私规则和跨后端 trap 语义 | `crates/relon-ir/src/lowering/mod.rs:8340-8385`, `crates/relon-test-harness/src/ledger.rs:1263-1279` |
| P2 | schema method operator IR 转换待办 | analyzer 测试只检查 constraint registry entry 的形状，`for / a[i] / arithmetic` 等 operator 的 IR 转换仍未覆盖 | 需要先明确 schema method 中表达式到约束/IR 的映射 | `crates/relon-analyzer/tests/schema_methods.rs:228` |

## 2. Host Value / serde / 输入边界

| 优先级 | 项 | 为什么优先 | 依赖什么 | 代表性文件/行 |
| --- | --- | --- | --- | --- |
| P1 | 抽出共享 host JSON args 解码 helper | CLI 和 WASM 现在都按 `#main` 参数目标类型解码：`Option<T>` / `T?` 接受 JSON `null`、直接 payload 和 `Some/None` 外部标签，`Result<T, E>` 接受 `Ok/Err` 外部标签，tuple/list 分流，enum unit variant 支持从 string 输入；但逻辑仍有重复 | 依赖把当前 CLI/WASM 私有 helper 上移到公共 crate，并保留现有回归测试 | `crates/relon-cli/src/main.rs:320-720`, `crates/relon-wasm-bindings/src/lib.rs:378-840`, `crates/relon-eval-api/src/value.rs:165-240` |
| P1 | 扩展 host JSON args 回归测试到共享 helper | CLI/WASM 已覆盖 typed `Option<T>` null、`Option.Some` 外部标签、`Result.Ok` 外部标签、`T?` 直接 payload/null、targetless null 拒绝、tuple/list/option 嵌套、enum unit string 输入、payload variant externally-tagged 输入和 payload variant string 拒绝；共享 helper 落地后需要把这些测试下沉，避免两份实现漂移 | 依赖共享 helper | `crates/relon-cli/tests/backend_flag.rs`, `crates/relon-wasm-bindings/src/lib.rs` |
| P2 | `from_json` 遗留清理：确认所有 public docs、生成器、旧脚本不再把它列为 stdlib | 当前代码已删除且有测试；风险在旧 internal scratch 或外部文档继续误导 | 不依赖实现，只依赖文档/脚本扫描和必要清理 | `crates/relon-evaluator/src/stdlib.rs:122-148`, `crates/relon-evaluator/src/stdlib_drift_tests.rs:237-245`, `docs/internal/.wf-wave-a.js:73-89` |

## 3. IR 转换

| 优先级 | 项 | 为什么优先 | 依赖什么 | 代表性文件/行 |
| --- | --- | --- | --- | --- |
| P1 | 扩展编译端 tuple 返回到已确认的元素类型 | 当前已支持由标量组成的 tuple 字面量，并可作为参数/返回值。下一步要接通嵌套 tuple、`List`、`Option`、`Result`；返回值不是 tuple 字面量、元素数量不匹配等错误仍要明确报出 | 依赖上面几条的元素类型范围、主机入参/返回值编码、各后端数据布局 | `crates/relon-ir/src/lowering/cap.rs:57-74`, `crates/relon-ir/src/lowering/mod.rs:908-940`, `crates/relon-ir/src/lowering/mod.rs:2645-2710`, `crates/relon-ir/src/lowering/mod.rs:8020-8105` |
| P2 | 扩展 `&root/&sibling` 以外 reference IR 转换路径 | backward `&sibling`/`&root` scalar refs 已在 `SUPPORTED_SURFACE`，forward / dynamic / multi-segment 仍是 cap | 依赖引用解析设计、隐私规则、forward field-let 图策略 | `crates/relon-ir/src/lowering/mod.rs:8340-8385`, `crates/relon-test-harness/src/ledger.rs:1263-1290` |
| P2 | field decorator / branded schema decorator 的 IR 转换缺口 | anon-Dict field decorator 已进 supported surface，但多段/dynamic path、builtin decorator、named args、branded schema 场景仍 cap | 依赖 Dict/field path 运行时表示和 decorator ABI | `crates/relon-ir/src/lowering/mod.rs:1520-1557`, `crates/relon-ir/src/lowering/mod.rs:7875-7889`, `crates/relon-test-harness/src/ledger.rs:2206-2215` |
| P2 | `match` 的剩余动态场景和 no-match TypeMismatch trap | custom `#enum` 参数上的 variant tag 分派、match arm 内 payload 字段/索引访问和 payload pattern 解构已进入编译路径；剩余是非 enum 的动态/undecidable arms，以及 no-match trap 的跨后端错误形状 | 依赖类型系统可判定性、解构规则、trap kind 和后端错误映射 | `crates/relon-ir/src/lowering/mod.rs`, `crates/relon-evaluator/src/eval.rs` |

## 4. LLVM / Cranelift / WASM

| 优先级 | 项 | 为什么优先 | 依赖什么 | 代表性文件/行 |
| --- | --- | --- | --- | --- |
| P1 | tuple 支持面扩展后补四路后端验证 | 现有标量 tuple 已有 tree-walk / LLVM native / LLVM wasm / Cranelift 证明；下一批非标量 tuple 必须同步验证，避免只在 tree-walk 可用 | 依赖 IR 转换先放行新的 tuple shape | `crates/relon-test-harness/src/ledger.rs:2151-2183`, `crates/relon-codegen-llvm/tests/tuple_return_four_way.rs:1-23`, `crates/relon-codegen-llvm/src/evaluator.rs:1719-1724`, `crates/relon-codegen-cranelift/src/evaluator.rs:1881-1885` |
| P3 | LLVM wasm32 pointer width TODO | 当前 LLVM backend 多处假设 supported host 为 64-bit；wasm32 应从 DataLayout 取 pointer width，避免未来 32-bit target 出错 | 依赖 target abstraction/DataLayout plumbing；不阻塞当前 x86_64/wasm64-ish 路径 | `crates/relon-codegen-llvm/src/codegen/mod.rs:1430-1431`, `crates/relon-codegen-llvm/src/codegen/mod.rs:1815-1821`, `crates/relon-codegen-llvm/src/codegen/mem.rs:40-53` |
| P3 | WASM/CLI args 解码实现去重 | WASM playground 已支持 `#main(args)` 的目标类型感知解码；剩余问题是和 CLI 共享实现，避免规则漂移 | 依赖 Host Value 层 P1 的共享 helper | `crates/relon-wasm-bindings/src/lib.rs:378-840`, `crates/relon-cli/src/main.rs:320-720`, `docs/zh/guide/host-integration.md:41-58` |

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
| P0 | 清理旧 internal docs 中与当前 tuple/list 事实冲突的内容 | tuple/list 语义已定；旧文档仍写着“无 `Expr::Tuple` / `Value::Tuple`”“list literal inferred as Tuple”“tuple 用 `Value::List` 承载”等，会误导后续开发 | 不依赖语言设计决策；只按当前代码、测试、public docs 改写或标记历史 | `docs/internal/relon-ir-coverage-status.md:50-53`, `docs/internal/.relon-ir-remaining-decisions.md:27-31`, `docs/internal/.wf-wave-a.js:87-93` |
| P1 | public docs 中 tuple/std/host 边界的 drift audit | tuple/list 与 CLI host input 已同步；仍需核对 stdlib 表、英文/中文边界描述是否持续一致 | 依赖 `len(tuple)` / `is_empty(tuple)` API 决策 | `docs/zh/guide/spec.md:525-551`, `docs/zh/guide/spec.md:664-672`, `docs/zh/guide/host-integration.md:41-48`, `docs/en/guide/host-integration.md:47-55` |
| P3 | TLA/形式化占位按触发条件推进 | capability/sandbox spec 中 INV1-INV4 仍是 TODO placeholder；这属于 cap RFC、新 cap variant、multi-tenant、第三方 backend 触发项 | 依赖 capability model 出现真实新需求 | `docs/internal/formalization-targets-2026-05-23.md:75-85`, `docs/internal/capability-sandbox-spec-2026-05-23.tla:140-159` |

## 噪声与不要误判

| 来源 | 旧说法 | 当前判断 | 处理建议 |
| --- | --- | --- | --- |
| `docs/internal/relon-ir-coverage-status.md:50-53` | 没有 `Expr::Tuple` / `Value::Tuple`，tuple 已通过 List covered | 已过时；当前有 `Expr::Tuple`、`Value::Tuple`、tuple schema 和四路测试 | P0 标记为历史或改写 |
| `docs/internal/.relon-ir-remaining-decisions.md:27-31` | list literal 会 inferred as Tuple，spread 因此 blocked | 已过时；当前 list 和 tuple 分开推断，list spread 至少在 analyzer 层贡献元素类型 | 不作为当前事实，只保留 spread IR 转换待办 |
| `docs/internal/.wf-wave-a.js:73-93` | tier-2 stdlib 含 `from_json`；tuple 用 `Value::List` 表达 | 已过时；`from_json` 已删除，tuple 是 `Value::Tuple` | P0/P2 清理或归档 |
| 旧 wasm bench / v6 phase 文档 | 大量阶段性 TODO / DONE 混杂 | 多数是历史执行记录，必须用当前源码/ledger 复核后才能转为待办 | 不直接进入优先级队列 |

## 建议执行顺序

1. **P0**：清理或标记 tuple/from_json 旧 internal 噪声。
2. **P1**：若 WASM/JS 暴露 `#main(args)`，沿用 CLI 的目标类型感知 JSON 解码规则并补测试。
3. **P1**：把已经进入 IR 类型表示的嵌套 tuple / `List` tuple 元素扩展到四路后端回归。
4. **P2**：继续补 spread、更多 list-producing source 形状，以及动态/no-match trap 场景；继续确保 `Unit` 只作为内部 void/unit 槽出现。
5. **P2**：按需要推进 spread、Dict compiled value、reference/decorator/match 缺口。
6. **P3**：wasm32 pointer width、TLA 占位和远期 stdlib roadmap 单独排期。
