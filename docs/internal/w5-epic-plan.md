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

---

## 实施结果(2026-06-04,P1-P4 全部完成、全绿、四方对齐)

| 阶段 | 结果 | 关键 |
|---|---|---|
| **P1 dict-value 地基** | ✅ | `IrType::Dict`(i32 arena 指针)+ `Op::ConstDict` + probe-friendly 布局(`[count][shape_hash]` + 排序 `[key_off][key_len][value]` 表 + UTF-8 key 池,可二分);两后端 const-pool byte-identical |
| **P2 string-list 物化** | ✅ | `ConstListString`(指针数组)+ ListString 按 int 索引(`keys[i]`→String handle,复用 `EmitTailRecordFromAbsoluteAddr{String}`,不发 trace-only 的 ListGetByIntIdx);cranelift+llvm byte-identical,three-way 对齐 |
| **P3 DictGetByStringKey 静态探针** | ✅ | `d[k]` 降成 **IR-lowered 线性扫描 + 逐字节比较**(纯既有原语,零新运行时符号/零 wasm import → wasm-portable);挂在 `lower_variable` 动态索引分发;not-found→`Trap{IndexOutOfBounds}`(诚实);cranelift 无需改动(原语已支持) |
| **P4 收口** | ✅ | classifier 接受 `#internal` ListString 字段 + `list.sum` 标量分类;map peephole 内联体经 `lower_expr` 在外层 ctx **天然捕获** d/keys 句柄(无需改 peephole),`d[keys[i%10]]` 自动接 P2 索引 + P3 探针;翻 parity 守卫为真值断言 |

**最终达成**:完整 w5 `#main(Int n) -> Dict { #internal d:{a:1..j:10}, #internal keys:["a".."j"], result: list.sum(range(n).map(i => d[keys[i%10]])) }` **真编译到 native + wasm32**,n=10 跑出 `result=55`,**四方对齐**:tree-walk(金标准)/ cranelift / llvm native / **wasm32→wasm-ld→wasmtime**。IR-shape 钉死降出 `ConstDict`+`ConstListString`+探针 op、**不含** `Op::DictGetByStringKey`(静态 codegen 红线)。

**诚实记录**:
- `scope_cut_smoke::w5` 不变 —— 旧 `WasmEvaluator` scope-cut 由其自带文本 classifier 驱动(不查 relon-ir),仍 tree-walk 返回 55,**无误编**。
- P4 的 `list.sum` 分类拓宽**副作用**让 W8 production Dict 也能经旧 `relon-codegen-wasm` 编译;核实其编出**正确值**(n=8→`result=20`,对齐 tree-walk oracle,非静默误编)后,把 `w8_production_dict_source_still_scope_cuts` 从 scope-cut 断言**诚实改为 value-pinned**(仅测试,未动生产代码)。
- 全程 cranelift 零回归;W7 递归闭包守卫仍正确 reject(那是另一独立 epic)。

**新增可编译 op 面**:`ConstDict` / `IrType::Dict` / `ConstListString` / ListString-index / `d[k]` 静态 dict 探针 —— 跨 native + wasm32,three-way oracle 完整。**w5 不再是 wasm 退役阻塞** —— 现在退役 codegen-wasm + wasm-evaluator 零覆盖损失(w4 已补、w5 已 wasm-编译;w7 是独立 epic)。
