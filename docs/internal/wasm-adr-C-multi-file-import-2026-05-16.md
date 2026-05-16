# ADR-C：多文件 `#import` 的 lowering 策略（2026-05-16）

> Phase 0 子项 6/8。
> 上游：[`wasm-backend-design-draft.md`](./wasm-backend-design-draft.md)
> §二 待定子问题 C "多文件 `#import "lib/utils"` 怎么 lowering"。

## Context

Relon 支持多文件项目：

```relon
;; lib/utils.relon
#schema Money: { Int amount: *, String currency: * }

#default add_tax(m: Money) -> Money : {
    amount: m.amount * 110 / 100,
    currency: m.currency,
}

;; main.relon
#import { add_tax, Money } from "lib/utils"

#main(Money m) -> Money : add_tax(m)
```

tree-walker 通过 `ModuleResolver` 在运行期 resolve `#import` 路径。
WASM 后端编译期就要决定 import 的 fn / schema 如何呈现在 bytecode 里。

三个候选策略：

- **C1 静态 inline**：编译期把所有 `#import` 的 module **inline 展开**
  进同一个 wasm module；运行期单文件
- **C2 component model link**：每个 module 编成独立 wasm component；
  主 module via wit-bindgen 接口 link
- **C3 dynamic linker**：编出多个 wasm module；host runtime 在加载期
  通过 wasm dynamic link 把 import 解析为另一个 module 的 export

## Decision

**MVP 走 C1（静态 inline）**。C2 / C3 暂不实施，留 v2 / Phase Z+。

## Rationale

### 1. wasm-backend-design 决策 2 已选 "stdlib self-contained"

决策 2 已经承诺把 stdlib 完整 inline 进 wasm 模块。用户模块走同样
路径——**两条路径合并为一条**："所有 Relon 代码（stdlib + 用户 import +
main）lowering 到同一个 wasm bytecode 容器"——比维护两条路径简单
得多。

### 2. component model 生态尚在演进

wit-bindgen / WASI 0.2 / component-model tooling 是 wasm 生态的方向，
但当下：

- wasmtime 对 component model 的稳定支持仍在迭代
- wasm-encoder 库的 component-model API 比 core API 不稳定
- 浏览器端 component model 加载需要 polyfill
- 与 tree-walker 的 `ModuleResolver` 不直接 mapping，需要把
  Relon module 概念翻译成 component import/export interface

MVP 不应该把 codegen 工作量绑在尚未稳定的工具链上。Phase 1-9 都走 C1
完全够用；C2 等 wasm 生态成熟（≥1 年）后再立项替换，**自然 ABI bump
就是切换点**。

### 3. 静态 inline 与决策 4（topo eager）配合好

决策 4 要求 dict 字段做全局 DAG 拓扑排序。多文件场景下，`lib/utils`
的 `#schema Money` 在 main.relon 的 dict 字段类型推断里出现——分析
跨文件的 dependency graph 在**已 inline 的同一 AST 树**里做最简单。
C2 / C3 把模块边界做硬，跨模块 DAG 分析复杂度上一个量级。

### 4. analyzer 已经做了 inline 等价的工作

`relon-analyzer/src/workspace.rs::WorkspaceTree` 已经把多文件分析合并
成 single workspace 视图。`WorkspaceImportIndex` 给跨 module schema /
fn 解析做了 forwarding。**wasm codegen 直接吃 WorkspaceTree** 就拿到
inline 形式的所有内容——不需要再做额外 module-boundary 处理。

### 5. 模块体积可控

担心 inline 让 wasm 模块体积爆炸？实测推算：

- stdlib bundled = ~50-200 KB（决策 2 已经接受）
- 用户 lib（典型项目 5-20 个文件，每个 50-500 LOC）= ~10-80 KB lowering
  bytecode
- main = ~5-20 KB
- 合计：~65-300 KB per wasm module

