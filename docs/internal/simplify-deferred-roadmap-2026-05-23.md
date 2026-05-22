# Simplify Deferred Roadmap

**编制日期**：2026-05-23  
**来源**：2026-05-22 simplify wave (commits `753b7fa..199e3b0`，跨 20 crate 3-agent reuse/quality/efficiency review) 的 deferred 项汇总  
**用途**：每项给出 *what / where / why deferred / trigger* 四要素，下一轮判断"该不该动手"时不必重读 3000 行 agent 报告。

Tier 排序：
- **P0** — 风险项（行为差异、半成品、潜在 bug），下次触碰对应文件务必清理。
- **P1** — 高 ROI 结构清理，单次 1-3 commit 可收，需要专门 session 而非 batch。
- **P2** — 性能优化，落地前必须 bench fixture 验证（参见 `memory/feedback_bench_methodology_first.md`）。
- **P3** — 跨 crate 类型 / abstraction 合并，需评估 API 影响 + ABI 兼容。
- **P4** — 长尾 nit，路过顺手即可。

---

## P0 — 风险项

### P0-1 [analyzer] `in_method_block(_schema)` stub 永远返 false
- **位置**：`crates/relon-analyzer/src/typecheck/fn_call.rs:439-450, 354`
- **现象**：`#private` method 内调同 schema 的合法方法被误报 `PrivateMethodViolation`。注释自承 "known false-positive surface"。邻近 `let _ = (key, node);` 是悬空状态。
- **修法**：接入真正的 method_call_context；或临时禁用 `PrivateMethodViolation` 直到 context 接入。
- **trigger**：下次有 user bug 报告 `#private` 误报 / method-call 上下文需要扩展时。

### P0-2 [recorder] LoopMarker / begin_loop 双 loop 录制路径不闭合
- **位置**：`crates/relon-trace-recorder/src/recorder.rs:1014-1028` (LoopMarker emit MarkLoopHead 不 push `open_loops`)；与 `begin_loop` 公共 API 静默不兼容
- **现象**：之后 `end_loop` 弹不到对应帧 → `LoopBackWithoutHead` abort。
- **修法**：LoopMarker arm 也 push `open_loops`，或显式拒绝混用两 API。
- **trigger**：trace-recorder 加新 loop 入口 / 调试 LoopBackWithoutHead abort 误判。

### P0-3 [recorder] `GuardKind::BoundsCheck(base, base)` 偷换 NotNull 语义
- **位置**：`crates/relon-trace-recorder/src/lowering.rs:389,400`；配合 `emit_guard` 仅过滤 `v == NONE` (`recorder.rs:1165`)
- **修法**：用 `GuardKind::NotNull(base)` 直接表达；或在 BoundsCheck 上加 single-arg 重载。
- **trigger**：trace-recorder 扩 guard 类型 / 调试 bounds violation 误判。

### P0-4 [recorder] `record_op` 与 `emit_*` 直接入口 PC 不一致
- **位置**：`crates/relon-trace-recorder/src/recorder.rs:746` (record_op bump next_external_pc) vs `:367+, 408+, 494+, 542+, 256+` (emit_list_get / emit_dict_lookup_* / emit_str_concat / emit_str_contains / emit_branch_*_guard 都不 bump)
- **现象**：混用两种 API 时直接路径会沿用旧 PC，site lookup 撞车。
- **修法**：统一在 emit_* 上 bump PC；或文档化"只能选一种 API 入口"。
- **trigger**：trace-recorder 加 host 直发 API / site dispatch 调试。

### P0-5 [cli] `--lite` 与 `--backend` 冲突检查不全
- **位置**：`crates/relon-cli/src/main.rs:300-310` (只拒绝 cranelift-aot/bytecode + lite) vs `:693` (`--lite` 强行覆写 effective_backend = TreeWalk)
- **修法**：把冲突表完整化，或把 lite 改成一个 backend 选项而非 flag。
- **trigger**：cli 用户报告 `--lite --backend X` 行为意外。

### P0-6 [trace-jit] `const_fold::is_safe_const_source` 自相矛盾
- **位置**：`crates/relon-trace-jit/src/optimizer/const_fold.rs:66` 注释 vs 实现
- **现象**：注释说 "检查 input 不是 RecoverableWrite output"，函数实际逻辑仅 match Const* 短路；input 应查 a/b 的 def-op 而非 `trace.ops[idx]`
- **修法**：实现对齐注释，或把注释改成实际语义。
- **trigger**：fold 误报 / pass interaction 调试。

