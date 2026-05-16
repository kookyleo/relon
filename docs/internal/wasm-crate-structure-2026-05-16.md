# Crate 结构决策：IR-first vs 直接 wasm-codegen（2026-05-16）

> Phase 0 子项 1/8。上游：[`wasm-backend-design-draft.md`](./wasm-backend-design-draft.md)
> "决议：单 crate `relon-wasm-codegen` vs 拆 `relon-bytecode-ir` +
> `relon-wasm-codegen-from-ir`"。
>
> 本文档锁 crate 物理结构和 lowering pipeline 的层次。

## 待答问题

WASM 后端需要把 `AnalyzedTree` lowering 成 wasm bytecode。两种 crate
结构：

- **A 单 crate** `relon-wasm-codegen`：`AnalyzedTree → wasm` 一步到位
- **B IR-first 两段**：新建 `relon-ir`（typed lowering IR + verify + opt
  passes）+ `relon-wasm-codegen-from-ir`（IR → wasm bytecode）

## 决策

**走 B（IR-first 两段）**。

## 推导出的物理结构

```
crates/
  relon-eval-api/                  ← 已有（B0）
    Value / Scope / Thunk / Context / trait Evaluator / ...
  relon-evaluator/                 ← 已有（B0）
    impl Evaluator for TreeWalkEvaluator
  relon-ir/                        ← 新 Phase 0+ crate
    IR types + lowering AnalyzedTree -> IR + verifier
  relon-codegen-wasm/              ← 新 Phase 1+ crate
    IR -> wasm bytecode
    impl Evaluator for WasmAotEvaluator
  ...future...
  relon-codegen-native/            ← 假想 (Phase Z+)
    IR -> cranelift / LLVM
  relon-codegen-js/                ← 假想 (Phase Z+)
    IR -> JavaScript
```

## 理由

### 1. 与已锁的 4 个主决策一致

策略文档 Pillar I 暗示"未来还可能加 native AOT / JS codegen / 直跑
wasm bytecode"。这些后端**共享同一份 lowering 工作**——dict eager
ordering、closure capture snapshot、stdlib bytecode 内联、capability
opcode 插入。**写一次 IR，多个 codegen 复用，比写一次直 wasm codegen
后再 forking 出来便宜**。

### 2. IR 是天然 stable contract

`AnalyzedTree` 表面会随 analyzer feature（新语法、新约束、新错误诊断
等）持续演进。IR 应该**比 AnalyzedTree 更窄**——只描述
"可执行语义"，不带 source location 之外的诊断细节、不带 type
inference 中间状态、不带 schema-rooted dispatch 的多源候选表。

IR 的稳定性让 codegen 不被 analyzer 的 feature flag 颠簸。这也是 Rust
（MIR）/ Swift（SIL）/ Dart（Kernel）/ Java（bytecode）这类成熟编译器
都走 IR 的原因。

### 3. 测试边界清晰

- `relon-ir` 测试：`AnalyzedTree → IR` round-trip 等价（fixture
  driven），verifier 正确捕获 lowering bug
- `relon-codegen-wasm` 测试：`IR → wasm` 可执行性 + binary correctness
- 不混在一起就不会出现"测试 codegen 时其实是 lowering 错"的归因混乱

### 4. Cycle detection 等共用 pass 有家可归

决策 4（lazy thunk → 静态拓扑 + eager）需要的 cycle detection
pass，逻辑上属于 lowering 期。**这个 pass 放 IR 层** ——
所有 codegen target 共享。`Phase 3` 实施时 cycle detection 在
`relon-ir/passes/cycle.rs`，未来 native codegen 不用重写。

### 5. 失败 fallback 是退化，不是返工

如果 Phase 1 实测发现 IR 抽象增加复杂度过大、收益小，**可以把
`relon-ir` 当作"很薄的中间表示"** —— 只保留必需 op codes（call /
const / load / store / br_if / loop / block），不引入 SSA 优化等
重型机制。这就是事实上的"IR-as-thin-as-MVP-needs"，等价于单 crate
方案的成本——但保留了未来扩展接口。

反之如果先走单 crate，后来要拆 IR，需要把 codegen 内部状态全部
mechanically 抽出来，工程成本几倍。

## IR 的初步形状（不在本 ADR 范围，留给 Phase 0 后续）

提一句方向供 Phase 0 后续 ADR 参考：

- **不走 SSA**——MVP 不需要 SSA 的 dataflow 分析能力；从 AST 直接
  lowering 到 stack-based IR（与 wasm 自身 stack-machine 模型一致）
  足够
- **op codes 集合**：load / store / const / call / call_indirect /
  cap_check / br_if / block / loop / return / trap / local.get /
  local.set / dict_emit / list_emit + 算术 + 比较
- **type system**：IR-level 仅区分 `i32 / i64 / f64 / addr`（4 个原生
  类型）；Relon-level type 通过 `addr` 类型 + binary layout offset
  表达。这与 wasm 直接对齐
- **每个 op carry source range**：IR 节点附带 `TokenRange` 以便后续
  custom section srcmap pass（[`wasm-srcmap-section-v1`](./wasm-srcmap-section-v1-2026-05-16.md)
  参考）拷贝过去

## Workspace Cargo.toml 影响

新增两个 `members`：

```toml
[workspace]
members = [
    "crates/*",
]
```

`crates/relon-ir/Cargo.toml` 依赖：

```toml
[dependencies]
relon-eval-api = { workspace = true }   # Value / Scope shared types
relon-parser = { workspace = true }     # TokenRange / Node
relon-analyzer = { workspace = true }   # AnalyzedTree input
thiserror = { workspace = true }
miette = { workspace = true }
```

`crates/relon-codegen-wasm/Cargo.toml` 依赖：

```toml
[dependencies]
relon-eval-api = { workspace = true }
relon-ir = { workspace = true }
wasm-encoder = "0.270"          # bytecode emit
wasmparser = "0.270"            # validation 用
```

未来 `relon-evaluator`（tree-walker）**不需要** 依赖 `relon-ir` —
tree-walker 仍然直接吃 `AnalyzedTree` + Node。IR 只是 codegen 路径
专用。这点很重要：保持 tree-walker 路径轻量。

## 验收 checklist

Phase 0 之后开 Phase 1 smoke test 之前必须就位：

- [ ] `crates/relon-ir/` 骨架 + `Cargo.toml`（lib 占位即可，0 行实际
  逻辑）
- [ ] `crates/relon-codegen-wasm/` 骨架 + `Cargo.toml`
- [ ] workspace Cargo.toml 加 members
- [ ] `cargo build --workspace` 通过（含两个新 empty crate）
- [ ] `cargo test --workspace` 全绿

骨架就位后 Phase 1 才能开 codegen 实施。
