# Relon 2.0 Todo List

## 目标 (Target)
实现 Relon 2.0 的高级契约特性（Schema 组合与身份守卫）。
1. **身份卫兵 (Identity Guard)**: 已完成。带有 `brand` 的字典在 `+` / `dict.merge` 修改后会自动重新校验。
2. **Schema 组合 (Composition)**: 已完成。支持 `Schema + Schema` 与 `Schema + Dict` 派生新 Schema。

## 已解决的问题 (Resolved Issues)
1. **身份守卫功能恢复**：重启用 `eval_binary` 中的 Identity Guard（`Operator::Add` + 带 brand 的 Dict 自动 `check_type`）。
2. **字典展开 (Spread) 作用域泄漏与引用解析**：修复了 `resolve_dict_reference_step` 与 `Expr::Dict` 求值中 `&sibling.X` 在展开上下文中错查的问题。
3. **`&sibling` 作用域雪崩**：通过 `reference_root_scope` 阻断了每次 sibling 引用都重新评估全量 ROOT AST 的递归。
4. **Schema 组合栈溢出（`test_schema_composition_mixins`）** —— 见下文。

## Schema 组合栈溢出修复纪要
**重现路径**：
```relon
@schema Base: { String type: * },
@schema Button: &sibling.Base + { String label: * },
Button ok_btn: { type: "btn", label: "OK" }
```

**根因（无限递归链）**：
1. `@schema` 装饰器抽取分支只识别 `Expr::Dict`，`Expr::Binary` 直接 fall-through，没有专门的 schema 合成路径。
2. `prepare_dict_scope` 对所有带 `@schema` 的条目做急切求值；遇到 Binary 时实际上是按普通 `+` 运算去求值。
3. Binary 的左侧 `&sibling.Base` 触发 `resolve_reference` → `eval_reference_path`。由于 `eval_doc` / CLI 把根节点 clone 进 `Arc::new(node.clone())` 又把原 `&node` 传给 `eval`，`is_root` 的指针比较恒为 false，`reference_root_scope` 永远不会被设到外层 `dict_scope` 上。
4. 找不到匹配的 `reference_root_scope`，每次 sibling 跳转都构造一个全新 root_scope，再次跑 `prepare_dict_scope`；该函数又急切求值 `Button` Binary → 再次走 `&sibling.Base` → 死循环 → 栈爆。

**修复（最小改动，三处）**：
- `eval.rs::eval_internal` `@schema` 分支扩展为同时识别 `Expr::Dict` 与 `Expr::Binary(Add, _, _)`。新增 `extract_schema_for_node` / `extract_schema_fields_from_dict` 两个 helper：直接遍历 AST 把两侧都解释为 schema 字段并合并（右侧 `{ String label: * }` 不再被错误地走 Dict 求值路径，避免丢失 `Type field: pred` 的语义）。
- `eval.rs::prepare_dict_scope` 仅对 *Dict-bodied* 的 `@schema` 做急切求值；Binary 等组合形式只注册 thunk，留待主循环 / 类型校验时通过 `force_thunk` 懒求值，从根本上切断重入。
- 清理了 `eval_internal`、`eval_binary` 和 `check_type_internal` 中的 `println!` DEBUG 输出与未使用的 `tname_key` 变量。

**测试结果**：`cargo test --workspace` 全绿（含 `relon-evaluator` 48 项、`relon-parser` 63 项等共 122 项）。

## Relon 2.x 架构改造（详见 `docs/zh/architecture-review.md`）

### ✅ 阶段 A — 宿主扩展骨架（已落地）
- **A1 ✅ `DecoratorPlugin` trait + 注册表**：`name == "import" / "schema" / "expect" / "msg" / "error" / "default" / "value"` 等字面量分支已改为查表；7 个内置 plugin 在 `Context::new` 预注册；新增文件 `decorator.rs` + `builtin_decorators.rs`。
- **A2 ✅ `ModuleResolver` trait + 链表**：默认链 `StdModuleResolver + FilesystemModuleResolver`；`Context::prepend_module_resolver` / `append_module_resolver` 暴露给宿主；新增文件 `module.rs`。
- **A3 ✅ `RelonFunction` 升级到 `NativeArgs`**：含 `positional` / `named` 双视图；stdlib 28 个函数一次性迁移完毕。

