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

## 后续可选优化
1. **`is_root` 指针比较脆弱**：`Context::with_root(node.clone())` 与调用 `eval(&node, …)` 的双 Arc 让指针不一致，`is_root` 永远 false。当前依赖懒求值 + path_cache 也能跑通，但留着隐患。建议让 `eval_doc` / CLI 统一传 `&*ctx.root_node`。
2. **谓词组合**：`extract_schema_for_node` 中 Binary 合并时，对 predicate 采用「右侧非 Wildcard 即覆盖」的简单策略；按设计文档应改为逻辑 AND 合并（需要把 `SchemaField.predicate` 从单值改为列表或合成新闭包）。
3. **`dict.merge(Schema, …)` 对齐**：当前 `Operator::Add` 对 `(Schema, Schema)` / `(Schema, Dict)` 走 `deep_merge`，未走 `extract_schema_for_node` 那条更精细的路径。`@schema` 上下文外的 `Schema + Dict` 仍只能设默认值、不能新增字段；如要打通就把这条路径也接到 `extract_schema_for_node` 上。
4. **抽象契约 (Parameterized Schemas)**：`@schema Page<T>: …` 暂未实现，待泛型语法落地后再做。
