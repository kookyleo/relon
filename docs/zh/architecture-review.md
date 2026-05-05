# Relon 架构评审与改造路线（2026-05）

> 本文记录对当前 Relon 实现的整体理解、批判性分析、以及为提升「作为宿主嵌入语言／DSL 母体」之能力所规划的重构路线。
>
> **状态**：阶段 A、B、C、D 全部落地。测试基线 122 → 126 项全绿（parser 63 + evaluator 51 + relon 7 + fmt 5）；clippy 无 warning；fmt 全部通过。

## 1. 现状速览

| 维度 | 实现 |
| --- | --- |
| 数据层 | JSON 原语 + List/Dict + spread + comprehension + f-string + raw-string |
| 表达式 | 算术 / 比较 / 逻辑 / 三元 / `\|` 管道 / `where` / `match` |
| 逻辑层 | 箭头函数、方法简写糖、闭包捕获 Scope；懒求值用 `Thunk + path_cache` |
| 引用层 | `&root / &sibling / &uncle / &this / &prev / &next / &index` + 可选链 `?.` |
| 类型层 | 标签式 `TypeNode` + `@schema`（nominal brand + predicate + `@default` + `@expect`）+ Schema-as-value 组合 |
| 模块层 | `@import(path, as=, spread=)` + `std/...` 虚拟模块（`include_str!` 内联） |
| 宿主接口 | `Context::register_fn` 注册原生函数（仅位置参数 `Vec<Value>`）+ `globals: HashMap<String, Value>` |

测试基线：parser 63 + evaluator 48 + relon 6 + fmt 5 = 122 项全绿。

## 2. 最大短板：宿主扩展接口缺位

> Relon 的目标是「嵌入宿主／DSL 母体语言」，但所有元能力都硬编码在 evaluator 中，宿主只能塞普通函数。

证据：

1. **装饰器调度是字符串硬编码**（`crates/relon-evaluator/src/eval.rs`）：
   - `eval_internal` 里 `name == "import"`、`name == "schema"` 直接 `if/else if` 字面量分支；
   - `apply_decorator` 把 `"value" / "expect" / "msg" / "error"` 当作关键字短路返回；
   - `extract_schema_fields_from_dict` 又再次分支 `"expect" / "default"`。
   - 结果：宿主无法注册新的「编译期装饰器」，例如 `@i18n("zh-CN")` 改写 schema 字段，或 `@deprecated` 在求值期附带 warning。

2. **模块加载只有两条硬编码路径**：`std/` 走 `include_str!`，其他一律 `std::fs::canonicalize`。无法做：
   - 沙箱白名单 / 黑名单
   - 内存模块（host 注入字符串模块）
   - 注册表 / URL / 数据库 / 多租户隔离

3. **类型系统是闭集**：`check_type_internal` 中内置类型集合（`Int / String / Bool / List / Dict / Enum / Number / Closure / ...`）写死在 `match`。宿主不能注入「这是一个 `Color` 名义类型，校验逻辑由我提供」。

4. **`RelonFunction` 接口贫弱**：`call(args: Vec<Value>, range)` 没有命名参数、没有类型签名、没有结构化错误返回。`@import("path", as="x")` 这种关键字参数特性，被宿主原生函数完全用不到。

5. **输出投影不可定制**：`to_json_value` 是唯一出口；想保留 schema 信息做 SDL/TS 类型生成、或者投影为宿主 `struct` 而绕过 serde，都没有 hook。

## 3. 其它实现层短板（来自 todo.md 已识别 + 本次评审新增）

