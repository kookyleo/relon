# W5-to-wasm epic:dict-literal + DictGetByStringKey 编译后端落地

- **状态**:开工中(2026-06-04)。用户决策:正式立项,双后端(cranelift+llvm)实现以保 three-way oracle,落地后 w5 真 wasm-编译再退役旧 wasm crate。
- **关联**:[`phase1-execution-plan.md`](./phase1-execution-plan.md)(P3 wasm 现状)。w5 调研结论见该 epic 缘起。
- **目标**:让 `#main(Int n) -> Dict { #internal d:{a:1..j:10}, #internal keys:["a".."j"], result: list.sum(range(n).map(i => d[keys[i%10]])) }`(n=10→`result=55`)**真编译到 native + wasm32**,对齐 tree-walk 金标准,cranelift 不回归。

## 为什么是 epic(6 个未实现 AOT 原语)
w5 单 `result` 字段牵出:① dict-literal 作可捕获值(无 `DictNew`/arena `{str→int}` 物化)② `ConstListString` codegen(cranelift+llvm **都 unsupported**)③ `DictGetByStringKey`(都 unsupported,IR 注明"仅 trace-recorder")④ ListString 按 int 索引(`keys[i%10]`)⑤ 闭包捕获 Dict+ListString 进内联 map loop ⑥ 静态 dict-probe 运行时 ABI(arena dict 布局 + key 比较,wasm32 buffer 协议净新)。**cranelift 亦无这些 op → 无 oracle**,故须双后端 lockstep 实现。

## 阶段 DAG(严格串行,每阶段 three-way 全绿可回退)

| 阶段 | 内容 | 主要文件 | green floor |
|---|---|---|---|
| **P1 dict-value 地基** | relon-ir:dict-literal 作可捕获值(arena `{str→int}` 布局 + dict-value Op + lowering);classifier 接受 Dict-valued `#internal` 字段(暂不解 DictGet) | `relon-ir`(lowering/ir) | workspace 绿 + dict-value 构造单测 + 无回归(旧 scope_cut 仍 tree-walk 直到 P4) |
| **P2 string-list 物化** | `ConstListString` codegen + ListString 按 int 索引,**cranelift ∥ llvm 双实现**(恢复 oracle) | relon-ir lowering + `relon-codegen-cranelift` ∥ `relon-codegen-llvm`(collections) | three-way(tree-walk/cranelift/llvm)对 const-list-string + index 对齐 |
| **P3 DictGetByStringKey 静态路径** | `d[k]` 静态降级(替 tree-walk route)+ arena dict-probe 运行时 ABI,双后端 | relon-ir + 双后端 codegen + 运行时 helper | three-way dict-get 对齐 |
| **P4 捕获进 map loop + 收口** | anon-dict `#internal` Dict/List 捕获进 map peephole 内联体(`lower_expr` 的 Index/dict-key 臂);**翻 w5 守卫**(`aot_wasm_parity.rs` + `scope_cut_smoke.rs`)为 wasm 值对齐 55 | relon-ir peephole + 双后端 + 测试 | w5 经 native + wasm32→wasmtime 跑出 55 对齐 tree-walk + cranelift |

**差分底线**:每阶段 test-harness three-way(tree-walk 金标准 + cranelift + llvm),full workspace 绿(含旧 wasm smokes / scope_cut)。**严禁假绿 / 严禁误编出错值**:cranelift 回归或 wasm 没真出 55 即回退。

## blast radius
- 改共享 relon-ir classifier 让 w5 不再 scope-cut → 旧 `WasmEvaluator` 失去 scope-cut 假设。**P1-P3 阶段 classifier 暂不对旧路径放行**(保 `scope_cut_smoke::w5` 的 TreeWalker 断言);**P4 才翻守卫**,届时旧路径若不支持须仍明确 scope-cut/报错(不误编),诚实更新 scope_cut 测试。
- 不删/不改 `relon-codegen-wasm`/`relon-wasm-evaluator`/`relon-wasm-bindings` 生产代码(epic 期间)。退役是 epic 落地后的独立步。

## 起点
**P1(dict-value 地基,串行)** 先行 → P2 双后端并行 → P3 → P4 收口翻守卫。loop 驱动 + 集成。落地后 → 退役 codegen-wasm + wasm-evaluator(留 bindings)→ 终版汇报。