---

## P1 — 高 ROI 结构清理

### P1-1 [analyzer] NodeIndexer deep clone O(N²)
- **位置**：`crates/relon-analyzer/src/resolve/mod.rs:228` `Arc::new(node.clone())` per visit
- **影响**：每次 `analyze_with_options` 都付一遍递归深拷贝 — workspace 越大 N² 系数越大，cold-start 主项。
- **修法**：parser 一开始把 `Node` 包成 `Arc<Node>`，indexer 仅 walk + 拷贝指针。
- **预估**：parser API 改 (Node→Arc<Node>) 跨 crate；2-3 commit。
- **trigger**：cold-start bench 显示 indexer 占 >15% / LSP keystroke 延迟问题。

### P1-2 [analyzer] workspace 串行 analyze
- **位置**：`crates/relon-analyzer/src/workspace_build.rs:84-115` BFS 单线程
- **修法**：每个 PendingImport 用 `rayon::par_iter` 并行 parse+analyze；写回 `ws.modules` 串行 insert。
- **预估**：1-2 commit。
- **trigger**：3+ 模块 workspace 上 cold-start 痛点。

### P1-3 [analyzer] `recheck_cross_module_calls` 双 typecheck
- **位置**：`crates/relon-analyzer/src/workspace_build.rs:1004,1063`
- **影响**：含 import 的模块 typecheck 跑 2 次。
- **修法**：第一次 analyze 时若模块有 #import 跳过 typecheck，留 post-pass 一次跑完。
- **预估**：1 commit。
- **trigger**：workspace ≥ 2 模块的项目 cold-start 优化。

### P1-4 [parser] `lower_dict_field` 350 行 state machine
- **位置**：`crates/relon-parser/src/lower.rs:2474-2828`，12 个 bool/Option 状态
- **修法**：按字段位置拆 4-5 个小函数 (leading-attrs / key / optional-value / method-shorthand body)。
- **预估**：3-5 commit, 大重构。
- **trigger**：parser 加新 dict field 语法 / 维护成本疼。

### P1-5 [parser] `lower_schema_method` 150 行
- **位置**：`crates/relon-parser/src/lower.rs:1644-1801`，同类布尔泥潭
- **修法**：拆 enum-variant + generic + method-pragma 分支
- **trigger**：同 P1-4 触发条件。

### P1-6 [parser] `lower_type_node_from_cst` 5+ 层嵌套
- **位置**：`crates/relon-parser/src/lower.rs:1852-2077`
- **修法**：variant-struct 分支抽 helper；2049-2062 `is_enum_head` 后处理与注释矛盾。
- **trigger**：parser type-node 扩展。

### P1-7 [parser] `source.rs` 与 `lex.rs` 双 lexer
- **位置**：`crates/relon-parser/src/source.rs` (444 行) vs `lex.rs` (561 行)
- **修法**：让 `relon-fmt` 直接消费 `lex::lex` 输出，加 `leading_newlines` 字段；删 `source.rs` ~400 行。
- **预估**：触动 fmt API，3-5 commit。
- **trigger**：lex bug 在两边都需修 / 新 token 同步出错。

### P1-8 [parser] CST `DirectiveShape` 私有副本
- **位置**：`crates/relon-parser/src/cst.rs:2389-2410` vs `directive.rs::DIRECTIVE_SHAPES`
- **修法**：CST 直接 `use crate::directive::directive_shape`，删 private 副本 ~30 行
- **预估**：1 commit。
- **trigger**：directive 加新 shape 时手工同步出 bug。

### P1-9 [codegen-native] `evaluator.rs` 3 入口 unify
- **位置**：`crates/relon-codegen-native/src/evaluator.rs:1019-1135, 1469-1519` (`run_main_legacy_i64` / `run_main_smallmap` / `Evaluator::run_main`)
- **修法**：抽 `fn pack_legacy_argv(&self, lookup: impl Fn(&str) -> Option<&Value>) -> Result<[i64;4], _>`。
- **预估**：1-2 commit。
- **trigger**：dispatch 入口扩展。

