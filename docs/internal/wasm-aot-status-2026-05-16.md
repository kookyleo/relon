# Wasm AOT backend 支持子集快照（2026-05-16）

> 本文档配套 `wasm-bench-report-2026-05-16.md` 一起读：bench 报告说"性能
> 怎么样"，这里说"哪些语法 / 类型现在能跑、哪些还不能"。给 host 集成方
> 和后续 phase 工作清单用。
>
> 范围：`WasmAotEvaluator::from_source` 入口 + `Evaluator::run_main`
> 路径，即 Phase 8 之后高层 surface 暴露的功能。低层 BufferBuilder /
> BufferReader 接口可表达的子集更广（method_dispatch_smoke 测试覆盖
> Schema 类型 `#main` 参数），但 host 一般不直接消费低层 API。
>
> 状态符号：
>
> - ✅ 已支持，有 smoke 测试覆盖
> - ⚠️ 部分支持，存在已知坑（见备注）
> - ❌ 不支持，会在 build 或 run 时返回错误
> - 🚧 计划中（详见报告"未来工作"）

## 一、`#main` 参数类型

| 类型 | 状态 | 备注 |
| --- | :-: | --- |
| `Int`            | ✅ | `parity_int_doubling` |
| `Float`          | ✅ | BufferBuilder write_float / read_float |
| `Bool`           | ✅ | Phase 3.a |
| `Null`           | ✅ | Phase 2.a |
| `String`         | ✅ | 指针间接 tail record，`parity_string_passthrough` |
| `List<Int>`      | ✅ | Phase 2.c 指针间接布局 |
| `List<其他>`     | ❌ | `UnsupportedFieldType` |
| `Schema { ... }` | ❌ | `Schema-typed #main` 显式不支持；`method_dispatch_smoke` 通过低层 BufferBuilder 绕过 |
| `Option<T>`      | ❌ | `UnsupportedFieldType` |
| `Result<T, E>`   | ❌ | `UnsupportedFieldType` |

## 二、`#main` 返回类型

| 类型 | 状态 | 备注 |
| --- | :-: | --- |
| `Int` / `Float` / `Bool` / `Null` | ✅ | 固定槽 |
| `String`        | ✅ | Phase 3.a 输出 |
| `List<Int>`     | ✅ | Phase 3.a 输出 |
| `Schema { ... }`| ✅ | Phase 3.b sub-record + `parity_dict_literal_return` |
| 嵌套 schema     | ✅ | Phase 3.b nested smoke |
| `List<其他>`    | ❌ | 同入参 |
| `Option` / `Result` | ❌ | 同入参 |

## 三、表达式 / 语法

| 特性 | 状态 | 备注 |
| --- | :-: | --- |
| 整数 / 浮点字面量 | ✅ | Phase 1.beta |
| 字符串字面量 | ✅ | Phase 3.a |
| List<Int> 字面量 | ✅ | Phase 3.a |
| Bool 字面量 | ✅ | Phase 3.a |
| 算术运算（`+ - * / %`） | ✅ | i64 / f64 ops |
| 比较运算（`< <= > >= == !=`） | ✅ | Phase 2.c |
| 逻辑运算（`&& || !`） | ✅ | Phase 2.c if/cmp 联动 |
| 三元 `cond ? a : b` | ✅ | Phase 2.c if |
| dict literal | ✅ | Phase 3.b |
| branded dict literal `T { ... }` | ✅ | Phase 3.b |
| dict field access `d.field` | ❌ | "顶层 dict 才能从 root 出来；中间表达式访问 field 未支持" |
| schema-rooted method `s.length()` | ✅ | Phase 4.a/b stdlib + Phase 5 用户方法 |
| 自由调用 stdlib `length(s)` | ✅ | Phase 4.a |
| `with { method() ... }` 用户方法 | ✅ | Phase 5（method_dispatch_smoke） |
| `self.field` / `self.method()` | ✅ | Phase 5 |
| `let x = ... in ...` | ❌ | parser 拒绝 |
| 括号 `(expr)` 后接成员 / 方法 | ❌ | parser 当前不支持 chain |
| comprehension `[x for x in xs]` | ❌ | Phase 4.c |
| `loop` op | ❌ | Phase 4.c |
| `&sibling.path` reference | ❌ | wasm-AOT topo eager 已 inline 解析掉，无 reference 残留 |
| `#default` decorator | ✅ | Phase 3.b topo eager 阶段解析 |
| `#strict` / `#relaxed` 顶部指令 | ✅ | analyzer 阶段处理；wasm 无关 |
| `where` 子句 | ⚠️ | analyzer 已支持；`#strict` + where 的作用域可见性还有 bug（见报告"未来工作"） |