1. **`is_root` 指针比较脆弱**：`Context::with_root(node.clone())` 与调用 `eval(&node, …)` 双 Arc 化导致指针不一致，`is_root` 永远 false，依赖懒求值兜底。
2. **Predicate 合成语义粗糙**：`extract_schema_for_node` 中 `Schema + Schema` 对 predicate 用「右侧非 Wildcard 即覆盖」的策略，按设计应为逻辑 AND 合并。
3. **`Schema + Dict` 路径不对齐**：`Operator::Add` 走的是 `deep_merge`，没有走更精细的 `extract_schema_for_node`，所以「`Schema + Dict` 添加新字段」目前不可行（仅能改默认值）。
4. **`eval.rs` 单文件 2249 行**：`Scope`、`Thunk`、reference 解析、import、schema 抽取、装饰器调度、binary/unary 求值、type checker 全在一处。耦合大、心智负担高。
5. **Scope 状态字段过多**：`parent / path_node / locals / current_dir / cache_namespace / reference_root / reference_root_parent / reference_root_scope / list_context / thunks` 共 10 个字段，部分语义重叠（`path_node` vs full_path、`reference_root_parent` vs `reference_root_scope`）。
6. **多份变量解析路径**：`resolve_variable / resolve_reference / lookup_value_path / eval_reference_path / eval_reference_path_from / resolve_dict_reference_step` 共 6 套，存在重复逻辑。
7. **`Value` 全量 clone**：所有传递、缓存、合并都 deep clone；大型嵌套字典代价不低。
8. **`Closure.captured_env: Arc<Scope>` 容易泄漏整个文档作用域**——闭包持续持有 Arc<Scope>，导致 root_node 与 scope 树长生命周期。

## 4. 改造路线

### ✅ 阶段 A：宿主扩展骨架（已落地）

1. **`DecoratorPlugin` trait + 注册表（A1 ✅）**
   - 三个默认 noop hook：`pre_eval`（@import / @schema 类，可 `Pass` / `Rescope` / `Override`）、`wrap`（普通装饰器）、`schema_field_meta`（@expect/@default 类）
   - evaluator 中所有 `name == "import" / "schema" / "expect" / "msg" / "error" / "default" / "value"` 字面量分支已改为查 `Context::decorators` 注册表
   - 内置 7 个 plugin（`import / schema / expect / msg / error / default / value`）在 `Context::new` 预注册；宿主只需 `ctx.register_decorator("name", Arc::new(MyPlugin))` 即可加新装饰器、或同名覆盖内置
   - 文件：`crates/relon-evaluator/src/decorator.rs` + `builtin_decorators.rs`

2. **`ModuleResolver` trait + 链表（A2 ✅）**
   - 按顺序询问 resolver 链，首个返回 `Some(ModuleSource)` 胜出；`Err(_)` 短路全部
   - 默认链：`StdModuleResolver`（处理 `std/` 前缀）+ `FilesystemModuleResolver`（兜底）
   - 宿主入口：`ctx.prepend_module_resolver(...)` / `ctx.append_module_resolver(...)`
   - `ModuleSource { canonical_id, source, current_dir }` 三元组承载 cache key + 源码 + 嵌套 import 的相对路径基准
   - 文件：`crates/relon-evaluator/src/module.rs`

3. **`RelonFunction` 升级到 `NativeArgs`（A3 ✅）**
   - `call(args: NativeArgs, range)`，`NativeArgs { positional: Vec<Value>, named: HashMap<String, Value> }`
   - 现有 stdlib 28 个函数一次迁移完成（每个函数顶部 `let args = args.into_positional();`）
   - 命名参数从此对宿主原生函数也是头等公民
   - 文件：`crates/relon-evaluator/src/eval.rs`、`stdlib.rs`

### ✅ 阶段 B：实现层稳定化（已落地）