### P1-10 [codegen-native] `trace_install.rs:988-1199` host helper declare 13 处复制
- **位置**：13 个 `build_host_helper_signature` + `module.declare_function` 串
- **修法**：表驱动 `[(HostHookId, &[Type], &[Type])]` 一次循环。
- **预估**：1 commit。
- **trigger**：加新 HostHookId。

### P1-11 [codegen-native] Codegen 字段三组互斥状态
- **位置**：`crates/relon-codegen-native/src/codegen/mod.rs:589-616`：`captures_ptr: Option<CValue>` / `lambda_param_tys: Option<&[IrType]>` / `inline_frames` 三组只一时刻一组活
- **修法**：`enum CodegenMode { Entry, Lambda{...}, Inline{...} }`
- **预估**：1-2 commit。
- **trigger**：codegen mode 扩展。

### P1-12 [trace-emitter] emitter.rs ↔ inline_emit.rs ~600 行 line-for-line copy
- **位置**：`crates/relon-trace-emitter/src/emitter.rs:696-2025` vs `inline_emit.rs:331-622`
- **修法**：抽 `trait OpLowerer` 或 `EmitterCore<E: From<GuardEmitError>>`。
- **预估**：3-5 commit，大重构 + lint 守护重写。
- **trigger**：op 加 lowering 时双改维护成本疼。

### P1-13 [evaluator] `try_call_schema_method` 3-4 层嵌套
- **位置**：`crates/relon-evaluator/src/eval.rs:1605-1717`
- **修法**：早返 + static / dynamic / multi-segment 三态拆 3 个 helper。
- **预估**：1-2 commit。

### P1-14 [evaluator] 3 处 method dispatch 复制粘贴
- **位置**：`crates/relon-evaluator/src/arithmetic.rs:381-551` + `eval.rs:1814-1843`
- **修法**：抽 `dispatch_method(brand, name, receiver, args, ...)`。
- **预估**：1 commit。

### P1-15 [evaluator] Iter 协议 stringly-typed
- **位置**：`crates/relon-evaluator/src/eval.rs:1884-1980` + `stdlib.rs:1756-1872, 1911-1920` 8+ 处 `"_kind" / "_source" / "_id" / "Iter" / "list" / "string" / "dict_entries"`
- **修法**：`enum IterKind { List, String, DictEntries }` + 常量；抽 `iter.rs` mod。
- **预估**：1-2 commit。

### P1-16 [evaluator] reference.rs 3 处 Dict/List 结构 lookup 重复
- **位置**：`crates/relon-evaluator/src/reference.rs:97-211, 463-654, 813-954`
- **修法**：抽 `step_into_value(&mut current, &key, is_optional, display, range)`。
- **预估**：2-3 commit (3 callers 微妙差异)。

### P1-17 [trace-jit] TraceOp variant style mixed
- **位置**：`crates/relon-trace-jit/src/trace_ir.rs` tuple-style vs struct-style 混存
- **修法**：全 struct-style + derive `output/inputs/defs` (或 visitor trait)。
- **预估**：3+ commit，大改 IR API。
- **trigger**：trace-IR op 加 N 次后某次漏写四联导致 bug。

### P1-18 [trace-jit] 3 处 rebind_guard_pcs 实现
- **位置**：`crates/relon-trace-jit/src/optimizer/{licm,type_spec,noop_typecheck_elim}.rs`
- **修法**：抽到 `buffer.rs` 上 `TraceBuffer::rebind_guards_by_*`。
- **预估**：1 commit。

### P1-19 [bytecode] `invoke_from_with_stack` vs `invoke_pooled_typed_i64` 120 行 dispatch 骨架重复
- **位置**：`crates/relon-bytecode/src/vm.rs:811-1033, 1063-1194`
- **修法**：闭包/方法参数化 locals/stack 来源。
- **预估**：1-2 commit。

### P1-20 [bytecode] `compile_inline_one` vs `OpVisitor` 双 Op→BcOp 映射
- **位置**：`crates/relon-bytecode/src/compile.rs:708-835` vs `:1149+`
- **修法**：inline 路径复用 visitor 或下沉共享 helper。
- **预估**：1-2 commit。

