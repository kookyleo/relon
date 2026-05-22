# Relon workspace 逐 crate 复审报告

日期：2026-05-22  
范围：workspace 内 20 个 crate；按 crate 阅读核心源码、公共 API 边界、unsafe / sandbox / cache / import 路径、测试覆盖和验证 gate。  
说明：这是对既有 `docs/internal/crate-review-2026-05-22.md` 之后的复审补充，不覆盖历史报告。

## 验证结果

| 命令 | 结果 | 备注 |
|---|---:|---|
| `cargo test --workspace` | 初始通过；当前失败 | 初始复审时 unit / integration / doc 主线通过；发现外部 `crates/relon-ir/src/ir.rs` 未提交改动后复跑，当前在 `relon-trace-recorder` 编译失败 |
| `cargo fmt --all -- --check` | 通过 | 对当前 worktree 复跑通过 |
| `cargo clippy --workspace --all-targets -- -D warnings` | 初始通过 | 当前未复跑到底，因为 `cargo test --workspace` 已先暴露编译失败 |
| `cargo test --workspace --all-targets` | 未通过 | 初始失败来自 `relon-bench/benches/cmp_lua.rs` 的机器静默度保护：当前 governor / loadavg 不满足 bench 条件，不是功能测试失败 |

工作区注记：复审结束时工作区已有非本报告的未提交源码修改；最终 `git status` 显示 `crates/relon-ir/src/ir.rs` 和 `crates/relon-trace-recorder/src/lowering.rs` 已修改。本报告未回退这些改动，也未把这些外部改动计入本轮修复。

当前额外 gate 问题：外部改动把 IR 侧 `EffectClass` re-export 到 `relon_trace_abi::EffectClass`，variant 名称从 `UnrecoverableEffect` 变成 `Unrecoverable`；`crates/relon-trace-recorder/src/lowering.rs:1137` 仍引用旧名，导致 `cargo test --workspace` 编译失败。

## 主要发现

### P1: `TreeWalkEvaluator` 对外是 `Send + Sync`，但顶层调用共享同一份运行态

`Evaluator` trait 要求实现者 `Send + Sync`（`crates/relon-eval-api/src/lib.rs:86`）。`TreeWalkEvaluator::eval_root` / `run_main` 会在共享 `Context` 上重置 `step_counter`，并清空 `path_cache` / `iter_cursors`（`crates/relon-evaluator/src/eval.rs:182`、`:187`、`:196`、`:202`、`:228`、`:234`、`:239`、`:244`）。同一执行流中，`eval_internal` 用同一个 `step_counter` 计步（`crates/relon-evaluator/src/eval.rs:471`），reference resolution 用同一个 `evaluating_paths` 和 `path_cache` 做循环检测与缓存（`crates/relon-evaluator/src/reference.rs:724`、`:737`、`:741`、`:752`）。cache key 只包含 namespace/path，不含单次调用 id（`crates/relon-eval-api/src/scope.rs:283`）。

影响：两个线程复用同一个 evaluator 或同一个 `Context` 做并发 `eval_root` / `run_main` 时，一个调用可以清空另一个调用的 path cache / iterator cursor，重置对方的 step budget，或把对方正在解析的路径误判成 circular reference。这不一定造成 memory unsoundness，但违反 `Send + Sync` 给 host 的并发可用性预期，也会产生伪错误和资源限制绕过。

建议：把 `path_cache`、`evaluating_paths`、`iter_cursors`、`step_counter` 移进 per-run `EvalSession` / `RunState`；短期可在顶层 entry 上加 run mutex，并在 API 文档中明确同一 evaluator 不支持并发顶层调用。补并发 regression：两个线程同时跑相同 import / reference path，不应互相报 circular 或突破 step limit。

### P1: remote import 在 inline `sha256` pin 校验前已经写入缓存

`load_module` 文档和注释承诺 inline integrity 会在 parse / cache / evaluation 前校验，失败时“zero side effects”（`crates/relon-evaluator/src/eval.rs:1264`、`:1279`、`:1284`）。实际调用顺序是先 `resolve_module_source(...)`，再 `verify_module_integrity(...)`。而 `RemoteHttpResolver::fetch` 在返回 body 前已经执行 `cache.store(...)`（`crates/relon-evaluator/src/module.rs:653`）。`RemoteImportCache::store` 会直接写入 body 和 meta（`crates/relon-evaluator/src/module.rs:476`、`:485`、`:492`）。

影响：带错误 pin 的远端响应会被正确拒绝执行，但攻击者控制的 body 已经落盘。后续同 URL 的未 pin 或信任导入可能命中这份缓存；即使有 pin 的路径仍会失败，当前实现也不满足“pin mismatch 无缓存副作用”的安全说明。