对比一份 Java jar 几 MB / 一份 .NET assembly 几 MB，wasm 模块这个体积
**远小于其他语言生态**。如果某用户真做 100+ 文件 monorepo 的 wasm
build 体积变成问题，那时再切到 component model 也不迟——届时已经
有 v1 实测数据驱动决策。

## Implementation hints

### codegen 输入：WorkspaceTree（不是单 AnalyzedTree）

```rust
// relon-codegen-wasm/src/lib.rs

pub fn compile_workspace(
    ws: &WorkspaceTree,
    entry_module: &str,    // e.g. "main"
) -> Result<WasmModule, CodegenError> {
    // ws 已经把所有文件 analyze 完，schemas / fns 跨 module 可见
    // codegen 把它视为单一逻辑命名空间
    ...
}
```

`entry_module` 指定哪个文件含 `#main`——它是 entry 点。其他 module
的内容按 reachability 收进同一份 wasm。

### Dead-code elimination

inline 后大量 unused stdlib fn 会被一起编。MVP 阶段可以**接受**
（决策 2 已经接受 +50-200 KB 体积）。Phase 9 bench 后可以加 DCE
pass，砍掉 unreferenced fn——但 DCE 是优化，不是 correctness 必需。

DCE pass 放 `relon-ir` 层（参考 ADR crate structure）：

```rust
// relon-ir/passes/dce.rs
pub fn dead_code_elimination(ir: &mut Module) -> usize {
    let reachable = compute_reachable_from_entry(ir);
    let removed = ir.fns.len() - reachable.len();
    ir.retain_fns(reachable);
    removed
}
```

### Schema deduplication

不同 module 可能引用同一个 schema（`Money`）通过 import 链。codegen
**必须**只 emit 一份该 schema 的 binary layout 元数据——通过
`canonical_schema_hash`（参考 ADR-B 的 hash 定义）作为 dedup 键。

### path 与 fn name 冲突

`lib/utils::add_tax` 和 `lib/other::add_tax` 都叫 `add_tax`——wasm
内部需要 unique fn name。codegen 走 mangling 规则：

```
mangled(module_path, fn_name) = "<sanitized_module_path>__<fn_name>"
e.g. "lib_utils__add_tax"
```

mangle 出来的 name 进 wasm function index；不暴露给 source map（src
map 仍然走 file_idx + line + col，不是 mangled name）。

### `#import "std/..."` 与 user import 的区别

`#import "std/string"` 等 stdlib import 已经在决策 2 处理（stdlib
bundled）。codegen 看到 stdlib import 时**不需要**做 inline——
stdlib 已经在 bundle 表里。

user import（`#import "lib/utils"`）走本 ADR 的 inline 路径。

## Consequences

正面：

- 与 analyzer 的 WorkspaceTree 设计一一对齐；最小工作量
- 单 wasm 文件部署，DevOps 简单
- 与决策 2/4 配合好

负面：

- 模块体积单调上涨（无跨模块复用）；典型项目可控
- 用户改一个 lib 文件需要重编整个 wasm（不是问题——build 系统会
  caching analyzer 输出）
- 不能跨进程动态加载 module（v1 不支持，与 sandboxed runtime 模型
  一致）

## 测试覆盖

Phase 4+ 实施时：

- `lib/foo.relon` + `main.relon` 经 codegen 出一个 wasm，运行正确
- 同名 fn 在不同 module 不冲突（mangling 工作）
- circular `#import` 由 analyzer 提前拒绝（已有），codegen 不会触达
- `#import "std/string"` 与 user import 协同正确（stdlib bundled +
  user inlined）

## 未来路径（v2+）

如果有用户场景驱动跨 module 复用：

- v2 引入 component model：每个 lib 编成独立 component，主模块
  import 之；ABI version bump 触发明确 reject
- 或：dynamic link via wasm `dynamic-link` proposal（更轻的方案）

无论哪条都是替换 codegen 的 inline pass，不影响其他决策——所以本
ADR 选 C1 不会锁死未来。
