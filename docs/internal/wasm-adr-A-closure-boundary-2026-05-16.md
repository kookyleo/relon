# ADR-A：Closure 跨 host↔wasm 边界（2026-05-16）

> Phase 0 子项 4/8。
> 上游：[`wasm-backend-design-draft.md`](./wasm-backend-design-draft.md)
> §二 待定子问题 A "Closure 能不能跨 host↔wasm 边界"。

## Context

WASM 后端打算让 host 通过 binary handshake 给 `#main` 传参数、收返
回值。Relon 的 `Value` 包含 `Closure { params, body, captured_env }`
变体——一个高阶函数值，可以作为参数或返回值出现在 dict 字段、list
元素等位置。

问题：closure 能不能跨 host↔wasm 边界？

具体场景：

1. **Closure 作为 `#main` 入参**：`#main(Closure<Int, Int> f) -> Int`
2. **Closure 作为 `#main` 返回值**：`#main(...) -> Closure<Int, Int>`
3. **Closure 作为 dict 字段值 / list 元素值**：`#main(...) -> { f: Closure<..>, ... }`
4. **Closure 在 wasm 内部使用**（不跨边界）：`xs.map((n) => n * 2)`

## Decision

**禁止 closure 跨 host↔wasm 边界**：

- 场景 1（入参）：analyzer + codegen 双重 ban，错误形状
  `ClosureParamNotAllowed { where: #main, range }`
- 场景 2（返回值）：同上，错误形状 `ClosureReturnNotAllowed`
- 场景 3（dict 字段 / list 元素值）：只要 schema 静态包含 Closure
  类型且这个 dict 出现在边界，同上 ban
- 场景 4（wasm 内部使用）：**完全允许**，照常工作

## Rationale

### 1. analyzer 已经 ban 场景 1+2

`relon-analyzer` 在 `#main` 签名校验阶段已经禁止 Closure 入参 / 返回
（roadmap.md 阶段 I "ban bare 泛型" 之后通过）。本 ADR 在 wasm codegen
阶段**重复 ban + 给出更具体错误信息**，而不是引入新限制。

### 2. Closure 没有可序列化的二进制 layout

Closure 由三部分组成：

- `params: Vec<String>` — 参数名
- `body: Node` — AST 节点（**深嵌套树结构**）
- `captured_env: Arc<Scope>` — 捕获环境（**指向其他 Value 的引用**）

后两者**不可能编码进 binary handshake 协议**：

- `body: Node` 是用户 source code 的 AST 树，host 没法以 binary buffer
  形式合理表达
- `captured_env: Arc<Scope>` 引用 evaluator 内部 Mutex 状态，跨进程 /
  跨 wasm 边界传递无意义

强行支持需要：(a) 把 body 重新 lowering 成 wasm bytecode 嵌进
buffer + (b) 把 captured_env snapshot 编码进 buffer。这是**写一个
完整的 closure-serialization 协议**，等同于把 wasm codegen 自身做成
runtime-pluggable——投入产出比极差。

### 3. 用户实际场景不需要

实测 fixture 库（`fixtures/*.relon`）+ playground preset：

- 0 个 `#main` 把 Closure 作为入参
- 0 个 `#main` 返回带 Closure 字段的 dict
- 大多数 high-order 用法是 **schema-rooted method**（`xs.map(x => ...)`），
  closure 在 .map() 调用站点构造、立即消费，不跨任何边界

scenarios where closure crossing IS useful（plug-in pattern、callback
注册）也都通过**已有的两条机制**绕过：

- **host 注册 native fn**：`Context::register_fn("my_callback", gate, func)`
  让 host 提供 Rust 闭包当 native fn 用——比传 Relon closure 更安全
  也更快
- **schema-rooted method**：`#extend MyType with { hook(...) -> ... : ... }`
  定义 method，wasm 内部走 dispatch

两条已存路径覆盖 99% 的 plug-in 需求，**不需要**给 closure 加边界
穿透能力。

### 4. 与决策 1（Binary Handshake）一致

决策 1 走 binary memory handshake——所有跨边界类型都必须有 stable
binary layout。Closure 没有这个属性。**强行支持就等于回退到决策 1
的 fallback 模式（JSON 序列化）**——而决策 1 已经明确拒绝 JSON
fallback。

### 5. 与决策 4（静态拓扑 + eager）一致

决策 4 假设 dict 字段依赖在 codegen 期能完整 lowering 为顺序求值。
如果允许 closure 作为 dict 字段值跨边界，那 host 可能在 wasm 加载后
"注入"一个 closure 字段——而该 closure 的 body 又可能引用其他字段——
打破 codegen 期的 DAG 不变量。**禁止跨边界自然消除这个 corner**。

## Implementation hints

### analyzer 侧（已存在，仅强化错误信息）

`relon-analyzer/src/main_signature.rs`（或类似）现有的 `#main` 签名
校验加诊断：

```rust
if param_type.contains_closure() {
    diagnostics.push(Diagnostic::ClosureParamNotAllowed {
        param_name: param.name.clone(),
        range: param.range,
        hint: "Pass a native function via Context::register_fn instead, or wrap behavior in a schema method.",
    });
}
```

### codegen 侧（新增）

`relon-codegen-wasm/src/main_abi.rs`：

```rust
fn validate_main_signature(sig: &MainSignature) -> Result<(), CodegenError> {
    for param in &sig.params {
        if param.type_hint.contains_closure_recursive() {
            return Err(CodegenError::ClosureInBoundary {
                kind: BoundaryKind::Param(param.name.clone()),
                range: param.range,
            });
        }
    }
    if let Some(ret) = &sig.return_type {
        if ret.contains_closure_recursive() {
            return Err(CodegenError::ClosureInBoundary {
                kind: BoundaryKind::Return,
                range: ret.range,
            });
        }
    }
    Ok(())
}
```

`contains_closure_recursive`：对嵌套 schema / list / dict 递归
搜索；任何深度发现 `TypeNode::Closure` 即返回 true。

### wasm 内部不受影响

`xs.map((n) => n * 2)` 这种站点：
- analyzer 给 method body 关联 closure value
- IR 把 closure 表达成 `(funcref, captured_env_ptr)` 二元组
- 调 `.map` 的 wasm 实现接 funcref 调用
- 全程不出 wasm 模块边界

## Consequences

正面：

- API 表面清晰，类型系统不留歧义
- binary handshake protocol 不需要 closure encoding
- host-side SDK 不需要任何"Relon closure 序列化"代码

负面：

- 用户如果想跨边界传"行为"必须改写法（用 native fn 注册或 schema
  method）——这是教育成本，不是技术成本
- 未来如果出现非常强烈的"必须跨边界传 closure"业务场景，需要重新
  立项 v2 ABI 支持，伴随 abi_version bump

## 测试覆盖

Phase 1 实施时必须包含：

- analyzer test：`#main(Closure<Int, Int> f)` 报 `ClosureParamNotAllowed`，
  diagnostic 有 hint
- codegen test：同上但绕过 analyzer 直接喂 codegen——返回
  `CodegenError::ClosureInBoundary { kind: Param("f") }`
- codegen test：return type 带 closure 同样 reject
- positive test：`#main(List<Int> xs) -> Int : xs.map(x => x * 2).sum()`
  内部 closure 用 OK