4. **`is_root` 指针比较修复（B4 ✅）**：新增 `Evaluator::eval_root` 入口，让 `scope.reference_root` 与传入 `&Node` 共享同一个 `Arc<Node>`；`facade`、CLI、test helper 全部迁移。同时修了 closure body 的潜伏 bug：body 创建时 `Arc::new(body.clone())` 后传 `&*body_arc`，并把 `reference_root_scope` 重置为 `None` 以避免外层 dict 的 reference scope 泄漏到 closure 内部。
5. **拆分 `eval.rs`（B5 ✅）**：原 2350 行单文件已分解为 7 个聚焦模块：`decorator.rs`（plugin 接口）、`module.rs`（resolver 链）、`native_fn.rs`（NativeArgs / RelonFunction）、`builtin_decorators.rs`（7 个内置 plugin）、`scope.rs`（Scope / Thunk / ListContext）、`schema.rs`（type checker + schema 抽取）、`reference.rs`（`&root/&sibling/...` 解析 + Thunk forcing）、`arithmetic.rs`（binary / unary / 数值算术）。剩余 `eval.rs` 约 880 行，专注顶层 `eval_internal` dispatcher、closure 调用、模块加载与 `prepare_dict_scope`。每个模块以「跨文件 `impl Evaluator`」组织，避免引入额外间接层。
6. **Predicate AND 合成（B6 ✅）**：`SchemaField.predicate: Value` 改为 `predicates: Vec<Value>`，`extract_schema_for_node` 与 `Value::deep_merge` 在 `Schema + Schema` 路径上累积 predicate 而非右侧覆盖；`check_custom_schema` 顺序 AND 短路。新增 `test_schema_composition_and_combines_predicates` 三组用例锁定行为。
7. **`Schema + Dict` 添加字段路径（B7 ✅）**：新增 `Evaluator::merge_schema_with_dict_pairs` 走 AST 而非求值后再合并。每对字段做 hybrid 派发：带 `type_hint` / 闭包 predicate 的形如 `Type field: pred` 视为 schema 字段定义（add/replace），无类型的字面量值视为 `default_value`（落到既有字段或新建 `Any` 默认字段）。`extract_schema_for_node` 的 Binary 分支同步路由，覆盖 `@schema X: Base + { ... }` 顶层语法。两条路径下 `Schema + Dict` 现在能添加新字段而非仅改默认值，原 `test_schema_composition_defaults` 用例从注释状态恢复，新加 `test_schema_plus_dict_adds_typed_fields` 锁定。

### ✅ 阶段 C：可观测与性能（已落地）

9. **结构化诊断（C9 ✅）**：`RuntimeError::CircularReference` 从 `Vec<String>` tuple 升级为 struct variant `{ cycle, range }`，附带触发点 `TokenRange` 与 miette label。Display 用 `→` 而非 `Debug` 渲染 cycle。所有 variant 的 miette label 文本结构化（`expected X, got Y` / `divisor is zero` / `triggers the cycle` / ...），`VariableNotFound` / `DivisionByZero` / `InvalidIdentifier` / `ModuleNotFound` / `CircularImport` 补 `help(...)` 上下文提示。
10. **`Value` clone 优化（C10 ✅）**：`Value::List` 改为 `Arc<Vec<Value>>`，`Value::Dict` 改为 `Arc<ValueDict>`。clone 退化为引用计数 bump；mutation 走 `Arc::make_mut` (CoW)。`Value::list(...)` / `Value::dict(...)` / `Value::list_mut()` / `Value::dict_mut()` 收敛构造与就地修改入口。serde 通过 `serde = { features = ["derive", "rc"] }` 透传 Arc 序列化；外部 API 形态保持兼容（destructuring `Value::List(l)` / `Value::Dict(d)` 通过 Arc 的 Deref 仍可读访问内部 vec / map）。

### ✅ 阶段 D：投影与扩展（已落地）

11. **`Projector` trait（D11 ✅）**：`relon::projector` 模块定义 `trait Projector { type Output; type Error; fn project(&self, value: &Value) -> Result<Output, Error>; }`，宿主自定义投影目标。`JsonProjector` 是默认实现，复用既有 `to_json_value` 语义；`relon::project_with(&projector, &value)` / `relon::project_from_str(source, &projector)` 是入口。
12. **`BrandRegistry`（待迭代）**：暂未实现；当前 `@schema` 已能覆盖大多数名义类型用法，独立注册表延后到有具体宿主需求时再加。

### 待迭代：