### ✅ 阶段 B — 实现层稳定化（已落地）
- **B4 ✅ `is_root` 指针比较**：新增 `Evaluator::eval_root` 入口；`facade`、CLI、test helper 全部迁移；同步修了 closure body 的 `reference_root_scope` 泄漏 bug。
- **B5 ✅ 拆分 `eval.rs`**：从单文件 2350 行降到 ~880 行。抽出的子模块：`scope.rs`（`Scope/Thunk/ListContext` + `child` 工厂）、`schema.rs`（`check_type` + schema 抽取 + `merge_schema_with_dict_pairs`）、`reference.rs`（`&root/&sibling/&prev/...` 解析、Thunk forcing、path cache）、`arithmetic.rs`（Binary / Unary / 数值 op）、外加阶段 A 已抽的 `decorator.rs / module.rs / native_fn.rs / builtin_decorators.rs`。每个子模块用「跨文件 `impl Evaluator`」组织，无额外 trait 间接层。
- **B6 ✅ Predicate AND 合成**：`SchemaField.predicate: Value` → `predicates: Vec<Value>`；`extract_schema_for_node` 与 `Value::deep_merge` 在 `Schema + Schema` 路径上 AND 累积；`check_custom_schema` 顺序短路；新增 `test_schema_composition_and_combines_predicates` 锁定行为。
- **B7 ✅ `Schema + Dict` 添加字段**：新增 `merge_schema_with_dict_pairs`，按 AST 而非求值后合并。每对字段 hybrid 派发：`Type field: pred` → schema 字段定义；裸字面量值 → 默认值 patch（覆盖既有字段或新建 `Any` 默认字段）。`extract_schema_for_node` Binary 分支同步路由。原 `test_schema_composition_defaults` 从注释状态恢复，新加 `test_schema_plus_dict_adds_typed_fields`。

### ✅ 阶段 C — 可观测与性能（已落地）
- **C8 错误聚合（待迭代）**：当前未实现，`@schema` / `@ensure.*` 仍是 fail-fast。
- **C9 ✅ 结构化诊断**：`RuntimeError::CircularReference` 升级为 struct variant `{ cycle, range }`，`Display` 用 `→` 渲染；所有 variant 的 miette label 文本结构化（`expected X, got Y` / `divisor is zero` / `triggers the cycle` / ...），`VariableNotFound` / `DivisionByZero` / `InvalidIdentifier` / `ModuleNotFound` / `CircularImport` 补 `help(...)`。
- **C10 ✅ `Value` clone 优化**：`Value::List(Arc<Vec<Value>>)` + `Value::Dict(Arc<ValueDict>)`；clone 退化为 Arc bump，mutation 走 `Arc::make_mut` (CoW)。新增 `Value::list / dict / list_mut / dict_mut` 收敛构造与就地修改。serde 通过 `features = ["rc"]` 透传。

### ✅ 阶段 D — 投影与扩展（已落地）
- **D11 ✅ `Projector` trait**：`relon::projector::Projector { type Output; type Error; fn project(&self, value: &Value) -> Result<...>; }`；默认 `JsonProjector` 复用旧 `to_json_value` 语义；`relon::project_with(&projector, &value)` / `relon::project_from_str(source, &projector)` 入口。新增 `custom_projector_extracts_typed_field_set` 测试演示宿主接入。
- **D12 `BrandRegistry`（待迭代）**：当前 `@schema` 已能覆盖大多数名义类型用法，独立注册表延后到有具体宿主需求时再加。

### 阶段 E — 暂缓
- **抽象契约 (Parameterized Schemas)**：`@schema Page<T>: …`，等泛型语法落地后再做。

### 测试基线
parser 63 + evaluator 51 + relon 7 + fmt 5 = **126 项全绿**。clippy `--all-targets -- -D warnings` 通过；`cargo fmt --check` 通过。