建议：让 resolver 返回 fetched body + validator metadata，但不要自行 store；由 evaluator 在 integrity 通过后提交缓存。另一种做法是 resolver 接收 integrity policy / verifier callback，只缓存已通过 pin 的 body。补 regression：`#import "https://..." sha256:"bad"` 后，不应生成 `.body` / `.meta`。

### P2: `Capabilities::reads_fs` 文档宣称约束 FS import，但 resolver 实际不看 capability

`Capabilities::reads_fs` 注释写明它也是 `FilesystemModuleResolver` consult 的 policy bit（`crates/relon-eval-api/src/context.rs:40`）。`FilesystemModuleResolver::trusted` 也提示 host 必须 flip `Capabilities::reads_fs`（`crates/relon-evaluator/src/module.rs:101`）。但 resolver 注册 API 只把 resolver 放入链中（`crates/relon-eval-api/src/context.rs:388`），`FilesystemModuleResolver::resolve` 只能看到自身 `trusted/root` 与路径，不读取 `Context.capabilities`（`crates/relon-evaluator/src/module.rs:114`、`:121`、`:133`、`:151`）。

影响：如果 host 以为 `Context::sandboxed()` + `reads_fs=false` 是统一 policy，但又安装了 `FilesystemModuleResolver::with_root_dir` 或 `trusted()`，本地 import 仍会被允许。facade / CLI 的默认姿态基本靠是否挂 resolver 来表达权限，主路径未直接暴露，但 public API 文档和实际授权模型不一致，容易导致集成方误配。

建议：在 evaluator 解析 module 前按 capability gate 拦截 FS resolver，或把 capability policy 显式传入 resolver；如果设计上“resolver 是否注册”才是权限源，则修正文档，删除 `reads_fs` 会被 `FilesystemModuleResolver` consult 的说法。

### P2: remote import cache 写入不是原子提交

remote cache 的 body / meta 都用 `std::fs::write` 直接覆盖（`crates/relon-evaluator/src/module.rs:443`、`:452`、`:456`、`:485`、`:492`）。同一项目的 object cache 已经采用 temp file + atomic rename，且注释明确避免并发 producer 暴露半写文件（`crates/relon-object-cache/src/storage.rs:192`、`:223`、`:230`）。

影响：两个进程或线程并发 fetch 同一 URL 时，读者可能看到半写 body 或 body/meta 不匹配。当前 `body_sha256` 校验会把 mismatch 当 cache miss 并删除缓存，因此更像可用性 / flake 问题，不是直接执行污染 body；但 remote import 在 CI 或长生命周期服务中可能出现偶发失败。

建议：remote cache 沿用 object cache 的 temp + rename 模式，先原子写 body，再原子写 meta，读取方只把 meta 视为提交标记。

### P3: analyzer trusted resolver 链与 runtime resolver 顺序不完全一致

`ResolverChainLoader::trusted()` 的顺序是 `StdModuleResolver`、`FilesystemModuleResolver::trusted()`、`RemoteHttpResolver::new()`（`crates/relon/src/lib.rs:315`、`:319`、`:321`、`:322`），注释要求 host 改链时镜像 `evaluate_source`。runtime 组装路径中先 prepend FS，再 prepend remote，最终 remote 会排在 FS 前（`crates/relon/src/lib.rs:531`、`:538`）。

影响：在 trusted analyzer loader 中，字符串形如 `https://...` 的 import 会先被 trusted FS resolver 尝试解释成本地路径；如果工作目录下恰好存在 `https:/...` 这种路径，analyzer 与 runtime 可能解析到不同 module。触发条件较窄，但这类“安全/语义 loader 顺序不一致”会让 hash pin、diagnostics 和运行结果分叉。

建议：把 trusted analyzer loader 顺序调整为 runtime 等价顺序，或让 FS resolver 明确跳过 URL-like path。

## 逐 crate review

### `relon`

职责：公共 facade、backend 选择、`EvaluatorBuilder` / `AutoEvaluator`、JSON 边界和 workspace loader。

状态：crate root 安全边界清楚，facade 把 tree-walk / bytecode / native / analyzer 组合成 host API。主要风险不是本 crate 自身 unsafe，而是它把 evaluator、remote resolver、native cache 的策略暴露给普通 host。

发现：trusted analyzer loader 与 runtime resolver 顺序存在 P3 级不一致，详见上文。建议把 resolver order 做成共享 helper，避免 facade 和 analyzer 各自维护链。

### `relon-parser`