### P1-21 [bytecode] `apply_stack_effect` 225 行 mega-match
- **位置**：`crates/relon-bytecode/src/compile.rs:332-558`
- **修法**：抽 `push_snapshot()` / `pop_n(n)` helper，26 arm 减半。
- **预估**：1 commit (机械)。

### P1-22 [cli] `main.rs` 680 行单 fn (4+ 层嵌套)
- **位置**：`crates/relon-cli/src/main.rs:227-906`
- **修法**：拆 Commands / backend dispatch / arg-handling 子函数。
- **预估**：3+ commit。

---

## P2 — 性能（bench 验证后）

### P2-1 [evaluator] `Expr::List` 求值 O(N²) thunks clone
- **位置**：`crates/relon-evaluator/src/eval.rs:515-549`
- **影响**：N 元素列表 O(N²) refcount + N 个 `Arc<Scope>` + N 个 `Arc<ListContext>`
- **修法**：`thunks` 整体 `Arc<Vec<...>>` 一次；或检测 &prev/&next 使用，否则跳过构造。
- **bench**：list-heavy workload (json-like config)。

### P2-2 [evaluator] `eval_closure` per-call 双重深拷贝
- **位置**：`crates/relon-evaluator/src/eval.rs:2154-2183`
- **影响**：`xs.map(f)`/`filter(p)`/`reduce(...)` 每元素 deep body clone + 两个 `Arc<Scope>` + `format!`
- **修法**：`ClosureData::body` 改 `Arc<Node>` (与 `Thunk::node` 一致)，调用时 `Arc::clone`。
- **bench**：closure-heavy workload (list ops)。

### P2-3 [evaluator] `caps.call_relon` per-element 新建 Evaluator
- **位置**：`crates/relon-evaluator/src/eval.rs:2517-2528`
- **影响**：`_list_map`/`_list_filter`/`_list_reduce`/`_list_contains` 每元素新构造 `TreeWalkEvaluator` + OnceLock×2
- **修法**：caps 改持 `Arc<TreeWalkEvaluator>` 或走入 eval_closure 直接路径。
- **bench**：list ops on long lists。

### P2-4 [evaluator] `(String, String)` HashMap key in method dispatch
- **位置**：`crates/relon-evaluator/src/eval.rs:1814-1843` + `arithmetic.rs:421, 537`
- **修法**：HashMap key 改 `(Arc<str>, Arc<str>)` 或前置 brand 检查。
- **bench**：method-call heavy workload。

### P2-5 [evaluator] `check_type` 整张 SchemaData clone
- **位置**：`crates/relon-evaluator/src/schema.rs:166-204`
- **修法**：`Value::Schema` 内层 `Arc<SchemaData>` 让 clone 是 refcount bump。
- **bench**：schema-validated payload。

### P2-6 [bytecode] BcOp enum 24B → 8B
- **位置**：`crates/relon-bytecode/src/op.rs:25-381`
- **影响**：dispatch loop 每 op 拉 24B slot 进 L1，cache line 仅装 2-3 op
- **修法**：hot variants 落 8B (`Box<CallNativeArgs>` 包大 payload)
- **bench**：long-loop bytecode workload。
- **风险**：cross-VM ABI 变化，需谨慎。

### P2-7 [bytecode] `Instant::now()` + `max_steps` 每 op 采样
- **位置**：`crates/relon-bytecode/src/vm.rs:962-986, 1130-1141, 1230-1241`
- **修法**：每 64/1024 op 采一次。
- **bench**：W12 / hot loop。

### P2-8 [bytecode] `BcVmConfig` per-call clone (含 `HashMap` clone)
- **位置**：`crates/relon-bytecode/src/evaluator.rs:489, 914-925`
- **修法**：`Arc<BcVmConfig>` 让 clone 是 refcount。
- **bench**：per-call dispatch overhead。

### P2-9 [codegen-native] arena `vec![0u8; ~70KiB]` per-dispatch
- **位置**：`crates/relon-codegen-native/src/evaluator.rs:1374`
- **修法**：thread-local 池化；仅 zero 必要前缀。
- **bench**：dispatch hot path。

### P2-10 [codegen-native] `SandboxState` `Box::new` per-call
- **位置**：`crates/relon-codegen-native/src/evaluator.rs:1171, 1235` (+ Mutex lock capabilities_snapshot)
- **修法**：thread-local `RefCell<SandboxState>` 池；`ArcSwap` 替 `Mutex<Arc<_>>`。
- **bench**：dispatch hot path。