## 四、Stdlib 函数

> wasm-AOT 的 stdlib 与 tree-walker 的 carrier file
> （`crates/relon-analyzer/src/core/string.relon` 等）**命名独立**，
> 详见 bench 报告 §2.3。未来工作里有一条"统一 stdlib 命名"。

| Stdlib op | wasm-AOT | tree-walker | 备注 |
| --- | :-: | :-: | --- |
| `String` 字节长度 | ✅ `length` | ✅ `len` | 同语义不同名 |
| `String.upper` | ❌ | ✅ | tree-walker only |
| `String.lower` | ❌ | ✅ | tree-walker only |
| `String.split` | ❌ | ✅ | tree-walker only |
| `String.replace` | ❌ | ✅ | tree-walker only |
| `String.contains` | ❌ | ✅ | tree-walker only |
| `String.iter` | ❌ | ✅ | tree-walker only |
| `List<Int>.length` | ✅ `list_int_length` | ✅ | 名字不同 |
| `abs(Int) -> Int` | ✅ | ❌ | wasm-AOT only |
| `min(Int, Int)` / `max(Int, Int)` | ✅ | ❌ | wasm-AOT only |
| `is_empty(String)` / `is_empty(List<Int>)` | ✅ | ❌ | wasm-AOT only |
| `list_sum` / `list_map` / `concat` | ❌ | ✅ | tree-walker only（wasm-AOT Phase 4.c 计划） |

## 五、Host 集成

| 特性 | 状态 | 备注 |
| --- | :-: | --- |
| `Evaluator::run_main` | ✅ | Phase 8 |
| `Evaluator::eval(node, scope)` | ❌ | 返回 `Unsupported`（AOT 后无 AST） |
| `Evaluator::eval_root(scope)` | ❌ | 返回 `Unsupported` |
| `Evaluator::force_thunk(thunk)` | ❌ | 返回 `Unsupported`（topo eager 无 live thunk） |
| `Evaluator::invoke_closure(closure, args)` | ❌ | 返回 `Unsupported`（闭包不是头等值） |
| `#native` 函数声明 + 注册 | ✅ | Phase 6 ABI v2 + `host_fns` section |
| capability gating | ⚠️ | grants mask 已接通，但 wasm 侧固定 `i64::MAX`；细粒度待 Phase 9 |
| 错误 traceback | ✅ | Phase 7 `translate_trap` + `relon.uctab` section |
| srcmap 反查（trap PC → source range） | ✅ | Phase 1.gamma `relon.srcmap` section |
| `--backend wasm-aot` CLI 切换 | ✅ | Phase 8 |
| `relon::new_evaluator(src, Backend)` facade | ✅ | Phase 8 |
| 多文件 `#import` workspace 编译 | ❌ | `lower_workspace_single` 名字本身就排除了多文件 |
| `WasmAotEvaluator::from_bytes` 缓存复用 | ✅ | Phase 8 |

## 六、运行时 trap → RuntimeError 映射

Phase 7 完成了以下 trap 翻译（`translate_trap` in `WasmModule`）：

| Wasm trap | RuntimeError | 备注 |
| --- | --- | --- |
| `IntegerDivisionByZero` | `DivisionByZero(range)` | `evaluator_smoke::division_by_zero_matches_tree_walker` |
| `IntegerOverflow` | `NumericOverflow(range)` | Phase 7 |
| `Unreachable` (codegen-emit guard) | `RuntimeError::WasmGuard{...}` | `relon.uctab` 提供 reason label |
| `Unreachable` (其他) | `RuntimeError::WasmTrap(...)` | 兜底 |
| `MemoryOutOfBounds` | `RuntimeError::WasmTrap(...)` | 不应发生，触发即 bug |
| `StackOverflow` | `RuntimeError::WasmTrap(...)` | 编译期已 topo eager |

所有 trap 通过 `relon.srcmap` 反查 wasm PC → 源码 `TokenRange`，
所以 host 拿到的 `RuntimeError` 自带源码定位。

## 七、Bench 总结

详见 `wasm-bench-report-2026-05-16.md`。简短结论：

- wasm-AOT cold start ~2.2 ms（cranelift JIT 主导，对 body 无敏感性）
- wasm-AOT warm invoke ~44 μs（wasmtime instantiate 主导）
- tree-walker warm invoke 在 `arithmetic` 上 2.7 μs，比 wasm-AOT
  warm 快 16 倍

v1 wasm-AOT 优势在沙箱深度 + AOT 缓存可移植，**不在裸跑速度**；后者
等"未来工作"里 Pool-of-Stores 落地后会有显著改善。