职责：lexer、rowan CST、legacy AST lowering、recovering parser、source range。

状态：无 blocking finding。parser 仍是纯 Rust 安全核心；CST / legacy AST 双轨带来维护成本，但 fixture 与 recover 测试覆盖较好。

建议：新语法进入后继续补 round-trip / broken-input fixture，避免 analyzer、fmt、wasm playground 对 CST 和 AST 的解释分叉。

### `relon-analyzer`

职责：workspace build、diagnostics、strict mode、schema/typecheck、import hash pin、module graph。

状态：无独立 blocking finding。analyzer 的 hash pin 和 workspace import 检查比较完整，但它不是唯一安全边界，runtime evaluator 必须维持同等校验。

建议：loader 顺序应与 runtime 保持一致；对 import 解析结果可加入 fixture，覆盖 URL-like path、本地 shadow path、hash pin mismatch 三者组合。

### `relon-bytecode`

职责：bytecode compiler / VM、stack execution、typed fast path、deopt / trace handoff。

状态：无 blocking finding。crate 使用 safe Rust，resource / sandbox gate 和 differential harness 覆盖较强。当前 fallback 策略对渐进式后端仍合适。

建议：对已宣称支持的 IR tier，把 unsupported fallback 逐步 ratchet 成 test failure，避免性能后端悄悄退回 tree-walk。

### `relon-eval-api`

职责：公共 `Evaluator` trait、`Context`、`Scope`、`Value`、buffer/schema/layout 基础类型。

状态：发现 P1/P2 边界问题都从这里的 API 契约扩散：`Evaluator: Send + Sync` 给出并发可用性预期，但 tree-walk 运行态是 shared；`Capabilities::reads_fs` 文档描述和 resolver 实现不一致。

建议：收紧 trait 文档：区分“可在线程间持有”与“可并发顶层调用”。能力模型上要么由 `Context.capabilities` 统一 gate resolver，要么明确 resolver registration 是 import 权限源。

### `relon-ir`

职责：IR 类型、lowering 共享契约、stdlib IR、shape/hash/Unicode 辅助。

状态：无 blocking finding。IR 是多后端同步压力最大的 crate；这轮未看到新的 soundness 问题。

建议：继续保持 visitor/exhaustiveness 测试。新增 op 时必须同时触发 bytecode、native、trace recorder/emitter、harness 的编译或测试失败，而不是静默 fallback。

### `relon-codegen-native`

职责：Cranelift native AOT/JIT、sandbox state、object/schema cache、trace install glue。

状态：高风险区近期已有多轮修复：per-call sandbox state、HMAC-required cache、atomic hot counter、no-op signal policy、trace string scoped reclaim 等主线看起来比早期稳定。`unsafe impl Send/Sync` 的边界需要持续用并发测试守住。

建议：把所有 unsafe island 维持在“注释 + invariant + regression test”三件套；尤其是 trace install、dlopen/memfd、arena pointer 返回路径，避免从 low-level API 泄露给普通 facade caller。

### `relon-object-cache`

职责：native object cache blob、metadata、HMAC key、integrity mode、loader 支持。

状态：无新的 blocking finding。object cache 的原子写入和 HMAC 模式比 remote import cache 更成熟，可作为后者实现参考。

建议：继续推动 `TrustOnWrite` 退场；未认证 object bytes 不应成为生产可执行路径。

### `relon-object-link`

职责：ET_REL -> ET_DYN 链接，默认 subprocess linker。

状态：无 blocking finding。边界主要是 operator trust：host triple、linker、`RELON_LD` 和 PATH。

建议：保持 Linux/x86_64 假设显式；非支持平台必须 clean fallback，而不是半启用 native cache。

### `relon-trace-abi`

职责：trace JIT 共享 ABI 类型、hook table、deopt 状态、layout contract。

状态：无 blocking finding。layout / discriminant 测试是关键资产。

建议：明确 ABI 是同一 Rust workspace 内部 ABI，不承诺稳定 C ABI。任何字段新增都必须伴随 offset/size 测试。

### `relon-trace-emitter`

职责：Trace IR -> Cranelift IR，guard、helper call、inline dict/string/list lowering。

状态：无新的 blocking finding。active dict lookup 已切到碰撞安全 v2 helper；legacy v1 inline 路径若继续保留，需要清楚标成 fixture/reference-only。

建议：补跨 crate smoke：hash collision、record_len truncation、prechecked path、missing hint deopt，避免 recorder/emitter/runtime 任何一侧单独演进。

### `relon-trace-jit`

职责：trace IR、optimizer、runtime helper、string/dict/list/deopt/call-table helper。