### P2-11 [trace-jit] `__relon_trace_resolve_call` thread-local + RefCell + HashMap
- **位置**：`crates/relon-trace-jit/src/runtime/call_table.rs:138-145`
- **修法**：trace install 时冻结成 sorted array / perfect-hash；或 patch fn_ptr 进 IR imm。
- **bench**：trace Call hot path。

### P2-12 [trace-jit] `RwLock<HashMap>` for trace lookup
- **位置**：`crates/relon-trace-jit/src/trace_install.rs:622, 635-637, 785, 1422`
- **修法**：`ArcSwap<HashMap<...>>` 或 `[OnceCell<Arc<JITedTraceFn>>; MAX_FN_ID]` 数组。
- **bench**：trace dispatch hot path。

### P2-13 [trace-jit] LICM hoist_one_loop 全 pass restart
- **位置**：`crates/relon-trace-jit/src/optimizer/licm.rs:97-116, 178-220`
- **修法**：单次 collect_loops + 累积所有 hoist 候选后 splice。
- **bench**：trace install 时间。

### P2-14 [trace-recorder] `LowerOutcome::Emit` per-op Vec alloc
- **位置**：`crates/relon-trace-recorder/src/lowering.rs:68-77` + 12+ callers
- **修法**：`guards_before/after` 改 `ArrayVec<GuardKind, 2>` / `Option×2`。
- **bench**：trace recording cost。

### P2-15 [trace-recorder] HashMap → FxHashMap
- **位置**：`crates/relon-trace-recorder/src/recorder.rs:148-154` 3 个 + `relon-trace-jit::TraceBuffer` 6 个
- **修法**：workspace 加 `rustc-hash` dep，机械替换。
- **bench**：trace record cost。
- **风险**：workspace dep change。

### P2-16 [trace-recorder] `ssa_stack.clone()` per-guard heap alloc
- **位置**：`crates/relon-trace-recorder/src/recorder.rs:1209`
- **修法**：`Box<[SsaVar]>` / `SmallVec`。
- **bench**：guard-heavy trace。

### P2-17 [eval-api] `ValueDict.map: BTreeMap<String, Value>` 不走 SmolStr
- **位置**：`crates/relon-eval-api/src/value.rs:11`
- **影响**：每个 dict 键 String malloc/clone
- **修法**：`BTreeMap<SmolStr, Value>`，配合 `Serialize` 兼容性验证
- **bench**：dict-heavy workload。

### P2-18 [eval-api] `Scope.path_node/current_dir/cache_namespace` `String` clone per child
- **位置**：`crates/relon-eval-api/src/scope.rs:141, 146, 150, 314-315`
- **修法**：`Arc<str>`。
- **bench**：scope-heavy workload (closures, list ops)。

### P2-19 [eval-api] `relocate_pointers` per nested schema layout 重算
- **位置**：`crates/relon-eval-api/src/buffer.rs:888-1012`
- **修法**：`OffsetTable` 在 `BufferBuilder::new` 时一次 cache。
- **bench**：List<Schema> / Dict<_, Schema> workload。

### P2-20 [analyzer] `infer_type` Closure/Comprehension/Where 内 locals/frames clone
- **位置**：`crates/relon-analyzer/src/infer/mod.rs:801-810, 939-946, 955-981`
- **修法**：栈式 `Vec<HashMap>` + push/pop。
- **bench**：含闭包/列表推导链源码。

### P2-21 [analyzer] `capability_check` 双 walk + 反复 fold
- **位置**：`crates/relon-analyzer/src/capability_check.rs:77-89`
- **修法**：单遍 walk + 记忆化 fold (`HashMap<NodeId, Option<ConstValue>>`)。
- **bench**：gates 非空时启用。

---

## P3 — 跨 crate 类型 / abstraction 合并

### P3-1 [parser+analyzer+ir] builtin type name 三份不一致
- **位置**：`relon-parser::is_builtin_type_name` (13) vs `analyzer::extend.rs::BUILTIN_TYPE_NAMES` (14) vs `analyzer::lib.rs::is_scalar_builtin_type` (5)
- **现状**：语义不同（grammar / extend-target / scalar-return），非真 drift
- **修法**：保留三套但加 cross-link doc + cross-crate audit test 防漂移
- **trigger**：加新 builtin 类型时一次梳理。