- **错误聚合（C8）**：`@schema` / `@ensure.*` 失败时一次返回多条而非 fail-fast；当前未实现。



## 5. 宿主集成入口（已可用）

```rust
use relon_evaluator::{
    Context, DecoratorPlugin, EvaluatedArg, Evaluator, ModuleResolver, ModuleSource,
    NativeArgs, PreEvalOutcome, RelonFunction, Scope, SchemaField, Value,
};
use relon_parser::{parse_document, CallArg, Node, TokenRange};
use std::sync::Arc;

// 1) 自定义 decorator —— @audit("note") 在求值后挂一条审计日志，原值透传
struct AuditDecorator;
impl DecoratorPlugin for AuditDecorator {
    fn wrap(
        &self,
        _eval: &Evaluator<'_>,
        value: Value,
        _scope: &Arc<Scope>,
        args: &[EvaluatedArg],
        _range: TokenRange,
    ) -> Result<Value, relon_evaluator::RuntimeError> {
        if let Some(arg) = args.first() {
            eprintln!("[audit] {}: {value}", arg.value);
        }
        Ok(value)
    }
}

// 2) 自定义 module resolver —— 把 "host://..." 路径映射到内存中的字符串模块
struct HostModuleResolver;
impl ModuleResolver for HostModuleResolver {
    fn resolve(
        &self,
        path: &str,
        _scope: &Arc<Scope>,
        _range: TokenRange,
    ) -> Result<Option<ModuleSource>, relon_evaluator::RuntimeError> {
        if let Some(name) = path.strip_prefix("host://") {
            return Ok(Some(ModuleSource {
                canonical_id: format!("host:{name}"),
                source: "{ greeting: \"hi\" }".to_string(),
                current_dir: String::new(),
            }));
        }
        Ok(None)
    }
}

// 3) 自定义原生函数（带命名参数）
struct GreetFn;
impl RelonFunction for GreetFn {
    fn call(
        &self,
        args: NativeArgs,
        _range: TokenRange,
    ) -> Result<Value, relon_evaluator::RuntimeError> {
        let name = args
            .get_named("name")
            .or_else(|| args.get(0))
            .cloned()
            .unwrap_or(Value::String("world".to_string()));
        Ok(Value::String(format!("hello, {name}")))
    }
}

// 装配
let source = "{ msg: \"ok\" }";
let node = parse_document(source).unwrap();
let mut ctx = Context::new().with_root(node);
ctx.register_decorator("audit", Arc::new(AuditDecorator));
ctx.prepend_module_resolver(Arc::new(HostModuleResolver));
ctx.register_fn("greet", Arc::new(GreetFn));

let eval = Evaluator::new(&ctx);
let result = eval.eval_root(&Arc::new(Scope::default())).unwrap();
```

## 6. 兼容承诺

未发布期间不保留兼容承诺；本轮重构按「最合理架构、最优实现」推进。重要破坏性变更：

- `Value::List(Vec<Value>)` → `Value::List(Arc<Vec<Value>>)`，`Value::Dict(ValueDict)` → `Value::Dict(Arc<ValueDict>)`。读取通过 Arc Deref 兼容旧代码；构造请用 `Value::list(...)` / `Value::dict(...)`。
- `RuntimeError::CircularReference(Vec<String>)` → `CircularReference { cycle, range }`。模式匹配请改用 `RuntimeError::CircularReference { .. }`。
- `SchemaField.predicate: Value` → `predicates: Vec<Value>`。
- `Evaluator::eval_root` 是新的、推荐的根入口；旧的 `eval(&node, &scope)` 仍在但不再保证 `is_root` 触发。

## 7. 实施清单

每一步结束都跑：

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
```

最终 gate 状态（2026-05）：126 项测试全绿，clippy 无 warning，fmt 全部通过。`cargo run -q -p relon-fmt -- --check fixtures/*.relon ...` 在 baseline 就因 `fixtures/closures.relon` 的 fmt parse 异常而报错，pre-existing，未在本轮覆盖范围内。