状态：无新的 blocking finding，但仍是 unsafe / ABI 高风险区。string arena 的 scoped reclaim 已有标准调用面，低层 `invoke_raw` 仍容易被误用。

建议：给 pointer-return string success 路径提供更高层 materialise API，让 caller 不需要理解 arena 生命周期；继续隔离或删除 v1 dict helper。

### `relon-trace-recorder`

职责：从 IR / op stream 记录 trace、类型观察、effect 分类、unsupported abort。

状态：无 blocking finding。当前 abort/fallback 策略合理，风险在支持矩阵长期不收敛。

建议：把 recorder 支持矩阵接入 harness 趋势，对核心 op 逐步禁止 silent abort。

### `relon-evaluator`

职责：tree-walk reference evaluator、stdlib、module resolver、sandbox capability gate、runtime import。

状态：本轮最主要问题集中在此 crate：tree-walk 运行态并发共享、remote import cache 在 pin 校验前落盘、FS capability 文档/实现不一致、remote cache 非原子写入。

建议：优先处理 P1 两项。tree-walk 是 reference evaluator，正确性和 host 集成语义应该比性能优化更优先；remote import 的 pin/cache 顺序应与文档承诺完全一致。

### `relon-fmt`

职责：formatter、syntax checker CLI。

状态：无 blocking finding。idempotence 和 syntax preset 覆盖良好，风险主要来自新语法 AST/CST 不同步。

建议：把 import integrity、schema method、match/tuple、generic schema 的组合加入 golden corpus。

### `relon-bench`

职责：内部 benchmark harness、LuaJIT/native/trace/bytecode 对比、methodology guard。

状态：`cargo test --workspace --all-targets` 会进入 bench target，并被 `cmp_lua` 的 quiescence guard 拦下。这是 bench 环境保护，不是功能失败，但会让常见 contributor 命令看起来红。

建议：在 contributor docs 里明确“功能 gate 用 `cargo test --workspace`，bench/all-targets 需要静默机器或 `RELON_BENCH_FORCE_RUN=1`”。bench-only unsafe / FFI wrapper 继续保持醒目标注。

### `relon-cli`

职责：命令行入口、backend flag、workspace load、sandbox/trust flag、输出。

状态：无 CLI 独立 blocking finding。CLI 安全姿态取决于 facade/evaluator 的 resolver、remote import、native cache 策略。

建议：`--trust`、remote import、hash pin、cache 行为要与 docs 同步；如果默认启用 native cache，help 文本应说明 HMAC key 缺失时的行为。

### `relon-lsp`

职责：Language Server、diagnostics、hover/completion/definition/references/rename。

状态：无 blocking finding。主要风险是多文件 workspace state 清理和 edit sequence。

建议：继续补新增、删除、rename、hash pin mismatch 修复后的 diagnostics 清理测试。

### `relon-test-harness`

职责：多后端 differential corpus、fallback / unsupported 分类、three/four-way comparison。

状态：无 blocking finding。它是后端同步最关键的质量资产。

建议：把 P1/P2 修复后的 regression 放进 harness 或邻近 crate tests：并发 tree-walk、remote pin mismatch no-cache、FS capability deny、remote cache partial write recovery。

### `relon-wasm`

职责：browser wasm bindings、in-memory 多文件 evaluate/format/diagnostics。

状态：无 blocking finding。wasm 路径避开 native AOT/cache 风险，import 主要依赖 host 提供的 virtual/in-memory module。

建议：发布 gate 加 headless browser 或 `wasm-pack test` 等价验证，覆盖 JS binding、panic/diagnostic serialization、playground 真实运行方式。

## 建议修复顺序

1. P1 tree-walk per-run state：先修并发正确性和 step budget 语义，再扩大 host 并发使用面。
2. P1 remote import verify-before-cache：把实现改到与安全注释一致，并补 no-cache-on-pin-mismatch regression。
3. P2 filesystem capability model：选择“capabilities 统一 gate”或“resolver registration 是 authority”，然后统一文档和实现。
4. P2 remote cache atomic write：复用 object cache 的 temp + rename 模式。
5. P3 resolver order：共享 trusted resolver chain 或让 FS resolver skip URL-like paths。

## Gate 注记

本轮没有修改源码逻辑，只新增本报告。初始复审 gate 中 `cargo test --workspace`、fmt、clippy 通过；当前 worktree 因非本报告的 `crates/relon-ir/src/ir.rs` 改动导致 `cargo test --workspace` 编译失败。另一个红色命令 `cargo test --workspace --all-targets` 会触发 bench 静默度保护。