### P3-2 [parser+analyzer] `format_type` / `position_to_offset` 多份
- **位置**：4 处 inline `position_to_offset/offset_to_position` (analyzer + lsp + parser)
- **修法**：parser 暴露 `LineIndex` API 一次性 build；analyzer + LSP 借用
- **预估**：2-3 commit, parser API 扩展。
- **trigger**：LSP keystroke 延迟 / 多处实现漂移。

### P3-3 [evaluator+codegen-native] `verify_module_integrity` / digest 复制
- **位置**：`evaluator::eval.rs:2452-2506 verify_module_integrity` + `compute_module_digest` ↔ `analyzer::workspace_build.rs:513-526 compute_digest/digest_matches`
- **修法**：抽到 `relon-eval-api` 或 `relon-parser`（`IntegrityHash` 在 parser）
- **trigger**：digest 计算行为分叉。

### P3-4 [codegen-native] `cache.rs` IrSerde 180 行镜像
- **位置**：`crates/relon-codegen-native/src/cache.rs:137-321`
- **现状**：绕 `relon-ir` 缺 serde feature；只覆 v5-beta-1 子集，新 Op 会静默丢
- **修法**：给 `relon_ir::{Op, IrType, Module}` 加 `serde` feature，删整 180 行 + 镜像 tests
- **预估**：1-2 commit (feature gate)
- **trigger**：cache 加新 Op 没生效 / 想 bincode 替手撸。

### P3-5 [codegen-native] `write_value_into_builder` / `read_value_from_reader` 在 eval-api 而非这里
- **位置**：`crates/relon-codegen-native/src/evaluator.rs:1571-1720` 三 helper "Mirrors wasm-AOT"，但 wasm-AOT crate 已退役
- **修法**：迁到 `relon-eval-api::value_codec` 模块
- **trigger**：第二个 backend (wasm-AOT 再入 / 别的) 需要同一 schema-driven dispatch。

### P3-6 [trace-jit+bytecode] HotCounter 双份
- **位置**：`relon-trace-jit::counter` (table form) vs `relon-bytecode::hot_counter` (single-slot)
- **现状**：存储形态不同（table vs slot），语义共通的部分是 `RecordResult` enum + saturate 逻辑
- **修法**：提取 `relon-trace-abi::counter` 共享 `RecordResult` + `record_one` core helper
- **trigger**：再加第三个 backend hot counter / RecordResult 加新 variant 时双改痛。

### P3-7 [trace-recorder+trace-jit] `ty_to_observed` 与 `observed_type_from_ir_type` 双份
- **位置**：`recorder::lowering.rs:204` 私有版本 + `type_obs.rs:59` pub 版本
- **修法**：删私有版本，全 callers 走 pub `observed_type_from_ir_type`
- **预估**：1 commit。

### P3-8 [bytecode+eval-api] `decode_cap_bit` / 6-bit 枚举 4 处重复
- **位置**：`bytecode::vm.rs:299-306, 319-326, 438-447` + `eval-api::context.rs:590-595` + `capability.rs:216-221, 233-238`
- **修法**：`relon-eval-api` 加 `CapabilityBit::ALL` / `from_bit_index`，全 callers 走它
- **预估**：1-2 commit。

### P3-9 [trace-emitter+codegen-native] 3 处 JIT module setup 重复
- **位置**：`codegen/mod.rs:218-262` + `trace_install.rs:1537-1574` + `trace_inline.rs:389-414`
- **修法**：抽 `fn build_jit_module(opts: JitOpts) -> JITModule`
- **预估**：1 commit。

### P3-10 [analyzer+evaluator] `is_valid_identifier` / `validate_identifier` 重复
- **位置**：`evaluator::eval.rs:124` + `analyzer::rename.rs:196`
- **修法**：下沉到 `relon-parser`，分别 import
- **预估**：1 commit。

---

## P4 — 长尾 nits

| crate | 位置 | 项 |
|---|---|---|
| ir | `unicode/normalization.rs:208` | `canonical_reorder sort_by_key` 反复 ccc binary_search |
| ir | `glob.rs:90-91` | `s.chars().collect::<Vec<char>>()` 双扫 (cold) |
| ir | `op_visitor.rs:400-409` | 用 `Vec::with_capacity(body.len())` (已 ok) |
| analyzer | `references.rs:72-79` | `find_references` 全表 filter；可建反向索引 |
| analyzer | `goto_def.rs:113-131` | `smallest_node_at` early-return when covers 失败 |
| evaluator | `eval.rs:182-211 eval_root` vs `:228-338 run_main` | 头 6 行 step_counter / path_cache 清空相同 |
| evaluator | `stdlib.rs:817 StringJoin` | `parts: Vec<String>` 中间体；可单 `String::with_capacity` |
| codegen-native | `cache.rs:201-321 OpSerde` v5-γ 切 bincode 后整段删 | 半成品 |
| codegen-native | `glob_helper.rs / trace_glob_helper.rs` 双 wrapper | UTF-8 + null 防御抽 `try_match` |
| trace-jit | `IC check` MRU 已命中仍写回 | `inline_cache.rs:97-101` |
| trace-jit | `hops < 1024` defense (已修一处) | 其余 alias chain 检查可一致化 |
| trace-emitter | `four-tier ctor` (emit/with_pointer_ty/with_hooks/and_call_conv) | builder pattern |
| trace-emitter | `deopt-jump idiom` 14 处 | `guard_emit::synth_deopt_branch` helper |
| trace-recorder | `emit_dict_lookup_with_hint` 中间 wrapper 0 callers | 删 |
| eval-api | `Value::Display` 用 `{:?}` 兜底 List/Dict | 给合理 token |
| eval-api | `iter_id_counter` "wraps at u64::MAX" 注释 2 处 | dedupe |
| eval-api | `RuntimeError` 30+ variant 含 wasm trap | 拆 EvalError / WasmTrap |
| bytecode | `BcStdlibKind::{IntAbs,IntMin,IntMax}` 与 IR stdlib defs 双份 | 浅，跨 crate 痛 |
| bytecode | `BcVmError::HostFnError reason: String` 二次 format 丢结构化 | 改 `#[from]` |
| parser | `MULTI_CHAR_OPS` 线性扫 | 2-byte switch |
| parser | `raw_close` String 反复构造 | `[u8; 8]` 栈 buffer |
| parser | `*_for_cst` 别名 (utf8_codepoint_len_for_cst, scan_normal_string_for_cst) | re-export |
| lsp | `empty_document()` 手建 Node | 用 parser fallback |
| lsp | `miette_span_to_lsp_range` 2 份相同 | server.rs:468 + diagnostics.rs:67 |
| relon (facade) | `evaluate_source` Trusted/Sandboxed 双 path + 4 resolver ctor | 收敛 |
| fmt | `parse_strict` double-parse | 暂保留为兼容 |
| trace-abi | `DeoptStateSnapshot.value_stack_copy` cranelift path 无用 | doc |
| object-cache | `hex_lower` write! 手撸 vs hex crate | 路径冷无所谓 |
| cli | `try_parse_run_fast` 手撸 argv parser | cold-start perf 决策保留 |
| object-link | `linker_lld.rs` 整 module FeatureNotImplemented stub | 删 或 cfg |

---

## 整体进展记录

| Wave | Commit 范围 | 净 delta | 备注 |
|---|---|---|---|
| simplify 20-crate | `753b7fa..751c64b` | +284 / -816 | 全 crate 安全 dedup + dead code 删 |
| Tier A 收敛 | `d11ed8e..199e3b0` | +28 / -129 | EffectClass 合并 + type_repr_to_ir_type 复用 |
| **累计 simplify** | | **+312 / -945** | tests 2330 / 0 fail |

---

## 触发策略

- **P0** 项：路过对应文件务必清理（每个都标了 trigger 条件）。  
- **P1** 项：每月一次"单 crate deep refactor"专项 session，每次选 1-2 项。  
- **P2** 项：bench fixture 显示对应 hot path 占比 >5% 时才动手；落地前 + 后必跑 cmp_lua / trace_jit_smoke / W3-W12 bench 对照。  
- **P3** 项：等"再加第 N 个 backend"或"双份漂移导致 bug" 时触发。  
- **P4** 项：路过即修。

不要 batch P2 — 每项独立 bench 验证，避免相互掩盖 delta。
